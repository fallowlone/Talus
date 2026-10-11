//! Builders for the data editor: paged SELECT with filter/sort, and single-row
//! UPDATE/INSERT/DELETE keyed by primary key (or SQLite rowid). Values are always bound.

use super::dialect::Dialect;
use crate::error::{DbError, DbResult};
use crate::model::{Cell, Engine, FilterOp, PageQuery, RowKey, SortDir, TableStructure};

/// Hidden column added to SQLite pages of tables without a primary key.
pub const ROWID_ALIAS: &str = "__talus_rowid";

#[derive(Debug, Clone, PartialEq)]
pub struct BuiltQuery {
    pub sql: String,
    pub params: Vec<Cell>,
}

struct Params<'a> {
    d: Dialect,
    s: &'a TableStructure,
    values: Vec<Cell>,
}

impl Params<'_> {
    fn push(&mut self, column: &str, value: Cell) -> String {
        self.values.push(value);
        let ty = self.s.column(column).map(|c| c.data_type.as_str());
        self.d.placeholder(self.values.len(), ty)
    }
    fn push_text(&mut self, value: String) -> String {
        self.values.push(Some(value));
        self.d.placeholder(self.values.len(), None)
    }
}

/// Key columns used to identify rows: the primary key, else SQLite's rowid.
/// `None` means rows cannot be safely identified (editing must be disabled).
pub fn key_columns(engine: Engine, s: &TableStructure) -> Option<Vec<String>> {
    let pk = s.primary_key();
    if !pk.is_empty() {
        return Some(pk.into_iter().map(String::from).collect());
    }
    (engine == Engine::Sqlite).then(|| vec![ROWID_ALIAS.to_string()])
}

fn checked_column<'a>(s: &TableStructure, name: &'a str) -> DbResult<&'a str> {
    if name == ROWID_ALIAS || s.column(name).is_some() {
        Ok(name)
    } else {
        Err(DbError::Invalid(format!("unknown column: {name}")))
    }
}

fn col_expr(d: Dialect, name: &str) -> DbResult<String> {
    if name == ROWID_ALIAS { Ok("rowid".into()) } else { d.quote_ident(name) }
}

/// Expression that compares as text: PostgreSQL and SQL Server need an explicit cast for LIKE on
/// non-text columns.
fn text_expr(d: Dialect, col: &str) -> String {
    match d.0 {
        Engine::Postgres => format!("CAST({col} AS text)"),
        Engine::SqlServer => format!("CAST({col} AS nvarchar(max))"),
        _ => col.to_string(),
    }
}

fn like_pattern(value: &str, prefix_only: bool) -> String {
    let escaped: String = value
        .chars()
        .flat_map(|c| match c {
            '!' | '%' | '_' => vec!['!', c],
            c => vec![c],
        })
        .collect();
    if prefix_only { format!("{escaped}%") } else { format!("%{escaped}%") }
}

