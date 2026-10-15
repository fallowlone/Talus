//! Structure synchronization: diff two schemas of the same engine and produce the DDL that
//! makes the target's tables, columns, indexes and foreign keys match the source. Drops are
//! opt-in. Views are not synchronized.

use super::Dialect;
use super::lexer::{TokenKind, tokenize};
use std::collections::HashMap;

use super::constraints::{self, FkDef, IndexDef};
use super::ddl::{AlterPlan, ColumnDef, DesignColumn, alter_table, create_table_with};
use crate::error::{DbError, DbResult};
use crate::model::{Engine, ForeignKeyInfo, ObjectKind, ObjectRef, TableStructure};

pub type Model = [(ObjectRef, TableStructure)];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    pub statements: Vec<String>,
    pub destructive: Vec<String>,
    /// Human-readable lines: what differs, what is skipped and why.
    pub summary: Vec<String>,
}

/// Referenced tables are compared by name and re-qualified into the target namespace, so a
/// foreign key that points inside the source schema points inside the target schema after sync.
fn normalize_fks(engine: Engine, db: &str, schema: Option<&str>, list: &[ForeignKeyInfo]) -> Vec<ForeignKeyInfo> {
    let prefix = match engine {
        Engine::Postgres | Engine::SqlServer | Engine::Oracle | Engine::Snowflake => schema,
        Engine::MySql => Some(db).filter(|d| !d.is_empty()),
        Engine::Sqlite | Engine::Redis | Engine::MongoDb => None,
    };
    list.iter()
        .map(|f| {
            let last = f.ref_table.rsplit('.').next().unwrap_or(&f.ref_table).trim_matches(['"', '`']);
            let ref_table = prefix.map_or_else(|| last.to_string(), |p| format!("{p}.{last}"));
            ForeignKeyInfo { ref_table, ..f.clone() }
        })
        .collect()
}

