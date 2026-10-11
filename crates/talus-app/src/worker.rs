//! Bridge between the UI thread and the tokio runtime. The UI submits `Request`s and
//! receives `JobEvent`s; it never blocks on I/O. Sessions are pooled per
//! (connection, purpose, database) so editor tabs run independently of metadata loading.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use talus_core::debugger::{DebugEvent, PgDebugger};
use talus_core::engine::{CancelHandle, ScriptOptions, ScriptOutcome, Session};
use talus_core::export::ExportFormat;
use talus_core::model::{ApplyReport, ChangeSet, ConnectionConfig, ObjectRef, PageQuery, QueryResult, TableStructure};
use talus_core::tunnel::Tunnel;
use talus_core::{DbError, Secret};
use uuid::Uuid;

pub type JobId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Purpose {
    /// Navigation tree, info pane, designers.
    Meta,
    /// One editor/data tab.
    Tab(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionKey {
    conn: Uuid,
    purpose: Purpose,
    database: Option<String>,
}

#[derive(Clone)]
pub struct ConnInfo {
    pub cfg: ConnectionConfig,
    pub password: Option<Secret>,
    /// SSH password / key passphrase or HTTP proxy password when the connection uses a tunnel.
    pub tunnel_secret: Option<Secret>,
}

impl ConnInfo {
    pub fn plain(cfg: ConnectionConfig, password: Option<Secret>) -> Self {
        Self { cfg, password, tunnel_secret: None }
    }
}

pub enum Request {
    /// Fresh connection that is closed right away; returns the server version.
    Test,
    /// Opens the meta session; returns version and databases.
    Connect,
    ListDatabases,
    ListSchemas,
    ListObjects {
        database: String,
        schema: Option<String>,
    },
    Structure(ObjectRef),
    Ddl(ObjectRef),
    RunScript {
        script: String,
        opts: ScriptOptions,
    },
    FetchPage {
        query: PageQuery,
        structure: TableStructure,
    },
    Apply {
        table: ObjectRef,
        structure: TableStructure,
        changes: ChangeSet,
    },
    /// Views of a database/schema with their definitions (view synchronization).
    SchemaViews {
        database: String,
        schema: Option<String>,
    },
    /// Tables + structures of a database/schema (ER diagram).
    SchemaModel {
        database: String,
        schema: Option<String>,
    },
    /// Find in database (names or row contents).
    Find {
        database: String,
        schema: Option<String>,
        needle: String,
        data: bool,
    },
    /// CSV import into a table, one transaction.
    ImportCsv {
        obj: ObjectRef,
        spec: talus_core::import::ImportSpec,
    },
    /// JSON array-of-objects import into a table, one transaction.
    ImportJson {
        obj: ObjectRef,
        path: PathBuf,
    },
    /// XML import (the XML export format) into a table, one transaction.
    ImportXml {
        obj: ObjectRef,
        path: PathBuf,
    },
    /// DDL batch (designer); all-or-nothing when `transactional`.
    ExecuteBatch {
        statements: Vec<String>,
        transactional: bool,
    },
    /// Row-level sync of `src` (this job's session) into `dst` on another connection.
    SyncData {
        target: Box<ConnInfo>,
        target_database: Option<String>,
        src: ObjectRef,
        dst: ObjectRef,
        delete_missing: bool,
        apply: bool,
    },
    ListUsers,
    UserGrants(talus_core::sql::users::UserInfo),
    /// Stored routines and triggers with their definitions.
    ListProgrammables {
        database: String,
        schema: Option<String>,
    },
    /// Schema documentation written to a file.
    Dictionary {
        database: String,
        schema: Option<String>,
        format: talus_core::dictionary::DictFormat,
        path: PathBuf,
    },
    Profile(ObjectRef),
    FillTable {
        obj: ObjectRef,
        rows: usize,
        seed: u64,
    },
    /// One scheduled/saved job action on this job's session.
    RunJob(talus_core::jobs::JobAction),
    /// Key-value / document operation for Redis and MongoDB connections.
    NoSql(talus_core::nosql::NoSqlOp),
    /// SQL dump of a database/schema to a file.
    DumpSchema {
        database: String,
        schema: Option<String>,
        path: PathBuf,
        include_data: bool,
    },
    /// Executes every statement of a SQL file.
    ExecuteFile {
        path: PathBuf,
        transactional: bool,
    },
    /// Streams a whole table/view to a file in chunks (never fully in memory).
    Export {
        obj: ObjectRef,
        format: ExportFormat,
        path: PathBuf,
    },
    /// PL/pgSQL debugger: what routine `oid` takes and its source.
    DebugDescribe(u32),
    /// PL/pgSQL debugger operation for the debug tab `tab` (own sessions, outside the session map).
    Debug {
        tab: u64,
        op: DebugOp,
    },
}

#[derive(Debug, Clone)]
pub enum DebugOp {
    Start { target: Box<talus_core::debugger::DebugTarget>, args: Vec<Option<String>>, breakpoints: Vec<i32> },
    Step(talus_core::debugger::Step),
    Breakpoint { func: u32, line: i32, on: bool },
    SetValue { name: String, line: i32, value: String },
    Stop,
}

#[derive(Debug)]
pub enum Response {
    Tested(String),
    Connected {
        version: String,
        databases: Vec<String>,
    },
    Databases(Vec<String>),
    Schemas(Vec<String>),
    Objects(Vec<ObjectRef>),
    Structure(TableStructure),
    Ddl(String),
    Script(ScriptOutcome),
    Page(QueryResult),
    Applied(ApplyReport),
    Exported {
        rows: u64,
        path: PathBuf,
    },
    BatchDone(usize),
    Imported(u64),
    Found(Vec<talus_core::model::FindHit>),
    Model(Vec<(ObjectRef, TableStructure)>),
    Views(Vec<(ObjectRef, String)>),
    DataSync(talus_core::data_sync::DataSyncReport),
    Dumped(talus_core::dump::DumpReport, PathBuf),
    Programmables(Vec<talus_core::programmability::ProgItem>),
    DebugTarget(Box<talus_core::debugger::DebugTarget>),
    /// After a start / step / value change; `None` for breakpoint changes and stop.
    Debug(Option<talus_core::debugger::DebugEvent>),
    Users(Vec<talus_core::sql::users::UserInfo>),
    Grants(Vec<String>),
    NoSql(talus_core::nosql::NoSqlOut),
    JobDone(String),
    DictionaryWritten {
        tables: usize,
        path: PathBuf,
    },
    Profile(Vec<talus_core::profile::ColumnProfile>),
    Filled(u64),
}

#[derive(Debug)]
pub struct JobEvent {
    pub id: JobId,
    pub conn: Uuid,
    pub result: Result<Response, DbError>,
}

/// A pooled connection: SQL or one of the non-SQL browsers.
enum Backend {
    Sql(Session),
    NoSql(talus_core::nosql::NoSqlSession),
}

impl Backend {
    async fn connect(
        cfg: &ConnectionConfig,
        password: Option<&Secret>,
        database: Option<&str>,
    ) -> Result<Self, DbError> {
        if cfg.engine.is_sql() {
            Session::connect(cfg, password, database).await.map(Backend::Sql)
        } else {
            talus_core::nosql::NoSqlSession::connect(cfg, password, database).await.map(Backend::NoSql)
        }
    }

    async fn server_version(&mut self) -> Result<String, DbError> {
        match self {
            Backend::Sql(s) => s.server_version().await,
            Backend::NoSql(n) => n.server_version().await,
        }
    }

    async fn close(self) -> Result<(), DbError> {
        match self {
            Backend::Sql(s) => s.close().await,
            Backend::NoSql(n) => n.close().await,
        }
    }
}

type SharedSession = Arc<tokio::sync::Mutex<Option<Backend>>>;

struct Shared {
    sessions: tokio::sync::Mutex<HashMap<SessionKey, SharedSession>>,
    cancels: Mutex<HashMap<JobId, CancelHandle>>,
    /// Jobs whose cancel was requested before their session existed.
    cancelled_early: Mutex<std::collections::HashSet<JobId>>,
    /// One SSH tunnel per connection, shared by all its sessions.
    tunnels: tokio::sync::Mutex<HashMap<Uuid, Arc<Tunnel>>>,
    /// Running PL/pgSQL debuggers by debug tab, and their targets' cancel handles (usable while a
    /// step holds the debugger).
    debuggers: Mutex<HashMap<u64, DebugSlot>>,
    known_hosts: PathBuf,
}

pub struct Worker {
    rt: tokio::runtime::Handle,
    shared: Arc<Shared>,
    tx: mpsc::Sender<JobEvent>,
    rx: mpsc::Receiver<JobEvent>,
    next_id: AtomicU64,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl Worker {
    pub fn new(rt: tokio::runtime::Handle, known_hosts: PathBuf, repaint: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            rt,
            shared: Arc::new(Shared {
                sessions: tokio::sync::Mutex::new(HashMap::new()),
                cancels: Mutex::new(HashMap::new()),
                cancelled_early: Mutex::new(Default::default()),
                tunnels: tokio::sync::Mutex::new(HashMap::new()),
                debuggers: Mutex::new(HashMap::new()),
                known_hosts,
            }),
            tx,
            rx,
            next_id: AtomicU64::new(1),
            repaint,
        }
    }

    pub fn poll(&self) -> Vec<JobEvent> {
        self.rx.try_iter().collect()
    }

    /// Submits a request. `database` selects the database for engines that need one
    /// session per database, and the default database for MySQL tabs.
    pub fn submit(&self, conn: &ConnInfo, purpose: Purpose, database: Option<String>, req: Request) -> JobId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let caps = conn.cfg.engine.capabilities();
        let database = match purpose {
            _ if caps.session_per_database => database,
            Purpose::Tab(_) if caps.network => database,
            _ => None,
        };
        let key = SessionKey { conn: conn.cfg.id, purpose, database };
        let (conn, shared, tx, repaint) = (conn.clone(), self.shared.clone(), self.tx.clone(), self.repaint.clone());
        self.rt.spawn(async move {
            let result = run_job(id, &shared, &conn, key, req).await;
            let _ = tx.send(JobEvent { id, conn: conn.cfg.id, result });
            repaint();
        });
        id
    }

    /// Requests cancellation of a running job. Safe to call for finished jobs.
    pub fn cancel(&self, id: JobId) {
        let handle = self.shared.cancels.lock().ok().and_then(|m| m.get(&id).cloned());
        match handle {
            Some(h) => {
                self.rt.spawn(async move {
                    if let Err(e) = h.cancel().await {
                        tracing::warn!(error = %e, "cancel failed");
                    }
                });
            }
            None => {
                if let Ok(mut s) = self.shared.cancelled_early.lock() {
                    s.insert(id);
                }
            }
        }
    }

    /// Closes every session of a connection (all tabs and metadata).
    pub fn disconnect(&self, conn: Uuid) {
        self.close_where(move |k| k.conn == conn);
        let shared = self.shared.clone();
        self.rt.spawn(async move {
            shared.tunnels.lock().await.remove(&conn);
        });
    }

    /// Closes the sessions owned by one tab.
    pub fn close_tab(&self, tab: u64) {
        self.close_where(move |k| k.purpose == Purpose::Tab(tab));
    }

    /// Closes every session bound to one database of a connection (before dropping it).
    pub fn close_database(&self, conn: Uuid, db: String) {
        self.close_where(move |k| k.conn == conn && k.database.as_deref() == Some(db.as_str()));
    }

    fn close_where(&self, pred: impl Fn(&SessionKey) -> bool + Send + 'static) {
        let shared = self.shared.clone();
        self.rt.spawn(async move {
            let victims: Vec<SharedSession> = {
                let mut map = shared.sessions.lock().await;
                let keys: Vec<SessionKey> = map.keys().filter(|k| pred(k)).cloned().collect();
                keys.iter().filter_map(|k| map.remove(k)).collect()
            };
            for v in victims {
                if let Some(s) = v.lock().await.take()
                    && let Err(e) = s.close().await
                {
                    tracing::debug!(error = %e, "close failed");
                }
            }
        });
    }
}

async fn run_job(
    id: JobId,
    shared: &Shared,
    conn: &ConnInfo,
    key: SessionKey,
    req: Request,
) -> Result<Response, DbError> {
    if let Request::Debug { tab, op } = req {
        return debug_op(shared, conn, key.database.as_deref(), tab, op).await;
    }
    if let Request::Test = req {
        // A test never reuses (or leaves behind) a tunnel.
        let tunnel = open_tunnel_for(shared, conn).await?;
        let cfg = tunnel.as_ref().map_or_else(|| conn.cfg.clone(), |t| tunneled(&conn.cfg, t.local_port));
        let mut s = Backend::connect(&cfg, conn.password.as_ref(), None).await?;
        let v = s.server_version().await;
        let _ = s.close().await;
        drop(tunnel);
        let via = match (&conn.cfg.ssh, &conn.cfg.http_proxy) {
            (Some(_), _) => " (via SSH)",
            (None, Some(_)) => " (via HTTP tunnel)",
            (None, None) => "",
        };
        return v.map(|v| Response::Tested(format!("{v}{via}")));
    }
    let slot = {
        let mut map = shared.sessions.lock().await;
        map.entry(key.clone()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))).clone()
    };
    let mut guard = slot.lock().await;
    if guard.is_none() {
        let cfg = match shared_tunnel(shared, conn).await? {
            Some(t) => tunneled(&conn.cfg, t.local_port),
            None => conn.cfg.clone(),
        };
        match Backend::connect(&cfg, conn.password.as_ref(), key.database.as_deref()).await {
            Ok(s) => *guard = Some(s),
            Err(e) => {
                // A dead tunnel must not be reused by the next attempt.
                if conn.cfg.has_tunnel() {
                    shared.tunnels.lock().await.remove(&conn.cfg.id);
                }
                return Err(e);
            }
        }
    }
    let Some(backend) = guard.as_mut() else { return Err(DbError::ConnectionLost("session unavailable".into())) };
    let early = shared.cancelled_early.lock().map(|mut s| s.remove(&id)).unwrap_or(false);
    let result = match backend {
        Backend::Sql(session) => {
            if let Ok(mut c) = shared.cancels.lock() {
                c.insert(id, session.cancel_handle());
            }
            if early {
                Err(DbError::Cancelled)
            } else if let Request::SyncData { target, target_database, src, dst, delete_missing, apply } = req {
                sync_data(shared, session, &target, target_database.as_deref(), &src, &dst, delete_missing, apply).await
            } else {
                execute(session, req).await
            }
        }
        Backend::NoSql(n) => {
            if early {
                Err(DbError::Cancelled)
            } else {
                execute_nosql(n, req).await
            }
        }
    };
    if let Ok(mut c) = shared.cancels.lock() {
        c.remove(&id);
    }
    let lost = match &result {
        Err(DbError::ConnectionLost(_)) => true,
        Ok(Response::Script(o)) => o.connection_lost,
        _ => false,
    };
    if lost {
        // Drop the broken session; the next request reconnects. Nothing is re-sent.
        guard.take();
        shared.sessions.lock().await.remove(&key);
    }
    result
}

const TUNNEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

async fn open_tunnel_for(shared: &Shared, conn: &ConnInfo) -> Result<Option<Tunnel>, DbError> {
    let timeout = TUNNEL_TIMEOUT.max(std::time::Duration::from_secs(conn.cfg.connect_timeout_secs));
    talus_core::tunnel::open_for(&conn.cfg, conn.tunnel_secret.as_ref(), &shared.known_hosts, timeout).await
}

/// The connection's shared tunnel (SSH or HTTP), opened on first use and replaced when it died.
async fn shared_tunnel(shared: &Shared, conn: &ConnInfo) -> Result<Option<Arc<Tunnel>>, DbError> {
    if !conn.cfg.has_tunnel() {
        return Ok(None);
    }
    let mut map = shared.tunnels.lock().await;
    if let Some(t) = map.get(&conn.cfg.id).filter(|t| t.is_alive()) {
        return Ok(Some(t.clone()));
    }
    let Some(t) = open_tunnel_for(shared, conn).await? else { return Ok(None) };
    let t = Arc::new(t);
    map.insert(conn.cfg.id, t.clone());
    Ok(Some(t))
}

type DebugSlot = (Arc<tokio::sync::Mutex<Option<PgDebugger>>>, Option<CancelHandle>);

async fn debug_op(
    shared: &Shared,
    conn: &ConnInfo,
    database: Option<&str>,
    tab: u64,
    op: DebugOp,
) -> Result<Response, DbError> {
    let slot = {
        let mut map = shared.debuggers.lock().map_err(|_| DbError::Invalid("debugger state poisoned".into()))?;
        if let DebugOp::Stop = op {
            // Unblock a step that is waiting inside the routine before taking the debugger.
            if let Some((_, Some(cancel))) = map.get(&tab) {
                let cancel = cancel.clone();
                tokio::spawn(async move { cancel.cancel().await });
            }
        }
        map.entry(tab).or_insert_with(|| (Arc::new(tokio::sync::Mutex::new(None)), None)).0.clone()
    };
    let mut guard = slot.lock().await;
    match op {
        DebugOp::Start { target, args, breakpoints } => {
            if let Some(old) = guard.take() {
                let _ = old.stop().await;
            }
            let cfg = match shared_tunnel(shared, conn).await? {
                Some(t) => tunneled(&conn.cfg, t.local_port),
                None => conn.cfg.clone(),
            };
            let (dbg, event) =
                PgDebugger::start(&cfg, conn.password.as_ref(), database, &target, &args, &breakpoints).await?;
            if let Ok(mut map) = shared.debuggers.lock()
                && let Some(entry) = map.get_mut(&tab)
            {
                entry.1 = Some(dbg.target_cancel());
            }
            *guard = Some(dbg);
            Ok(Response::Debug(Some(event)))
        }
        DebugOp::Step(step) => {
            let dbg = guard.as_mut().ok_or_else(|| DbError::Invalid("the debugger is not running".into()))?;
            Ok(Response::Debug(Some(dbg.step(step).await?)))
        }
        DebugOp::Breakpoint { func, line, on } => {
            if let Some(dbg) = guard.as_mut().filter(|d| d.is_running()) {
                dbg.set_breakpoint(func, line, on).await?;
            }
            Ok(Response::Debug(None))
        }
        DebugOp::SetValue { name, line, value } => {
            let dbg = guard.as_mut().ok_or_else(|| DbError::Invalid("the debugger is not running".into()))?;
            let snap = dbg.set_value(&name, line, &value).await?;
            Ok(Response::Debug(Some(DebugEvent::Paused(snap))))
        }
        DebugOp::Stop => {
            if let Some(dbg) = guard.take() {
                dbg.stop().await?;
            }
            drop(guard);
            if let Ok(mut map) = shared.debuggers.lock() {
                map.remove(&tab);
            }
            Ok(Response::Debug(None))
        }
    }
}

