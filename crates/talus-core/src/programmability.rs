//! Listing of stored routines and triggers with their definitions, plus the
//! creation skeletons and `DROP` statements the Routines tab builds on.

use crate::engine::{ScriptOptions, Session};
use crate::error::{DbError, DbResult};
use crate::model::{Engine, ObjectKind, ObjectRef, Outcome};
use crate::sql::Dialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgKind {
    Function,
    Procedure,
    Trigger,
    /// Oracle package (specification and body).
    Package,
}

impl ProgKind {
    pub fn label(self) -> &'static str {
        match self {
            ProgKind::Function => "Function",
            ProgKind::Procedure => "Procedure",
            ProgKind::Trigger => "Trigger",
            ProgKind::Package => "Package",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgItem {
    pub kind: ProgKind,
    pub name: String,
    /// Table a trigger is attached to.
    pub table: Option<String>,
    /// `CREATE …` text; empty when the server hides it (insufficient privileges, encrypted).
    pub definition: String,
    /// What the debugger attaches to: a PostgreSQL function / procedure OID, or the OBJECT_ID of an Oracle
    /// standalone procedure / function.
    pub debug_id: Option<u32>,
}

async fn rows(s: &mut Session, sql: &str) -> DbResult<Vec<Vec<Option<String>>>> {
    let out = s.run_script(sql, &ScriptOptions::default()).await;
    if let Some(e) = out.errors.first() {
        return Err(DbError::Invalid(e.error.to_string()));
    }
    match out.results.into_iter().next().map(|r| r.outcome) {
        Some(Outcome::Rows(q)) => Ok(q.rows),
        _ => Ok(Vec::new()),
    }
}

fn cell(r: &[Option<String>], i: usize) -> String {
    r.get(i).cloned().flatten().unwrap_or_default()
}

/// `SHOW FUNCTIONS / PROCEDURES` reports arguments as `NAME TYPE, …`; `GET_DDL` wants just the
/// types. Commas inside a type's own parentheses (`NUMBER(10,2)`) are not separators.
fn ddl_signature(args: &str) -> String {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for c in args.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    let types: Vec<String> = parts
        .iter()
        .map(|p| {
            let mut words = p.split_whitespace();
            words.next();
            words.collect::<Vec<_>>().join(" ")
        })
        .filter(|t| !t.is_empty())
        .collect();
    format!("({})", types.join(", "))
}

/// Routines and triggers of a MySQL database / PostgreSQL schema / the SQLite file.
pub async fn list(s: &mut Session, database: &str, schema: Option<&str>) -> DbResult<Vec<ProgItem>> {
    let d = Dialect(s.engine());
    let mut items = Vec::new();
    match s.engine() {
        Engine::Redis | Engine::MongoDb => {}
        Engine::Sqlite => {
            for r in
                rows(s, "SELECT name, tbl_name, sql FROM sqlite_master WHERE type = 'trigger' ORDER BY name").await?
            {
                items.push(ProgItem {
                    kind: ProgKind::Trigger,
                    name: cell(&r, 0),
                    table: Some(cell(&r, 1)),
                    definition: cell(&r, 2),
                    debug_id: None,
                });
            }
        }
        Engine::SqlServer => {
            let ns = d.string_literal(schema.unwrap_or("dbo"));
            let routines = format!(
                "SELECT o.type, o.name, m.definition FROM sys.objects o JOIN sys.schemas sc ON sc.schema_id = o.schema_id \
                 LEFT JOIN sys.sql_modules m ON m.object_id = o.object_id \
                 WHERE sc.name = {ns} AND o.type IN ('P', 'FN', 'IF', 'TF') AND o.is_ms_shipped = 0 ORDER BY o.name"
            );
            for r in rows(s, &routines).await? {
                let kind = if cell(&r, 0).trim() == "P" { ProgKind::Procedure } else { ProgKind::Function };
                items.push(ProgItem { kind, name: cell(&r, 1), table: None, definition: cell(&r, 2), debug_id: None });
            }
            let triggers = format!(
                "SELECT t.name, OBJECT_NAME(t.parent_id), m.definition FROM sys.triggers t \
                 JOIN sys.objects po ON po.object_id = t.parent_id JOIN sys.schemas sc ON sc.schema_id = po.schema_id \
                 LEFT JOIN sys.sql_modules m ON m.object_id = t.object_id \
                 WHERE sc.name = {ns} AND t.is_ms_shipped = 0 ORDER BY t.name"
            );
            for r in rows(s, &triggers).await? {
                items.push(ProgItem {
                    kind: ProgKind::Trigger,
                    name: cell(&r, 0),
                    table: Some(cell(&r, 1)),
                    definition: cell(&r, 2),
                    debug_id: None,
                });
            }
        }
        Engine::Oracle => {
            let owner = match schema {
                Some(s) => d.string_literal(s),
                None => "SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')".to_string(),
            };
            let objects = format!(
                "SELECT object_type, object_name, object_id FROM all_objects WHERE owner = {owner} \
                 AND object_type IN ('PROCEDURE', 'FUNCTION', 'PACKAGE', 'TRIGGER') ORDER BY object_type, object_name"
            );
            let tables = format!("SELECT trigger_name, table_name FROM all_triggers WHERE owner = {owner}");
            let trigger_tables: std::collections::HashMap<String, String> =
                rows(s, &tables).await?.iter().map(|r| (cell(r, 0), cell(r, 1))).collect();
            for r in rows(s, &objects).await? {
                let (ty, name) = (cell(&r, 0), cell(&r, 1));
                let ddl = |kind: &str| {
                    format!("SELECT DBMS_METADATA.GET_DDL('{kind}', {}, {owner}) FROM dual", d.string_literal(&name))
                };
                let kinds: &[&str] = if ty == "PACKAGE" { &["PACKAGE_SPEC", "PACKAGE_BODY"] } else { &[ty.as_str()] };
                let mut parts = Vec::new();
                for kind in kinds {
                    // No privilege to read the source (wrapped / other owner): leave the definition empty.
                    if let Ok(v) = rows(s, &ddl(kind)).await {
                        parts.extend(v.first().map(|r| cell(r, 0).trim().to_string()).filter(|t| !t.is_empty()));
                    }
                }
                let (kind, table) = match ty.as_str() {
                    "PROCEDURE" => (ProgKind::Procedure, None),
                    "FUNCTION" => (ProgKind::Function, None),
                    "PACKAGE" => (ProgKind::Package, None),
                    _ => (ProgKind::Trigger, trigger_tables.get(&name).cloned()),
                };
                let debug_id = matches!(kind, ProgKind::Procedure | ProgKind::Function)
                    .then(|| cell(&r, 2).parse().ok())
                    .flatten();
                items.push(ProgItem { kind, name, table, definition: parts.join("\n/\n\n"), debug_id });
            }
        }
        Engine::Snowflake => {
            // Overloads share a name: GET_DDL wants the signature, which SHOW reports as `arguments`.
            // (Snowflake has no triggers, so only routines are listed.)
            let sch = schema.unwrap_or("PUBLIC");
            let scope = format!("IN SCHEMA {}.{}", d.quote_ident(database)?, d.quote_ident(sch)?);
            for (what, kind, get) in
                [("FUNCTIONS", ProgKind::Function, "FUNCTION"), ("PROCEDURES", ProgKind::Procedure, "PROCEDURE")]
            {
                let out = s.run_script(&format!("SHOW {what} {scope}"), &ScriptOptions::default()).await;
                if let Some(e) = out.errors.first() {
                    return Err(DbError::Invalid(e.error.to_string()));
                }
                let found = match out.results.into_iter().next().map(|r| r.outcome) {
                    Some(Outcome::Rows(q)) => {
                        let idx = |n: &str| q.columns.iter().position(|c| c.name.eq_ignore_ascii_case(n));
                        match (idx("name"), idx("arguments")) {
                            (Some(n), Some(a)) => q.rows.iter().map(|r| (cell(r, n), cell(r, a))).collect::<Vec<_>>(),
                            _ => Vec::new(),
                        }
                    }
                    _ => Vec::new(),
                };
                for (name, args) in found {
                    let full = format!(
                        "{}.{}.{}{}",
                        d.quote_ident(database)?,
                        d.quote_ident(sch)?,
                        d.quote_ident(&name)?,
                        ddl_signature(&args)
                    );
                    // No privilege to read the source: leave the definition empty.
                    let definition = rows(s, &format!("SELECT GET_DDL('{get}', '{}')", full.replace('\'', "''")))
                        .await
                        .map(|v| v.first().map(|r| cell(r, 0).trim().to_string()).unwrap_or_default())
                        .unwrap_or_default();
                    items.push(ProgItem { kind, name, table: None, definition, debug_id: None });
                }
            }
        }
        Engine::Postgres => {
            // Routines owned by extensions (e.g. pldbgapi's pldbg_*) are not the user's code.
            let ns = d.string_literal(schema.unwrap_or("public"));
            let funcs = format!(
                "SELECT p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')', p.prokind::text, pg_get_functiondef(p.oid), p.oid::text \
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = {ns} AND p.prokind IN ('f', 'p') \
                   AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_proc'::regclass \
                                   AND d.objid = p.oid AND d.deptype = 'e') ORDER BY 1"
            );
            for r in rows(s, &funcs).await? {
                let kind = if cell(&r, 1) == "p" { ProgKind::Procedure } else { ProgKind::Function };
                let debug_id = cell(&r, 3).parse().ok();
                items.push(ProgItem { kind, name: cell(&r, 0), table: None, definition: cell(&r, 2), debug_id });
            }
            let trig = format!(
                "SELECT t.tgname, c.relname, pg_get_triggerdef(t.oid, true) FROM pg_trigger t \
                 JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE NOT t.tgisinternal AND n.nspname = {ns} ORDER BY 1"
            );
            for r in rows(s, &trig).await? {
                items.push(ProgItem {
                    kind: ProgKind::Trigger,
                    name: cell(&r, 0),
                    table: Some(cell(&r, 1)),
                    definition: cell(&r, 2),
                    debug_id: None,
                });
            }
        }
        Engine::MySql => {
            let db = d.string_literal(database);
            let qdb = d.quote_ident(database)?;
            let routines = format!(
                "SELECT ROUTINE_TYPE, ROUTINE_NAME FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = {db} ORDER BY ROUTINE_NAME"
            );
            for r in rows(s, &routines).await? {
                let (is_proc, name) = (cell(&r, 0) == "PROCEDURE", cell(&r, 1));
                let show = format!(
                    "SHOW CREATE {} {qdb}.{}",
                    if is_proc { "PROCEDURE" } else { "FUNCTION" },
                    d.quote_ident(&name)?
                );
                // Column 3 holds the statement; it is NULL without the privilege to see the body.
                let definition = rows(s, &show).await.map(|v| v.first().map(|r| cell(r, 2)).unwrap_or_default());
                items.push(ProgItem {
                    kind: if is_proc { ProgKind::Procedure } else { ProgKind::Function },
                    name,
                    table: None,
                    definition: definition.unwrap_or_default(),
                    debug_id: None,
                });
            }
            let triggers = format!(
                "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = {db} ORDER BY TRIGGER_NAME"
            );
            for r in rows(s, &triggers).await? {
                let name = cell(&r, 0);
                let show = format!("SHOW CREATE TRIGGER {qdb}.{}", d.quote_ident(&name)?);
                let definition = rows(s, &show).await.map(|v| v.first().map(|r| cell(r, 2)).unwrap_or_default());
                items.push(ProgItem {
                    kind: ProgKind::Trigger,
                    name,
                    table: Some(cell(&r, 1)),
                    definition: definition.unwrap_or_default(),
                    debug_id: None,
                });
            }
        }
    }
    Ok(items)
}

/// Qualifies `name` the way generated DDL spells it on `engine` (schema-qualified
/// where the engine has schemas, database-qualified for MySQL, plain elsewhere).
fn qualify(engine: Engine, database: &str, schema: Option<&str>, name: &str) -> DbResult<String> {
    let obj = ObjectRef {
        database: database.to_string(),
        schema: schema.map(String::from),
        name: name.to_string(),
        kind: ObjectKind::Table,
    };
    Dialect(engine).qualified(&obj)
}

fn resolved_schema(engine: Engine, schema: Option<&str>) -> Option<String> {
    schema.map(String::from).or_else(|| engine.default_schema().map(String::from))
}

/// The `DROP` statement for a listed routine, trigger or package. PostgreSQL
/// routine names carry their identity arguments (`f(int, text)`), which the
/// statement keeps; a PostgreSQL trigger also needs its table.
pub fn drop_sql(engine: Engine, database: &str, schema: Option<&str>, item: &ProgItem) -> DbResult<String> {
    let d = Dialect(engine);
    let schema = resolved_schema(engine, schema);
    if item.kind == ProgKind::Trigger {
        if engine == Engine::Sqlite {
            return Ok(format!("DROP TRIGGER {}", d.quote_ident(&item.name)?));
        }
        if engine == Engine::Postgres {
            let table = item.table.as_deref().ok_or_else(|| {
                crate::error::DbError::Invalid("dropping a PostgreSQL trigger needs its table".into())
            })?;
            return Ok(format!(
                "DROP TRIGGER {} ON {}",
                d.quote_ident(&item.name)?,
                qualify(engine, database, schema.as_deref(), table)?
            ));
        }
    }
    let kw = match item.kind {
        ProgKind::Function => "FUNCTION",
        ProgKind::Procedure => "PROCEDURE",
        ProgKind::Trigger => "TRIGGER",
        ProgKind::Package => "PACKAGE",
    };
    // Only PostgreSQL names embed `(args)`; split them off so the name itself is quoted.
    let (base, args) = match item.name.split_once('(') {
        Some((b, a)) => (b, format!("({a}")),
        None => (item.name.as_str(), String::new()),
    };
    Ok(format!("DROP {kw} {}{args}", qualify(engine, database, schema.as_deref(), base)?))
}

/// A creation skeleton for the query editor: one script in the engine's own
/// spelling, with placeholder names to rename before running. PostgreSQL
/// triggers need their trigger function too, so that template has two statements.
pub fn template(engine: Engine, kind: ProgKind, database: &str, schema: Option<&str>) -> String {
    let schema = resolved_schema(engine, schema);
    let q = |name: &str| qualify(engine, database, schema.as_deref(), name).unwrap_or_else(|_| name.to_string());
    let (f, p, t, tbl) = (q("new_function"), q("new_procedure"), q("new_trigger"), q("table_name"));
    match (engine, kind) {
        (Engine::Postgres, ProgKind::Function) => format!(
            "CREATE OR REPLACE FUNCTION {f}()\nRETURNS integer\nLANGUAGE plpgsql\nAS $$\nBEGIN\n    RETURN 1;\nEND;\n$$;"
        ),
        (Engine::Postgres, ProgKind::Procedure) => format!(
            "CREATE OR REPLACE PROCEDURE {p}()\nLANGUAGE plpgsql\nAS $$\nBEGIN\n    -- statements here\nEND;\n$$;"
        ),
        (Engine::Postgres, ProgKind::Trigger) => {
            let tf = q("new_trigger_function");
            format!(
                "CREATE OR REPLACE FUNCTION {tf}()\nRETURNS trigger\nLANGUAGE plpgsql\nAS $$\nBEGIN\n    RETURN NEW;\nEND;\n$$;\n\nCREATE TRIGGER {t}\nAFTER INSERT ON {tbl}\nFOR EACH ROW EXECUTE FUNCTION {tf}();"
            )
        }
        (Engine::MySql, ProgKind::Function) => {
            format!("CREATE FUNCTION {f}() RETURNS INT DETERMINISTIC\nBEGIN\n    RETURN 1;\nEND")
        }
        (Engine::MySql, ProgKind::Procedure) => {
            format!("CREATE PROCEDURE {p}()\nBEGIN\n    -- statements here\nEND")
        }
        (Engine::MySql, ProgKind::Trigger) => {
            format!("CREATE TRIGGER {t} AFTER INSERT ON {tbl} FOR EACH ROW\nBEGIN\n    -- statements here\nEND")
        }
        (Engine::Sqlite, _) => {
            format!("CREATE TRIGGER {t} AFTER INSERT ON {tbl}\nBEGIN\n    SELECT 1;\nEND;")
        }
        (Engine::SqlServer, ProgKind::Function) => {
            format!("CREATE OR ALTER FUNCTION {f}() RETURNS INT AS BEGIN RETURN 1 END")
        }
        (Engine::SqlServer, ProgKind::Procedure) => {
            format!("CREATE OR ALTER PROCEDURE {p} AS BEGIN SELECT 1 END")
        }
        (Engine::SqlServer, ProgKind::Trigger) => {
            format!("CREATE OR ALTER TRIGGER {t} ON {tbl} AFTER INSERT AS BEGIN SELECT 1 END")
        }
        (Engine::Oracle, ProgKind::Function) => {
            format!("CREATE OR REPLACE FUNCTION {} RETURN NUMBER IS\nBEGIN\n    RETURN 1;\nEND;", q("new_function"))
        }
        (Engine::Oracle, ProgKind::Procedure) => {
            format!("CREATE OR REPLACE PROCEDURE {} IS\nBEGIN\n    NULL;\nEND;", q("new_procedure"))
        }
        (Engine::Oracle, ProgKind::Trigger) => format!(
            "CREATE OR REPLACE TRIGGER {} AFTER INSERT ON {} FOR EACH ROW\nBEGIN\n    NULL;\nEND;",
            q("new_trigger"),
            q("table_name")
        ),
        (Engine::Snowflake, ProgKind::Function) => {
            format!("CREATE OR REPLACE FUNCTION {f}()\nRETURNS NUMBER\nAS\n'SELECT 1';")
        }
        (Engine::Snowflake, ProgKind::Procedure) => format!(
            "CREATE OR REPLACE PROCEDURE {p}()\nRETURNS VARCHAR\nLANGUAGE SQL\nAS\n$$\nBEGIN\n    RETURN 'done';\nEND;\n$$;"
        ),
        // Engines without this kind (MongoDB / Redis have no programmability tab,
        // Snowflake has no triggers) still get a plain, ignorable skeleton.
        _ => format!("CREATE OR REPLACE PROCEDURE {p}()\nBEGIN\nEND"),
    }
}

/// A `CREATE VIEW` skeleton for the query editor. Views are edited by opening the DDL the
/// server reports (the object's Information pane), so only creation needs a template.
pub fn view_template(engine: Engine, database: &str, schema: Option<&str>) -> String {
    let schema = resolved_schema(engine, schema);
    let v = qualify(engine, database, schema.as_deref(), "new_view").unwrap_or_else(|_| "new_view".to_string());
    match engine {
        // SQLite has no CREATE OR REPLACE VIEW; SQL Server spells it CREATE OR ALTER.
        Engine::Sqlite => format!("CREATE VIEW {v} AS\nSELECT 1 AS id;"),
        Engine::SqlServer => format!("CREATE OR ALTER VIEW {v} AS\nSELECT 1 AS id;"),
        Engine::Oracle => format!("CREATE OR REPLACE VIEW {v} AS\nSELECT 1 AS id FROM dual"),
        _ => format!("CREATE OR REPLACE VIEW {v} AS\nSELECT 1 AS id;"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: ProgKind, name: &str) -> ProgItem {
        ProgItem { kind, name: name.into(), table: None, definition: String::new(), debug_id: None }
    }

    #[test]
    fn drop_sql_is_qualified_and_quoted_per_engine() {
        // PostgreSQL routine names carry their identity arguments.
        let f = item(ProgKind::Function, "add_tax(numeric, integer)");
        assert_eq!(
            drop_sql(Engine::Postgres, "db", Some("sales"), &f).unwrap(),
            "DROP FUNCTION \"sales\".\"add_tax\"(numeric, integer)"
        );
        let p = item(ProgKind::Procedure, "rebuild");
        assert_eq!(drop_sql(Engine::MySql, "shop", None, &p).unwrap(), "DROP PROCEDURE `shop`.`rebuild`");
        let t = item(ProgKind::Trigger, "trg");
        assert_eq!(drop_sql(Engine::Sqlite, "main", None, &t).unwrap(), "DROP TRIGGER \"trg\"");
        // A PostgreSQL trigger can only be dropped together with its table.
        assert!(drop_sql(Engine::Postgres, "db", Some("public"), &t).is_err());
        let mut t = item(ProgKind::Trigger, "trg");
        t.table = Some("orders".into());
        assert_eq!(
            drop_sql(Engine::Postgres, "db", Some("public"), &t).unwrap(),
            "DROP TRIGGER \"trg\" ON \"public\".\"orders\""
        );
        let pk = item(ProgKind::Package, "UTILS");
        assert_eq!(drop_sql(Engine::Oracle, "svc", Some("SCOTT"), &pk).unwrap(), "DROP PACKAGE \"SCOTT\".\"UTILS\"");
        let s = item(ProgKind::Procedure, "new_procedure");
        assert_eq!(drop_sql(Engine::SqlServer, "db", None, &s).unwrap(), "DROP PROCEDURE [dbo].[new_procedure]");
    }

    #[test]
    fn templates_are_single_valid_creates() {
        for (e, kind) in [
            (Engine::Postgres, ProgKind::Function),
            (Engine::Postgres, ProgKind::Procedure),
            (Engine::Postgres, ProgKind::Trigger),
            (Engine::MySql, ProgKind::Function),
            (Engine::MySql, ProgKind::Procedure),
            (Engine::MySql, ProgKind::Trigger),
            (Engine::Sqlite, ProgKind::Trigger),
            (Engine::SqlServer, ProgKind::Function),
            (Engine::SqlServer, ProgKind::Procedure),
            (Engine::SqlServer, ProgKind::Trigger),
            (Engine::Oracle, ProgKind::Function),
            (Engine::Oracle, ProgKind::Procedure),
            (Engine::Oracle, ProgKind::Trigger),
            (Engine::Snowflake, ProgKind::Function),
            (Engine::Snowflake, ProgKind::Procedure),
        ] {
            let t = template(e, kind, "db", Some("public"));
            assert!(t.to_uppercase().contains("CREATE"), "{e:?} {kind:?}: {t}");
            // A PostgreSQL trigger template is two statements (function, then trigger).
            let want = if (e, kind) == (Engine::Postgres, ProgKind::Trigger) { 2 } else { 1 };
            assert_eq!(crate::sql::split::split(&t, e).len(), want, "{e:?} {kind:?}: {t}");
        }
    }

    #[test]
    fn view_templates_are_single_creates() {
        for e in [Engine::Sqlite, Engine::Postgres, Engine::MySql, Engine::SqlServer, Engine::Oracle, Engine::Snowflake]
        {
            let t = view_template(e, "db", Some("public"));
            assert!(t.to_uppercase().contains("VIEW"), "{e:?}: {t}");
            assert_eq!(crate::sql::split::split(&t, e).len(), 1, "{e:?}: {t}");
        }
    }
}