/// `target_db`/`target_schema` place new tables in the target.
pub fn structure_sync(
    engine: Engine,
    source: &Model,
    target: &Model,
    target_db: &str,
    target_schema: Option<&str>,
    include_drops: bool,
) -> DbResult<SyncPlan> {
    let d = Dialect(engine);
    let mut plan = SyncPlan::default();
    let mut late_fks: Vec<String> = Vec::new();
    for (sobj, sst) in source {
        let cols: Vec<ColumnDef> = sst.columns.iter().map(ColumnDef::from).collect();
        match target.iter().find(|(t, _)| t.name == sobj.name) {
            None => {
                let obj = ObjectRef {
                    database: target_db.to_string(),
                    schema: target_schema.map(String::from),
                    name: sobj.name.clone(),
                    kind: ObjectKind::Table,
                };
                let fks: Vec<FkDef> =
                    FkDef::from_info(&normalize_fks(engine, target_db, target_schema, &sst.foreign_keys))
                        .into_iter()
                        .map(|f| FkDef { original: None, ..f })
                        .collect();
                // SQLite takes foreign keys only inside CREATE TABLE; the others add them once
                // every table exists, so creation order does not matter.
                let inline: &[FkDef] = if engine == Engine::Sqlite { &fks } else { &[] };
                // Source metadata is untrusted input here: a table whose definition does not pass
                // the DDL checks is reported and left out, like an alteration that cannot be made.
                match create_table_with(d, &obj, &cols, inline) {
                    Ok(sql) => plan.statements.push(sql),
                    Err(DbError::Invalid(why)) => {
                        plan.summary.push(format!("! skip table {}: {why}", sobj.name));
                        continue;
                    }
                    Err(e) => return Err(e),
                }
                plan.summary.push(format!("+ create table {}", sobj.name));
                for ix in IndexDef::from_info(&sst.indexes) {
                    plan.statements.push(constraints::create_index(d, &obj, &IndexDef { original: None, ..ix })?);
                }
                if engine != Engine::Sqlite {
                    let t = d.qualified(&obj)?;
                    for fk in &fks {
                        late_fks.push(format!("ALTER TABLE {t} ADD {}", constraints::fk_clause(d, fk)?));
                    }
                }
            }
            Some((tobj, tst)) => {
                let original: Vec<ColumnDef> = tst.columns.iter().map(ColumnDef::from).collect();
                let mut edited: Vec<DesignColumn> = cols
                    .iter()
                    .map(|c| DesignColumn {
                        original: original.iter().find(|o| o.name == c.name).map(|o| o.name.clone()),
                        def: c.clone(),
                    })
                    .collect();
                if !include_drops {
                    // Keep target-only columns untouched.
                    for o in original.iter().filter(|o| !cols.iter().any(|c| c.name == o.name)) {
                        edited.push(DesignColumn { original: Some(o.name.clone()), def: o.clone() });
                        plan.summary.push(format!("  keep extra column {}.{} (drops disabled)", tobj.name, o.name));
                    }
                }
                let names: Vec<String> = edited.iter().map(|e| e.def.name.clone()).collect();
                let result = alter_table(d, tobj, &original, &edited).and_then(|mut alter| {
                    let cp = constraint_diff(engine, tobj, target_db, target_schema, sst, tst, &names, include_drops)?;
                    alter.statements.splice(0..0, cp.drops);
                    alter.statements.extend(cp.creates);
                    Ok(alter)
                });
                match result {
                    Ok(AlterPlan { statements, destructive }) => {
                        if !statements.is_empty() {
                            plan.summary.push(format!(
                                "~ alter table {} ({} change{})",
                                tobj.name,
                                statements.len(),
                                if statements.len() == 1 { "" } else { "s" }
                            ));
                        }
                        plan.statements.extend(statements);
                        plan.destructive.extend(destructive.into_iter().map(|x| format!("{}: {x}", tobj.name)));
                    }
                    Err(DbError::Invalid(why)) => plan.summary.push(format!("! skip table {}: {why}", tobj.name)),
                    Err(e) => return Err(e),
                }
            }
        }
    }
    plan.statements.extend(late_fks);
    for (tobj, _) in target.iter().filter(|(t, _)| !source.iter().any(|(s, _)| s.name == t.name)) {
        if include_drops {
            plan.statements.push(format!("DROP TABLE {}", d.qualified(tobj)?));
            plan.destructive.push(format!("drop table {} and all its data", tobj.name));
            plan.summary.push(format!("- drop table {}", tobj.name));
        } else {
            plan.summary.push(format!("  keep extra table {} (drops disabled)", tobj.name));
        }
    }
    if plan.statements.is_empty() {
        plan.summary.push("Structures are identical (tables, columns, indexes, foreign keys).".into());
    }
    Ok(plan)
}

pub type ViewList<'a> = &'a [(ObjectRef, String)];

/// Whitespace-, case- and quoting-insensitive form of a view definition, for comparison only.
fn normalize_view(ddl: &str) -> String {
    ddl.trim()
        .trim_end_matches(';')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .replace(['"', '`', '[', ']'], "")
}

/// `CREATE VIEW …` as text that creates or replaces the view, without server-specific DEFINER noise.
fn replace_view_sql(engine: Engine, ddl: &str) -> String {
    let mut t = ddl.trim().trim_end_matches(';').to_string();
    if engine == Engine::MySql {
        // `CREATE ALGORITHM=… DEFINER=`u`@`h` SQL SECURITY DEFINER VIEW …`: the definer may not exist on the target.
        if let Some(i) = t.to_ascii_uppercase().find("DEFINER=") {
            let rest = &t[i..];
            let end = rest
                .to_ascii_uppercase()
                .find(" SQL SECURITY")
                .or_else(|| rest.to_ascii_uppercase().find(" VIEW "))
                .unwrap_or(rest.len());
            t.replace_range(i..i + end, "");
            t = t.replace("  ", " ");
        }
    }
    let upper = t.to_ascii_uppercase();
    if upper.starts_with("CREATE OR REPLACE ") || upper.starts_with("CREATE OR ALTER ") {
        return t;
    }
    match engine {
        Engine::MySql | Engine::Postgres | Engine::Oracle => t.replacen("CREATE ", "CREATE OR REPLACE ", 1),
        Engine::SqlServer => t.replacen("CREATE ", "CREATE OR ALTER ", 1),
        _ => t,
    }
}

