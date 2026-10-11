//! SQLite engine (rusqlite, bundled SQLite). Calls run on tokio's blocking pool;
//! cancellation uses `sqlite3_interrupt` through `InterruptHandle`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, OpenFlags};

use super::{CancelHandle, EngineSession, hex};
use crate::Secret;
use crate::error::{DbError, DbResult};
use crate::model::{
    ColumnInfo, ColumnMeta, ConnectionConfig, FetchLimits, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectRef, Outcome,
    QueryResult, TableStructure,
};
use crate::sql::Dialect;

pub struct SqliteSession {
    conn: Arc<Mutex<Connection>>,
    interrupt: Arc<rusqlite::InterruptHandle>,
}

fn map_err(e: rusqlite::Error) -> DbError {
    match e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::OperationInterrupted => DbError::Cancelled,
        rusqlite::Error::SqliteFailure(f, msg) => DbError::Server {
            message: msg.unwrap_or_else(|| f.to_string()),
            code: Some(f.extended_code.to_string()),
            position: None,
        },
        rusqlite::Error::SqlInputError { error, msg, sql, offset } => {
            let byte = usize::try_from(offset).unwrap_or(0).min(sql.len());
            let position = sql.get(..byte).map(|p| p.chars().count() + 1);
            DbError::Server { message: msg, code: Some(error.extended_code.to_string()), position }
        }
        other => DbError::server(other.to_string()),
    }
}

/// Creates an empty SQLite database file. Fails if the file already exists.
pub fn create_database_file(path: &Path) -> DbResult<()> {
    create_database_file_with_key(path, None)
}

/// Creates an empty database; with a key the file is encrypted (SQLCipher) from the first byte.
pub fn create_database_file_with_key(path: &Path, key: Option<&Secret>) -> DbResult<()> {
    if path.exists() {
        return Err(DbError::Invalid(format!("file already exists: {}", path.display())));
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE)
        .map_err(map_err)?;
    if let Some(k) = key {
        conn.pragma_update(None, "key", k.expose()).map_err(map_err)?;
    }
    // Force the header to be written so the file is a valid database.
    conn.execute_batch("PRAGMA user_version = 0; VACUUM;").map_err(map_err)?;
    Ok(())
}

fn fmt_real(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 { format!("{f:.1}") } else { f.to_string() }
}

fn read_rows(
    stmt: &mut rusqlite::Statement<'_>,
    params: &[Option<String>],
    limits: &FetchLimits,
) -> DbResult<QueryResult> {
    let n = stmt.column_count();
    let mut columns: Vec<ColumnMeta> = stmt
        .columns()
        .iter()
        .map(|c| ColumnMeta {
            name: c.name().to_string(),
            type_name: c.decl_type().unwrap_or("").to_string(),
            binary: false,
        })
        .collect();
    let mut rows_out = Vec::new();
    let mut truncated = false;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter())).map_err(map_err)?;
    while let Some(row) = rows.next().map_err(map_err)? {
        if rows_out.len() >= limits.max_rows {
            truncated = true;
            break;
        }
        let mut cells = Vec::with_capacity(n);
        for (i, col) in columns.iter_mut().enumerate() {
            cells.push(match row.get_ref(i).map_err(map_err)? {
                ValueRef::Null => None,
                ValueRef::Integer(v) => Some(v.to_string()),
                ValueRef::Real(v) => Some(fmt_real(v)),
                ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => {
                    col.binary = true;
                    Some(hex(b))
                }
            });
        }
        rows_out.push(cells);
    }
    Ok(QueryResult { columns, rows: rows_out, truncated })
}

/// Rows of a catalog query as text cells.
fn text_rows(conn: &Connection, sql: &str, params: &[Option<String>]) -> DbResult<Vec<Vec<Option<String>>>> {
    let mut stmt = conn.prepare(sql).map_err(map_err)?;
    Ok(read_rows(&mut stmt, params, &FetchLimits { max_rows: usize::MAX })?.rows)
}

