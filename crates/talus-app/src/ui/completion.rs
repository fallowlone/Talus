//! Context-aware SQL completion: decides what is being typed (keyword, table, `alias.column`)
//! and ranks candidates from cached metadata. Pure functions, unit-tested.

use std::collections::HashMap;

use talus_core::model::{Engine, ObjectKind};
use talus_core::sql::lexer::{TokenKind, tokenize};
use talus_core::sql::split;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    /// Byte offset where the word being completed starts.
    pub start: usize,
    pub prefix: String,
    /// Identifier before a `.` (alias, table or schema), unquoted.
    pub qualifier: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Column,
    Table,
    View,
    Schema,
    Keyword,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    pub kind: Kind,
    pub detail: String,
}

/// Metadata visible to the editor's target database.
#[derive(Debug, Default, Clone)]
pub struct MetaView {
    /// (name, kind) for tables/views; for PostgreSQL names of the default schema.
    pub objects: Vec<(String, ObjectKind)>,
    pub schemas: Vec<String>,
    /// lower-case table name → (column, type).
    pub columns: HashMap<String, Vec<(String, String)>>,
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    let b = t.as_bytes();
    if b.len() >= 2 && matches!((b[0], b[b.len() - 1]), (b'"', b'"') | (b'`', b'`') | (b'[', b']')) {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

/// What is being completed at `cursor`. `manual` (⌃Space) also allows an empty prefix.
pub fn context_at(text: &str, cursor: usize, engine: Engine, manual: bool) -> Option<Context> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];
    let tokens = tokenize(before, engine);
    let last = tokens.last()?;
    let (start, prefix, qual_idx) = match last.kind {
        TokenKind::Word => (last.start, last.text(before).to_string(), tokens.len().checked_sub(2)),
        TokenKind::Punct if last.text(before) == "." => (cursor, String::new(), Some(tokens.len() - 1)),
        TokenKind::Whitespace if manual => (cursor, String::new(), None),
        _ => return None,
    };
    let qualifier = qual_idx.and_then(|i| {
        let dot = tokens.get(i)?;
        if dot.kind != TokenKind::Punct || dot.text(before) != "." {
            return None;
        }
        let q = tokens.get(i.checked_sub(1)?)?;
        matches!(q.kind, TokenKind::Word | TokenKind::QuotedIdent).then(|| unquote(q.text(before)))
    });
    if prefix.is_empty() && qualifier.is_none() && !manual {
        return None;
    }
    Some(Context { start, prefix, qualifier })
}

const ALIAS_STOP: &[&str] = &[
    "where",
    "on",
    "join",
    "left",
    "right",
    "inner",
    "outer",
    "full",
    "cross",
    "natural",
    "group",
    "order",
    "limit",
    "having",
    "union",
    "set",
    "values",
    "using",
    "window",
    "offset",
    "returning",
    "as",
];

