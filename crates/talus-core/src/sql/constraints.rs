//! Index and foreign-key DDL for the table designer: the edited lists are diffed against the
//! structure as loaded. Drops are returned separately from creates so the designer can run
//! drops first (they free names and avoid dangling column references) and creates last.

use std::collections::{HashMap, HashSet};

use super::dialect::Dialect;
use crate::error::{DbError, DbResult};
use crate::model::{Engine, ForeignKeyInfo, IndexInfo, ObjectRef};

pub const FK_ACTIONS: [&str; 5] = ["NO ACTION", "RESTRICT", "CASCADE", "SET NULL", "SET DEFAULT"];

/// A designer index row; `original` is the name it has in the database (None = new).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexDef {
    pub original: Option<String>,
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

/// A designer foreign-key row; `ref_table` is `table` or `schema.table` as typed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FkDef {
    pub original: Option<String>,
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
}

/// Primary keys and SQLite's constraint-backed autoindexes are owned by the table definition.
fn is_managed(i: &IndexInfo) -> bool {
    i.primary || i.name.starts_with("sqlite_autoindex_")
}

impl IndexDef {
    /// The non-primary indexes of a loaded structure.
    pub fn from_info(list: &[IndexInfo]) -> Vec<IndexDef> {
        list.iter()
            .filter(|i| !is_managed(i))
            .map(|i| IndexDef {
                original: Some(i.name.clone()),
                name: i.name.clone(),
                columns: i.columns.clone(),
                unique: i.unique,
            })
            .collect()
    }
}

impl FkDef {
    pub fn from_info(list: &[ForeignKeyInfo]) -> Vec<FkDef> {
        list.iter()
            .map(|f| FkDef {
                original: Some(f.name.clone()),
                name: f.name.clone(),
                columns: f.columns.clone(),
                ref_table: f.ref_table.clone(),
                ref_columns: f.ref_columns.clone(),
                on_update: f.on_update.clone(),
                on_delete: f.on_delete.clone(),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConstraintPlan {
    pub drops: Vec<String>,
    pub creates: Vec<String>,
}

fn quote_list(d: Dialect, cols: &[String]) -> DbResult<String> {
    if cols.is_empty() {
        return Err(DbError::Invalid("at least one column is required".into()));
    }
    Ok(cols.iter().map(|c| d.quote_ident(c)).collect::<DbResult<Vec<_>>>()?.join(", "))
}

fn quote_ref_table(d: Dialect, t: &str) -> DbResult<String> {
    let parts: Vec<&str> = t.split('.').map(str::trim).collect();
    if parts.len() > 2 || parts.iter().any(|p| p.is_empty()) {
        return Err(DbError::Invalid(format!("invalid referenced table: {t:?}")));
    }
    Ok(parts.iter().map(|p| d.quote_ident(p)).collect::<DbResult<Vec<_>>>()?.join("."))
}

fn action(kind: &str, a: &str) -> DbResult<&'static str> {
    FK_ACTIONS
        .iter()
        .find(|x| x.eq_ignore_ascii_case(a.trim()))
        .copied()
        .ok_or_else(|| DbError::Invalid(format!("unsupported ON {kind} action: {a:?}")))
}

/// `CONSTRAINT name FOREIGN KEY (..) REFERENCES t (..) ON UPDATE .. ON DELETE ..`
pub fn fk_clause(d: Dialect, fk: &FkDef) -> DbResult<String> {
    // SQL Server has no RESTRICT; NO ACTION is its immediate-check equivalent.
    let act = |kind: &str, a: &str| -> DbResult<&'static str> {
        match action(kind, a)? {
            "RESTRICT" if d.0 == Engine::SqlServer => Ok("NO ACTION"),
            other => Ok(other),
        }
    };
    if fk.columns.len() != fk.ref_columns.len() {
        return Err(DbError::Invalid(format!(
            "foreign key {}: {} column(s) reference {} column(s)",
            fk.name,
            fk.columns.len(),
            fk.ref_columns.len()
        )));
    }
    if d.0 == Engine::Oracle {
        // Oracle has no ON UPDATE actions, and ON DELETE is CASCADE, SET NULL or (by default) none.
        if !matches!(act("UPDATE", &fk.on_update)?, "NO ACTION" | "RESTRICT") {
            return Err(DbError::Invalid("Oracle foreign keys have no ON UPDATE actions".into()));
        }
        let on_delete = match act("DELETE", &fk.on_delete)? {
            "CASCADE" => " ON DELETE CASCADE",
            "SET NULL" => " ON DELETE SET NULL",
            "NO ACTION" | "RESTRICT" => "",
            other => return Err(DbError::Invalid(format!("Oracle does not support ON DELETE {other}"))),
        };
        return Ok(format!(
            "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}){on_delete}",
            d.quote_ident(&fk.name)?,
            quote_list(d, &fk.columns)?,
            quote_ref_table(d, &fk.ref_table)?,
            quote_list(d, &fk.ref_columns)?,
        ));
    }
    if d.0 == Engine::Snowflake {
        // Snowflake does not enforce foreign keys and takes no referential actions; the clause
        // stays informational.
        if !matches!(act("UPDATE", &fk.on_update)?, "NO ACTION" | "RESTRICT") {
            return Err(DbError::Invalid("Snowflake foreign keys have no ON UPDATE actions".into()));
        }
        if !matches!(act("DELETE", &fk.on_delete)?, "NO ACTION" | "RESTRICT") {
            return Err(DbError::Invalid("Snowflake foreign keys have no ON DELETE actions".into()));
        }
        return Ok(format!(
            "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) NOT ENFORCED",
            d.quote_ident(&fk.name)?,
            quote_list(d, &fk.columns)?,
            quote_ref_table(d, &fk.ref_table)?,
            quote_list(d, &fk.ref_columns)?,
        ));
    }
    Ok(format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) ON UPDATE {} ON DELETE {}",
        d.quote_ident(&fk.name)?,
        quote_list(d, &fk.columns)?,
        quote_ref_table(d, &fk.ref_table)?,
        quote_list(d, &fk.ref_columns)?,
        act("UPDATE", &fk.on_update)?,
        act("DELETE", &fk.on_delete)?,
    ))
}