fn cell(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

impl SqliteSession {
    pub async fn open(cfg: &ConnectionConfig, key: Option<&Secret>) -> DbResult<Self> {
        let path = cfg.database.clone();
        let key = key.filter(|_| cfg.encrypted).map(|k| k.expose().to_string());
        let timeout = Duration::from_secs(cfg.connect_timeout_secs.max(1));
        let conn = tokio::task::spawn_blocking(move || -> DbResult<Connection> {
            if !Path::new(&path).exists() {
                return Err(DbError::Connect(format!("database file not found: {path}")));
            }
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|e| DbError::Connect(map_err(e).to_string()))?;
            if let Some(k) = &key {
                conn.pragma_update(None, "key", k).map_err(|e| DbError::Connect(map_err(e).to_string()))?;
            }
            conn.busy_timeout(timeout).map_err(map_err)?;
            // Reading the schema validates that the file really is a database (and that the key fits).
            conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(())).map_err(|e| {
                let msg = map_err(e).to_string();
                if key.is_some() {
                    DbError::Connect(format!("{msg} (wrong password, or the file is not encrypted)"))
                } else {
                    DbError::Connect(msg)
                }
            })?;
            conn.execute_batch("PRAGMA foreign_keys = ON").map_err(map_err)?;
            Ok(conn)
        })
        .await
        .map_err(|e| DbError::Connect(e.to_string()))??;
        let interrupt = Arc::new(conn.get_interrupt_handle());
        Ok(Self { conn: Arc::new(Mutex::new(conn)), interrupt })
    }

    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> DbResult<T> + Send + 'static,
    ) -> DbResult<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|_| DbError::ConnectionLost("SQLite connection lock poisoned".into()))?;
            f(&guard)
        })
        .await
        .map_err(|e| DbError::Io(e.to_string()))?
    }
}

impl EngineSession for SqliteSession {
    async fn server_version(&mut self) -> DbResult<String> {
        self.with(|c| c.query_row("SELECT sqlite_version()", [], |r| r.get::<_, String>(0)).map_err(map_err))
            .await
            .map(|v| format!("SQLite {v}"))
    }

    async fn execute(&mut self, sql: &str, limits: &FetchLimits) -> DbResult<Vec<Outcome>> {
        let sql = sql.to_string();
        let limits = *limits;
        self.with(move |c| {
            let mut stmt = c.prepare(&sql).map_err(map_err)?;
            if stmt.column_count() > 0 {
                Ok(vec![Outcome::Rows(read_rows(&mut stmt, &[], &limits)?)])
            } else {
                stmt.raw_execute().map_err(map_err)?;
                Ok(vec![Outcome::Affected(c.changes())])
            }
        })
        .await
    }

    async fn query_params(
        &mut self,
        sql: &str,
        params: &[Option<String>],
        limits: &FetchLimits,
    ) -> DbResult<QueryResult> {
        let (sql, params, limits) = (sql.to_string(), params.to_vec(), *limits);
        self.with(move |c| {
            let mut stmt = c.prepare(&sql).map_err(map_err)?;
            read_rows(&mut stmt, &params, &limits)
        })
        .await
    }

    async fn exec_params(&mut self, sql: &str, params: &[Option<String>]) -> DbResult<u64> {
        let (sql, params) = (sql.to_string(), params.to_vec());
        self.with(move |c| {
            c.execute(&sql, rusqlite::params_from_iter(params.iter())).map(|n| n as u64).map_err(map_err)
        })
        .await
    }

