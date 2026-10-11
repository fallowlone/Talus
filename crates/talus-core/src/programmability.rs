//! Read-only listing of stored routines and triggers with their definitions.

use crate::engine::{ScriptOptions, Session};
use crate::error::{DbError, DbResult};
use crate::model::{Engine, Outcome};
use crate::sql::Dialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgKind {
    Function,
    Procedure,
    Trigger,
}

impl ProgKind {
    pub fn label(self) -> &'static str {
        match self {
            ProgKind::Function => "Function",
            ProgKind::Procedure => "Procedure",
            ProgKind::Trigger => "Trigger",
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
    /// PostgreSQL function / procedure OID (what the debugger attaches to).
    pub pg_oid: Option<u32>,
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
                    pg_oid: None,
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
                items.push(ProgItem { kind, name: cell(&r, 1), table: None, definition: cell(&r, 2), pg_oid: None });
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
                    pg_oid: None,
                });
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
                let pg_oid = cell(&r, 3).parse().ok();
                items.push(ProgItem { kind, name: cell(&r, 0), table: None, definition: cell(&r, 2), pg_oid });
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
                    pg_oid: None,
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
                    pg_oid: None,
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
                    pg_oid: None,
                });
            }
        }
    }
    Ok(items)
}
