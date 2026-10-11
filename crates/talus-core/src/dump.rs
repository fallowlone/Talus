//! SQL dump of one database/schema (structure, optionally data) and execution of SQL files.
//! Tables are written in foreign-key dependency order so the dump restores without
//! disabling constraints (engines that support it also get the switch for cyclic references).

use std::io::Write;
use std::path::Path;

use crate::engine::Session;
use crate::error::{DbError, DbResult};
use crate::export::{ExportFormat, export_table_to};
use crate::model::{Engine, ObjectKind, ObjectRef, TableStructure};
use crate::sql::{Dialect, split};

/// SQL files larger than this are refused (the script is parsed in memory).
pub const MAX_SQL_FILE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DumpReport {
    pub tables: usize,
    pub views: usize,
    pub rows: u64,
}

fn last_segment(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name).trim_matches(['"', '`'])
}

/// Parents before children; tables in a reference cycle keep their original relative order.
fn dependency_order(tables: &[(ObjectRef, TableStructure)]) -> Vec<usize> {
    let deps: Vec<Vec<usize>> = tables
        .iter()
        .enumerate()
        .map(|(i, (_, st))| {
            st.foreign_keys
                .iter()
                .filter_map(|f| tables.iter().position(|(o, _)| o.name == last_segment(&f.ref_table)))
                .filter(|&d| d != i)
                .collect()
        })
        .collect();
    topo_order(&deps)
}

/// Order that puts every item after the items it depends on (`deps[i]` = what `i` needs); an item in
/// a cycle is emitted as soon as nothing else can progress, in its original position.
pub(crate) fn topo_order(deps: &[Vec<usize>]) -> Vec<usize> {
    let n = deps.len();
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while order.len() < n {
        let ready = (0..n).find(|&i| !done[i] && deps[i].iter().all(|&d| done[d]));
        // A cycle leaves nothing ready: emit the first unfinished item to make progress.
        let Some(i) = ready.or_else(|| (0..n).find(|&i| !done[i])) else { break };
        done[i] = true;
        order.push(i);
    }
    order
}

fn terminated(ddl: &str) -> String {
    let t = ddl.trim_end().trim_end_matches(';');
    format!("{t};\n\n")
}

/// Writes `database`/`schema` to `path` as a restorable SQL script.
pub async fn dump_schema(
    s: &mut Session,
    database: &str,
    schema: Option<&str>,
    path: &Path,
    include_data: bool,
) -> DbResult<DumpReport> {
    let engine = s.engine();
    let d = Dialect(engine);
    let objects = s.list_objects(database, schema).await?;
    let mut tables = Vec::new();
    let mut views = Vec::new();
    for o in objects {
        match o.kind {
            ObjectKind::Table => {
                let st = s.table_structure(&o).await?;
                tables.push((o, st));
            }
            ObjectKind::View => views.push(o),
        }
    }
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(
        out,
        "-- Talus SQL dump\n-- Database: {database}{}",
        schema.map(|x| format!(", schema: {x}")).unwrap_or_default()
    )?;
    writeln!(out, "-- Engine: {}\n", engine.label())?;
    match engine {
        Engine::Redis | Engine::MongoDb => {}
        Engine::MySql => writeln!(out, "SET NAMES utf8mb4;\nSET FOREIGN_KEY_CHECKS=0;\n")?,
        Engine::Sqlite => writeln!(out, "PRAGMA foreign_keys=OFF;\n")?,
        Engine::Postgres => {
            if let Some(sc) = schema.filter(|x| *x != "public") {
                writeln!(out, "CREATE SCHEMA IF NOT EXISTS {};\n", d.quote_ident(sc)?)?;
            }
        }
        Engine::SqlServer => {
            if let Some(sc) = schema.filter(|x| *x != "dbo") {
                writeln!(
                    out,
                    "IF SCHEMA_ID({}) IS NULL EXEC({});\n",
                    d.string_literal(sc),
                    d.string_literal(&format!("CREATE SCHEMA {}", d.quote_ident(sc)?))
                )?;
            }
        }
    }
    let order = dependency_order(&tables);
    for &i in &order {
        let (obj, _) = &tables[i];
        writeln!(out, "-- Table: {}", obj.name)?;
        out.write_all(terminated(&s.ddl(obj).await?).as_bytes())?;
    }
    let mut rows = 0;
    if include_data {
        for &i in &order {
            let (obj, _) = &tables[i];
            writeln!(out, "-- Data: {}", obj.name)?;
            let target = match engine {
                Engine::Postgres | Engine::SqlServer => d.qualified(obj)?,
                _ => d.quote_ident(&obj.name)?,
            };
            // Identity columns only accept explicit values while IDENTITY_INSERT is on.
            let identity = engine == Engine::SqlServer
                && tables[i].1.columns.iter().any(|c| c.extra.as_deref().is_some_and(|e| e.contains("IDENTITY")));
            if identity {
                writeln!(out, "SET IDENTITY_INSERT {target} ON;")?;
            }
            rows += export_table_to(s, obj, ExportFormat::SqlInsert, &mut out, &target).await?;
            if identity {
                writeln!(out, "SET IDENTITY_INSERT {target} OFF;")?;
            }
            writeln!(out)?;
        }
    }
    for v in &views {
        writeln!(out, "-- View: {}", v.name)?;
        out.write_all(terminated(&s.ddl(v).await?).as_bytes())?;
    }
    match engine {
        Engine::Redis | Engine::MongoDb => {}
        Engine::MySql => writeln!(out, "SET FOREIGN_KEY_CHECKS=1;")?,
        Engine::Sqlite => writeln!(out, "PRAGMA foreign_keys=ON;")?,
        Engine::Postgres | Engine::SqlServer => {}
    }
    out.flush()?;
    Ok(DumpReport { tables: tables.len(), views: views.len(), rows })
}

