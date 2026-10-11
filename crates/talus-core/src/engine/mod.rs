//! Database sessions. `Session` dispatches to one engine module; everything engine-specific
//! (SQL catalogs, drivers, cancellation) lives in those modules.

mod mssql;
mod mysql;
mod postgres;
mod sqlite;

pub use sqlite::{create_database_file, create_database_file_with_key};

use std::time::Instant;

use crate::error::{DbError, DbResult};
use crate::model::{
    ApplyReport, ChangeSet, ConnectionConfig, Engine, FetchLimits, FindHit, ObjectRef, Outcome, PageQuery, QueryResult,
    StatementResult, TableStructure,
};
use crate::secret::Secret;
use crate::sql::{Dialect, dml, split};

/// Operations every engine implements. Crate-private; the public surface is `Session`.
pub(crate) trait EngineSession {
    async fn server_version(&mut self) -> DbResult<String>;
    /// Executes one statement (no bind params). May yield several outcomes (e.g. MySQL `CALL`).
    async fn execute(&mut self, sql: &str, limits: &FetchLimits) -> DbResult<Vec<Outcome>>;
    /// Runs a parameterised query whose columns are all text-representable.
    async fn query_params(
        &mut self,
        sql: &str,
        params: &[Option<String>],
        limits: &FetchLimits,
    ) -> DbResult<QueryResult>;
    /// Runs a parameterised statement, returning affected (matched) rows.
    async fn exec_params(&mut self, sql: &str, params: &[Option<String>]) -> DbResult<u64>;
    async fn list_databases(&mut self) -> DbResult<Vec<String>>;
    /// Schemas of the session's current database (engines with `Capabilities::schemas`).
    async fn list_schemas(&mut self) -> DbResult<Vec<String>>;
    async fn list_objects(&mut self, database: &str, schema: Option<&str>) -> DbResult<Vec<ObjectRef>>;
    async fn table_structure(&mut self, obj: &ObjectRef) -> DbResult<TableStructure>;
    async fn ddl(&mut self, obj: &ObjectRef) -> DbResult<String>;
    fn cancel_handle(&self) -> CancelHandle;
    fn current_database(&self) -> String;
    async fn close(self) -> DbResult<()>;
}

/// Cancels the statement currently running on the session it was taken from.
#[derive(Clone)]
pub enum CancelHandle {
    Sqlite(std::sync::Arc<rusqlite::InterruptHandle>),
    Postgres(Box<postgres::PgCancel>),
    MySql(mysql::MyCancel),
    SqlServer(mssql::MsCancel),
}

impl CancelHandle {
    pub async fn cancel(&self) -> DbResult<()> {
        match self {
            CancelHandle::Sqlite(h) => {
                h.interrupt();
                Ok(())
            }
            CancelHandle::Postgres(c) => c.cancel().await,
            CancelHandle::MySql(c) => c.cancel().await,
            CancelHandle::SqlServer(c) => c.cancel().await,
        }
    }
}

pub enum Session {
    Sqlite(sqlite::SqliteSession),
    Postgres(Box<postgres::PgSession>),
    MySql(mysql::MySession),
    SqlServer(Box<mssql::MsSession>),
}

