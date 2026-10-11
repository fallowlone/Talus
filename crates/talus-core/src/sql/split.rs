//! Splits a script into statements the way an interactive client must:
//! respects strings/comments/dollar quotes (via the lexer), MySQL `DELIMITER`,
//! and `BEGIN … END` bodies of CREATE TRIGGER/FUNCTION/PROCEDURE without a delimiter change.

use super::lexer::{Token, TokenKind, tokenize};
use crate::model::Engine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement<'a> {
    /// Statement text without the trailing delimiter.
    pub text: &'a str,
    /// Byte offset of the first significant token.
    pub start: usize,
    /// Byte offset just past the statement text (before the delimiter).
    pub end: usize,
    /// Byte offset just past the delimiter (== `end` if none).
    pub end_with_delimiter: usize,
    /// 0-based line of `start`.
    pub line: usize,
}

struct Builder<'a> {
    src: &'a str,
    out: Vec<Statement<'a>>,
    start: Option<usize>,
    last_sig_end: usize,
    /// Newlines counted in `src[..counted_upto]` (statements arrive in order → linear time).
    counted_upto: usize,
    lines: usize,
}

impl<'a> Builder<'a> {
    fn finish(&mut self, delim_end: usize) {
        if let Some(start) = self.start.take() {
            let end = self.last_sig_end;
            self.lines += self.src[self.counted_upto..start].bytes().filter(|&b| b == b'\n').count();
            self.counted_upto = start;
            let line = self.lines;
            self.out.push(Statement {
                text: &self.src[start..end],
                start,
                end,
                end_with_delimiter: delim_end.max(end),
                line,
            });
        }
    }
}

fn word_eq(src: &str, t: &Token, w: &str) -> bool {
    t.kind == TokenKind::Word && t.text(src).eq_ignore_ascii_case(w)
}

/// `GO` (or `GO 5`) with nothing else on its line.
fn alone_on_line(src: &str, t: &Token) -> bool {
    let before = &src[..t.start];
    let line_start = before.rfind('\n').map_or(0, |p| p + 1);
    if !before[line_start..].trim().is_empty() {
        return false;
    }
    let rest = &src[t.end..];
    let line = rest.split('\n').next().unwrap_or("").trim();
    line.is_empty() || line.chars().all(|c| c.is_ascii_digit())
}

pub fn split(src: &str, engine: Engine) -> Vec<Statement<'_>> {
    let tokens = tokenize(src, engine);
    let mut b = Builder { src, out: Vec::new(), start: None, last_sig_end: 0, counted_upto: 0, lines: 0 };
    let mut delimiter = String::from(";");
    // Block tracking for CREATE … BEGIN … END bodies.
    let mut first_word_create = false;
    let mut block_mode = false;
    let mut depth: i32 = 0;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t.kind.is_trivia() {
            i += 1;
            continue;
        }
        // MySQL client-side DELIMITER directive (only at statement start).
        if engine == Engine::MySql && b.start.is_none() && word_eq(src, &t, "delimiter") {
            let line_end = src[t.end..].find('\n').map_or(src.len(), |p| t.end + p);
            let new = src[t.end..line_end].trim();
            if !new.is_empty() {
                delimiter = new.to_string();
            }
            while i < tokens.len() && tokens[i].start < line_end {
                i += 1;
            }
            continue;
        }
        // T-SQL batch separator: `GO` alone on its line (optionally followed by a repeat count).
        if engine == Engine::SqlServer && word_eq(src, &t, "go") && alone_on_line(src, &t) {
            let line_end = src[t.end..].find('\n').map_or(src.len(), |p| t.end + p);
            b.finish(line_end);
            first_word_create = false;
            block_mode = false;
            depth = 0;
            while i < tokens.len() && tokens[i].start < line_end {
                i += 1;
            }
            continue;
        }
        // Delimiter match (outside strings/comments by construction).
        let at_delim = if delimiter == ";" {
            t.kind == TokenKind::Punct && t.text(src) == ";"
        } else {
            !matches!(t.kind, TokenKind::String | TokenKind::QuotedIdent)
                && src[t.start..].starts_with(delimiter.as_str())
        };
        if at_delim && depth <= 0 {
            let delim_end = t.start + delimiter.len();
            b.finish(delim_end);
            first_word_create = false;
            block_mode = false;
            depth = 0;
            while i < tokens.len() && tokens[i].start < delim_end {
                i += 1;
            }
            continue;
        }
        if b.start.is_none() {
            b.start = Some(t.start);
            first_word_create = word_eq(src, &t, "create");
        }
        if engine == Engine::SqlServer && t.kind == TokenKind::Word {
            // BEGIN…END / CASE…END / TRY…CATCH may hold semicolons anywhere in a batch, not only in CREATE bodies.
            let next_word = tokens[i + 1..].iter().find(|x| !x.kind.is_trivia());
            if word_eq(src, &t, "begin") {
                let starts_tx = next_word
                    .is_some_and(|n| ["tran", "transaction", "distributed"].iter().any(|k| word_eq(src, n, k)));
                if !starts_tx {
                    depth += 1;
                }
            } else if word_eq(src, &t, "case") {
                depth += 1;
            } else if word_eq(src, &t, "end") {
                depth -= 1;
            }
        } else if t.kind == TokenKind::Word && delimiter == ";" {
            let w = t.text(src);
            if first_word_create && !block_mode {
                block_mode = ["trigger", "function", "procedure", "event"].iter().any(|k| w.eq_ignore_ascii_case(k));
            } else if block_mode {
                if w.eq_ignore_ascii_case("begin") || w.eq_ignore_ascii_case("case") {
                    depth += 1;
                } else if w.eq_ignore_ascii_case("end") {
                    let next_word = tokens[i + 1..].iter().find(|x| !x.kind.is_trivia());
                    let closes_other =
                        next_word.is_some_and(|n| ["if", "loop", "while", "repeat"].iter().any(|k| word_eq(src, n, k)));
                    if !closes_other {
                        depth -= 1;
                    }
                }
            }
        }
        b.last_sig_end = t.end;
        i += 1;
    }
    b.finish(src.len());
    b.out
}

