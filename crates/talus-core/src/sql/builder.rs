//! SELECT statements assembled from a visual query specification (tables, joins, columns,
//! conditions, ordering). Every identifier is quoted and every value is a literal, so the
//! result is safe to show and run.

use super::dialect::Dialect;
use crate::error::{DbError, DbResult};
use crate::model::{Engine, ObjectRef, SortDir};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub obj: ObjectRef,
    pub alias: String,
}

/// `alias.column`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColRef {
    pub alias: String,
    pub column: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Join {
    pub kind: JoinKind,
    pub table: TableRef,
    pub left: ColRef,
    pub right: ColRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Like,
    NotLike,
    In,
    IsNull,
    NotNull,
}

impl Op {
    pub const ALL: [Op; 11] =
        [Op::Eq, Op::Ne, Op::Lt, Op::Le, Op::Gt, Op::Ge, Op::Like, Op::NotLike, Op::In, Op::IsNull, Op::NotNull];

    pub fn label(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "<>",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Like => "LIKE",
            Op::NotLike => "NOT LIKE",
            Op::In => "IN",
            Op::IsNull => "IS NULL",
            Op::NotNull => "IS NOT NULL",
        }
    }

    pub fn needs_value(self) -> bool {
        !matches!(self, Op::IsNull | Op::NotNull)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub col: ColRef,
    pub op: Op,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerySpec {
    pub base: TableRef,
    pub joins: Vec<Join>,
    /// Empty = every column (`*`).
    pub columns: Vec<ColRef>,
    /// Combined with AND.
    pub conditions: Vec<Condition>,
    pub order: Vec<(ColRef, SortDir)>,
    pub limit: Option<u64>,
    pub distinct: bool,
}

/// Plain numbers stay numbers; everything else becomes a quoted string.
fn literal(d: Dialect, v: &str) -> String {
    let t = v.trim();
    let digits = t.strip_prefix('-').unwrap_or(t);
    let numeric = !digits.is_empty()
        && digits.chars().filter(|c| *c == '.').count() <= 1
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && !digits.starts_with('.')
        && !digits.ends_with('.');
    if numeric { t.to_string() } else { d.string_literal(v) }
}

fn col(d: Dialect, c: &ColRef) -> DbResult<String> {
    Ok(format!("{}.{}", d.quote_ident(&c.alias)?, d.quote_ident(&c.column)?))
}

pub fn build(d: Dialect, q: &QuerySpec) -> DbResult<String> {
    let known: Vec<&str> =
        std::iter::once(q.base.alias.as_str()).chain(q.joins.iter().map(|j| j.table.alias.as_str())).collect();
    let mut seen = std::collections::HashSet::new();
    for a in &known {
        if !seen.insert(a.to_lowercase()) {
            return Err(DbError::Invalid(format!("table alias {a:?} is used twice")));
        }
    }
    let check = |c: &ColRef| -> DbResult<()> {
        if known.contains(&c.alias.as_str()) {
            Ok(())
        } else {
            Err(DbError::Invalid(format!("unknown table alias {:?}", c.alias)))
        }
    };
    let select = if q.columns.is_empty() {
        "*".to_string()
    } else {
        q.columns
            .iter()
            .map(|c| {
                check(c)?;
                col(d, c)
            })
            .collect::<DbResult<Vec<_>>>()?
            .join(", ")
    };
    // SQL Server has no LIMIT: TOP when unordered, OFFSET … FETCH after an ORDER BY.
    let top = match (d.0, q.limit) {
        (Engine::SqlServer, Some(n)) if q.order.is_empty() => format!("TOP ({n}) "),
        _ => String::new(),
    };
    // Oracle does not accept AS before a table alias.
    let as_kw = if d.0 == Engine::Oracle { " " } else { " AS " };
    let mut sql = format!(
        "SELECT {}{top}{select}\nFROM {}{as_kw}{}",
        if q.distinct { "DISTINCT " } else { "" },
        d.qualified(&q.base.obj)?,
        d.quote_ident(&q.base.alias)?
    );
    for j in &q.joins {
        check(&j.left)?;
        check(&j.right)?;
        sql.push_str(&format!(
            "\n{} JOIN {}{as_kw}{} ON {} = {}",
            if j.kind == JoinKind::Inner { "INNER" } else { "LEFT" },
            d.qualified(&j.table.obj)?,
            d.quote_ident(&j.table.alias)?,
            col(d, &j.left)?,
            col(d, &j.right)?
        ));
    }
    let mut wheres = Vec::new();
    for c in &q.conditions {
        check(&c.col)?;
        let left = col(d, &c.col)?;
        wheres.push(match c.op {
            Op::IsNull | Op::NotNull => format!("{left} {}", c.op.label()),
            Op::In => {
                let items: Vec<String> =
                    c.value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|s| literal(d, s)).collect();
                if items.is_empty() {
                    return Err(DbError::Invalid(format!("IN needs at least one value for {}", c.col.column)));
                }
                format!("{left} IN ({})", items.join(", "))
            }
            Op::Like | Op::NotLike => format!("{left} {} {}", c.op.label(), d.string_literal(&c.value)),
            op => format!("{left} {} {}", op.label(), literal(d, &c.value)),
        });
    }
    if !wheres.is_empty() {
        sql.push_str(&format!("\nWHERE {}", wheres.join("\n  AND ")));
    }
    if !q.order.is_empty() {
        let parts = q
            .order
            .iter()
            .map(|(c, dir)| {
                check(c)?;
                Ok(format!("{} {}", col(d, c)?, if *dir == SortDir::Asc { "ASC" } else { "DESC" }))
            })
            .collect::<DbResult<Vec<_>>>()?;
        sql.push_str(&format!("\nORDER BY {}", parts.join(", ")));
    }
    match (d.0, q.limit) {
        (Engine::SqlServer, Some(n)) if !q.order.is_empty() => {
            sql.push_str(&format!("\nOFFSET 0 ROWS FETCH NEXT {n} ROWS ONLY"))
        }
        (Engine::SqlServer, _) => {}
        (Engine::Oracle, Some(n)) => sql.push_str(&format!("\nFETCH FIRST {n} ROWS ONLY")),
        (_, Some(n)) => sql.push_str(&format!("\nLIMIT {n}")),
        (_, None) => {}
    }
    Ok(sql)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Engine, ObjectKind};

    fn t(name: &str, alias: &str) -> TableRef {
        TableRef {
            obj: ObjectRef { database: "main".into(), schema: None, name: name.into(), kind: ObjectKind::Table },
            alias: alias.into(),
        }
    }

    fn c(a: &str, n: &str) -> ColRef {
        ColRef { alias: a.into(), column: n.into() }
    }

    fn spec() -> QuerySpec {
        QuerySpec {
            base: t("orders", "o"),
            joins: vec![Join {
                kind: JoinKind::Left,
                table: t("customers", "c"),
                left: c("o", "cid"),
                right: c("c", "id"),
            }],
            columns: vec![c("o", "id"), c("c", "name")],
            conditions: vec![
                Condition { col: c("o", "total"), op: Op::Ge, value: "10.5".into() },
                Condition { col: c("c", "name"), op: Op::Like, value: "A%".into() },
                Condition { col: c("c", "id"), op: Op::In, value: "1, 2, x'y".into() },
                Condition { col: c("o", "note"), op: Op::IsNull, value: String::new() },
            ],
            order: vec![(c("o", "id"), SortDir::Desc)],
            limit: Some(100),
            distinct: false,
        }
    }

    #[test]
    fn builds_joins_conditions_order_and_limit() {
        let sql = build(Dialect(Engine::Sqlite), &spec()).unwrap();
        assert_eq!(
            sql,
            "SELECT \"o\".\"id\", \"c\".\"name\"\nFROM \"main\".\"orders\" AS \"o\"\nLEFT JOIN \"main\".\"customers\" AS \"c\" ON \"o\".\"cid\" = \"c\".\"id\"\nWHERE \"o\".\"total\" >= 10.5\n  AND \"c\".\"name\" LIKE 'A%'\n  AND \"c\".\"id\" IN (1, 2, 'x''y')\n  AND \"o\".\"note\" IS NULL\nORDER BY \"o\".\"id\" DESC\nLIMIT 100"
        );
    }

    #[test]
    fn sql_server_uses_top_or_offset_fetch_instead_of_limit() {
        let d = Dialect(Engine::SqlServer);
        let unordered = QuerySpec { order: vec![], ..spec() };
        assert!(build(d, &unordered).unwrap().starts_with("SELECT TOP (100) [o].[id]"));
        assert!(!build(d, &unordered).unwrap().contains("LIMIT"));
        let ordered = build(d, &spec()).unwrap();
        assert!(ordered.ends_with("ORDER BY [o].[id] DESC\nOFFSET 0 ROWS FETCH NEXT 100 ROWS ONLY"), "{ordered}");
    }

    #[test]
    fn star_distinct_and_mysql_quoting() {
        let q = QuerySpec {
            columns: vec![],
            conditions: vec![],
            order: vec![],
            limit: None,
            joins: vec![],
            distinct: true,
            ..spec()
        };
        assert_eq!(build(Dialect(Engine::MySql), &q).unwrap(), "SELECT DISTINCT *\nFROM `main`.`orders` AS `o`");
    }

    #[test]
    fn values_are_never_spliced_raw() {
        let mut q = spec();
        q.conditions = vec![Condition { col: c("o", "id"), op: Op::Eq, value: "1; DROP TABLE x".into() }];
        assert!(build(Dialect(Engine::Postgres), &q).unwrap().contains("'1; DROP TABLE x'"));
        q.conditions = vec![Condition { col: c("o", "id"), op: Op::Eq, value: "-5".into() }];
        assert!(build(Dialect(Engine::Postgres), &q).unwrap().contains("= -5"));
        q.conditions = vec![Condition { col: c("o", "id"), op: Op::Eq, value: "1.".into() }];
        assert!(build(Dialect(Engine::Postgres), &q).unwrap().contains("= '1.'"));
    }

    #[test]
    fn rejects_unknown_aliases_duplicates_and_empty_in() {
        let mut q = spec();
        q.columns = vec![c("zz", "id")];
        assert!(build(Dialect(Engine::Sqlite), &q).is_err());
        let mut q = spec();
        q.joins[0].table.alias = "o".into();
        assert!(build(Dialect(Engine::Sqlite), &q).is_err());
        let mut q = spec();
        q.conditions = vec![Condition { col: c("o", "id"), op: Op::In, value: " , ".into() }];
        assert!(build(Dialect(Engine::Sqlite), &q).is_err());
    }
}