/// SQLite index names live in the schema, not under the table.
fn index_name(d: Dialect, table: &ObjectRef, name: &str) -> DbResult<String> {
    let n = d.quote_ident(name)?;
    let schema = match d.0 {
        Engine::Postgres | Engine::Oracle | Engine::Snowflake => table.schema.as_deref(),
        Engine::Sqlite => Some(table.database.as_str()).filter(|x| !x.is_empty()),
        Engine::MySql | Engine::SqlServer | Engine::Redis | Engine::MongoDb => None,
    };
    Ok(match schema {
        Some(s) => format!("{}.{n}", d.quote_ident(s)?),
        None => n,
    })
}

pub fn create_index(d: Dialect, table: &ObjectRef, ix: &IndexDef) -> DbResult<String> {
    if d.0 == Engine::Snowflake {
        return Err(DbError::Invalid(
            "Snowflake has no secondary indexes (micropartitioning replaces them); declare UNIQUE constraints instead"
                .into(),
        ));
    }
    let target = match d.0 {
        Engine::Sqlite => d.quote_ident(&table.name)?,
        _ => d.qualified(table)?,
    };
    let name = match d.0 {
        Engine::Postgres | Engine::MySql | Engine::SqlServer | Engine::Redis | Engine::MongoDb => {
            d.quote_ident(&ix.name)?
        }
        // Oracle indexes live in the owner's schema and are created in the session's unless qualified.
        Engine::Oracle | Engine::Sqlite | Engine::Snowflake => index_name(d, table, &ix.name)?,
    };
    Ok(format!(
        "CREATE {}INDEX {name} ON {target} ({})",
        if ix.unique { "UNIQUE " } else { "" },
        quote_list(d, &ix.columns)?
    ))
}

fn drop_index(d: Dialect, table: &ObjectRef, name: &str) -> DbResult<String> {
    Ok(match d.0 {
        Engine::MySql | Engine::SqlServer => format!("DROP INDEX {} ON {}", d.quote_ident(name)?, d.qualified(table)?),
        _ => format!("DROP INDEX {}", index_name(d, table, name)?),
    })
}