pub fn select_page(d: Dialect, q: &PageQuery, s: &TableStructure) -> DbResult<BuiltQuery> {
    let mut p = Params { d, s, values: Vec::new() };
    let table = d.qualified(&q.table)?;
    let needs_rowid = d.0 == Engine::Sqlite && s.primary_key().is_empty();
    let mut sql = if needs_rowid {
        format!("SELECT rowid AS {}, * FROM {table}", d.quote_ident(ROWID_ALIAS)?)
    } else if d.0 == Engine::Postgres && !s.columns.is_empty() {
        // The extended protocol returns binary values; casting makes every column text.
        let cols = s
            .columns
            .iter()
            .map(|c| {
                let q = d.quote_ident(&c.name)?;
                Ok(format!("CAST({q} AS text) AS {q}"))
            })
            .collect::<DbResult<Vec<_>>>()?;
        format!("SELECT {} FROM {table}", cols.join(", "))
    } else {
        format!("SELECT * FROM {table}")
    };
    let mut conds = Vec::new();
    for f in q.filters.iter().filter(|f| f.enabled) {
        let col = col_expr(d, checked_column(s, &f.column)?)?;
        let text_col = text_expr(d, &col);
        let like = if d.0 == Engine::Postgres { "ILIKE" } else { "LIKE" };
        conds.push(match f.op {
            FilterOp::IsNull => format!("{col} IS NULL"),
            FilterOp::IsNotNull => format!("{col} IS NOT NULL"),
            FilterOp::Contains => {
                format!("{text_col} {like} {} ESCAPE '!'", p.push_text(like_pattern(&f.value, false)))
            }
            FilterOp::NotContains => {
                format!("{text_col} NOT {like} {} ESCAPE '!'", p.push_text(like_pattern(&f.value, false)))
            }
            FilterOp::BeginsWith => {
                format!("{text_col} {like} {} ESCAPE '!'", p.push_text(like_pattern(&f.value, true)))
            }
            op => {
                let sym = op.label();
                format!("{col} {sym} {}", p.push(&f.column, Some(f.value.clone())))
            }
        });
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    let order: Vec<String> = if q.sort.is_empty() {
        // Stable paging: order by key when one exists.
        key_columns(d.0, s).unwrap_or_default().iter().map(|c| col_expr(d, c)).collect::<DbResult<_>>()?
    } else {
        q.sort
            .iter()
            .map(|(c, dir)| {
                let e = col_expr(d, checked_column(s, c)?)?;
                Ok(format!("{e} {}", if *dir == SortDir::Asc { "ASC" } else { "DESC" }))
            })
            .collect::<DbResult<_>>()?
    };
    if !order.is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(&order.join(", "));
    }
    if d.0 == Engine::SqlServer {
        // OFFSET … FETCH is only valid after an ORDER BY.
        if order.is_empty() {
            sql.push_str(" ORDER BY (SELECT NULL)");
        }
        sql.push_str(&format!(" OFFSET {} ROWS FETCH NEXT {} ROWS ONLY", q.offset, q.limit));
    } else {
        sql.push_str(&format!(" LIMIT {} OFFSET {}", q.limit, q.offset));
    }
    Ok(BuiltQuery { sql, params: p.values })
}

fn where_key(p: &mut Params<'_>, key: &RowKey) -> DbResult<String> {
    if key.0.is_empty() {
        return Err(DbError::Invalid("row key is empty".into()));
    }
    let parts = key
        .0
        .iter()
        .map(|(c, v)| {
            let e = col_expr(p.d, checked_column(p.s, c)?)?;
            Ok(match v {
                None => format!("{e} IS NULL"),
                Some(v) => format!("{e} = {}", p.push(c, Some(v.clone()))),
            })
        })
        .collect::<DbResult<Vec<_>>>()?;
    Ok(parts.join(" AND "))
}

pub fn update_row(
    d: Dialect,
    table: &str,
    s: &TableStructure,
    key: &RowKey,
    values: &[(String, Cell)],
) -> DbResult<BuiltQuery> {
    if values.is_empty() {
        return Err(DbError::Invalid("no values to update".into()));
    }
    let mut p = Params { d, s, values: Vec::new() };
    let sets = values
        .iter()
        .map(|(c, v)| {
            if c == ROWID_ALIAS || s.column(c).is_none() {
                return Err(DbError::Invalid(format!("unknown column: {c}")));
            }
            Ok(format!("{} = {}", d.quote_ident(c)?, p.push(c, v.clone())))
        })
        .collect::<DbResult<Vec<_>>>()?;
    let w = where_key(&mut p, key)?;
    Ok(BuiltQuery { sql: format!("UPDATE {table} SET {} WHERE {w}", sets.join(", ")), params: p.values })
}

pub fn insert_row(d: Dialect, table: &str, s: &TableStructure, values: &[(String, Cell)]) -> DbResult<BuiltQuery> {
    let mut p = Params { d, s, values: Vec::new() };
    if values.is_empty() {
        let sql = match d.0 {
            Engine::MySql => format!("INSERT INTO {table} () VALUES ()"),
            _ => format!("INSERT INTO {table} DEFAULT VALUES"),
        };
        return Ok(BuiltQuery { sql, params: vec![] });
    }
    let mut cols = Vec::new();
    let mut vals = Vec::new();
    for (c, v) in values {
        if s.column(c).is_none() {
            return Err(DbError::Invalid(format!("unknown column: {c}")));
        }
        cols.push(d.quote_ident(c)?);
        vals.push(p.push(c, v.clone()));
    }
    Ok(BuiltQuery {
        sql: format!("INSERT INTO {table} ({}) VALUES ({})", cols.join(", "), vals.join(", ")),
        params: p.values,
    })
}