/// Connection settings rewritten to go through a local tunnel port.
fn tunneled(cfg: &ConnectionConfig, local_port: u16) -> ConnectionConfig {
    ConnectionConfig { host: "127.0.0.1".into(), port: local_port, ssh: None, http_proxy: None, ..cfg.clone() }
}

/// Opens a throw-away session (own tunnel if needed) for the target side of a data sync.
#[allow(clippy::too_many_arguments)]
async fn sync_data(
    shared: &Shared,
    source: &mut Session,
    target: &ConnInfo,
    target_database: Option<&str>,
    src: &ObjectRef,
    dst: &ObjectRef,
    delete_missing: bool,
    apply: bool,
) -> Result<Response, DbError> {
    let tunnel = open_tunnel_for(shared, target).await?;
    let cfg = tunnel.as_ref().map_or_else(|| target.cfg.clone(), |t| tunneled(&target.cfg, t.local_port));
    let mut other = Session::connect(&cfg, target.password.as_ref(), target_database).await?;
    let report = talus_core::data_sync::sync_table(source, src, &mut other, dst, delete_missing, apply).await;
    let _ = other.close().await;
    report.map(Response::DataSync)
}

/// Requests a non-SQL connection understands: the navigator's, and `NoSql` operations.
async fn execute_nosql(n: &mut talus_core::nosql::NoSqlSession, req: Request) -> Result<Response, DbError> {
    match req {
        Request::Connect => {
            let version = n.server_version().await?;
            let databases = n.databases().await?;
            Ok(Response::Connected { version, databases })
        }
        Request::ListDatabases => Ok(Response::Databases(n.databases().await?)),
        Request::ListObjects { database, .. } => Ok(Response::Objects(n.objects(&database).await?)),
        Request::NoSql(op) => Ok(Response::NoSql(n.run(op).await?)),
        _ => Err(DbError::Invalid(format!("this action is not available for {}", n.engine().label()))),
    }
}