/// The identifier inside a quoted token (`"a""b"`, `` `a` ``, `[a]`), unescaped.
fn unquoted(text: &str) -> String {
    let b = text.as_bytes();
    if text.len() >= 2 {
        match (b[0], b[text.len() - 1]) {
            (b'"', b'"') => return text[1..text.len() - 1].replace("\"\"", "\""),
            (b'`', b'`') => return text[1..text.len() - 1].replace("``", "`"),
            (b'[', b']') => return text[1..text.len() - 1].to_string(),
            _ => {}
        }
    }
    text.to_string()
}

/// Rewrites every `from.` qualifier in a definition to the target namespace, token
/// by token, so string literals, comments and identifiers that merely contain the
/// name are left alone.
fn rewrite_namespace(ddl: &str, engine: Engine, from: &str, to: &str) -> String {
    let quoted = Dialect(engine).quote_ident(to).unwrap_or_else(|_| to.to_string());
    let toks = tokenize(ddl, engine);
    let mut out = String::with_capacity(ddl.len());
    let mut last = 0;
    for (i, t) in toks.iter().enumerate() {
        if !matches!(t.kind, TokenKind::Word | TokenKind::QuotedIdent) {
            continue;
        }
        if !unquoted(t.text(ddl)).eq_ignore_ascii_case(from) {
            continue;
        }
        let qualifier = toks[i + 1..]
            .iter()
            .find(|x| !matches!(x.kind, TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment))
            .is_some_and(|x| x.kind == TokenKind::Punct && x.text(ddl) == ".");
        if !qualifier {
            continue;
        }
        out.push_str(&ddl[last..t.start]);
        out.push_str(&quoted);
        last = t.end;
    }
    out.push_str(&ddl[last..]);
    out
}

/// Makes the target's views match the source's. Definitions embed their own
/// namespace (schema, or database where that is the qualifier), so when the two
/// sides name it differently every `source.` qualifier is rewritten to the
/// target's before comparing or copying. `namespaces` is `(source, target)`;
/// `None` runs the definitions as they are.
pub fn view_sync(
    engine: Engine,
    source: ViewList<'_>,
    target: ViewList<'_>,
    include_drops: bool,
    namespaces: Option<(&str, &str)>,
) -> DbResult<SyncPlan> {
    let d = Dialect(engine);
    let mut plan = SyncPlan::default();
    for (sobj, sddl) in source {
        // The definition is the source server's text and runs on the target: exactly one statement.
        if super::split::split(sddl, engine).len() != 1 {
            plan.summary.push(format!("! skip view {}: its definition is not a single statement", sobj.name));
            continue;
        }
        let ddl = match namespaces {
            Some((from, to)) if !from.eq_ignore_ascii_case(to) => rewrite_namespace(sddl, engine, from, to),
            _ => sddl.clone(),
        };
        match target.iter().find(|(t, _)| t.name == sobj.name) {
            None => {
                plan.statements.push(replace_view_sql(engine, &ddl));
                plan.summary.push(format!("+ create view {}", sobj.name));
            }
            Some((tobj, tddl)) if normalize_view(tddl) != normalize_view(&ddl) => {
                if engine == Engine::Sqlite {
                    // SQLite has no CREATE OR REPLACE VIEW.
                    plan.statements.push(format!("DROP VIEW {}", d.qualified(tobj)?));
                }
                plan.statements.push(replace_view_sql(engine, &ddl));
                plan.summary.push(format!("~ replace view {}", sobj.name));
            }
            Some(_) => {}
        }
    }
    for (tobj, _) in target.iter().filter(|(t, _)| !source.iter().any(|(s, _)| s.name == t.name)) {
        if include_drops {
            plan.statements.push(format!("DROP VIEW {}", d.qualified(tobj)?));
            plan.destructive.push(format!("drop view {}", tobj.name));
            plan.summary.push(format!("- drop view {}", tobj.name));
        } else {
            plan.summary.push(format!("  keep extra view {} (drops disabled)", tobj.name));
        }
    }
    Ok(plan)
}