/// `SELECT COUNT(*)` of rows where any non-binary column contains `needle` (case-insensitive).
pub fn count_matches(d: Dialect, table: &str, s: &TableStructure, needle: &str) -> DbResult<Option<BuiltQuery>> {
    let mut p = Params { d, s, values: Vec::new() };
    let like = if d.0 == Engine::Postgres { "ILIKE" } else { "LIKE" };
    let conds = s
        .columns
        .iter()
        .filter(|c| {
            let lower = c.data_type.to_ascii_lowercase();
            let base = lower.split('(').next().unwrap_or("").trim();
            let binary_like = matches!(
                base,
                "blob"
                    | "bytea"
                    | "binary"
                    | "varbinary"
                    | "longblob"
                    | "mediumblob"
                    | "tinyblob"
                    | "image"
                    | "geography"
                    | "geometry"
                    | "hierarchyid"
                    | "sql_variant"
            );
            // SQL Server's `timestamp` is a row version, not a date.
            let row_version = d.0 == Engine::SqlServer && matches!(base, "timestamp" | "rowversion");
            !(binary_like || row_version)
        })
        .map(|c| {
            let col = d.quote_ident(&c.name)?;
            let text_col = text_expr(d, &col);
            Ok(format!("{text_col} {like} {} ESCAPE '!'", p.push_text(like_pattern(needle, false))))
        })
        .collect::<DbResult<Vec<_>>>()?;
    if conds.is_empty() {
        return Ok(None);
    }
    // Parameterised reads are text-only on PostgreSQL (see `select_page`).
    let count = if d.0 == Engine::Postgres { "CAST(COUNT(*) AS text)" } else { "COUNT(*)" };
    Ok(Some(BuiltQuery { sql: format!("SELECT {count} FROM {table} WHERE {}", conds.join(" OR ")), params: p.values }))
}

/// Multi-row `INSERT INTO t (cols) VALUES (…), (…)` with bound parameters, row-major.
pub fn insert_rows(d: Dialect, table: &str, s: &TableStructure, columns: &[String], rows: usize) -> DbResult<String> {
    if columns.is_empty() || rows == 0 {
        return Err(DbError::Invalid("nothing to insert".into()));
    }
    let quoted = columns
        .iter()
        .map(|c| {
            if s.column(c).is_some() { d.quote_ident(c) } else { Err(DbError::Invalid(format!("unknown column: {c}"))) }
        })
        .collect::<DbResult<Vec<_>>>()?;
    let mut n = 0;
    let groups: Vec<String> = (0..rows)
        .map(|_| {
            let ph: Vec<String> = columns
                .iter()
                .map(|c| {
                    n += 1;
                    d.placeholder(n, s.column(c).map(|x| x.data_type.as_str()))
                })
                .collect();
            format!("({})", ph.join(", "))
        })
        .collect();
    Ok(format!("INSERT INTO {table} ({}) VALUES {}", quoted.join(", "), groups.join(", ")))
}