/// alias/table (lower-case) → table name, for tables referenced in `statement`.
pub fn aliases(statement: &str, engine: Engine) -> HashMap<String, String> {
    let toks: Vec<_> = tokenize(statement, engine).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let mut map = HashMap::new();
    let mut i = 0;
    while i < toks.len() {
        let w = toks[i].text(statement).to_ascii_lowercase();
        let follows_from = matches!(w.as_str(), "from" | "join" | "update" | "into") || (w == "," && !map.is_empty());
        if follows_from && i + 1 < toks.len() && matches!(toks[i + 1].kind, TokenKind::Word | TokenKind::QuotedIdent) {
            // Possibly qualified name a.b.c — take the last part.
            let mut j = i + 1;
            let mut name = unquote(toks[j].text(statement));
            while j + 2 < toks.len()
                && toks[j + 1].text(statement) == "."
                && matches!(toks[j + 2].kind, TokenKind::Word | TokenKind::QuotedIdent)
            {
                j += 2;
                name = unquote(toks[j].text(statement));
            }
            map.insert(name.to_lowercase(), name.clone());
            let mut k = j + 1;
            if toks.get(k).is_some_and(|t| t.text(statement).eq_ignore_ascii_case("as")) {
                k += 1;
            }
            if let Some(a) = toks.get(k).filter(|t| matches!(t.kind, TokenKind::Word | TokenKind::QuotedIdent)) {
                let alias = unquote(a.text(statement));
                if !ALIAS_STOP.contains(&alias.to_ascii_lowercase().as_str()) {
                    map.insert(alias.to_lowercase(), name.clone());
                }
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
    map
}

/// Candidates for `ctx`, best first. Also returns tables whose columns are needed but not loaded.
pub fn candidates(
    text: &str,
    cursor: usize,
    engine: Engine,
    ctx: &Context,
    meta: &MetaView,
    keywords: &[&str],
) -> (Vec<Candidate>, Vec<String>) {
    let statement = split::statement_at(text, engine, cursor).map(|s| s.text).unwrap_or(text);
    let refs = aliases(statement, engine);
    let mut need = Vec::new();
    let mut out: Vec<Candidate> = Vec::new();
    let add_columns =
        |table: &str, out: &mut Vec<Candidate>, need: &mut Vec<String>| match meta.columns.get(&table.to_lowercase()) {
            Some(cols) => out.extend(cols.iter().map(|(c, t)| Candidate {
                text: c.clone(),
                kind: Kind::Column,
                detail: format!("{table} · {t}"),
            })),
            None => {
                if meta.objects.iter().any(|(n, _)| n.eq_ignore_ascii_case(table)) {
                    need.push(table.to_string());
                }
            }
        };
    match &ctx.qualifier {
        Some(q) => {
            let ql = q.to_lowercase();
            if let Some(table) = refs
                .get(&ql)
                .cloned()
                .or_else(|| meta.objects.iter().find(|(n, _)| n.to_lowercase() == ql).map(|(n, _)| n.clone()))
            {
                add_columns(&table, &mut out, &mut need);
            } else if meta.schemas.iter().any(|s| s.to_lowercase() == ql) {
                out.extend(meta.objects.iter().map(|(n, k)| obj_candidate(n, *k)));
            }
        }
        None => {
            let mut seen = std::collections::HashSet::new();
            for t in refs.values() {
                if seen.insert(t.to_lowercase()) {
                    add_columns(t, &mut out, &mut need);
                }
            }
            out.extend(meta.objects.iter().map(|(n, k)| obj_candidate(n, *k)));
            out.extend(meta.schemas.iter().map(|s| Candidate {
                text: s.clone(),
                kind: Kind::Schema,
                detail: "schema".into(),
            }));
            out.extend(keywords.iter().map(|k| Candidate {
                text: k.to_ascii_uppercase(),
                kind: Kind::Keyword,
                detail: "keyword".into(),
            }));
        }
    }
    let p = ctx.prefix.to_lowercase();
    let mut ranked: Vec<(u8, Candidate)> = out
        .into_iter()
        .filter_map(|c| {
            let l = c.text.to_lowercase();
            if p.is_empty() || l.starts_with(&p) {
                Some((0, c))
            } else if p.len() >= 2 && l.contains(&p) {
                Some((1, c))
            } else {
                None
            }
        })
        .filter(|(_, c)| !c.text.eq_ignore_ascii_case(&ctx.prefix) || c.kind == Kind::Column)
        .collect();
    ranked.sort_by(|a, b| {
        a.0.cmp(&b.0).then(a.1.kind.cmp(&b.1.kind)).then_with(|| a.1.text.to_lowercase().cmp(&b.1.text.to_lowercase()))
    });
    ranked.dedup_by(|a, b| a.1.text == b.1.text && a.1.kind == b.1.kind);
    need.sort();
    need.dedup();
    (ranked.into_iter().map(|(_, c)| c).take(60).collect(), need)
}

fn obj_candidate(name: &str, kind: ObjectKind) -> Candidate {
    match kind {
        ObjectKind::Table => Candidate { text: name.to_string(), kind: Kind::Table, detail: "table".into() },
        ObjectKind::View => Candidate { text: name.to_string(), kind: Kind::View, detail: "view".into() },
    }
}

/// Quotes `name` for insertion if it is not a plain lower-case identifier.
pub fn insertable(name: &str, kind: Kind, engine: Engine) -> String {
    if kind == Kind::Keyword {
        return name.to_string();
    }
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    let plain = plain || (engine != Engine::Postgres && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    if plain {
        name.to_string()
    } else {
        talus_core::sql::Dialect(engine).quote_ident(name).unwrap_or_else(|_| name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> MetaView {
        MetaView {
            objects: vec![
                ("customers".into(), ObjectKind::Table),
                ("orders".into(), ObjectKind::Table),
                ("v_totals".into(), ObjectKind::View),
            ],
            schemas: vec!["public".into()],
            columns: HashMap::from([(
                "customers".into(),
                vec![("id".into(), "integer".into()), ("name".into(), "text".into())],
            )]),
        }
    }

    #[test]
    fn context_detection() {
        let t = "select c.na";
        assert_eq!(
            context_at(t, t.len(), Engine::Postgres, false),
            Some(Context { start: 9, prefix: "na".into(), qualifier: Some("c".into()) })
        );
        let t = "select \"C\".";
        assert_eq!(context_at(t, t.len(), Engine::Postgres, false).unwrap().qualifier.as_deref(), Some("C"));
        assert!(context_at("select 'abc", 11, Engine::Postgres, false).is_none(), "inside string");
        assert!(context_at("select 1 -- cu", 14, Engine::Postgres, false).is_none(), "inside comment");
        assert!(context_at("select ", 7, Engine::Postgres, false).is_none());
        assert!(context_at("select ", 7, Engine::Postgres, true).is_some());
    }

    #[test]
    fn alias_resolution() {
        let a = aliases(
            "SELECT * FROM public.customers AS c JOIN orders o ON o.cid = c.id, items WHERE 1",
            Engine::Postgres,
        );
        assert_eq!(a.get("c").map(String::as_str), Some("customers"));
        assert_eq!(a.get("o").map(String::as_str), Some("orders"));
        assert_eq!(a.get("items").map(String::as_str), Some("items"));
        assert!(!a.contains_key("on"));
        assert!(!a.contains_key("where"));
    }

    #[test]
    fn columns_after_alias_and_needs_for_unloaded_tables() {
        let text = "SELECT c. FROM customers c JOIN orders o ON true";
        let ctx = context_at(text, 9, Engine::Postgres, false).unwrap();
        let (c, need) = candidates(text, 9, Engine::Postgres, &ctx, &meta(), &[]);
        assert_eq!(c.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(), ["id", "name"]);
        assert!(need.is_empty());
        let text = "SELECT o. FROM customers c JOIN orders o ON true";
        let ctx = context_at(text, 9, Engine::Postgres, false).unwrap();
        let (c, need) = candidates(text, 9, Engine::Postgres, &ctx, &meta(), &[]);
        assert!(c.is_empty());
        assert_eq!(need, vec!["orders".to_string()]);
    }

    #[test]
    fn prefix_ranking_mixes_kinds() {
        let text = "SELECT n FROM customers";
        let ctx = context_at(text, 8, Engine::Postgres, false).unwrap();
        let (c, _) = candidates(text, 8, Engine::Postgres, &ctx, &meta(), &["not", "null", "natural"]);
        let names: Vec<&str> = c.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(names, ["name", "NATURAL", "NOT", "NULL"]);
        let text = "select * from cu";
        let ctx = context_at(text, text.len(), Engine::Postgres, false).unwrap();
        let (c, _) = candidates(text, text.len(), Engine::Postgres, &ctx, &meta(), &[]);
        assert_eq!(c[0].text, "customers");
    }

    #[test]
    fn insert_quotes_when_needed() {
        assert_eq!(insertable("Order Items", Kind::Table, Engine::Postgres), "\"Order Items\"");
        assert_eq!(insertable("CamelCase", Kind::Column, Engine::Postgres), "\"CamelCase\"");
        assert_eq!(insertable("CamelCase", Kind::Column, Engine::MySql), "CamelCase");
        assert_eq!(insertable("SELECT", Kind::Keyword, Engine::Postgres), "SELECT");
    }
}
