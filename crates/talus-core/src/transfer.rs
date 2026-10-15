//! Cross-engine table transfer (ADV-04): plan the `CREATE TABLE` statements that
//! recreate a source schema's tables on a *different* engine, with column types
//! translated through the portable logical types of `model_design`. Rows are not
//! moved here; once the tables exist, `crate::data_sync` copies them (it works on
//! any two sessions), and the sync tab drives exactly that sequence.
//!
//! Deliberate limits, all reported in the plan instead of hidden:
//! - column defaults and identity / auto-increment markers are dropped (their
//!   spellings do not translate; copied rows carry explicit values anyway);
//! - foreign keys are not recreated (referenced types and actions do not
//!   translate reliably across engines);
//! - indexes the target cannot express (e.g. Snowflake secondary indexes) are
//!   skipped with the builder's reason;
//! - views are not transferred.

use crate::model::{Engine, ObjectKind, ObjectRef, TableStructure};
use crate::model_design::LType;
use crate::sql::Dialect;
use crate::sql::constraints::{IndexDef, create_index};
use crate::sql::ddl::{ColumnDef, create_table};
use crate::sql::sync::SyncPlan;

/// The type `data_type` (as reported by the source engine) becomes on `target`.
pub fn mapped_type(data_type: &str, target: Engine) -> String {
    LType::from_sql(data_type).sql(target)
}

/// Creation plan for every source table the target does not have yet.
pub fn plan(
    source: &[(ObjectRef, TableStructure)],
    target: &[(ObjectRef, TableStructure)],
    dst_engine: Engine,
    dst_db: &str,
    dst_schema: Option<&str>,
) -> SyncPlan {
    let d = Dialect(dst_engine);
    let mut plan = SyncPlan::default();
    plan.summary.push(format!(
        "Transfer to {}: types are mapped to portable equivalents; defaults, identity markers and foreign keys are not carried.",
        dst_engine.label()
    ));
    let mut created = 0;
    for (sobj, sst) in source {
        if sobj.kind == ObjectKind::View {
            plan.summary.push(format!("! {}: views are not transferred", sobj.name));
            continue;
        }
        let obj = ObjectRef {
            database: dst_db.to_string(),
            schema: dst_schema.map(String::from),
            name: sobj.name.clone(),
            kind: ObjectKind::Table,
        };
        if target.iter().any(|(t, _)| t.name == sobj.name) {
            plan.summary.push(format!("{}: already on the target; its rows can be copied below", sobj.name));
            continue;
        }
        let cols: Vec<ColumnDef> = sst
            .columns
            .iter()
            .map(|c| ColumnDef {
                name: c.name.clone(),
                data_type: mapped_type(&c.data_type, dst_engine),
                nullable: c.nullable,
                // Defaults and extras (identity, auto_increment, …) do not translate.
                default: None,
                primary_key: c.primary_key,
                extra: String::new(),
                comment: c.comment.clone(),
            })
            .collect();
        match create_table(d, &obj, &cols) {
            Ok(sql) => {
                plan.statements.push(sql);
                created += 1;
                let types: Vec<String> = sst
                    .columns
                    .iter()
                    .map(|c| format!("{} {}", c.name, mapped_type(&c.data_type, dst_engine)))
                    .collect();
                plan.summary.push(format!("+ {}: create table ({})", sobj.name, types.join(", ")));
            }
            Err(e) => {
                plan.summary.push(format!("! {}: skipped: {e}", sobj.name));
                continue;
            }
        }
        for ix in sst.indexes.iter().filter(|i| !i.primary) {
            let def =
                IndexDef { original: None, name: ix.name.clone(), columns: ix.columns.clone(), unique: ix.unique };
            match create_index(d, &obj, &def) {
                Ok(sql) => plan.statements.push(sql),
                Err(e) => plan.summary.push(format!("! {}: index {} skipped: {e}", sobj.name, ix.name)),
            }
        }
        if !sst.foreign_keys.is_empty() {
            plan.summary.push(format!("! {}: foreign keys are not recreated by transfer", sobj.name));
        }
    }
    if created > 0 {
        plan.summary.push("After executing, press Compare again and use Data synchronization to copy the rows.".into());
    }
    if plan.statements.is_empty() && created == 0 {
        plan.summary.push("Nothing to create: every source table already exists on the target.".into());
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnInfo, IndexInfo};

    fn col(name: &str, data_type: &str, pk: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: data_type.into(),
            nullable: !pk,
            default: None,
            primary_key: pk,
            comment: None,
            extra: None,
        }
    }

    fn table(name: &str, cols: Vec<ColumnInfo>) -> (ObjectRef, TableStructure) {
        (
            ObjectRef {
                database: "src".into(),
                schema: Some("public".into()),
                name: name.into(),
                kind: ObjectKind::Table,
            },
            TableStructure { columns: cols, indexes: vec![], foreign_keys: vec![] },
        )
    }

    #[test]
    fn types_map_to_portable_equivalents() {
        assert_eq!(mapped_type("varchar(50)", Engine::Sqlite), "VARCHAR(50)");
        assert_eq!(mapped_type("integer", Engine::Postgres), "integer");
        assert_eq!(mapped_type("boolean", Engine::MySql), "tinyint(1)");
        assert_eq!(mapped_type("numeric(10,2)", Engine::Sqlite), "NUMERIC");
        assert_eq!(mapped_type("bytea", Engine::MySql), "blob");
        assert_eq!(mapped_type("jsonb", Engine::Snowflake), "VARIANT");
        assert_eq!(mapped_type("timestamp", Engine::Snowflake), "TIMESTAMP_NTZ");
        // Unknown types pass through untouched (the DDL builder validates them).
        assert_eq!(mapped_type("geometry", Engine::Postgres), "geometry");
    }

    #[test]
    fn missing_tables_are_created_existing_ones_are_left_alone() {
        let src = vec![
            table("people", vec![col("id", "integer", true), col("name", "varchar(50)", false)]),
            table("kept", vec![col("id", "integer", true)]),
        ];
        let dst = vec![table("kept", vec![col("id", "integer", true)])];
        let plan = plan(&src, &dst, Engine::Sqlite, "main", None);
        assert_eq!(plan.statements.len(), 1, "{plan:?}");
        assert!(plan.statements[0].contains("\"people\"") && plan.statements[0].contains("VARCHAR(50)"), "{plan:?}");
        assert!(plan.summary.iter().any(|l| l.contains("already on the target")));
        assert!(plan.destructive.is_empty());
    }

    #[test]
    fn unexpressible_indexes_are_noted_not_fatal() {
        let mut t = table("t", vec![col("id", "integer", true)]);
        t.1.indexes.push(IndexInfo {
            name: "ix_t_id".into(),
            columns: vec!["id".into()],
            unique: false,
            primary: false,
        });
        // Snowflake has no secondary indexes: the table is still created.
        let plan = plan(&[t], &[], Engine::Snowflake, "DB", Some("PUBLIC"));
        assert_eq!(plan.statements.len(), 1, "{plan:?}");
        assert!(plan.summary.iter().any(|l| l.contains("index ix_t_id skipped")), "{plan:?}");
    }
}
