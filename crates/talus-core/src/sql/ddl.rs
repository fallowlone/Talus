//! DDL generation for the table designer: CREATE TABLE and ALTER TABLE plans that diff the
//! original columns against the edited ones. Destructive steps are reported separately so the
//! UI can demand confirmation.

use super::constraints::{FkDef, IndexDef, create_index, fk_clause, validate_new};
use super::dialect::Dialect;
use super::lexer::{TokenKind, tokenize};
use crate::error::{DbError, DbResult};
use crate::model::{ColumnInfo, Engine, ObjectRef};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColumnDef {
    pub name: String,
    /// SQL type as typed by the user, e.g. `varchar(50)`.
    pub data_type: String,
    pub nullable: bool,
    /// SQL default expression as typed by the user (`0`, `'x'`, `now()`), or none.
    pub default: Option<String>,
    pub primary_key: bool,
    /// Preserved engine attributes (MySQL `auto_increment`, `on update …`); not user-edited.
    pub extra: String,
    pub comment: Option<String>,
}

impl From<&ColumnInfo> for ColumnDef {
    fn from(c: &ColumnInfo) -> Self {
        ColumnDef {
            name: c.name.clone(),
            data_type: c.data_type.clone(),
            nullable: c.nullable,
            default: c.default.clone(),
            primary_key: c.primary_key,
            extra: c.extra.clone().unwrap_or_default(),
            comment: c.comment.clone(),
        }
    }
}

/// T-SQL batch that drops the default constraint of a column, whatever name the server gave it.
fn drop_default_sql(d: Dialect, table_sql: &str, column: &str) -> String {
    format!(
        "DECLARE @n sysname = (SELECT dc.name FROM sys.default_constraints dc JOIN sys.columns c \
         ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id \
         WHERE dc.parent_object_id = OBJECT_ID({}) AND c.name = {}); \
         IF @n IS NOT NULL BEGIN DECLARE @sql nvarchar(max) = N'ALTER TABLE ' + {} + N' DROP CONSTRAINT ' + QUOTENAME(@n); EXEC(@sql) END",
        d.string_literal(table_sql),
        d.string_literal(column),
        d.string_literal(table_sql)
    )
}