    async fn list_databases(&mut self) -> DbResult<Vec<String>> {
        self.with(|c| {
            Ok(text_rows(c, "SELECT name FROM pragma_database_list WHERE name <> 'temp' ORDER BY seq", &[])?
                .iter()
                .map(|r| cell(r, 0))
                .collect())
        })
        .await
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<String>> {
        Ok(Vec::new())
    }

    async fn list_objects(&mut self, database: &str, _schema: Option<&str>) -> DbResult<Vec<ObjectRef>> {
        let db = database.to_string();
        let master = format!("{}.sqlite_master", Dialect(crate::model::Engine::Sqlite).quote_ident(&db)?);
        self.with(move |c| {
            let sql = format!("SELECT name, type FROM {master} WHERE type IN ('table','view') AND name NOT LIKE 'sqlite!_%' ESCAPE '!' ORDER BY name");
            Ok(text_rows(c, &sql, &[])?
                .iter()
                .map(|r| ObjectRef {
                    database: db.clone(),
                    schema: None,
                    name: cell(r, 0),
                    kind: if cell(r, 1) == "view" { ObjectKind::View } else { ObjectKind::Table },
                })
                .collect())
        })
        .await
    }

    async fn table_structure(&mut self, obj: &ObjectRef) -> DbResult<TableStructure> {
        let (db, name) = (obj.database.clone(), obj.name.clone());
        self.with(move |c| {
            let p = [Some(name.clone()), Some(db.clone())];
            let columns: Vec<ColumnInfo> = text_rows(c, "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1, ?2) ORDER BY cid", &p)?
                .iter()
                .map(|r| ColumnInfo {
                    name: cell(r, 0),
                    data_type: cell(r, 1),
                    nullable: cell(r, 2) == "0",
                    default: r.get(3).cloned().flatten(),
                    primary_key: cell(r, 4) != "0",
                    comment: None,
                    extra: None,
                })
                .collect();
            if columns.is_empty() {
                return Err(DbError::Invalid(format!("table not found: {name}")));
            }
            let mut indexes = Vec::new();
            for r in text_rows(c, "SELECT name, \"unique\", origin FROM pragma_index_list(?1, ?2) ORDER BY name", &p)? {
                let idx = cell(&r, 0);
                let cols = text_rows(c, "SELECT coalesce(name, '<expr>') FROM pragma_index_info(?1, ?2) ORDER BY seqno", &[Some(idx.clone()), Some(db.clone())])?
                    .iter()
                    .map(|x| cell(x, 0))
                    .collect();
                indexes.push(IndexInfo { name: idx, columns: cols, unique: cell(&r, 1) == "1", primary: cell(&r, 2) == "pk" });
            }
            let pk: Vec<String> = columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
            if !pk.is_empty() && !indexes.iter().any(|i| i.primary) {
                // INTEGER PRIMARY KEY aliases rowid and has no separate index.
                indexes.insert(0, IndexInfo { name: "PRIMARY".into(), columns: pk, unique: true, primary: true });
            }
            let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
            let mut last_id = String::new();
            for r in text_rows(c, "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq", &p)? {
                let id = cell(&r, 0);
                if id != last_id || foreign_keys.is_empty() {
                    foreign_keys.push(ForeignKeyInfo {
                        name: format!("fk_{name}_{id}"),
                        columns: Vec::new(),
                        ref_table: cell(&r, 1),
                        ref_columns: Vec::new(),
                        on_update: cell(&r, 4),
                        on_delete: cell(&r, 5),
                    });
                    last_id = id;
                }
                if let Some(fk) = foreign_keys.last_mut() {
                    fk.columns.push(cell(&r, 2));
                    fk.ref_columns.push(cell(&r, 3));
                }
            }
            Ok(TableStructure { columns, indexes, foreign_keys })
        })
        .await
    }

    async fn ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let (db, name) = (obj.database.clone(), obj.name.clone());
        let master = format!("{}.sqlite_master", Dialect(crate::model::Engine::Sqlite).quote_ident(&db)?);
        self.with(move |c| {
            let main = text_rows(c, &format!("SELECT sql FROM {master} WHERE name = ?1 AND type IN ('table','view')"), &[Some(name.clone())])?;
            let Some(first) = main.first().and_then(|r| r.first().cloned().flatten()) else {
                return Err(DbError::Invalid(format!("object not found: {name}")));
            };
            let mut parts = vec![first];
            let extra = text_rows(
                c,
                &format!("SELECT sql FROM {master} WHERE tbl_name = ?1 AND type IN ('index','trigger') AND sql IS NOT NULL ORDER BY type, name"),
                &[Some(name)],
            )?;
            parts.extend(extra.into_iter().filter_map(|r| r.into_iter().next().flatten()));
            Ok(parts.join(";\n\n") + ";")
        })
        .await
    }

    fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::Sqlite(self.interrupt.clone())
    }

    fn current_database(&self) -> String {
        "main".into()
    }

    async fn close(self) -> DbResult<()> {
        // Dropping the last Arc closes the connection; do it off the async thread.
        let conn = self.conn;
        tokio::task::spawn_blocking(move || drop(conn)).await.map_err(|e| DbError::Io(e.to_string()))
    }
}
