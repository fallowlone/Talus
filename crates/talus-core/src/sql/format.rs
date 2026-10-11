//! Token-based SQL beautifier and comment toggling. Never changes the meaning of a script:
//! strings, quoted identifiers and comments are copied verbatim; only whitespace and keyword
//! case change.

use super::lexer::{Token, TokenKind, tokenize};
use crate::model::Engine;

/// Keywords that start a new line at the current indentation.
const CLAUSES: &[&str] = &[
    "select",
    "from",
    "where",
    "group",
    "order",
    "having",
    "limit",
    "offset",
    "union",
    "intersect",
    "except",
    "values",
    "set",
    "returning",
    "window",
    "join",
    "left",
    "right",
    "inner",
    "full",
    "cross",
    "natural",
    "on",
];

/// Words that are upper-cased (keywords commonly written in upper case).
const UPPER: &[&str] = &[
    "add",
    "all",
    "alter",
    "and",
    "as",
    "asc",
    "between",
    "by",
    "case",
    "cast",
    "check",
    "column",
    "constraint",
    "create",
    "cross",
    "default",
    "delete",
    "desc",
    "distinct",
    "drop",
    "else",
    "end",
    "exists",
    "foreign",
    "from",
    "full",
    "group",
    "having",
    "if",
    "in",
    "index",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "key",
    "left",
    "like",
    "ilike",
    "limit",
    "natural",
    "not",
    "null",
    "offset",
    "on",
    "or",
    "order",
    "outer",
    "primary",
    "references",
    "returning",
    "right",
    "select",
    "set",
    "table",
    "then",
    "union",
    "unique",
    "update",
    "using",
    "values",
    "view",
    "when",
    "where",
    "with",
    "except",
    "window",
    "over",
    "partition",
    "recursive",
    "true",
    "false",
];

fn lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// Beautifies a script: keywords upper-cased, one clause per line, statements separated by
/// a blank line. Text inside strings/comments/quoted identifiers is untouched.
pub fn format_sql(src: &str, engine: Engine) -> String {
    let tokens: Vec<Token> = tokenize(src, engine);
    let last_significant = tokens.iter().rposition(|t| !t.kind.is_trivia());
    let mut out = String::with_capacity(src.len() + src.len() / 4);
    let mut depth: usize = 0;
    let mut prev: Option<(TokenKind, String)> = None;
    let mut pending_space = false;
    let newline = |out: &mut String, depth: usize| {
        while out.ends_with(' ') {
            out.pop();
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&"    ".repeat(depth));
    };
    for (i, t) in tokens.iter().enumerate() {
        let text = t.text(src);
        match t.kind {
            TokenKind::Whitespace => {
                pending_space = true;
                continue;
            }
            TokenKind::LineComment => {
                if pending_space && !out.is_empty() && !out.ends_with('\n') {
                    out.push(' ');
                }
                out.push_str(text.trim_end());
                newline(&mut out, depth);
                pending_space = false;
                prev = Some((t.kind, text.to_string()));
                continue;
            }
            _ => {}
        }
        let lw = lower(text);
        let is_word = t.kind == TokenKind::Word;
        let prev_word = prev.as_ref().filter(|(k, _)| *k == TokenKind::Word).map(|(_, w)| lower(w));
        // Clause starts get their own line (`LEFT JOIN`, `ORDER BY` stay together).
        let joins_prev =
            matches!(prev_word.as_deref(), Some("left" | "right" | "inner" | "full" | "cross" | "natural" | "outer"))
                && matches!(lw.as_str(), "join" | "outer");
        let clause = is_word
            && CLAUSES.contains(&lw.as_str())
            && !joins_prev
            && !matches!(prev_word.as_deref(), Some("union" | "intersect" | "except"));
        let is_semicolon = t.kind == TokenKind::Punct && text == ";";
        let is_op = t.kind == TokenKind::Punct && matches!(text, "=" | "<" | ">" | "!");
        let prev_op =
            prev.as_ref().is_some_and(|(k, p)| *k == TokenKind::Punct && matches!(p.as_str(), "=" | "<" | ">" | "!"));
        // Space around comparison operators (`a=b` → `a = b`, `>=` stays together).
        if is_op != prev_op && prev.is_some() && !matches!(prev.as_ref().map(|(_, p)| p.as_str()), Some("(" | ".")) {
            pending_space = true;
        }
        if clause && !out.is_empty() && !out.trim_end().ends_with('(') {
            newline(&mut out, depth);
        } else if pending_space
            && !out.is_empty()
            && !out.ends_with('\n')
            && !out.ends_with(' ')
            && !is_semicolon
            && text != ","
            && text != ")"
            && !out.ends_with('(')
            && text != "."
            && !out.ends_with('.')
        {
            out.push(' ');
        } else if text == "," || text == ")" || is_semicolon || text == "." {
            while out.ends_with(' ') {
                out.pop();
            }
        }
        pending_space = false;
        if is_word && UPPER.contains(&lw.as_str()) {
            out.push_str(&text.to_ascii_uppercase());
        } else {
            out.push_str(text);
        }
        match text {
            "(" => depth += 1,
            ")" => depth = depth.saturating_sub(1),
            "," => out.push(' '),
            _ => {}
        }
        if is_semicolon {
            depth = 0;
            let more = last_significant.is_some_and(|last| i < last);
            if more {
                out.push_str("\n\n");
            }
        }
        prev = Some((t.kind, text.to_string()));
    }
    let mut s = out.trim_end().to_string();
    // Normalise ", " that ended up before a newline.
    while s.contains(", \n") {
        s = s.replace(", \n", ",\n");
    }
    s.push('\n');
    s
}