/// Index / foreign-key changes of a table that exists on both sides, matched by name.
#[allow(clippy::too_many_arguments)]
fn constraint_diff(
    engine: Engine,
    tobj: &ObjectRef,
    target_db: &str,
    target_schema: Option<&str>,
    sst: &TableStructure,
    tst: &TableStructure,
    columns: &[String],
    include_drops: bool,
) -> DbResult<constraints::ConstraintPlan> {
    let t_fks = normalize_fks(engine, target_db, target_schema, &tst.foreign_keys);
    let s_fks = normalize_fks(engine, target_db, target_schema, &sst.foreign_keys);
    let mut ix: Vec<IndexDef> = IndexDef::from_info(&sst.indexes)
        .into_iter()
        .map(|i| IndexDef { original: tst.indexes.iter().find(|o| o.name == i.name).map(|o| o.name.clone()), ..i })
        .collect();
    let mut fk: Vec<FkDef> = FkDef::from_info(&s_fks)
        .into_iter()
        .map(|f| FkDef { original: t_fks.iter().find(|o| o.name == f.name).map(|o| o.name.clone()), ..f })
        .collect();
    if !include_drops {
        let extra_ix: Vec<IndexDef> =
            IndexDef::from_info(&tst.indexes).into_iter().filter(|i| !ix.iter().any(|x| x.name == i.name)).collect();
        let extra_fk: Vec<FkDef> =
            FkDef::from_info(&t_fks).into_iter().filter(|f| !fk.iter().any(|x| x.name == f.name)).collect();
        ix.extend(extra_ix);
        fk.extend(extra_fk);
    }
    constraints::diff(Dialect(engine), tobj, columns, &HashMap::new(), (&tst.indexes, &t_fks), (&ix, &fk))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnInfo;

    fn t(name: &str, cols: &[(&str, &str, bool)]) -> (ObjectRef, TableStructure) {
        let columns = cols
            .iter()
            .map(|(n, ty, pk)| ColumnInfo {
                name: (*n).into(),
                data_type: (*ty).into(),
                nullable: !pk,
                default: None,
                primary_key: *pk,
                comment: None,
                extra: None,
            })
            .collect();
        (
            ObjectRef { database: "main".into(), schema: None, name: name.into(), kind: ObjectKind::Table },
            TableStructure { columns, ..Default::default() },
        )
    }

    #[test]
    fn a_view_definition_must_be_one_statement() {
        let v = |n: &str| ObjectRef {
            database: "db".into(),
            schema: Some("public".into()),
            name: n.into(),
            kind: ObjectKind::View,
        };
        let source = vec![
            (v("ok"), "CREATE VIEW public.ok AS SELECT 1".to_string()),
            (v("evil"), "CREATE VIEW public.evil AS SELECT 1; DROP TABLE public.users".to_string()),
        ];
        let plan = view_sync(Engine::Postgres, &source, &[], false, None).unwrap();
        assert_eq!(plan.statements.len(), 1, "{:?}", plan.statements);
        assert!(!plan.statements[0].contains("DROP"));
        assert!(plan.summary.iter().any(|l| l.starts_with("! skip view evil")));
    }

    #[test]
    fn hostile_source_metadata_is_skipped_not_executed() {
        let mut evil = t("evil", &[("id", "INTEGER", true), ("c", "integer", false)]);
        evil.1.columns[1].default = Some("0, OWNER TO attacker".into());
        let mut changed = t("a", &[("id", "INTEGER", true), ("c", "bigint USING (pg_sleep(1))::bigint", false)]);
        changed.1.columns[0].data_type = "INTEGER".into();
        let target = vec![t("a", &[("id", "INTEGER", true), ("c", "integer", false)])];
        let plan = structure_sync(Engine::Postgres, &[evil, changed], &target, "db", Some("public"), false).unwrap();
        assert!(
            plan.statements.iter().all(|s| !s.contains("OWNER TO") && !s.contains("USING")),
            "{:?}",
            plan.statements
        );
        assert!(plan.summary.iter().any(|l| l.starts_with("! skip table evil")), "{:?}", plan.summary);
        assert!(plan.summary.iter().any(|l| l.starts_with("! skip table a")), "{:?}", plan.summary);
    }

    #[test]
    fn creates_alters_and_respects_drop_flag() {
        let source = vec![
            t("a", &[("id", "INTEGER", true), ("x", "TEXT", false), ("y", "TEXT", false)]),
            t("new_one", &[("id", "INTEGER", true)]),
        ];
        let target = vec![
            t("a", &[("id", "INTEGER", true), ("x", "TEXT", false), ("old", "TEXT", false)]),
            t("legacy", &[("id", "INTEGER", true)]),
        ];
        let keep = structure_sync(Engine::Sqlite, &source, &target, "main", None, false).unwrap();
        assert_eq!(
            keep.statements,
            vec![
                "ALTER TABLE \"main\".\"a\" ADD COLUMN \"y\" TEXT",
                "CREATE TABLE \"main\".\"new_one\" (\n  \"id\" INTEGER PRIMARY KEY\n)",
            ]
        );
        assert!(keep.destructive.is_empty());
        assert!(keep.summary.iter().any(|s| s.contains("keep extra table legacy")));
        let drop = structure_sync(Engine::Sqlite, &source, &target, "main", None, true).unwrap();
        assert!(drop.statements.contains(&"ALTER TABLE \"main\".\"a\" DROP COLUMN \"old\"".to_string()));
        assert!(drop.statements.contains(&"DROP TABLE \"main\".\"legacy\"".to_string()));
        assert_eq!(drop.destructive.len(), 2);
        let same = structure_sync(Engine::Sqlite, &target, &target, "main", None, true).unwrap();
        assert!(same.statements.is_empty());
    }

    fn view(name: &str, ddl: &str) -> (ObjectRef, String) {
        (ObjectRef { database: "main".into(), schema: None, name: name.into(), kind: ObjectKind::View }, ddl.into())
    }

    #[test]
    fn views_are_created_replaced_and_dropped_on_request() {
        let src = vec![view("a", "CREATE VIEW a AS SELECT 1"), view("b", "CREATE VIEW b AS select  2 ;")];
        let tgt = vec![view("b", "create view b as select 2"), view("old", "CREATE VIEW old AS SELECT 0")];
        let p = view_sync(Engine::Postgres, &src, &tgt, false, None).unwrap();
        assert_eq!(p.statements, vec!["CREATE OR REPLACE VIEW a AS SELECT 1"], "b only differs in spelling");
        assert!(p.summary.iter().any(|s| s.contains("keep extra view old")));
        let changed = vec![view("b", "CREATE VIEW b AS SELECT 3")];
        let p = view_sync(Engine::Sqlite, &changed, &tgt, true, None).unwrap();
        assert_eq!(p.statements[0], "DROP VIEW \"main\".\"b\"");
        assert_eq!(p.statements[1], "CREATE VIEW b AS SELECT 3");
        assert!(p.statements.contains(&"DROP VIEW \"main\".\"old\"".to_string()) && p.destructive.len() == 1);
        let p = view_sync(Engine::SqlServer, &src[..1], &[], false, None).unwrap();
        assert_eq!(p.statements, vec!["CREATE OR ALTER VIEW a AS SELECT 1"]);
    }

    #[test]
    fn views_follow_the_target_namespace() {
        // The qualifier is rewritten in the header and the body; strings and
        // identifiers that merely contain the name are left alone.
        let src = vec![view(
            "v",
            "CREATE VIEW s1.v AS SELECT * FROM s1.t JOIN plain ON plain.id = s1.t.id WHERE note = 's1.t' AND s1x = 1",
        )];
        let p = view_sync(Engine::Postgres, &src, &[], false, Some(("s1", "s2"))).unwrap();
        assert_eq!(
            p.statements,
            vec![
                "CREATE OR REPLACE VIEW \"s2\".v AS SELECT * FROM \"s2\".t JOIN plain ON plain.id = \"s2\".t.id WHERE note = 's1.t' AND s1x = 1"
            ],
            "{:?}",
            p.summary
        );
        // A target that already holds the rewritten definition is in sync.
        let tgt = vec![view(
            "v",
            "create view s2.v as select * from s2.t join plain on plain.id = s2.t.id where note = 's1.t' and s1x = 1",
        )];
        let p = view_sync(Engine::Postgres, &src, &tgt, false, Some(("s1", "s2"))).unwrap();
        assert!(p.statements.is_empty(), "{:?}", p.statements);
        // Quoted identifiers rewrite too, including the view's own name.
        let src = vec![view("v", "CREATE VIEW \"S1\".\"v\" AS SELECT \"S1\".\"t\".\"x\" FROM \"S1\".\"t\"")];
        let p = view_sync(Engine::Postgres, &src, &[], false, Some(("s1", "s2"))).unwrap();
        assert!(
            p.statements[0].contains("\"s2\".\"v\"") && p.statements[0].contains("\"s2\".\"t\""),
            "{:?}",
            p.statements
        );
    }

    #[test]
    fn mysql_definer_clauses_are_removed() {
        let ddl = "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v` AS select 1 AS `x`";
        assert_eq!(
            replace_view_sql(Engine::MySql, ddl),
            "CREATE OR REPLACE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v` AS select 1 AS `x`"
        );
    }

    #[test]
    fn unsupported_changes_are_reported_not_fatal() {
        let source = vec![t("a", &[("id", "INTEGER", true), ("x", "INTEGER", false)])];
        let target = vec![t("a", &[("id", "INTEGER", true), ("x", "TEXT", false)])];
        let plan = structure_sync(Engine::Sqlite, &source, &target, "main", None, false).unwrap();
        assert!(plan.statements.is_empty());
        assert!(plan.summary.iter().any(|s| s.starts_with("! skip table a")), "{:?}", plan.summary);
    }

    fn pg(name: &str, cols: &[(&str, &str, bool)]) -> (ObjectRef, TableStructure) {
        let (mut o, st) = t(name, cols);
        o.schema = Some("public".into());
        (o, st)
    }

    fn index(name: &str, cols: &[&str], unique: bool) -> crate::model::IndexInfo {
        crate::model::IndexInfo {
            name: name.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            unique,
            primary: false,
        }
    }

    #[test]
    fn indexes_and_foreign_keys_are_synced_after_all_tables_exist() {
        let (po, ps) = pg("parent", &[("id", "int", true)]);
        let (mut co, mut cs) = pg("child", &[("id", "int", true), ("pid", "int", false)]);
        co.schema = Some("src".into());
        cs.indexes.push(index("ix_pid", &["pid"], false));
        cs.foreign_keys.push(ForeignKeyInfo {
            name: "fk_pid".into(),
            columns: vec!["pid".into()],
            ref_table: "src.parent".into(),
            ref_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "CASCADE".into(),
        });
        let source = vec![(co, cs), (po, ps)];
        let plan = structure_sync(Engine::Postgres, &source, &[], "db", Some("dst"), false).unwrap();
        let pos = |needle: &str| plan.statements.iter().position(|s| s.contains(needle)).unwrap();
        assert!(pos("CREATE INDEX \"ix_pid\"") > pos("CREATE TABLE \"dst\".\"child\""));
        assert!(pos("ADD CONSTRAINT \"fk_pid\"") > pos("CREATE TABLE \"dst\".\"parent\""), "{:?}", plan.statements);
        assert!(plan.statements.last().unwrap().contains("REFERENCES \"dst\".\"parent\""), "{:?}", plan.statements);
    }

    #[test]
    fn existing_table_index_diff_and_opt_in_drops() {
        let (o, mut src) = pg("a", &[("id", "int", true), ("x", "int", false)]);
        let (_, mut tgt) = pg("a", &[("id", "int", true), ("x", "int", false)]);
        src.indexes.push(index("ix_new", &["x"], false));
        tgt.indexes.push(index("ix_old", &["x"], false));
        let source = vec![(o.clone(), src)];
        let target = vec![(o, tgt)];
        let keep = structure_sync(Engine::Postgres, &source, &target, "db", Some("public"), false).unwrap();
        assert_eq!(keep.statements.len(), 1, "{:?}", keep.statements);
        assert!(keep.statements[0].starts_with("CREATE INDEX \"ix_new\""));
        let drop = structure_sync(Engine::Postgres, &source, &target, "db", Some("public"), true).unwrap();
        assert_eq!(drop.statements[0], "DROP INDEX \"public\".\"ix_old\"");
        assert!(drop.statements[1].starts_with("CREATE INDEX \"ix_new\""));
        let same = structure_sync(Engine::Postgres, &target, &target, "db", Some("public"), true).unwrap();
        assert!(same.statements.is_empty());
    }
}
