//! Dialect-aware SQL tokenizer. Shared by the statement splitter, the syntax highlighter
//! and completion-context detection, so all three agree on what is a string or comment.

use crate::model::Engine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Whitespace,
    LineComment,
    BlockComment,
    /// String literal (including PG dollar-quoted bodies and MySQL double-quoted strings).
    String,
    QuotedIdent,
    Number,
    Word,
    /// Bind parameter such as `$1`, `?`, `:name`.
    Param,
    Punct,
}

impl TokenKind {
    pub fn is_trivia(self) -> bool {
        matches!(self, TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

impl Token {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }
}

fn is_word_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

pub fn tokenize(src: &str, engine: Engine) -> Vec<Token> {
    let bytes = src.as_bytes();
    let mut tokens = Vec::with_capacity(src.len() / 4);
    let mut pos = 0;
    while pos < bytes.len() {
        let start = pos;
        let (kind, end) = next_token(bytes, pos, engine);
        debug_assert!(end > start);
        pos = end;
        tokens.push(Token { kind, start, end });
    }
    tokens
}

fn next_token(b: &[u8], pos: usize, engine: Engine) -> (TokenKind, usize) {
    let c = b[pos];
    let next = b.get(pos + 1).copied();
    match c {
        _ if c.is_ascii_whitespace() => (TokenKind::Whitespace, scan_while(b, pos, |x| x.is_ascii_whitespace())),
        b'-' if next == Some(b'-') && mysql_dash_comment_ok(b, pos, engine) => {
            (TokenKind::LineComment, scan_line(b, pos))
        }
        b'#' if engine == Engine::MySql => (TokenKind::LineComment, scan_line(b, pos)),
        b'/' if next == Some(b'*') => (TokenKind::BlockComment, scan_block_comment(b, pos, engine)),
        b'\'' => (TokenKind::String, scan_quoted(b, pos, b'\'', engine == Engine::MySql)),
        b'"' if engine == Engine::MySql => (TokenKind::String, scan_quoted(b, pos, b'"', true)),
        b'"' => (TokenKind::QuotedIdent, scan_quoted(b, pos, b'"', false)),
        b'`' if engine != Engine::Postgres => (TokenKind::QuotedIdent, scan_quoted(b, pos, b'`', false)),
        b'[' if engine == Engine::Sqlite => {
            let end = b[pos + 1..].iter().position(|&x| x == b']').map_or(b.len(), |i| pos + i + 2);
            (TokenKind::QuotedIdent, end)
        }
        // `[name]` where `]]` stands for a literal `]`.
        b'[' if engine == Engine::SqlServer => {
            let mut i = pos + 1;
            while i < b.len() {
                if b[i] == b']' {
                    if b.get(i + 1) == Some(&b']') {
                        i += 2;
                        continue;
                    }
                    return (TokenKind::QuotedIdent, i + 1);
                }
                i += 1;
            }
            (TokenKind::QuotedIdent, b.len())
        }
        // T-SQL variables and system functions: @name, @@ROWCOUNT.
        b'@' if engine == Engine::SqlServer && next.is_some_and(|x| is_word_start(x) || x == b'@') => {
            (TokenKind::Param, scan_while(b, pos + 1, |x| is_word_char(x) || x == b'@'))
        }
        b'$' if engine == Engine::Postgres => scan_dollar(b, pos, true),
        // Snowflake procedure bodies quote with `$$`; `$1` is a staged-file column, not a bind.
        b'$' if engine == Engine::Snowflake => scan_dollar(b, pos, false),
        b'?' => (TokenKind::Param, scan_while(b, pos + 1, |x| x.is_ascii_digit())),
        b':' if engine == Engine::Sqlite && next.is_some_and(is_word_start) => {
            (TokenKind::Param, scan_while(b, pos + 1, is_word_char))
        }
        // Oracle binds: `:name` and `:1`.
        b':' if engine == Engine::Oracle && next.is_some_and(is_word_char) => {
            (TokenKind::Param, scan_while(b, pos + 1, is_word_char))
        }
        b'0'..=b'9' => (TokenKind::Number, scan_number(b, pos)),
        b'.' if next.is_some_and(|x| x.is_ascii_digit()) => (TokenKind::Number, scan_number(b, pos)),
        _ if is_word_start(c) => {
            let end = scan_while(b, pos, is_word_char);
            // PostgreSQL escape string E'...' (backslash escapes).
            if engine == Engine::Postgres && end == pos + 1 && (c == b'E' || c == b'e') && b.get(end) == Some(&b'\'') {
                return (TokenKind::String, scan_quoted(b, end, b'\'', true));
            }
            // Oracle alternative quoting: q'[it's]', nq'{...}'.
            if engine == Engine::Oracle
                && end - pos <= 2
                && b[pos..end].eq_ignore_ascii_case(if end - pos == 1 { b"q" } else { b"nq" })
                && b.get(end) == Some(&b'\'')
                && b.get(end + 1).is_some_and(|d| !d.is_ascii_whitespace())
            {
                return (TokenKind::String, scan_q_quoted(b, end));
            }
            (TokenKind::Word, end)
        }
        _ => {
            // Advance one full UTF-8 char for unknown punctuation.
            let len = utf8_len(c);
            (TokenKind::Punct, (pos + len).min(b.len()))
        }
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// MySQL only treats `--` as a comment when followed by whitespace/control or end of input.
fn mysql_dash_comment_ok(b: &[u8], pos: usize, engine: Engine) -> bool {
    engine != Engine::MySql || b.get(pos + 2).is_none_or(|x| x.is_ascii_whitespace() || x.is_ascii_control())
}

fn scan_while(b: &[u8], mut pos: usize, f: impl Fn(u8) -> bool) -> usize {
    while pos < b.len() && f(b[pos]) {
        pos += 1;
    }
    pos
}

fn scan_line(b: &[u8], pos: usize) -> usize {
    b[pos..].iter().position(|&x| x == b'\n').map_or(b.len(), |i| pos + i)
}

fn scan_block_comment(b: &[u8], pos: usize, engine: Engine) -> usize {
    let nested = matches!(engine, Engine::Postgres | Engine::SqlServer);
    let mut depth = 0usize;
    let mut i = pos;
    while i + 1 < b.len() {
        if b[i] == b'/' && b[i + 1] == b'*' && (nested || depth == 0) {
            depth += 1;
            i += 2;
        } else if b[i] == b'*' && b[i + 1] == b'/' {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// Scans a quoted run starting at `pos` (the opening quote). Doubled quotes escape;
/// optionally backslash escapes. Unterminated input runs to the end.
fn scan_quoted(b: &[u8], pos: usize, quote: u8, backslash: bool) -> usize {
    let mut i = pos + 1;
    while i < b.len() {
        match b[i] {
            b'\\' if backslash => i += 2,
            x if x == quote => {
                if b.get(i + 1) == Some(&quote) {
                    i += 2;
                } else {
                    return i + 1;
                }
            }
            _ => i += 1,
        }
    }
    b.len()
}

/// Oracle `q'<d>…<d'>'` literal; `quote` is the position of the opening `'`. The closing delimiter
/// mirrors brackets (`[` `]`, `{` `}`, `(` `)`, `<` `>`) and is the same character otherwise.
fn scan_q_quoted(b: &[u8], quote: usize) -> usize {
    let close = match b[quote + 1] {
        b'[' => b']',
        b'{' => b'}',
        b'(' => b')',
        b'<' => b'>',
        other => other,
    };
    let mut i = quote + 2;
    while i + 1 < b.len() {
        if b[i] == close && b[i + 1] == b'\'' {
            return i + 2;
        }
        i += 1;
    }
    b.len()
}

fn scan_dollar(b: &[u8], pos: usize, positional_param: bool) -> (TokenKind, usize) {
    // `$1` positional parameter.
    if positional_param && b.get(pos + 1).is_some_and(|x| x.is_ascii_digit()) {
        return (TokenKind::Param, scan_while(b, pos + 1, |x| x.is_ascii_digit()));
    }
    // `$tag$ ... $tag$` where tag is empty or an identifier.
    let tag_end = if b.get(pos + 1).is_some_and(|&x| is_word_start(x) && x < 0x80) {
        scan_while(b, pos + 1, |x| x.is_ascii_alphanumeric() || x == b'_')
    } else {
        pos + 1
    };
    if b.get(tag_end) != Some(&b'$') {
        return (TokenKind::Punct, pos + 1);
    }
    let tag = &b[pos..=tag_end];
    let body_start = tag_end + 1;
    let end = b[body_start..].windows(tag.len()).position(|w| w == tag).map_or(b.len(), |i| body_start + i + tag.len());
    (TokenKind::String, end)
}

fn scan_number(b: &[u8], pos: usize) -> usize {
    let mut i = pos;
    while i < b.len() {
        let x = b[i];
        let exp_sign = (x == b'+' || x == b'-')
            && i > pos
            && matches!(b[i - 1], b'e' | b'E')
            && b[pos].is_ascii_digit()
            && !b[pos..i].starts_with(b"0x");
        if x.is_ascii_alphanumeric() || x == b'.' || x == b'_' || exp_sign {
            i += 1;
        } else {
            break;
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str, e: Engine) -> Vec<(TokenKind, &str)> {
        tokenize(src, e)
            .into_iter()
            .filter(|t| t.kind != TokenKind::Whitespace)
            .map(|t| (t.kind, t.text(src)))
            .collect()
    }

    #[test]
    fn strings_comments_and_idents() {
        let src = "select 'it''s', \"Col\" -- c;\n/* a; */ from t;";
        let k = kinds(src, Engine::Postgres);
        assert_eq!(k[1], (TokenKind::String, "'it''s'"));
        assert_eq!(k[3], (TokenKind::QuotedIdent, "\"Col\""));
        assert_eq!(k[4], (TokenKind::LineComment, "-- c;"));
        assert_eq!(k[5], (TokenKind::BlockComment, "/* a; */"));
    }

    #[test]
    fn postgres_dollar_quotes_and_params() {
        let src = "do $fn$ begin; end $fn$; select $1, $$x;y$$";
        let k = kinds(src, Engine::Postgres);
        assert_eq!(k[1], (TokenKind::String, "$fn$ begin; end $fn$"));
        assert!(k.contains(&(TokenKind::Param, "$1")));
        assert!(k.contains(&(TokenKind::String, "$$x;y$$")));
    }

    #[test]
    fn postgres_nested_comments_and_escape_strings() {
        let k = kinds("/* a /* b */ c */ E'x\\'y'", Engine::Postgres);
        assert_eq!(k[0], (TokenKind::BlockComment, "/* a /* b */ c */"));
        assert_eq!(k[1], (TokenKind::String, "E'x\\'y'"));
    }

    #[test]
    fn mysql_specifics() {
        let k = kinds("select `a``b`, \"s\\\"x\", 'a\\'b' # hash\n--x", Engine::MySql);
        assert_eq!(k[1], (TokenKind::QuotedIdent, "`a``b`"));
        assert_eq!(k[3], (TokenKind::String, "\"s\\\"x\""));
        assert_eq!(k[5], (TokenKind::String, "'a\\'b'"));
        assert_eq!(k[6], (TokenKind::LineComment, "# hash"));
        // `--x` without a space is not a comment in MySQL.
        assert_eq!(k[7], (TokenKind::Punct, "-"));
    }

    #[test]
    fn oracle_q_quotes_and_binds() {
        let k = kinds("select q'[it's; ]' , nq'{a}', Q'!x!', :name, :1 from dual", Engine::Oracle);
        assert_eq!(k[1], (TokenKind::String, "q'[it's; ]'"));
        assert_eq!(k[3], (TokenKind::String, "nq'{a}'"));
        assert_eq!(k[5], (TokenKind::String, "Q'!x!'"));
        assert_eq!(k[7], (TokenKind::Param, ":name"));
        assert_eq!(k[9], (TokenKind::Param, ":1"));
        // a plain identifier that happens to be `q` is not a literal
        let k = kinds("select q from t", Engine::Oracle);
        assert_eq!(k[1], (TokenKind::Word, "q"));
    }

    #[test]
    fn unicode_and_unterminated() {
        let src = "select имя, 'незакрыт";
        let k = kinds(src, Engine::Sqlite);
        assert_eq!(k[1], (TokenKind::Word, "имя"));
        assert_eq!(k[3], (TokenKind::String, "'незакрыт"));
        // tokens cover the input exactly
        let total: usize = tokenize(src, Engine::Sqlite).iter().map(|t| t.end - t.start).sum();
        assert_eq!(total, src.len());
    }

    #[test]
    fn numbers() {
        let k = kinds("1.5e+3 0x1F .5", Engine::Sqlite);
        assert_eq!(k, vec![(TokenKind::Number, "1.5e+3"), (TokenKind::Number, "0x1F"), (TokenKind::Number, ".5")]);
    }
}