pub fn delete_row(d: Dialect, table: &str, s: &TableStructure, key: &RowKey) -> DbResult<BuiltQuery> {
    let mut p = Params { d, s, values: Vec::new() };
    let w = where_key(&mut p, key)?;
    Ok(BuiltQuery { sql: format!("DELETE FROM {table} WHERE {w}"), params: p.values })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnInfo, Filter, ObjectKind, ObjectRef};

    fn structure(pk: bool) -> TableStructure {
        let col = |n: &str, t: &str, pk: bool| ColumnInfo {
            name: n.into(),
            data_type: t.into(),
            nullable: !pk,
            default: None,
            primary_key: pk,
            comment: None,
            extra: None,
        };
        TableStructure { columns: vec![col("id", "integer", pk), col("name", "text", false)], ..Default::default() }
    }

    fn page(filters: Vec<Filter>, sort: Vec<(String, SortDir)>) -> PageQuery {
        PageQuery {
            table: ObjectRef {
                database: "main".into(),
                schema: Some("public".into()),
                name: "t".into(),
                kind: ObjectKind::Table,
            },
            filters,
            sort,
            offset: 20,
            limit: 10,
        }
    }

    #[test]
    fn pg_page_with_filters_binds_values_and_casts() {
        let f = vec![
            Filter { enabled: true, column: "id".into(), op: FilterOp::Gt, value: "5".into() },
            Filter { enabled: true, column: "name".into(), op: FilterOp::Contains, value: "50%_!".into() },
            Filter { enabled: false, column: "name".into(), op: FilterOp::IsNull, value: String::new() },
        ];
        let q = select_page(Dialect(Engine::Postgres), &page(f, vec![]), &structure(true)).unwrap();
        assert_eq!(
            q.sql,
            "SELECT CAST(\"id\" AS text) AS \"id\", CAST(\"name\" AS text) AS \"name\" FROM \"public\".\"t\" WHERE \"id\" > $1::text::integer AND CAST(\"name\" AS text) ILIKE $2 ESCAPE '!' ORDER BY \"id\" LIMIT 10 OFFSET 20"
        );
        assert_eq!(q.params, vec![Some("5".into()), Some("%50!%!_!!%".into())]);
    }

    #[test]
    fn sqlite_without_pk_uses_rowid() {
        let q = select_page(
            Dialect(Engine::Sqlite),
            &page(vec![], vec![("name".into(), SortDir::Desc)]),
            &structure(false),
        )
        .unwrap();
        assert_eq!(
            q.sql,
            "SELECT rowid AS \"__talus_rowid\", * FROM \"main\".\"t\" ORDER BY \"name\" DESC LIMIT 10 OFFSET 20"
        );
        assert_eq!(key_columns(Engine::Sqlite, &structure(false)).unwrap(), vec![ROWID_ALIAS]);
        assert!(key_columns(Engine::Postgres, &structure(false)).is_none());
    }

    #[test]
    fn rejects_unknown_columns_in_filter_and_sort() {
        let f = vec![Filter { enabled: true, column: "x; drop".into(), op: FilterOp::Eq, value: "1".into() }];
        assert!(select_page(Dialect(Engine::MySql), &page(f, vec![]), &structure(true)).is_err());
        assert!(
            select_page(Dialect(Engine::MySql), &page(vec![], vec![("nope".into(), SortDir::Asc)]), &structure(true))
                .is_err()
        );
    }

    #[test]
    fn row_dml() {
        let s = structure(true);
        let key = RowKey(vec![("id".into(), Some("7".into()))]);
        let d = Dialect(Engine::MySql);
        let u = update_row(d, "`t`", &s, &key, &[("name".into(), None)]).unwrap();
        assert_eq!(u.sql, "UPDATE `t` SET `name` = ? WHERE `id` = ?");
        assert_eq!(u.params, vec![None, Some("7".into())]);
        let i = insert_row(Dialect(Engine::Sqlite), "\"t\"", &s, &[("name".into(), Some("a".into()))]).unwrap();
        assert_eq!(i.sql, "INSERT INTO \"t\" (\"name\") VALUES (?1)");
        let del = delete_row(Dialect(Engine::Postgres), "\"t\"", &s, &key).unwrap();
        assert_eq!(del.sql, "DELETE FROM \"t\" WHERE \"id\" = $1::text::integer");
        assert!(update_row(d, "`t`", &s, &key, &[("missing".into(), None)]).is_err());
        assert_eq!(insert_row(d, "`t`", &s, &[]).unwrap().sql, "INSERT INTO `t` () VALUES ()");
    }

    #[test]
    fn multi_row_insert_numbers_placeholders() {
        let s = structure(true);
        let cols = vec!["id".to_string(), "name".to_string()];
        assert_eq!(
            insert_rows(Dialect(Engine::Postgres), "\"t\"", &s, &cols, 2).unwrap(),
            "INSERT INTO \"t\" (\"id\", \"name\") VALUES ($1::text::integer, $2::text::text), ($3::text::integer, $4::text::text)"
        );
        assert_eq!(
            insert_rows(Dialect(Engine::MySql), "`t`", &s, &cols, 1).unwrap(),
            "INSERT INTO `t` (`id`, `name`) VALUES (?, ?)"
        );
        assert!(insert_rows(Dialect(Engine::MySql), "`t`", &s, &["x".into()], 1).is_err());
    }

    #[test]
    fn rowid_key_where() {
        let key = RowKey(vec![(ROWID_ALIAS.into(), Some("3".into()))]);
        let q = delete_row(Dialect(Engine::Sqlite), "\"t\"", &structure(false), &key).unwrap();
        assert_eq!(q.sql, "DELETE FROM \"t\" WHERE rowid = ?1");
    }
}