/// Runs every statement of a SQL file. With `transactional` (engines with transactional DDL)
/// the file is all-or-nothing; otherwise execution stops at the first error and the message
/// says how many statements were applied before it.
pub async fn execute_sql_file(s: &mut Session, path: &Path, transactional: bool) -> DbResult<usize> {
    let size = std::fs::metadata(path)?.len();
    if size > MAX_SQL_FILE_BYTES {
        return Err(DbError::Invalid(format!("{} is larger than {} MB", path.display(), MAX_SQL_FILE_BYTES >> 20)));
    }
    let script = std::fs::read_to_string(path)
        .map_err(|e| DbError::Invalid(format!("cannot read {} as UTF-8 text: {e}", path.display())))?;
    let stmts: Vec<String> = split::split(&script, s.engine()).iter().map(|st| st.text.to_string()).collect();
    if stmts.is_empty() {
        return Err(DbError::Invalid("the file contains no statements".into()));
    }
    s.execute_batch(&stmts, transactional).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ForeignKeyInfo;

    fn table(name: &str, refs: &[&str]) -> (ObjectRef, TableStructure) {
        (
            ObjectRef { database: "d".into(), schema: None, name: name.into(), kind: ObjectKind::Table },
            TableStructure {
                foreign_keys: refs
                    .iter()
                    .map(|r| ForeignKeyInfo {
                        name: format!("fk_{r}"),
                        columns: vec!["x".into()],
                        ref_table: format!("public.\"{r}\""),
                        ref_columns: vec!["id".into()],
                        on_update: "NO ACTION".into(),
                        on_delete: "NO ACTION".into(),
                    })
                    .collect(),
                ..Default::default()
            },
        )
    }

    #[test]
    fn parents_come_first_and_cycles_terminate() {
        let t = vec![table("c", &["b"]), table("b", &["a"]), table("a", &[])];
        assert_eq!(dependency_order(&t), vec![2, 1, 0]);
        let cyc = vec![table("x", &["y"]), table("y", &["x"]), table("z", &[])];
        let o = dependency_order(&cyc);
        assert_eq!(o.len(), 3);
        assert_eq!(o[0], 2, "independent table first, then the cycle in original order");
        let selfref = vec![table("s", &["s"])];
        assert_eq!(dependency_order(&selfref), vec![0]);
    }

    #[test]
    fn terminated_adds_exactly_one_semicolon() {
        assert_eq!(terminated("CREATE TABLE t (a int)"), "CREATE TABLE t (a int);\n\n");
        assert_eq!(terminated("CREATE TABLE t (a int);\n"), "CREATE TABLE t (a int);\n\n");
    }
}