async fn execute(s: &mut Session, req: Request) -> Result<Response, DbError> {
    Ok(match req {
        Request::Test | Request::Debug { .. } => unreachable!("handled before acquiring a pooled session"),
        Request::SyncData { .. } => unreachable!("handled by run_job"),
        Request::NoSql(_) => Err(DbError::Invalid("this connection is a SQL database".into()))?,
        Request::Dictionary { database, schema, format, path } => {
            let tables =
                talus_core::dictionary::write_dictionary(s, &database, schema.as_deref(), format, &path).await?;
            Response::DictionaryWritten { tables, path }
        }
        Request::Profile(obj) => Response::Profile(talus_core::profile::profile_table(s, &obj, 5).await?),
        Request::FillTable { obj, rows, seed } => {
            Response::Filled(talus_core::datagen::fill_table(s, &obj, rows, seed).await?)
        }
        Request::RunJob(action) => {
            Response::JobDone(talus_core::jobs::execute_action(s, &action, chrono::Local::now().naive_local()).await?)
        }
        Request::Connect => {
            let version = s.server_version().await?;
            let databases = s.list_databases().await?;
            Response::Connected { version, databases }
        }
        Request::ListDatabases => Response::Databases(s.list_databases().await?),
        Request::ListSchemas => Response::Schemas(s.list_schemas().await?),
        Request::ListObjects { database, schema } => {
            Response::Objects(s.list_objects(&database, schema.as_deref()).await?)
        }
        Request::Structure(o) => Response::Structure(s.table_structure(&o).await?),
        Request::Ddl(o) => Response::Ddl(s.ddl(&o).await?),
        Request::RunScript { script, opts } => Response::Script(s.run_script(&script, &opts).await),
        Request::FetchPage { query, structure } => Response::Page(s.fetch_page(&query, &structure).await?),
        Request::Apply { table, structure, changes } => {
            Response::Applied(s.apply_changes(&table, &structure, &changes).await?)
        }
        Request::SchemaViews { database, schema } => {
            Response::Views(s.views_with_ddl(&database, schema.as_deref()).await?)
        }
        Request::SchemaModel { database, schema } => {
            Response::Model(s.schema_model(&database, schema.as_deref()).await?)
        }
        Request::Find { database, schema, needle, data } => {
            Response::Found(s.find(&database, schema.as_deref(), &needle, data).await?)
        }
        Request::ImportCsv { obj, spec } => {
            let st = s.table_structure(&obj).await?;
            Response::Imported(talus_core::import::import_csv(s, &obj, &st, &spec).await?.rows)
        }
        Request::ImportJson { obj, path } => {
            let st = s.table_structure(&obj).await?;
            Response::Imported(talus_core::import::import_json(s, &obj, &st, &path).await?.rows)
        }
        Request::ImportXml { obj, path } => {
            let st = s.table_structure(&obj).await?;
            Response::Imported(talus_core::import::import_xml(s, &obj, &st, &path).await?.rows)
        }
        Request::ExecuteBatch { statements, transactional } => {
            Response::BatchDone(s.execute_batch(&statements, transactional).await?)
        }
        Request::ListUsers => Response::Users(talus_core::sql::users::list_users(s).await?),
        Request::UserGrants(u) => Response::Grants(talus_core::sql::users::grants(s, &u).await?),
        Request::DebugDescribe(oid) => Response::DebugTarget(Box::new(talus_core::debugger::describe(s, oid).await?)),
        Request::ListProgrammables { database, schema } => {
            Response::Programmables(talus_core::programmability::list(s, &database, schema.as_deref()).await?)
        }
        Request::DumpSchema { database, schema, path, include_data } => {
            let report = talus_core::dump::dump_schema(s, &database, schema.as_deref(), &path, include_data).await?;
            Response::Dumped(report, path)
        }
        Request::ExecuteFile { path, transactional } => {
            Response::BatchDone(talus_core::dump::execute_sql_file(s, &path, transactional).await?)
        }
        Request::Export { obj, format, path } => {
            Response::Exported { rows: talus_core::export::export_table(s, &obj, format, &path).await?, path }
        }
    })
}