/// MySQL column attributes that a MODIFY must repeat (generation markers excluded).
fn mysql_extra(extra: &str) -> String {
    extra.replace("DEFAULT_GENERATED", "").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A designer row: the edited definition plus the name it had in the database (None = new).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignColumn {
    pub original: Option<String>,
    pub def: ColumnDef,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlterPlan {
    pub statements: Vec<String>,
    /// Human-readable descriptions of steps that can lose data.
    pub destructive: Vec<String>,
}

/// A column type or default is spliced verbatim into DDL. It comes from the designer or, in
/// structure sync and modelling, from *another* database's metadata — and runs on the target with
/// the privileges of whoever applies it. So it must stay a single value, checked on the dialect's
/// tokens (strings and quoted identifiers may contain anything):
/// - no statement break, comment (incl. MySQL `#`), bind parameter or NUL;
/// - balanced parentheses, so nothing closes the column list or the statement early;
/// - no top-level comma: PostgreSQL reads `… SET DEFAULT 0, OWNER TO x` as a second ALTER action;
/// - for types, no `USING`: `ALTER COLUMN … TYPE int USING f(…)` evaluates `f` on every row.
fn check_fragment(engine: Engine, what: &str, s: &str) -> DbResult<()> {
    let bad = |why: &str| Err(DbError::Invalid(format!("{what} {why}: {s}")));
    if s.contains('\0') {
        return bad("must not contain NUL");
    }
    let mut depth = 0usize;
    for t in tokenize(s, engine) {
        let text = t.text(s);
        match t.kind {
            TokenKind::LineComment | TokenKind::BlockComment => return bad("must not contain comments"),
            TokenKind::Param => return bad("must not contain bind parameters"),
            TokenKind::Word if what == "type" && text.eq_ignore_ascii_case("USING") => {
                return bad("must not contain USING");
            }
            TokenKind::Punct => match text {
                ";" => return bad("must not contain ';'"),
                "(" => depth += 1,
                ")" if depth == 0 => return bad("has an unmatched ')'"),
                ")" => depth -= 1,
                "," if depth == 0 => return bad("must be a single value (no top-level ',')"),
                _ => {}
            },
            _ => {}
        }
    }
    if depth != 0 {
        return bad("has an unclosed '('");
    }
    Ok(())
}

fn column_sql(d: Dialect, c: &ColumnDef, inline_pk: bool) -> DbResult<String> {
    if c.name.trim().is_empty() {
        return Err(DbError::Invalid("column name is required".into()));
    }
    if c.data_type.trim().is_empty() {
        return Err(DbError::Invalid(format!("type is required for column {}", c.name)));
    }
    check_fragment(d.0, "type", &c.data_type)?;
    let mut s = format!("{} {}", d.quote_ident(&c.name)?, c.data_type.trim());
    if inline_pk {
        s.push_str(" PRIMARY KEY");
    }
    if d.0 == Engine::SqlServer && c.extra.contains("IDENTITY") {
        s.push_str(" IDENTITY(1,1)");
    }
    if d.0 == Engine::Postgres && c.extra.contains("IDENTITY") {
        s.push_str(" GENERATED BY DEFAULT AS IDENTITY");
    }
    if !c.nullable && !inline_pk {
        s.push_str(" NOT NULL");
    } else if d.0 == Engine::SqlServer && c.nullable {
        // Without it the column's nullability depends on a session setting.
        s.push_str(" NULL");
    }
    if let Some(def) = c.default.as_deref().map(str::trim).filter(|x| !x.is_empty()) {
        check_fragment(d.0, "default", def)?;
        s.push_str(&format!(" DEFAULT {def}"));
    }
    if d.0 == Engine::MySql {
        let extra = mysql_extra(&c.extra);
        if !extra.is_empty() {
            check_fragment(d.0, "column attributes", &extra)?;
            s.push(' ');
            s.push_str(&extra);
        }
        if let Some(cm) = c.comment.as_deref().filter(|x| !x.is_empty()) {
            s.push_str(&format!(" COMMENT {}", d.string_literal(cm)));
        }
    }
    Ok(s)
}

pub fn create_table(d: Dialect, table: &ObjectRef, columns: &[ColumnDef]) -> DbResult<String> {
    create_table_with(d, table, columns, &[])
}

/// CREATE TABLE with foreign keys as table constraints (the only place SQLite accepts them).
pub fn create_table_with(d: Dialect, table: &ObjectRef, columns: &[ColumnDef], fks: &[FkDef]) -> DbResult<String> {
    if columns.is_empty() {
        return Err(DbError::Invalid("a table needs at least one column".into()));
    }
    let mut names = std::collections::HashSet::new();
    for c in columns {
        if !names.insert(c.name.to_lowercase()) {
            return Err(DbError::Invalid(format!("duplicate column name: {}", c.name)));
        }
    }
    let pk: Vec<&ColumnDef> = columns.iter().filter(|c| c.primary_key).collect();
    // A single INTEGER primary key in SQLite should be inline so it aliases rowid.
    let inline = pk.len() == 1 && d.0 == Engine::Sqlite && pk[0].data_type.trim().eq_ignore_ascii_case("integer");
    let mut parts = columns.iter().map(|c| column_sql(d, c, inline && c.primary_key)).collect::<DbResult<Vec<_>>>()?;
    if !pk.is_empty() && !inline {
        let cols = pk.iter().map(|c| d.quote_ident(&c.name)).collect::<DbResult<Vec<_>>>()?;
        parts.push(format!("PRIMARY KEY ({})", cols.join(", ")));
    }
    for fk in fks {
        parts.push(fk_clause(d, fk)?);
    }
    Ok(format!("CREATE TABLE {} (\n  {}\n)", d.qualified(table)?, parts.join(",\n  ")))
}

pub fn alter_table(
    d: Dialect,
    table: &ObjectRef,
    original: &[ColumnDef],
    edited: &[DesignColumn],
) -> DbResult<AlterPlan> {
    let t = d.qualified(table)?;
    let q = |s: &str| d.quote_ident(s);
    let mut plan = AlterPlan::default();
    let mut names = std::collections::HashSet::new();
    for c in edited {
        if !names.insert(c.def.name.to_lowercase()) {
            return Err(DbError::Invalid(format!("duplicate column name: {}", c.def.name)));
        }
    }
    let old_pk: Vec<&str> = original.iter().filter(|c| c.primary_key).map(|c| c.name.as_str()).collect();
    let new_pk: Vec<String> = edited
        .iter()
        .filter(|c| c.def.primary_key)
        .map(|c| c.original.clone().unwrap_or_else(|| c.def.name.clone()))
        .collect();
    if old_pk.iter().map(|s| s.to_string()).collect::<Vec<_>>() != new_pk {
        return Err(DbError::Invalid("changing the primary key is not supported by the designer; use a query".into()));
    }
    // Drops first (frees names), then renames/changes, then adds.
    for o in original {
        if !edited.iter().any(|e| e.original.as_deref() == Some(o.name.as_str())) {
            if d.0 == Engine::SqlServer {
                plan.statements.push(drop_default_sql(d, &t, &o.name));
            }
            plan.statements.push(format!("ALTER TABLE {t} DROP COLUMN {}", q(&o.name)?));
            plan.destructive.push(format!("drop column {} and its data", o.name));
        }
    }
    for e in edited {
        let Some(orig_name) = &e.original else { continue };
        let Some(o) = original.iter().find(|o| &o.name == orig_name) else {
            return Err(DbError::Invalid(format!("unknown original column {orig_name}")));
        };
        let new = &e.def;
        if new.name != o.name {
            if new.name.trim().is_empty() {
                return Err(DbError::Invalid("column name is required".into()));
            }
            plan.statements.push(if d.0 == Engine::SqlServer {
                format!(
                    "EXEC sp_rename {}, {}, N'COLUMN'",
                    d.string_literal(&format!("{t}.{}", q(&o.name)?)),
                    d.string_literal(&new.name)
                )
            } else {
                format!("ALTER TABLE {t} RENAME COLUMN {} TO {}", q(&o.name)?, q(&new.name)?)
            });
        }
        let type_changed = !new.data_type.trim().eq_ignore_ascii_case(o.data_type.trim());
        let null_changed = new.nullable != o.nullable;
        let norm = |x: &Option<String>| x.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        let default_changed = norm(&new.default) != norm(&o.default);
        if !(type_changed || null_changed || default_changed) {
            continue;
        }
        let col = q(&new.name)?;
        match d.0 {
            Engine::Sqlite | Engine::Redis | Engine::MongoDb => {
                return Err(DbError::Invalid(format!(
                    "SQLite cannot change the type, nullability or default of an existing column ({}); create a new table and copy the data",
                    o.name
                )));
            }
            Engine::Postgres => {
                if type_changed {
                    check_fragment(d.0, "type", &new.data_type)?;
                    plan.statements.push(format!("ALTER TABLE {t} ALTER COLUMN {col} TYPE {}", new.data_type.trim()));
                    plan.destructive.push(format!("change type of {} (values may be converted or rejected)", new.name));
                }
                if null_changed {
                    let what = if new.nullable { "DROP NOT NULL" } else { "SET NOT NULL" };
                    plan.statements.push(format!("ALTER TABLE {t} ALTER COLUMN {col} {what}"));
                }
                if default_changed {
                    match norm(&new.default) {
                        Some(def) => {
                            check_fragment(d.0, "default", &def)?;
                            plan.statements.push(format!("ALTER TABLE {t} ALTER COLUMN {col} SET DEFAULT {def}"));
                        }
                        None => plan.statements.push(format!("ALTER TABLE {t} ALTER COLUMN {col} DROP DEFAULT")),
                    }
                }
            }
            Engine::SqlServer => {
                check_fragment(d.0, "type", &new.data_type)?;
                let new_default = norm(&new.default);
                if let Some(def) = &new_default {
                    check_fragment(d.0, "default", def)?;
                }
                // A default constraint blocks ALTER COLUMN, so it is dropped first and re-added.
                if o.default.is_some() || type_changed || null_changed {
                    plan.statements.push(drop_default_sql(d, &t, &new.name));
                }
                if type_changed || null_changed {
                    plan.statements.push(format!(
                        "ALTER TABLE {t} ALTER COLUMN {col} {} {}",
                        new.data_type.trim(),
                        if new.nullable { "NULL" } else { "NOT NULL" }
                    ));
                }
                if type_changed {
                    plan.destructive.push(format!("change type of {} (values may be converted or rejected)", new.name));
                }
                if let Some(def) = new_default {
                    plan.statements.push(format!("ALTER TABLE {t} ADD DEFAULT {def} FOR {col}"));
                }
            }
            Engine::MySql => {
                if mysql_extra(&new.extra).contains("GENERATED") {
                    return Err(DbError::Invalid(format!(
                        "{} is a generated column; change it with a query",
                        new.name
                    )));
                }
                plan.statements.push(format!("ALTER TABLE {t} MODIFY COLUMN {}", column_sql(d, new, false)?));
                if type_changed {
                    plan.destructive.push(format!("change type of {} (values may be truncated)", new.name));
                }
            }
        }
    }
    for e in edited.iter().filter(|e| e.original.is_none()) {
        if e.def.primary_key {
            return Err(DbError::Invalid("new columns cannot join the primary key in the designer".into()));
        }
        let add = if d.0 == Engine::SqlServer { "ADD" } else { "ADD COLUMN" };
        plan.statements.push(format!("ALTER TABLE {t} {add} {}", column_sql(d, &e.def, false)?));
    }
    Ok(plan)
}

/// First statement of a SQLite rebuild plan. `Session::execute_batch` runs it before opening the
/// transaction (the pragma is a no-op inside one) and restores `foreign_keys = ON` afterwards.
pub const SQLITE_FK_OFF: &str = "PRAGMA foreign_keys = OFF";

/// SQLite cannot ALTER a column's type/nullability/default or touch foreign keys, so the table
/// is rebuilt: create the new shape, copy the surviving columns, drop the old table, rename.
pub fn rebuild_sqlite(
    d: Dialect,
    table: &ObjectRef,
    edited: &[DesignColumn],
    fks: &[FkDef],
    indexes: &[IndexDef],
) -> DbResult<AlterPlan> {
    let temp = ObjectRef { name: format!("{}__talus_new", table.name), ..table.clone() };
    let cols: Vec<ColumnDef> = edited.iter().map(|e| e.def.clone()).collect();
    let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
    validate_new(d, &names, indexes, fks)?;
    let mut statements = vec![SQLITE_FK_OFF.to_string(), create_table_with(d, &temp, &cols, fks)?];
    let copied: Vec<&DesignColumn> = edited.iter().filter(|e| e.original.is_some()).collect();
    if !copied.is_empty() {
        let to = copied.iter().map(|e| d.quote_ident(&e.def.name)).collect::<DbResult<Vec<_>>>()?.join(", ");
        let from = copied
            .iter()
            .filter_map(|e| e.original.as_deref())
            .map(|o| d.quote_ident(o))
            .collect::<DbResult<Vec<_>>>()?
            .join(", ");
        statements.push(format!(
            "INSERT INTO {} ({to}) SELECT {from} FROM {}",
            d.qualified(&temp)?,
            d.qualified(table)?
        ));
    }
    statements.push(format!("DROP TABLE {}", d.qualified(table)?));
    statements.push(format!("ALTER TABLE {} RENAME TO {}", d.qualified(&temp)?, d.quote_ident(&table.name)?));
    for i in indexes {
        statements.push(create_index(d, table, i)?);
    }
    Ok(AlterPlan {
        statements,
        destructive: vec![format!("{} is rebuilt: triggers defined on it are not preserved", table.name)],
    })
}

/// Common column types offered by the designer's type picker.
pub fn common_types(engine: Engine) -> &'static [&'static str] {
    match engine {
        Engine::Redis | Engine::MongoDb => &[],
        Engine::Sqlite => &["INTEGER", "TEXT", "REAL", "NUMERIC", "BLOB"],
        Engine::Postgres => &[
            "integer",
            "bigint",
            "smallint",
            "serial",
            "bigserial",
            "numeric(10,2)",
            "real",
            "double precision",
            "boolean",
            "text",
            "varchar(255)",
            "char(1)",
            "date",
            "timestamp",
            "timestamptz",
            "time",
            "interval",
            "uuid",
            "json",
            "jsonb",
            "bytea",
        ],
        Engine::SqlServer => &[
            "int",
            "bigint",
            "smallint",
            "tinyint",
            "bit",
            "decimal(18,2)",
            "money",
            "float",
            "real",
            "nvarchar(255)",
            "varchar(255)",
            "nvarchar(max)",
            "nchar(10)",
            "char(10)",
            "date",
            "datetime2",
            "datetime",
            "time",
            "datetimeoffset",
            "uniqueidentifier",
            "varbinary(max)",
            "xml",
        ],
        Engine::MySql => &[
            "int",
            "bigint",
            "smallint",
            "tinyint(1)",
            "decimal(10,2)",
            "float",
            "double",
            "varchar(255)",
            "char(1)",
            "text",
            "mediumtext",
            "longtext",
            "date",
            "datetime",
            "timestamp",
            "time",
            "json",
            "blob",
            "enum('a','b')",
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frag_col(ty: &str, default: Option<&str>) -> ColumnDef {
        ColumnDef {
            name: "c".into(),
            data_type: ty.into(),
            nullable: true,
            default: default.map(String::from),
            ..ColumnDef::default()
        }
    }

    #[test]
    fn fragments_from_another_database_cannot_add_actions_or_run_code() {
        let pg = Dialect(Engine::Postgres);
        let t = ObjectRef {
            database: "db".into(),
            schema: Some("public".into()),
            name: "t".into(),
            kind: crate::model::ObjectKind::Table,
        };
        let alter = |old: ColumnDef, new: ColumnDef| {
            alter_table(
                pg,
                &t,
                std::slice::from_ref(&old),
                &[DesignColumn { original: Some(old.name.clone()), def: new }],
            )
        };
        // A second ALTER action smuggled through a default or a type.
        assert!(alter(frag_col("int", None), frag_col("int", Some("0, OWNER TO attacker"))).is_err());
        assert!(alter(frag_col("int", None), frag_col("int, OWNER TO attacker", None)).is_err());
        // USING runs an expression on every row during ALTER TYPE.
        assert!(alter(frag_col("int", None), frag_col("bigint USING (pg_sleep(1))::int", None)).is_err());
        // Closing the column list / statement early, comments, parameters.
        for bad in ["0) WITH (fillfactor=10", "0; DROP TABLE t", "0 -- x", "0 /* x */", "$1"] {
            assert!(create_table(pg, &t, &[frag_col("int", Some(bad))]).is_err(), "{bad}");
        }
        assert!(create_table(Dialect(Engine::MySql), &t, &[frag_col("int", Some("0 # rest"))]).is_err());
        // Legitimate defaults and types keep working, including commas inside parentheses or strings.
        for ok in ["nextval('t_id_seq'::regclass)", "'a,b;c -- d'", "now()", "((0))", "'{}'::jsonb", "$q$x, y$q$"] {
            assert!(create_table(pg, &t, &[frag_col("text", Some(ok))]).is_ok(), "{ok}");
        }
        for ok in
            ["numeric(10,2)", "character varying(255)", "timestamp(3) with time zone", "int[]", "geometry(Point,4326)"]
        {
            assert!(create_table(pg, &t, &[frag_col(ok, None)]).is_ok(), "{ok}");
        }
        assert!(create_table(Dialect(Engine::MySql), &t, &[frag_col("enum('a','b,c')", None)]).is_ok());
    }
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

    fn col(name: &str, ty: &str, nullable: bool, pk: bool) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            nullable,
            default: None,
            primary_key: pk,
            ..Default::default()
        }
    }

    #[test]
    fn create_table_per_engine() {
        let cols = vec![
            col("id", "INTEGER", false, true),
            ColumnDef { default: Some("'x'".into()), ..col("name", "TEXT", false, false) },
        ];
        assert_eq!(
            create_table(Dialect(Engine::Sqlite), &obj(Engine::Sqlite), &cols).unwrap(),
            "CREATE TABLE \"main\".\"t\" (\n  \"id\" INTEGER PRIMARY KEY,\n  \"name\" TEXT NOT NULL DEFAULT 'x'\n)"
        );
        assert_eq!(
            create_table(Dialect(Engine::MySql), &obj(Engine::MySql), &cols).unwrap(),
            "CREATE TABLE `main`.`t` (\n  `id` INTEGER NOT NULL,\n  `name` TEXT NOT NULL DEFAULT 'x',\n  PRIMARY KEY (`id`)\n)"
        );
    }

    #[test]
    fn rejects_bad_fragments_and_duplicates() {
        let bad = vec![col("a", "int; drop table x", true, false)];
        assert!(create_table(Dialect(Engine::Postgres), &obj(Engine::Postgres), &bad).is_err());
        let bad = vec![ColumnDef { default: Some("1 -- x".into()), ..col("a", "int", true, false) }];
        assert!(create_table(Dialect(Engine::Postgres), &obj(Engine::Postgres), &bad).is_err());
        let dup = vec![col("a", "int", true, false), col("A", "int", true, false)];
        assert!(create_table(Dialect(Engine::Postgres), &obj(Engine::Postgres), &dup).is_err());
        assert!(create_table(Dialect(Engine::Postgres), &obj(Engine::Postgres), &[]).is_err());
    }

    #[test]
    fn alter_plan_postgres_and_destructive_flags() {
        let original =
            vec![col("id", "integer", false, true), col("a", "text", true, false), col("b", "text", true, false)];
        let edited = vec![
            DesignColumn { original: Some("id".into()), def: col("id", "integer", false, true) },
            DesignColumn {
                original: Some("a".into()),
                def: ColumnDef { default: Some("'z'".into()), ..col("a2", "varchar(10)", false, false) },
            },
            DesignColumn { original: None, def: col("c", "int", true, false) },
        ];
        let plan = alter_table(Dialect(Engine::Postgres), &obj(Engine::Postgres), &original, &edited).unwrap();
        assert_eq!(
            plan.statements,
            vec![
                "ALTER TABLE \"public\".\"t\" DROP COLUMN \"b\"",
                "ALTER TABLE \"public\".\"t\" RENAME COLUMN \"a\" TO \"a2\"",
                "ALTER TABLE \"public\".\"t\" ALTER COLUMN \"a2\" TYPE varchar(10)",
                "ALTER TABLE \"public\".\"t\" ALTER COLUMN \"a2\" SET NOT NULL",
                "ALTER TABLE \"public\".\"t\" ALTER COLUMN \"a2\" SET DEFAULT 'z'",
                "ALTER TABLE \"public\".\"t\" ADD COLUMN \"c\" int",
            ]
        );
        assert_eq!(plan.destructive.len(), 2);
    }

    #[test]
    fn alter_plan_mysql_modify_and_sqlite_limits() {
        let original = vec![col("id", "int", false, true), col("a", "varchar(5)", true, false)];
        let edited = vec![
            DesignColumn { original: Some("id".into()), def: col("id", "int", false, true) },
            DesignColumn { original: Some("a".into()), def: col("a", "varchar(50)", false, false) },
        ];
        let plan = alter_table(Dialect(Engine::MySql), &obj(Engine::MySql), &original, &edited).unwrap();
        assert_eq!(plan.statements, vec!["ALTER TABLE `main`.`t` MODIFY COLUMN `a` varchar(50) NOT NULL"]);
        assert!(alter_table(Dialect(Engine::Sqlite), &obj(Engine::Sqlite), &original, &edited).is_err());
        // MODIFY repeats auto_increment / on update / comment so they are not lost.
        let mut o2 = original.clone();
        o2[1].extra = "DEFAULT_GENERATED on update CURRENT_TIMESTAMP".into();
        o2[1].comment = Some("it's".into());
        let mut e2 = edited.clone();
        e2[1].def.extra = o2[1].extra.clone();
        e2[1].def.comment = o2[1].comment.clone();
        let plan = alter_table(Dialect(Engine::MySql), &obj(Engine::MySql), &o2, &e2).unwrap();
        assert_eq!(
            plan.statements,
            vec![
                "ALTER TABLE `main`.`t` MODIFY COLUMN `a` varchar(50) NOT NULL on update CURRENT_TIMESTAMP COMMENT 'it''s'"
            ]
        );
        // SQLite can still add, rename and drop.
        let edited = vec![
            DesignColumn { original: Some("id".into()), def: col("id", "int", false, true) },
            DesignColumn { original: Some("a".into()), def: col("renamed", "varchar(5)", true, false) },
            DesignColumn { original: None, def: col("n", "TEXT", true, false) },
        ];
        let plan = alter_table(Dialect(Engine::Sqlite), &obj(Engine::Sqlite), &original, &edited).unwrap();
        assert_eq!(plan.statements.len(), 2);
        assert!(plan.destructive.is_empty());
        // No changes → empty plan; PK changes rejected.
        let same: Vec<DesignColumn> =
            original.iter().map(|c| DesignColumn { original: Some(c.name.clone()), def: c.clone() }).collect();
        assert!(
            alter_table(Dialect(Engine::MySql), &obj(Engine::MySql), &original, &same).unwrap().statements.is_empty()
        );
        let mut pk = same.clone();
        pk[1].def.primary_key = true;
        assert!(alter_table(Dialect(Engine::MySql), &obj(Engine::MySql), &original, &pk).is_err());
    }
}