/// Toggles `-- ` line comments on every line touched by `range` (byte offsets).
/// Returns the new text and the new byte range covering the same lines.
pub fn toggle_comment(text: &str, range: std::ops::Range<usize>) -> (String, std::ops::Range<usize>) {
    let start = text[..range.start.min(text.len())].rfind('\n').map_or(0, |p| p + 1);
    let end_probe =
        if range.end > range.start && text[..range.end].ends_with('\n') { range.end - 1 } else { range.end };
    let end = text[end_probe.min(text.len())..].find('\n').map_or(text.len(), |p| end_probe + p);
    let block = &text[start..end];
    let lines: Vec<&str> = block.split('\n').collect();
    let all_commented = lines.iter().filter(|l| !l.trim().is_empty()).all(|l| l.trim_start().starts_with("--"));
    let new_lines: Vec<String> = lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                return (*l).to_string();
            }
            let indent = l.len() - l.trim_start().len();
            let (ws, rest) = l.split_at(indent);
            if all_commented {
                let rest = rest.strip_prefix("--").unwrap_or(rest);
                format!("{ws}{}", rest.strip_prefix(' ').unwrap_or(rest))
            } else {
                format!("{ws}-- {rest}")
            }
        })
        .collect();
    let replaced = new_lines.join("\n");
    let new_text = format!("{}{}{}", &text[..start], replaced, &text[end..]);
    (new_text, start..start + replaced.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_clauses_and_keywords() {
        let s = format_sql(
            "select a,b from t left join u on t.id=u.id where a > 1 and b like 'x;y' order by a desc;select 1",
            Engine::Postgres,
        );
        assert_eq!(
            s,
            "SELECT a, b\nFROM t\nLEFT JOIN u\nON t.id = u.id\nWHERE a > 1 AND b LIKE 'x;y'\nORDER BY a DESC;\n\nSELECT 1\n"
        );
    }

    #[test]
    fn preserves_strings_comments_and_quoted_identifiers() {
        let src = "select \"From\" , 'select  from' -- keep this\nfrom t";
        let s = format_sql(src, Engine::Postgres);
        assert!(s.contains("\"From\""));
        assert!(s.contains("'select  from'"));
        assert!(s.contains("-- keep this\n"));
        // Same token stream apart from whitespace/case.
        let norm = |x: &str| {
            tokenize(x, Engine::Postgres)
                .iter()
                .filter(|t| t.kind != TokenKind::Whitespace)
                .map(|t| t.text(x).to_lowercase())
                .collect::<Vec<_>>()
        };
        assert_eq!(norm(src), norm(&s));
    }

    #[test]
    fn subquery_parenthesis_and_function_calls() {
        let s = format_sql("select count(*) from (select id from t) x", Engine::Sqlite);
        assert_eq!(s, "SELECT count(*)\nFROM (SELECT id\n    FROM t) x\n");
    }

    #[test]
    fn toggles_comments_on_touched_lines() {
        let text = "select 1;\n  select 2;\nselect 3;";
        let (t, r) = toggle_comment(text, 3..14);
        assert_eq!(t, "-- select 1;\n  -- select 2;\nselect 3;");
        assert_eq!(&t[r.clone()], "-- select 1;\n  -- select 2;");
        let (back, _) = toggle_comment(&t, r);
        assert_eq!(back, text);
        // Cursor only (empty range) toggles the current line.
        let (t2, _) = toggle_comment(text, 25..25);
        assert_eq!(t2, "select 1;\n  select 2;\n-- select 3;");
    }
}