/// The statement under the cursor: the one whose range (including its delimiter) contains
/// `cursor`, else the closest statement that starts before it.
pub fn statement_at(src: &str, engine: Engine, cursor: usize) -> Option<Statement<'_>> {
    let stmts = split(src, engine);
    if let Some(s) = stmts.iter().find(|s| cursor >= s.start && cursor <= s.end_with_delimiter) {
        return Some(s.clone());
    }
    stmts.into_iter().rev().find(|s| s.start <= cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(src: &str, e: Engine) -> Vec<&str> {
        split(src, e).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn basic_split_skips_empty_and_comments() {
        let src = "select 1; ;\n-- only comment\n select ';' as x ; /* c */";
        assert_eq!(texts(src, Engine::Sqlite), vec!["select 1", "select ';' as x"]);
    }

    #[test]
    fn last_statement_without_delimiter_and_lines() {
        let s = split("select 1;\n\nselect 2", Engine::Postgres);
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].text, "select 2");
        assert_eq!(s[1].line, 2);
    }

    #[test]
    fn postgres_dollar_body_not_split() {
        let src = "create function f() returns int as $$ begin; return 1; end $$ language plpgsql; select f();";
        let t = texts(src, Engine::Postgres);
        assert_eq!(t.len(), 2);
        assert!(t[0].ends_with("language plpgsql"));
    }

    #[test]
    fn mysql_delimiter_directive() {
        let src = "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END //\nDELIMITER ;\nCALL p();";
        let t = texts(src, Engine::MySql);
        assert_eq!(t, vec!["CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END", "CALL p()"]);
    }

    #[test]
    fn mysql_procedure_without_delimiter_uses_block_depth() {
        let src =
            "CREATE PROCEDURE p() BEGIN IF 1 THEN SELECT 1; END IF; SELECT CASE WHEN 1 THEN 2 END; END; SELECT 3;";
        let t = texts(src, Engine::MySql);
        assert_eq!(t.len(), 2, "{t:?}");
        assert_eq!(t[1], "SELECT 3");
    }

    #[test]
    fn sqlite_trigger_body() {
        let src = "CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a=1; DELETE FROM u; END; select 1";
        let t = texts(src, Engine::Sqlite);
        assert_eq!(t.len(), 2);
        assert!(t[0].ends_with("END"));
    }

    #[test]
    fn plain_begin_transaction_still_splits() {
        assert_eq!(texts("BEGIN; UPDATE t SET a=1; COMMIT;", Engine::Postgres).len(), 3);
    }

    #[test]
    fn statement_at_cursor() {
        let src = "select 1;\nselect 2;\n\nselect 3";
        assert_eq!(statement_at(src, Engine::Sqlite, 0).unwrap().text, "select 1");
        assert_eq!(statement_at(src, Engine::Sqlite, 12).unwrap().text, "select 2");
        // cursor right after the delimiter still belongs to that statement
        assert_eq!(statement_at(src, Engine::Sqlite, 19).unwrap().text, "select 2");
        // blank line between: closest preceding
        assert_eq!(statement_at(src, Engine::Sqlite, 20).unwrap().text, "select 2");
        assert_eq!(statement_at(src, Engine::Sqlite, src.len()).unwrap().text, "select 3");
    }
}