fn check_unique_names<'a>(what: &str, names: impl Iterator<Item = &'a str>) -> DbResult<()> {
    let mut seen = HashSet::new();
    for n in names {
        if n.trim().is_empty() {
            return Err(DbError::Invalid(format!("{what} name is required")));
        }
        if !seen.insert(n.to_lowercase()) {
            return Err(DbError::Invalid(format!("duplicate {what} name: {n}")));
        }
    }
    Ok(())
}

fn check_known(what: &str, name: &str, cols: &[String], known: &HashSet<&str>) -> DbResult<()> {
    match cols.iter().find(|c| !known.contains(c.as_str())) {
        Some(c) => Err(DbError::Invalid(format!("{what} {name} uses unknown column {c}"))),
        None => Ok(()),
    }
}

/// Foreign keys of a table being created are emitted inside its CREATE TABLE.
pub fn validate_new(d: Dialect, columns: &[String], indexes: &[IndexDef], fks: &[FkDef]) -> DbResult<()> {
    let known: HashSet<&str> = columns.iter().map(String::as_str).collect();
    check_unique_names("index", indexes.iter().map(|i| i.name.as_str()))?;
    check_unique_names("foreign key", fks.iter().map(|f| f.name.as_str()))?;
    for i in indexes {
        check_known("index", &i.name, &i.columns, &known)?;
    }
    for f in fks {
        check_known("foreign key", &f.name, &f.columns, &known)?;
        fk_clause(d, f)?;
    }
    Ok(())
}

