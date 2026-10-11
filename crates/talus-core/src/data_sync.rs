//! Data synchronization of one table between two sessions (same or different server): rows are
//! matched by primary key and the target is brought in line with the source in a single
//! transaction. Values are compared as text, so the check is exact for same-engine pairs only.

use std::collections::HashMap;

use crate::engine::Session;
use crate::error::{DbError, DbResult};
use crate::model::{Cell, ChangeSet, ObjectRef, PageQuery, RowKey, RowUpdate, SortDir, TableStructure};

/// The target table is held in memory for matching; refuse tables beyond this.
pub const MAX_SYNC_ROWS: u64 = 1_000_000;
const PAGE: u64 = 5000;
const PREVIEW_LIMIT: usize = 20;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DataSyncReport {
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    pub unchanged: u64,
    /// False for a dry run.
    pub applied: bool,
    /// First few differences, human readable.
    pub preview: Vec<String>,
}

impl DataSyncReport {
    pub fn differences(&self) -> u64 {
        self.inserted + self.updated + self.deleted
    }
}

type Key = Vec<Cell>;

/// Reads a whole table in key order, projected to `cols`, handing each row to `f`.
async fn scan(
    s: &mut Session,
    obj: &ObjectRef,
    st: &TableStructure,
    key: &[String],
    cols: &[String],
    mut f: impl FnMut(Key, Vec<Cell>) -> DbResult<()>,
) -> DbResult<()> {
    let mut q = PageQuery {
        table: obj.clone(),
        filters: Vec::new(),
        sort: key.iter().map(|k| (k.clone(), SortDir::Asc)).collect(),
        offset: 0,
        limit: PAGE,
    };
    loop {
        let page = s.fetch_page(&q, st).await?;
        let pos = |name: &str| {
            page.columns
                .iter()
                .position(|c| c.name == name)
                .ok_or_else(|| DbError::Invalid(format!("column {name} missing from {}", obj.name)))
        };
        let key_idx = key.iter().map(|k| pos(k)).collect::<DbResult<Vec<_>>>()?;
        let col_idx = cols.iter().map(|c| pos(c)).collect::<DbResult<Vec<_>>>()?;
        for row in &page.rows {
            f(key_idx.iter().map(|&i| row[i].clone()).collect(), col_idx.iter().map(|&i| row[i].clone()).collect())?;
        }
        if (page.rows.len() as u64) < PAGE {
            return Ok(());
        }
        q.offset += PAGE;
    }
}

fn describe(kind: &str, key_names: &[String], key: &Key, extra: &str) -> String {
    let k = key_names
        .iter()
        .zip(key)
        .map(|(n, v)| format!("{n}={}", v.as_deref().unwrap_or("NULL")))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{kind} {k}{extra}")
}

/// Compares `src_obj` with `dst_obj` and, when `apply`, makes the target match the source.
/// Rows only in the target are deleted only if `delete_missing`.
pub async fn sync_table(
    src: &mut Session,
    src_obj: &ObjectRef,
    dst: &mut Session,
    dst_obj: &ObjectRef,
    delete_missing: bool,
    apply: bool,
) -> DbResult<DataSyncReport> {
    let sst = src.table_structure(src_obj).await?;
    let dst_st = dst.table_structure(dst_obj).await?;
    let key: Vec<String> = sst.primary_key().into_iter().map(String::from).collect();
    if key.is_empty() {
        return Err(DbError::Invalid(format!("{} has no primary key; data sync matches rows by key", src_obj.name)));
    }
    let dst_key: Vec<String> = dst_st.primary_key().into_iter().map(String::from).collect();
    if dst_key != key {
        return Err(DbError::Invalid(format!(
            "primary keys differ: source ({}) vs target ({})",
            key.join(", "),
            dst_key.join(", ")
        )));
    }
    let cols: Vec<String> =
        sst.columns.iter().filter(|c| dst_st.column(&c.name).is_some()).map(|c| c.name.clone()).collect();
    let key_pos: Vec<usize> = key.iter().filter_map(|k| cols.iter().position(|c| c == k)).collect();
    if key_pos.len() != key.len() {
        return Err(DbError::Invalid("key columns missing on one side".into()));
    }

    let mut target: HashMap<Key, Vec<Cell>> = HashMap::new();
    scan(dst, dst_obj, &dst_st, &key, &cols, |k, row| {
        if target.len() as u64 >= MAX_SYNC_ROWS {
            return Err(DbError::Invalid(format!(
                "target has more than {MAX_SYNC_ROWS} rows; too large for data sync"
            )));
        }
        target.insert(k, row);
        Ok(())
    })
    .await?;

    let mut report = DataSyncReport::default();
    let mut changes = ChangeSet::default();
    let note = |report: &mut DataSyncReport, line: String| {
        if report.preview.len() < PREVIEW_LIMIT {
            report.preview.push(line);
        }
    };
    scan(src, src_obj, &sst, &key, &cols, |k, row| {
        match target.remove(&k) {
            None => {
                report.inserted += 1;
                note(&mut report, describe("INSERT", &key, &k, ""));
                changes.inserts.push(cols.iter().cloned().zip(row).collect());
            }
            Some(old) if old == row => report.unchanged += 1,
            Some(old) => {
                report.updated += 1;
                let values: Vec<(String, Cell)> = cols
                    .iter()
                    .zip(row.iter().zip(&old))
                    .filter(|(_, (n, o))| n != o)
                    .map(|(c, (n, _))| (c.clone(), n.clone()))
                    .collect();
                let names = values.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>().join(", ");
                note(&mut report, describe("UPDATE", &key, &k, &format!(" ({names})")));
                changes.updates.push(RowUpdate { key: RowKey(key.iter().cloned().zip(k).collect()), values });
            }
        }
        if report.differences() > MAX_SYNC_ROWS {
            return Err(DbError::Invalid(format!("more than {MAX_SYNC_ROWS} differences; sync in smaller steps")));
        }
        Ok(())
    })
    .await?;
    if delete_missing {
        for (k, _) in target {
            report.deleted += 1;
            note(&mut report, describe("DELETE", &key, &k, ""));
            changes.deletes.push(RowKey(key.iter().cloned().zip(k).collect()));
        }
    }
    if apply && !changes.is_empty() {
        let done = dst.apply_changes(dst_obj, &dst_st, &changes).await?;
        if (done.inserted, done.updated, done.deleted) != (report.inserted, report.updated, report.deleted) {
            return Err(DbError::Invalid(format!("applied {done:?} but planned {report:?}")));
        }
    }
    report.applied = apply;
    Ok(report)
}