macro_rules! dispatch {
    ($self:expr, $s:ident => $body:expr) => {
        match $self {
            Session::Sqlite($s) => $body,
            Session::Postgres($s) => $body,
            Session::MySql($s) => $body,
            Session::SqlServer($s) => $body,
        }
    };
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ScriptOptions {
    pub limits: FetchLimits,
    pub continue_on_error: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatementError {
    pub index: usize,
    pub sql: String,
    /// 0-based line in the submitted script where the statement starts.
    pub line: usize,
    pub error: DbError,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScriptOutcome {
    pub results: Vec<StatementResult>,
    pub errors: Vec<StatementError>,
    pub cancelled: bool,
    /// The session's connection dropped; caller must reconnect explicitly.
    pub connection_lost: bool,
}

impl Session {
    /// Opens a session. `database` overrides `cfg.database` for engines with one session per database.
    pub async fn connect(
        cfg: &ConnectionConfig,
        password: Option<&Secret>,
        database: Option<&str>,
    ) -> DbResult<Session> {
        cfg.validate().map_err(DbError::Invalid)?;
        let mut s = match cfg.engine {
            Engine::Sqlite => Session::Sqlite(sqlite::SqliteSession::open(cfg, password).await?),
            Engine::Postgres => {
                Session::Postgres(Box::new(postgres::PgSession::connect(cfg, password, database).await?))
            }
            Engine::MySql => Session::MySql(mysql::MySession::connect(cfg, password, database).await?),
            Engine::SqlServer => {
                Session::SqlServer(Box::new(mssql::MsSession::connect(cfg, password, database).await?))
            }
            Engine::Redis | Engine::MongoDb => {
                return Err(DbError::Invalid(format!("{} is not a SQL engine", cfg.engine.label())));
            }
        };
        for stmt in split::split(&cfg.init_sql, cfg.engine) {
            if let Err(e) = s.execute_one(stmt.text, &FetchLimits { max_rows: 0 }).await {
                return Err(DbError::Connect(format!("initial query failed: {e}")));
            }
        }
        Ok(s)
    }

    pub fn engine(&self) -> Engine {
        match self {
            Session::Sqlite(_) => Engine::Sqlite,
            Session::Postgres(_) => Engine::Postgres,
            Session::MySql(_) => Engine::MySql,
            Session::SqlServer(_) => Engine::SqlServer,
        }
    }

    pub fn dialect(&self) -> Dialect {
        Dialect(self.engine())
    }

    /// PostgreSQL server notices for this session (taken once); `None` for other engines.
    pub fn take_pg_notices(&mut self) -> Option<tokio::sync::mpsc::Receiver<String>> {
        match self {
            Session::Postgres(p) => p.take_notices(),
            _ => None,
        }
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        dispatch!(self, s => s.cancel_handle())
    }

    pub fn current_database(&self) -> String {
        dispatch!(self, s => s.current_database())
    }

    pub async fn server_version(&mut self) -> DbResult<String> {
        dispatch!(self, s => s.server_version().await)
    }

    async fn execute_one(&mut self, sql: &str, limits: &FetchLimits) -> DbResult<Vec<Outcome>> {
        dispatch!(self, s => s.execute(sql, limits).await)
    }

    /// Splits `script` and executes statements in order. Stops at the first error unless
    /// `continue_on_error`; always stops on cancellation or connection loss.
    pub async fn run_script(&mut self, script: &str, opts: &ScriptOptions) -> ScriptOutcome {
        let mut out = ScriptOutcome::default();
        for (index, stmt) in split::split(script, self.engine()).into_iter().enumerate() {
            let started = Instant::now();
            match self.execute_one(stmt.text, &opts.limits).await {
                Ok(outcomes) => {
                    let elapsed = started.elapsed();
                    out.results.extend(outcomes.into_iter().map(|outcome| StatementResult {
                        sql: stmt.text.to_string(),
                        outcome,
                        elapsed,
                    }));
                }
                Err(error) => {
                    let fatal = matches!(error, DbError::Cancelled | DbError::ConnectionLost(_));
                    out.cancelled |= error == DbError::Cancelled;
                    out.connection_lost |= matches!(error, DbError::ConnectionLost(_));
                    out.errors.push(StatementError { index, sql: stmt.text.to_string(), line: stmt.line, error });
                    if fatal || !opts.continue_on_error {
                        break;
                    }
                }
            }
        }
        out
    }

    pub async fn list_databases(&mut self) -> DbResult<Vec<String>> {
        dispatch!(self, s => s.list_databases().await)
    }

    pub async fn list_schemas(&mut self) -> DbResult<Vec<String>> {
        dispatch!(self, s => s.list_schemas().await)
    }

    pub async fn list_objects(&mut self, database: &str, schema: Option<&str>) -> DbResult<Vec<ObjectRef>> {
        dispatch!(self, s => s.list_objects(database, schema).await)
    }

    pub async fn table_structure(&mut self, obj: &ObjectRef) -> DbResult<TableStructure> {
        dispatch!(self, s => s.table_structure(obj).await)
    }

    pub async fn ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        dispatch!(self, s => s.ddl(obj).await)
    }

    /// One page of table data. `structure` must describe `q.table` (from `table_structure`).
    pub async fn fetch_page(&mut self, q: &PageQuery, structure: &TableStructure) -> DbResult<QueryResult> {
        let built = dml::select_page(self.dialect(), q, structure)?;
        let limits = FetchLimits { max_rows: usize::try_from(q.limit).unwrap_or(usize::MAX) };
        let mut result = dispatch!(self, s => s.query_params(&built.sql, &built.params, &limits).await)?;
        // Prefer catalog type names over driver-reported ones.
        for c in &mut result.columns {
            if let Some(info) = structure.column(&c.name) {
                c.type_name.clone_from(&info.data_type);
            }
        }
        Ok(result)
    }

    /// Tables of one database/schema with their structure (ER diagram model).
    pub async fn schema_model(
        &mut self,
        database: &str,
        schema: Option<&str>,
    ) -> DbResult<Vec<(ObjectRef, TableStructure)>> {
        let mut out = Vec::new();
        for obj in
            self.list_objects(database, schema).await?.into_iter().filter(|o| o.kind == crate::model::ObjectKind::Table)
        {
            let st = self.table_structure(&obj).await?;
            out.push((obj, st));
        }
        Ok(out)
    }

    /// Views of one database/schema with their definitions.
    pub async fn views_with_ddl(&mut self, database: &str, schema: Option<&str>) -> DbResult<Vec<(ObjectRef, String)>> {
        let mut out = Vec::new();
        for obj in
            self.list_objects(database, schema).await?.into_iter().filter(|o| o.kind == crate::model::ObjectKind::View)
        {
            let ddl = self.ddl(&obj).await?;
            out.push((obj, ddl));
        }
        Ok(out)
    }

    /// Searches object/column names (`data == false`) or row contents (`data == true`) of one
    /// database/schema. Data search counts matching rows per table; it is cancellable.
    pub async fn find(
        &mut self,
        database: &str,
        schema: Option<&str>,
        needle: &str,
        data: bool,
    ) -> DbResult<Vec<FindHit>> {
        let needle = needle.trim();
        if needle.is_empty() {
            return Err(DbError::Invalid("enter text to search for".into()));
        }
        let lower = needle.to_lowercase();
        let objects = self.list_objects(database, schema).await?;
        let mut hits = Vec::new();
        for obj in objects {
            if !data {
                if obj.name.to_lowercase().contains(&lower) {
                    hits.push(FindHit { obj: obj.clone(), column: None, rows: None });
                }
                let st = self.table_structure(&obj).await?;
                for c in st.columns.iter().filter(|c| c.name.to_lowercase().contains(&lower)) {
                    hits.push(FindHit { obj: obj.clone(), column: Some(c.name.clone()), rows: None });
                }
                continue;
            }
            let st = self.table_structure(&obj).await?;
            let target = self.dialect().qualified(&obj)?;
            let Some(q) = dml::count_matches(self.dialect(), &target, &st, needle)? else { continue };
            let r = dispatch!(&mut *self, s => s.query_params(&q.sql, &q.params, &FetchLimits { max_rows: 1 }).await)?;
            let n: u64 =
                r.rows.first().and_then(|row| row.first().cloned().flatten()).and_then(|v| v.parse().ok()).unwrap_or(0);
            if n > 0 {
                hits.push(FindHit { obj, column: None, rows: Some(n) });
            }
        }
        Ok(hits)
    }

    /// Parameterised statement; returns affected rows.
    pub async fn exec_params(&mut self, sql: &str, params: &[Option<String>]) -> DbResult<u64> {
        dispatch!(self, s => s.exec_params(sql, params).await)
    }

    /// Applies grid edits in one transaction. Every UPDATE/DELETE must touch exactly one row;
    /// otherwise everything is rolled back so no partial or ambiguous change is committed.
    pub async fn apply_changes(
        &mut self,
        table: &ObjectRef,
        structure: &TableStructure,
        changes: &ChangeSet,
    ) -> DbResult<ApplyReport> {
        if changes.is_empty() {
            return Ok(ApplyReport::default());
        }
        let d = self.dialect();
        let target = d.qualified(table)?;
        let mut stmts = Vec::new();
        for key in &changes.deletes {
            stmts.push((dml::delete_row(d, &target, structure, key)?, true));
        }
        for u in &changes.updates {
            stmts.push((dml::update_row(d, &target, structure, &u.key, &u.values)?, true));
        }
        for row in &changes.inserts {
            stmts.push((dml::insert_row(d, &target, structure, row)?, false));
        }
        let limits = FetchLimits { max_rows: 0 };
        let begin = self.engine().begin_sql();
        self.execute_one(begin, &limits).await?;
        let mut report = ApplyReport::default();
        let res: DbResult<()> = async {
            for (q, exact_one) in &stmts {
                let n = dispatch!(&mut *self, s => s.exec_params(&q.sql, &q.params).await)?;
                if *exact_one && n != 1 {
                    return Err(DbError::Invalid(format!(
                        "{} affected {n} rows (expected exactly 1); the row may have been changed or deleted by someone else. All changes were rolled back.",
                        q.sql.split_whitespace().next().unwrap_or("statement")
                    )));
                }
            }
            Ok(())
        }
        .await;
        match res {
            Ok(()) => {
                self.execute_one("COMMIT", &limits).await?;
                report.deleted = changes.deletes.len() as u64;
                report.updated = changes.updates.len() as u64;
                report.inserted = changes.inserts.len() as u64;
                Ok(report)
            }
            Err(e) => {
                // Rollback failure is secondary; surface the original error.
                if let Err(rb) = self.execute_one("ROLLBACK", &limits).await {
                    tracing::warn!(error = %rb, "rollback after failed apply also failed");
                }
                Err(e)
            }
        }
    }

    /// Executes statements in order. When `transactional` (engine has transactional DDL), the
    /// batch is all-or-nothing. Otherwise it stops at the first error and the message states how
    /// many statements were already applied, so nothing is silently half-done.
    pub async fn execute_batch(&mut self, stmts: &[String], transactional: bool) -> DbResult<usize> {
        let limits = FetchLimits { max_rows: 0 };
        // SQLite table rebuild: foreign keys must be off while the old table is dropped, and the
        // pragma only works outside a transaction.
        if stmts.first().map(String::as_str) == Some(crate::sql::ddl::SQLITE_FK_OFF) && self.engine() == Engine::Sqlite
        {
            self.execute_one(crate::sql::ddl::SQLITE_FK_OFF, &limits).await?;
            let r = Box::pin(self.execute_batch(&stmts[1..], transactional)).await;
            let restore = self.execute_one("PRAGMA foreign_keys = ON", &limits).await;
            return match (r, restore) {
                (Ok(n), Ok(_)) => Ok(n + 1),
                (Err(e), _) => Err(e),
                (Ok(_), Err(e)) => Err(e),
            };
        }
        if transactional {
            let begin = self.engine().begin_sql();
            self.execute_one(begin, &limits).await?;
        }
        for (i, sql) in stmts.iter().enumerate() {
            if let Err(e) = self.execute_one(sql, &limits).await {
                if transactional {
                    if let Err(rb) = self.execute_one("ROLLBACK", &limits).await {
                        tracing::warn!(error = %rb, "rollback after failed batch also failed");
                    }
                    return Err(DbError::Invalid(format!("{e}\nNo changes were applied (rolled back).")));
                }
                return Err(DbError::Invalid(format!(
                    "{e}\n{i} of {} statements were applied before this error.",
                    stmts.len()
                )));
            }
        }
        if transactional {
            self.execute_one("COMMIT", &limits).await?;
        }
        Ok(stmts.len())
    }

    pub async fn close(self) -> DbResult<()> {
        dispatch!(self, s => s.close().await)
    }
}

/// Renders binary data as `0x…` hex (what the grid shows for BLOB/bytea values).
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        let _ = write!(s, "{b:02X}");
    }
    s
}
