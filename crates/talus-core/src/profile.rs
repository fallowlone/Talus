//! Column profiling: row / NULL / distinct counts, min / max / average and the most frequent
//! values of every column of a table. One failing column never hides the others.

use crate::engine::{ScriptOptions, Session};
use crate::error::{DbError, DbResult};
use crate::model::{Engine, ObjectRef, Outcome};
use crate::sql::Dialect;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ColumnProfile {
    pub name: String,
    pub data_type: String,
    pub rows: u64,
    pub nulls: u64,
    pub distinct: Option<u64>,
    pub min: Option<String>,
    pub max: Option<String>,
    pub avg: Option<String>,
    /// Most frequent values (`None` = NULL) with their counts.
    pub top: Vec<(Option<String>, u64)>,
    /// Why part of the profile is missing (unsupported type, permission, …).
    pub note: Option<String>,
}

impl ColumnProfile {
    pub fn null_percent(&self) -> f64 {
        if self.rows == 0 { 0.0 } else { self.nulls as f64 * 100.0 / self.rows as f64 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Numeric,
    Text,
    /// Binary / document / spatial types: counted only.
    CountOnly,
}

fn kind_of(engine: Engine, data_type: &str) -> Kind {
    let t = data_type.to_ascii_lowercase();
    let base = t.split('(').next().unwrap_or("").trim();
    let count_only = [
        "blob",
        "bytea",
        "binary",
        "varbinary",
        "image",
        "json",
        "jsonb",
        "xml",
        "geometry",
        "geography",
        "hierarchyid",
        "sql_variant",
    ]
    .iter()
    .any(|k| base.contains(k))
        || (engine == Engine::SqlServer && matches!(base, "text" | "ntext"));
    if count_only {
        return Kind::CountOnly;
    }
    let numeric = ["int", "serial", "numeric", "decimal", "float", "double", "real", "money", "number"]
        .iter()
        .any(|k| base.contains(k));
    // `interval` / `point` contain "int" but are not numbers.
    if numeric && !base.contains("interval") && !base.contains("point") { Kind::Numeric } else { Kind::Text }
}

fn text_cast(engine: Engine, expr: &str) -> String {
    match engine {
        Engine::Postgres => format!("CAST({expr} AS text)"),
        Engine::SqlServer => format!("CAST({expr} AS nvarchar(4000))"),
        _ => expr.to_string(),
    }
}

/// `(statistics query, top-values query)` for one column.
fn queries(d: Dialect, table: &str, col: &str, kind: Kind, top: usize) -> (String, String) {
    let stats = match kind {
        Kind::CountOnly => format!("SELECT COUNT(*), COUNT({col}), NULL, NULL, NULL, NULL FROM {table}"),
        Kind::Numeric => format!(
            "SELECT COUNT(*), COUNT({col}), COUNT(DISTINCT {col}), {}, {}, {} FROM {table}",
            text_cast(d.0, &format!("MIN({col})")),
            text_cast(d.0, &format!("MAX({col})")),
            text_cast(d.0, &format!("AVG({col})")),
        ),
        Kind::Text => format!(
            "SELECT COUNT(*), COUNT({col}), COUNT(DISTINCT {col}), MIN({0}), MAX({0}), NULL FROM {table}",
            text_cast(d.0, col)
        ),
    };
    let shown = text_cast(d.0, col);
    let top_sql = match d.0 {
        Engine::SqlServer => {
            format!("SELECT TOP ({top}) {shown}, COUNT(*) AS n FROM {table} GROUP BY {col} ORDER BY COUNT(*) DESC")
        }
        _ => format!("SELECT {shown}, COUNT(*) AS n FROM {table} GROUP BY {col} ORDER BY n DESC LIMIT {top}"),
    };
    (stats, top_sql)
}

fn rows_of(out: crate::engine::ScriptOutcome) -> DbResult<Vec<Vec<Option<String>>>> {
    if let Some(e) = out.errors.first() {
        return Err(DbError::Invalid(e.error.to_string()));
    }
    match out.results.into_iter().next().map(|r| r.outcome) {
        Some(Outcome::Rows(q)) => Ok(q.rows),
        _ => Ok(Vec::new()),
    }
}

pub async fn profile_table(s: &mut Session, obj: &ObjectRef, top: usize) -> DbResult<Vec<ColumnProfile>> {
    let engine = s.engine();
    if !engine.is_sql() {
        return Err(DbError::Invalid("profiling works on SQL tables".into()));
    }
    let d = Dialect(engine);
    let table = d.qualified(obj)?;
    let structure = s.table_structure(obj).await?;
    let mut out = Vec::with_capacity(structure.columns.len());
    for c in &structure.columns {
        let mut p = ColumnProfile { name: c.name.clone(), data_type: c.data_type.clone(), ..Default::default() };
        let col = d.quote_ident(&c.name)?;
        let kind = kind_of(engine, &c.data_type);
        let (stats_sql, top_sql) = queries(d, &table, &col, kind, top.max(1));
        match rows_of(s.run_script(&stats_sql, &ScriptOptions::default()).await) {
            Ok(rows) => {
                let r = rows.first().cloned().unwrap_or_default();
                let num = |i: usize| r.get(i).cloned().flatten().and_then(|v| v.parse::<f64>().ok()).map(|v| v as u64);
                p.rows = num(0).unwrap_or(0);
                p.nulls = p.rows.saturating_sub(num(1).unwrap_or(0));
                p.distinct = num(2);
                p.min = r.get(3).cloned().flatten();
                p.max = r.get(4).cloned().flatten();
                p.avg = r.get(5).cloned().flatten();
            }
            Err(e) => p.note = Some(e.to_string()),
        }
        if kind != Kind::CountOnly && p.note.is_none() {
            match rows_of(s.run_script(&top_sql, &ScriptOptions::default()).await) {
                Ok(rows) => {
                    p.top = rows
                        .into_iter()
                        .map(|r| {
                            (
                                r.first().cloned().flatten(),
                                r.get(1).cloned().flatten().and_then(|v| v.parse().ok()).unwrap_or(0),
                            )
                        })
                        .collect();
                }
                Err(e) => p.note = Some(e.to_string()),
            }
        } else if kind == Kind::CountOnly && p.note.is_none() {
            p.note = Some("binary or document type: counted only".into());
        }
        out.push(p);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_are_classified_per_engine() {
        assert_eq!(kind_of(Engine::Postgres, "integer"), Kind::Numeric);
        assert_eq!(kind_of(Engine::Postgres, "numeric(10,2)"), Kind::Numeric);
        assert_eq!(kind_of(Engine::Postgres, "interval"), Kind::Text);
        assert_eq!(kind_of(Engine::Postgres, "jsonb"), Kind::CountOnly);
        assert_eq!(kind_of(Engine::MySql, "varchar(20)"), Kind::Text);
        assert_eq!(kind_of(Engine::MySql, "longblob"), Kind::CountOnly);
        assert_eq!(kind_of(Engine::SqlServer, "ntext"), Kind::CountOnly);
        assert_eq!(kind_of(Engine::Postgres, "text"), Kind::Text);
        assert_eq!(kind_of(Engine::SqlServer, "varbinary(max)"), Kind::CountOnly);
    }

    #[test]
    fn queries_quote_and_paginate_per_dialect() {
        let (stats, top) = queries(Dialect(Engine::SqlServer), "[dbo].[t]", "[c]", Kind::Text, 5);
        assert!(
            stats.contains("MIN(CAST([c] AS nvarchar(4000)))") && top.starts_with("SELECT TOP (5) CAST([c]"),
            "{stats} / {top}"
        );
        let (_, top) = queries(Dialect(Engine::Postgres), "\"s\".\"t\"", "\"c\"", Kind::Numeric, 3);
        assert!(top.ends_with("ORDER BY n DESC LIMIT 3"), "{top}");
        let (stats, _) = queries(Dialect(Engine::Sqlite), "\"t\"", "\"c\"", Kind::Numeric, 3);
        assert!(stats.contains("AVG(\"c\")"));
    }

    #[test]
    fn null_percent_handles_empty_tables() {
        assert_eq!(ColumnProfile::default().null_percent(), 0.0);
        let p = ColumnProfile { rows: 8, nulls: 2, ..Default::default() };
        assert_eq!(p.null_percent(), 25.0);
    }
}