/// Diffs edited indexes / foreign keys against the loaded ones. `renames` maps columns
/// renamed in the same save (old -> new) so untouched constraints are not recreated.
pub fn diff(
    d: Dialect,
    table: &ObjectRef,
    columns: &[String],
    renames: &HashMap<String, String>,
    original: (&[IndexInfo], &[ForeignKeyInfo]),
    edited: (&[IndexDef], &[FkDef]),
) -> DbResult<ConstraintPlan> {
    let (orig_ix, orig_fk) = original;
    let (new_ix, new_fk) = edited;
    validate_new(d, columns, new_ix, new_fk)?;
    let map = |cols: &[String]| -> Vec<String> {
        cols.iter().map(|c| renames.get(c).cloned().unwrap_or_else(|| c.clone())).collect()
    };
    let t = d.qualified(table)?;
    let mut plan = ConstraintPlan::default();

    for o in orig_fk {
        let kept = new_fk.iter().find(|f| f.original.as_deref() == Some(o.name.as_str()));
        let unchanged = kept.is_some_and(|f| {
            f.name == o.name
                && f.columns == map(&o.columns)
                && f.ref_table == o.ref_table
                && f.ref_columns == o.ref_columns
                && f.on_update.eq_ignore_ascii_case(&o.on_update)
                && f.on_delete.eq_ignore_ascii_case(&o.on_delete)
        });
        if unchanged {
            continue;
        }
        plan.drops.push(match d.0 {
            Engine::Sqlite | Engine::Redis | Engine::MongoDb => {
                return Err(DbError::Invalid(
                    "SQLite cannot add or drop foreign keys on an existing table; create a new table and copy the data"
                        .into(),
                ));
            }
            Engine::Postgres | Engine::SqlServer | Engine::Oracle | Engine::Snowflake => {
                format!("ALTER TABLE {t} DROP CONSTRAINT {}", d.quote_ident(&o.name)?)
            }
            Engine::MySql => format!("ALTER TABLE {t} DROP FOREIGN KEY {}", d.quote_ident(&o.name)?),
        });
    }
    for o in orig_ix.iter().filter(|i| !is_managed(i)) {
        let unchanged = new_ix.iter().any(|i| {
            i.original.as_deref() == Some(o.name.as_str())
                && i.name == o.name
                && i.unique == o.unique
                && i.columns == map(&o.columns)
        });
        if !unchanged {
            // Snowflake "indexes" are UNIQUE constraints; only those exist there.
            if d.0 == Engine::Snowflake {
                plan.drops.push(format!("ALTER TABLE {t} DROP CONSTRAINT {}", d.quote_ident(&o.name)?));
            } else {
                plan.drops.push(drop_index(d, table, &o.name)?);
            }
        }
    }
    for i in new_ix {
        let existing =
            orig_ix.iter().filter(|o| !is_managed(o)).find(|o| i.original.as_deref() == Some(o.name.as_str()));
        let unchanged =
            existing.is_some_and(|o| i.name == o.name && i.unique == o.unique && i.columns == map(&o.columns));
        if !unchanged {
            if d.0 == Engine::Snowflake && i.unique {
                plan.creates.push(format!(
                    "ALTER TABLE {t} ADD CONSTRAINT {} UNIQUE ({}) NOT ENFORCED",
                    d.quote_ident(&i.name)?,
                    quote_list(d, &i.columns)?
                ));
            } else {
                plan.creates.push(create_index(d, table, i)?);
            }
        }
    }
    for f in new_fk {
        let unchanged = orig_fk.iter().any(|o| {
            f.original.as_deref() == Some(o.name.as_str())
                && f.name == o.name
                && f.columns == map(&o.columns)
                && f.ref_table == o.ref_table
                && f.ref_columns == o.ref_columns
                && f.on_update.eq_ignore_ascii_case(&o.on_update)
                && f.on_delete.eq_ignore_ascii_case(&o.on_delete)
        });
        if !unchanged {
            if d.0 == Engine::Sqlite {
                return Err(DbError::Invalid(
                    "SQLite cannot add or drop foreign keys on an existing table; create a new table and copy the data"
                        .into(),
                ));
            }
            plan.creates.push(format!("ALTER TABLE {t} ADD {}", fk_clause(d, f)?));
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ObjectKind;

    fn obj(engine: Engine) -> ObjectRef {
        match engine {
            Engine::Postgres => ObjectRef {
                database: "db".into(),
                schema: Some("public".into()),
                name: "t".into(),
                kind: ObjectKind::Table,
            },
            _ => ObjectRef { database: "main".into(), schema: None, name: "t".into(), kind: ObjectKind::Table },
        }
    }

    fn cols() -> Vec<String> {
        vec!["id".into(), "a".into(), "b".into()]
    }

    fn ix(original: Option<&str>, name: &str, columns: &[&str], unique: bool) -> IndexDef {
        IndexDef {
            original: original.map(String::from),
            name: name.into(),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            unique,
        }
    }

    fn info(name: &str, columns: &[&str], unique: bool) -> IndexInfo {
        IndexInfo {
            name: name.into(),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            unique,
            primary: false,
        }
    }

    #[test]
    fn index_sql_per_engine() {
        let i = ix(None, "ix_a", &["a", "b"], true);
        assert_eq!(
            create_index(Dialect(Engine::Sqlite), &obj(Engine::Sqlite), &i).unwrap(),
            "CREATE UNIQUE INDEX \"main\".\"ix_a\" ON \"t\" (\"a\", \"b\")"
        );
        assert_eq!(
            create_index(Dialect(Engine::Postgres), &obj(Engine::Postgres), &i).unwrap(),
            "CREATE UNIQUE INDEX \"ix_a\" ON \"public\".\"t\" (\"a\", \"b\")"
        );
        assert_eq!(
            drop_index(Dialect(Engine::Postgres), &obj(Engine::Postgres), "ix_a").unwrap(),
            "DROP INDEX \"public\".\"ix_a\""
        );
        assert_eq!(
            drop_index(Dialect(Engine::MySql), &obj(Engine::MySql), "ix_a").unwrap(),
            "DROP INDEX `ix_a` ON `main`.`t`"
        );
    }

    #[test]
    fn diff_creates_drops_and_recreates_changed_indexes() {
        let d = Dialect(Engine::Postgres);
        let o = [info("keep", &["a"], false), info("gone", &["b"], false), info("chg", &["a"], false)];
        let edited = [
            ix(Some("keep"), "keep", &["a"], false),
            ix(Some("chg"), "chg", &["a", "b"], false),
            ix(None, "fresh", &["b"], false),
        ];
        let p = diff(d, &obj(Engine::Postgres), &cols(), &HashMap::new(), (&o, &[]), (&edited, &[])).unwrap();
        assert_eq!(p.drops, vec!["DROP INDEX \"public\".\"gone\"", "DROP INDEX \"public\".\"chg\""]);
        assert_eq!(p.creates.len(), 2);
        assert!(p.creates[0].contains("\"chg\"") && p.creates[1].contains("\"fresh\""));
    }

    #[test]
    fn renamed_column_does_not_recreate_untouched_index() {
        let d = Dialect(Engine::MySql);
        let o = [info("ix", &["a"], false)];
        let renames = HashMap::from([("a".to_string(), "a2".to_string())]);
        let edited = [ix(Some("ix"), "ix", &["a2"], false)];
        let known = vec!["id".to_string(), "a2".to_string()];
        let p = diff(d, &obj(Engine::MySql), &known, &renames, (&o, &[]), (&edited, &[])).unwrap();
        assert_eq!(p, ConstraintPlan::default());
    }

    #[test]
    fn rejects_unknown_columns_duplicates_and_bad_actions() {
        let d = Dialect(Engine::Postgres);
        let t = obj(Engine::Postgres);
        let bad = [ix(None, "x", &["nope"], false)];
        assert!(diff(d, &t, &cols(), &HashMap::new(), (&[], &[]), (&bad, &[])).is_err());
        let dup = [ix(None, "x", &["a"], false), ix(None, "X", &["b"], false)];
        assert!(diff(d, &t, &cols(), &HashMap::new(), (&[], &[]), (&dup, &[])).is_err());
        let fk = FkDef {
            name: "fk".into(),
            columns: vec!["a".into()],
            ref_table: "other".into(),
            ref_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "CASCADE; DROP TABLE x".into(),
            original: None,
        };
        assert!(fk_clause(d, &fk).is_err());
    }

    #[test]
    fn foreign_key_add_drop_sql_and_sqlite_refusal() {
        let fk = FkDef {
            name: "fk_a".into(),
            columns: vec!["a".into()],
            ref_table: "public.other".into(),
            ref_columns: vec!["id".into()],
            on_update: "no action".into(),
            on_delete: "set null".into(),
            original: None,
        };
        let p = diff(
            Dialect(Engine::Postgres),
            &obj(Engine::Postgres),
            &cols(),
            &HashMap::new(),
            (&[], &[]),
            (&[], std::slice::from_ref(&fk)),
        )
        .unwrap();
        assert_eq!(
            p.creates,
            vec![
                "ALTER TABLE \"public\".\"t\" ADD CONSTRAINT \"fk_a\" FOREIGN KEY (\"a\") REFERENCES \"public\".\"other\" (\"id\") ON UPDATE NO ACTION ON DELETE SET NULL"
            ]
        );
        let existing = ForeignKeyInfo {
            name: "fk_a".into(),
            columns: vec!["a".into()],
            ref_table: "other".into(),
            ref_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "NO ACTION".into(),
        };
        let p = diff(
            Dialect(Engine::MySql),
            &obj(Engine::MySql),
            &cols(),
            &HashMap::new(),
            (&[], std::slice::from_ref(&existing)),
            (&[], &[]),
        )
        .unwrap();
        assert_eq!(p.drops, vec!["ALTER TABLE `main`.`t` DROP FOREIGN KEY `fk_a`"]);
        assert!(
            diff(
                Dialect(Engine::Sqlite),
                &obj(Engine::Sqlite),
                &cols(),
                &HashMap::new(),
                (&[], &[existing]),
                (&[], &[])
            )
            .is_err()
        );
        // Unchanged foreign keys are fine on SQLite.
        let same = FkDef {
            original: Some("fk_a".into()),
            on_update: "NO ACTION".into(),
            on_delete: "NO ACTION".into(),
            ref_table: "other".into(),
            ..fk
        };
        let existing = ForeignKeyInfo {
            name: "fk_a".into(),
            columns: vec!["a".into()],
            ref_table: "other".into(),
            ref_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "NO ACTION".into(),
        };
        assert!(
            diff(
                Dialect(Engine::Sqlite),
                &obj(Engine::Sqlite),
                &cols(),
                &HashMap::new(),
                (&[], &[existing]),
                (&[], &[same])
            )
            .unwrap()
            .creates
            .is_empty()
        );
    }
}
