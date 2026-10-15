//! Oracle Database engine (`oracledb`, Oracle's own pure-Rust thin driver: no Oracle Client libraries
//! needed). The driver is synchronous, so calls run on tokio's blocking pool like SQLite's.
//!
//! * One session per service; the tree's schema level lists accounts (owners) because Oracle schemas
//!   *are* users. Identifiers coming from the catalog are upper case and always quoted when used.
//! * Oracle has no auto-commit switch the driver exposes, so the session emulates what the other engines
//!   do: every non-query statement is committed unless a manual transaction is open
//!   (`Engine::begin_sql` = `SET TRANSACTION READ WRITE`; `COMMIT` / `ROLLBACK` end it).
//! * The driver cannot interrupt a running call (the TNS break/reset exchange hangs against Oracle Free
//!   23ai in `26.0.0-beta.4`), so cancelling ends the session with `ALTER SYSTEM KILL SESSION` from a
//!   side connection and the session reconnects — like SQL Server. That needs the `ALTER SYSTEM` privilege.
//! * Text cells: values are rendered in fixed formats (`YYYY-MM-DD HH24:MI:SS[.ffffff]`) and the session
//!   NLS settings are pinned to the same formats so edited text round-trips. Oracle stores `''` as NULL.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oracledb::{Config, Connection, Cursor, ErrorKind, Metadata, Row, ToDbValue};

use super::{CancelHandle, EngineSession, hex};
use crate::error::{DbError, DbResult};
use crate::model::{
    ColumnInfo, ColumnMeta, ConnectionConfig, FetchLimits, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectRef, Outcome,
    QueryResult, SslMode, TableStructure,
};
use crate::secret::Secret;

/// Session formats: what the grid shows is exactly what text edits are parsed with.
const NLS_SETUP: &str = "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD HH24:MI:SS' \
     NLS_TIMESTAMP_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF' \
     NLS_TIMESTAMP_TZ_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM' \
     NLS_NUMERIC_CHARACTERS = '.,'";

/// Lines of `DBMS_OUTPUT` shown after one PL/SQL statement.
const MAX_OUTPUT_LINES: usize = 10_000;

struct ConnectInfo {
    host: String,
    port: u16,
    service: String,
    user: String,
    password: Secret,
    tls: bool,
    /// Directory with `ewallet.pem` (private CA / client certificate), empty = system roots.
    wallet: String,
    timeout: Duration,
}

impl ConnectInfo {
    fn config(&self) -> DbResult<Config> {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let scheme = if self.tls { "tcps://" } else { "" };
        let mut cfg = Config::default()
            .set_credentials(&self.user, self.password.expose())
            .set_connect_string(&format!("{scheme}{host}:{}/{}", self.port, self.service))
            .map_err(|e| DbError::Connect(e.to_string()))?;
        if !self.wallet.is_empty() {
            cfg = cfg.set_wallet_location(self.wallet.clone());
        }
        Ok(cfg)
    }

    /// Opens a connection and applies the session setup. Runs on a blocking thread.
    fn open_blocking(&self) -> DbResult<Connection> {
        let conn = oracledb::connect(self.config()?).map_err(|e| DbError::Connect(map_err(e).to_string()))?;
        conn.execute(NLS_SETUP, &[]).map_err(map_err)?;
        // Output of DBMS_OUTPUT.PUT_LINE is collected after each PL/SQL statement.
        conn.execute("BEGIN DBMS_OUTPUT.ENABLE(1000000); END;", &[]).map_err(map_err)?;
        Ok(conn)
    }

    async fn open(self: &Arc<Self>) -> DbResult<Connection> {
        let me = self.clone();
        let attempt = tokio::task::spawn_blocking(move || me.open_blocking());
        // The driver has no connect timeout in this release: give up waiting (the thread ends with the OS timeout).
        tokio::time::timeout(self.timeout, attempt)
            .await
            .map_err(|_| DbError::Connect(format!("timed out after {}s", self.timeout.as_secs())))?
            .map_err(|e| DbError::Connect(e.to_string()))?
    }
}

/// State shared between a session and the handles that cancel it.
struct Shared {
    info: Arc<ConnectInfo>,
    sid: AtomicUsize,
    serial: AtomicUsize,
    /// A KILL was issued: the connection is (about to be) gone and must be replaced.
    requested: AtomicBool,
    /// Held for the whole KILL so the session never reconnects half-way through one.
    kill_lock: tokio::sync::Mutex<()>,
}

/// Cancels the running statement of the session it came from by killing the session (the driver has no
/// call interrupt); the session transparently reconnects and an open transaction is lost with it.
#[derive(Clone)]
pub struct OraCancel(Arc<Shared>);

impl OraCancel {
    pub async fn cancel(&self) -> DbResult<()> {
        let _guard = self.0.kill_lock.lock().await;
        self.0.requested.store(true, Ordering::SeqCst);
        let side = self.0.info.open().await.map_err(|e| DbError::Io(format!("cancel connection failed: {e}")))?;
        let (sid, serial) = (self.0.sid.load(Ordering::SeqCst), self.0.serial.load(Ordering::SeqCst));
        let r = tokio::task::spawn_blocking(move || {
            side.execute(&format!("ALTER SYSTEM KILL SESSION '{sid},{serial}' IMMEDIATE"), &[])
                .map(|_| ())
                .map_err(map_err)
        })
        .await
        .map_err(|e| DbError::Io(e.to_string()))?;
        r.or_else(|e| match e {
            // "marked for kill": the session is busy, Oracle ends it as soon as the call returns.
            DbError::Server { code: Some(c), .. } if c == "ORA-00031" => Ok(()),
            other => Err(other),
        })
        .map_err(|e| {
            self.0.requested.store(false, Ordering::SeqCst);
            match e {
                DbError::Server { code: Some(c), .. } if c == "ORA-01031" => DbError::Invalid(
                    "Cancelling needs the ALTER SYSTEM privilege (it ends the session with ALTER SYSTEM KILL SESSION)"
                        .into(),
                ),
                other => DbError::Io(format!("KILL SESSION failed: {other}")),
            }
        })
    }
}

pub struct OraSession {
    conn: Arc<Mutex<Connection>>,
    shared: Arc<Shared>,
    /// A manual transaction is open: statements are not committed until `COMMIT`.
    in_txn: Arc<AtomicBool>,
    service: String,
    user: String,
}

fn map_err(e: oracledb::Error) -> DbError {
    match e.kind() {
        ErrorKind::DbError(d) => {
            let code = format!("ORA-{:05}", d.code());
            let message = d.message();
            let message = message.strip_prefix(&format!("{code}: ")).unwrap_or(message).to_string();
            match d.code() {
                // session killed (own KILL, the monitor, an administrator) / not logged on
                28 | 1012 | 3113 | 3114 | 3135 => DbError::ConnectionLost(format!("{code}: {message}")),
                // user requested cancel of current operation
                1013 => DbError::Cancelled,
                _ => DbError::Server { message, code: Some(code), position: (d.offset() > 0).then(|| d.offset() + 1) },
            }
        }
        ErrorKind::DeadConnection | ErrorKind::NotConnected | ErrorKind::UnableToRecover => {
            DbError::ConnectionLost(e.to_string())
        }
        _ if e.to_string().contains("closed the connection")
            || e.to_string().contains("connection has been closed") =>
        {
            DbError::ConnectionLost(e.to_string())
        }
        _ => DbError::server(e.to_string()),
    }
}

/// One cell as text, `None` for NULL. Unsupported types are shown as `<TYPE>` rather than hidden.
fn cell_text(row: &Row, i: usize, m: &Metadata) -> DbResult<Option<String>> {
    use oracledb::{OracleIntervalDS, OracleIntervalYM, OracleNumber, OracleTimestamp};
    let t = m.db_type();
    macro_rules! get {
        ($ty:ty, $f:expr) => {
            row.get::<Option<$ty>>(i).map_err(map_err)?.map($f)
        };
    }
    Ok(match t.name() {
        "DB_TYPE_NUMBER" => get!(OracleNumber, |v| v.to_string()),
        "DB_TYPE_BINARY_DOUBLE" => get!(f64, |v| v.to_string()),
        "DB_TYPE_BINARY_FLOAT" => get!(f32, |v| v.to_string()),
        "DB_TYPE_BOOLEAN" => get!(bool, |v| if v { "TRUE" } else { "FALSE" }.to_string()),
        "DB_TYPE_DATE" | "DB_TYPE_TIMESTAMP" | "DB_TYPE_TIMESTAMP_LTZ" | "DB_TYPE_TIMESTAMP_TZ" => {
            get!(OracleTimestamp, |v| timestamp_text(&v, t.name(), m.scale()))
        }
        "DB_TYPE_INTERVAL_DS" => get!(OracleIntervalDS, |v| interval_ds_text(&v, m.scale())),
        "DB_TYPE_INTERVAL_YM" => get!(OracleIntervalYM, |v| {
            let (y, mo) = (i64::from(v.years()), i64::from(v.months()));
            let sign = if y < 0 || mo < 0 { '-' } else { '+' };
            format!("{sign}{:02}-{:02}", y.abs(), mo.abs())
        }),
        _ if t.is_binary_type() => get!(Vec<u8>, |v| hex(&v)),
        _ if t.is_string_type() => get!(String, |v| v),
        other => match row.get::<Option<String>>(i) {
            Ok(v) => v,
            Err(_) => Some(format!("<{}>", other.trim_start_matches("DB_TYPE_"))),
        },
    })
}

/// `YYYY-MM-DD HH24:MI:SS[.fff…][ ±HH:MM]`. The driver hands a `TIMESTAMP WITH TIME ZONE` over as the UTC
/// instant plus its offset; the text shows the wall-clock time the value was stored with.
fn timestamp_text(v: &oracledb::OracleTimestamp, type_name: &str, scale: i8) -> String {
    let zoned = type_name == "DB_TYPE_TIMESTAMP_TZ";
    let offset_min = i64::from(v.tz_hour_offset()) * 60
        + if v.tz_hour_offset() < 0 { -i64::from(v.tz_minute_offset()) } else { i64::from(v.tz_minute_offset()) };
    let local = chrono::NaiveDate::from_ymd_opt(i32::from(v.year()), u32::from(v.month()), u32::from(v.day()))
        .and_then(|d| {
            d.and_hms_nano_opt(u32::from(v.hour()), u32::from(v.minute()), u32::from(v.second()), v.nanoseconds())
        })
        .map(|dt| if zoned { dt + chrono::Duration::minutes(offset_min) } else { dt });
    let Some(dt) = local else { return v.to_string() };
    use chrono::{Datelike, Timelike};
    let mut s = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    );
    if type_name != "DB_TYPE_DATE" && scale > 0 {
        let digits = format!("{:09}", dt.nanosecond());
        s.push('.');
        s.push_str(&digits[..usize::try_from(scale).unwrap_or(6).min(9)]);
    }
    if zoned {
        let sign = if offset_min < 0 { '-' } else { '+' };
        s.push_str(&format!(" {sign}{:02}:{:02}", offset_min.abs() / 60, offset_min.abs() % 60));
    }
    s
}

/// `+DD HH24:MI:SS[.fff…]`, the way Oracle writes `INTERVAL DAY TO SECOND`.
fn interval_ds_text(v: &oracledb::OracleIntervalDS, scale: i8) -> String {
    let negative = v.days() < 0 || v.hours() < 0 || v.minutes() < 0 || v.seconds() < 0 || v.nanoseconds() < 0;
    let mut s = format!(
        "{}{:02} {:02}:{:02}:{:02}",
        if negative { '-' } else { '+' },
        v.days().abs(),
        v.hours().abs(),
        v.minutes().abs(),
        v.seconds().abs()
    );
    if scale > 0 {
        let digits = format!("{:09}", v.nanoseconds().unsigned_abs());
        s.push('.');
        s.push_str(&digits[..usize::try_from(scale).unwrap_or(6).min(9)]);
    }
    s
}

fn read_cursor(cursor: Cursor, limits: &FetchLimits) -> DbResult<QueryResult> {
    let metas: Vec<Metadata> = cursor.columns().to_vec();
    let mut out = QueryResult {
        columns: metas
            .iter()
            .map(|m| ColumnMeta {
                name: m.name().to_string(),
                type_name: m.data_type(),
                binary: m.db_type().is_binary_type(),
            })
            .collect(),
        ..Default::default()
    };
    for row in cursor {
        let row = row.map_err(map_err)?;
        if out.rows.len() >= limits.max_rows {
            out.truncated = true;
            break;
        }
        out.rows.push(metas.iter().enumerate().map(|(i, m)| cell_text(&row, i, m)).collect::<DbResult<Vec<_>>>()?);
    }
    Ok(out)
}

fn binds(params: &[Option<String>]) -> Vec<&dyn ToDbValue> {
    params.iter().map(|p| p as &dyn ToDbValue).collect()
}

// `oracledb 26.0.0-beta.4` re-uses a cached statement's bind buffers: executing the same SQL again with a
// longer string value can fail with "unknown TTC message type 1" (reproduced against Oracle Free 23ai).
// Statements that take values therefore bypass the statement cache; the extra parse is cheap.
fn query_bound(c: &Connection, sql: &str, params: &[Option<String>]) -> DbResult<Cursor> {
    c.statement(sql)
        .map_err(map_err)?
        .exclude_from_cache()
        .build()
        .map_err(map_err)?
        .query(&binds(params))
        .map_err(map_err)
}

fn execute_bound(c: &Connection, sql: &str, params: &[Option<String>]) -> DbResult<oracledb::ExecResult> {
    c.statement(sql)
        .map_err(map_err)?
        .exclude_from_cache()
        .build()
        .map_err(map_err)?
        .execute(&binds(params))
        .map_err(map_err)
}

fn cell(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

/// First word(s) show whether the text is a PL/SQL unit, whose final `;` belongs to it.
fn is_plsql_unit(sql: &str) -> bool {
    let lower = sql.trim_start().to_ascii_lowercase();
    let mut words = lower.split_whitespace();
    match words.next() {
        Some("begin" | "declare") => true,
        Some("create") => {
            let rest: Vec<&str> = words.take(6).collect();
            rest.iter()
                .any(|w| matches!(*w, "procedure" | "function" | "package" | "trigger" | "type" | "library" | "java"))
        }
        _ => false,
    }
}

/// Plain SQL must not end with `;` (ORA-00911); PL/SQL units must.
fn without_terminator(sql: &str) -> &str {
    let t = sql.trim_end();
    if is_plsql_unit(t) { t } else { t.trim_end_matches(';').trim_end() }
}

fn keyword_statement(sql: &str) -> Option<&'static str> {
    let norm: Vec<String> = sql.trim().trim_end_matches(';').split_whitespace().map(str::to_ascii_uppercase).collect();
    match norm.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["SET", "TRANSACTION", "READ", "WRITE"] => Some("begin"),
        ["COMMIT"] | ["COMMIT", "WORK"] => Some("commit"),
        ["ROLLBACK"] | ["ROLLBACK", "WORK"] => Some("rollback"),
        _ => None,
    }
}

/// Column type as written in `CREATE TABLE`, from the catalog's separate length/precision columns.
fn spell_type(
    data_type: &str,
    data_length: &str,
    char_length: &str,
    char_used: &str,
    precision: Option<&str>,
    scale: Option<&str>,
) -> String {
    match data_type {
        "VARCHAR2" | "NVARCHAR2" | "CHAR" | "NCHAR" => {
            if char_used == "C" || data_type.starts_with('N') {
                format!("{data_type}({char_length}{})", if data_type.starts_with('N') { "" } else { " CHAR" })
            } else {
                format!("{data_type}({data_length})")
            }
        }
        "RAW" => format!("RAW({data_length})"),
        "NUMBER" => match (precision, scale) {
            (Some(p), Some("0")) => format!("NUMBER({p})"),
            (Some(p), Some(s)) => format!("NUMBER({p},{s})"),
            (Some(p), None) => format!("NUMBER({p})"),
            (None, Some("0")) => "NUMBER(*,0)".into(),
            _ => "NUMBER".into(),
        },
        "FLOAT" => match precision {
            Some(p) if p != "126" => format!("FLOAT({p})"),
            _ => "FLOAT".into(),
        },
        other => other.to_string(),
    }
}

impl OraSession {
    pub async fn connect(cfg: &ConnectionConfig, password: Option<&Secret>) -> DbResult<Self> {
        let info = Arc::new(ConnectInfo {
            host: cfg.host.trim().to_string(),
            port: cfg.port,
            service: cfg.database.trim().to_string(),
            user: cfg.user.clone(),
            password: password.cloned().unwrap_or_else(|| Secret::new("")),
            tls: matches!(cfg.ssl_mode, SslMode::Require | SslMode::VerifyFull),
            wallet: cfg.ssl_ca_file.clone(),
            timeout: Duration::from_secs(cfg.connect_timeout_secs.max(1)),
        });
        let conn = info.open().await?;
        let (sid, serial) = (conn.session_id().map_err(map_err)?, conn.serial_num().map_err(map_err)?);
        let shared = Arc::new(Shared {
            info: info.clone(),
            sid: AtomicUsize::new(sid),
            serial: AtomicUsize::new(serial),
            requested: AtomicBool::new(false),
            kill_lock: tokio::sync::Mutex::new(()),
        });
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            shared,
            in_txn: Arc::new(AtomicBool::new(false)),
            service: info.service.clone(),
            user: info.user.to_ascii_uppercase(),
        })
    }

    /// Runs `f` on the blocking pool. When a cancel (KILL) hit this session meanwhile, the connection is
    /// replaced and a failed statement is reported as `Cancelled`.
    async fn with<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&Connection) -> DbResult<T> + Send + 'static,
    ) -> DbResult<T> {
        let conn = self.conn.clone();
        let result = tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|_| DbError::ConnectionLost("Oracle connection lock poisoned".into()))?;
            f(&guard)
        })
        .await
        .map_err(|e| DbError::Io(e.to_string()))?;
        if self.shared.requested.load(Ordering::SeqCst) {
            // Wait for the KILL to finish before opening the replacement.
            let _guard = self.shared.kill_lock.lock().await;
            self.shared.requested.store(false, Ordering::SeqCst);
            let fresh = self.shared.info.open().await?;
            self.shared.sid.store(fresh.session_id().map_err(map_err)?, Ordering::SeqCst);
            self.shared.serial.store(fresh.serial_num().map_err(map_err)?, Ordering::SeqCst);
            *self.conn.lock().map_err(|_| DbError::ConnectionLost("Oracle connection lock poisoned".into()))? = fresh;
            self.in_txn.store(false, Ordering::SeqCst);
            return match result {
                Ok(v) => Ok(v),
                Err(_) => Err(DbError::Cancelled),
            };
        }
        result
    }

    async fn text_rows(&mut self, sql: &str, params: &[Option<String>]) -> DbResult<Vec<Vec<Option<String>>>> {
        Ok(self.query_params(sql, params, &FetchLimits { max_rows: usize::MAX }).await?.rows)
    }

    fn owner_of(&self, obj: &ObjectRef) -> String {
        obj.schema.clone().unwrap_or_else(|| self.user.clone())
    }
}

/// Everything `DBMS_OUTPUT` collected since the last call, as one text column.
fn drain_output(conn: &Connection) -> DbResult<Option<QueryResult>> {
    let mut lines = Vec::new();
    while lines.len() < MAX_OUTPUT_LINES {
        let mut r = conn
            .execute(
                "DECLARE l VARCHAR2(32767); s PLS_INTEGER; BEGIN DBMS_OUTPUT.GET_LINE(l, s); :1 := s; :2 := l; END;",
                &[&oracledb::DB_TYPE_NUMBER, &oracledb::DB_TYPE_VARCHAR],
            )
            .map_err(map_err)?;
        let out = r.out_bind_data();
        let status: i32 = out.get(0).map_err(map_err)?;
        if status != 0 {
            break;
        }
        lines.push(vec![out.get::<Option<String>>(1).map_err(map_err)?.or_else(|| Some(String::new()))]);
    }
    if lines.is_empty() {
        return Ok(None);
    }
    Ok(Some(QueryResult {
        columns: vec![ColumnMeta { name: "DBMS_OUTPUT".into(), type_name: "VARCHAR2".into(), binary: false }],
        rows: lines,
        truncated: false,
    }))
}

impl EngineSession for OraSession {
    async fn server_version(&mut self) -> DbResult<String> {
        self.with(|c| c.version().map(|v| format!("Oracle Database {v}")).map_err(map_err)).await
    }

    async fn execute(&mut self, sql: &str, limits: &FetchLimits) -> DbResult<Vec<Outcome>> {
        if let Some(kw) = keyword_statement(sql) {
            let in_txn = self.in_txn.clone();
            return self
                .with(move |c| {
                    match kw {
                        "begin" => in_txn.store(true, Ordering::SeqCst),
                        "commit" => {
                            c.commit().map_err(map_err)?;
                            in_txn.store(false, Ordering::SeqCst);
                        }
                        _ => {
                            c.rollback().map_err(map_err)?;
                            in_txn.store(false, Ordering::SeqCst);
                        }
                    }
                    Ok(vec![Outcome::Affected(0)])
                })
                .await;
        }
        let sql = without_terminator(sql).to_string();
        let (limits, in_txn) = (*limits, self.in_txn.clone());
        self.with(move |c| {
            let mut stmt = c.statement(&sql).map_err(map_err)?.build().map_err(map_err)?;
            if stmt.is_query() {
                let cursor = stmt.query(&[]).map_err(map_err)?;
                let rows = read_cursor(cursor, &limits)?;
                // A locking read would hold its locks until the next commit.
                if !in_txn.load(Ordering::SeqCst) && sql.to_ascii_uppercase().contains("FOR UPDATE") {
                    c.commit().map_err(map_err)?;
                }
                return Ok(vec![Outcome::Rows(rows)]);
            }
            let plsql = stmt.is_plsql();
            let affected = match stmt.execute(&[]).map_err(map_err) {
                Ok(r) => r.rows_affected(),
                // "marked for kill": the busy session ends as soon as its call returns; the kill worked.
                Err(DbError::Server { code: Some(c), .. })
                    if c == "ORA-00031" && sql.to_ascii_uppercase().contains("KILL SESSION") =>
                {
                    0
                }
                Err(e) => return Err(e),
            };
            if !in_txn.load(Ordering::SeqCst) {
                c.commit().map_err(map_err)?;
            }
            let mut outcomes = vec![Outcome::Affected(affected)];
            if plsql && let Some(out) = drain_output(c)? {
                outcomes.push(Outcome::Rows(out));
            }
            Ok(outcomes)
        })
        .await
    }

    async fn query_params(
        &mut self,
        sql: &str,
        params: &[Option<String>],
        limits: &FetchLimits,
    ) -> DbResult<QueryResult> {
        let (sql, params, limits) = (without_terminator(sql).to_string(), params.to_vec(), *limits);
        self.with(move |c| read_cursor(query_bound(c, &sql, &params)?, &limits)).await
    }

    async fn exec_params(&mut self, sql: &str, params: &[Option<String>]) -> DbResult<u64> {
        let (sql, params, in_txn) = (without_terminator(sql).to_string(), params.to_vec(), self.in_txn.clone());
        self.with(move |c| {
            let n = execute_bound(c, &sql, &params)?.rows_affected();
            if !in_txn.load(Ordering::SeqCst) {
                c.commit().map_err(map_err)?;
            }
            Ok(n)
        })
        .await
    }

    async fn plsql_out(&mut self, block: &str, outs: usize) -> DbResult<Vec<Option<String>>> {
        let block = block.to_string();
        self.with(move |c| {
            let types: Vec<&'static oracledb::DbType> = (0..outs).map(|_| oracledb::DB_TYPE_VARCHAR).collect();
            let binds: Vec<&dyn ToDbValue> = types.iter().map(|t| t as &dyn ToDbValue).collect();
            let mut r = c.execute(&block, &binds).map_err(map_err)?;
            let row = r.out_bind_data();
            (0..outs).map(|i| row.get::<Option<String>>(i).map_err(map_err)).collect()
        })
        .await
    }

    async fn list_databases(&mut self) -> DbResult<Vec<String>> {
        // One session = one service: the tree shows the service as typed in the connection, whatever the
        // container is called inside (CON_NAME differs from the service name in many setups).
        Ok(vec![self.service.clone()])
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<String>> {
        let rows = self
            .text_rows("SELECT username FROM all_users WHERE oracle_maintained = 'N' ORDER BY username", &[])
            .await?;
        Ok(rows.iter().map(|r| cell(r, 0)).collect())
    }

    async fn list_objects(&mut self, database: &str, schema: Option<&str>) -> DbResult<Vec<ObjectRef>> {
        let owner = schema.map_or_else(|| self.user.clone(), str::to_string);
        let p = [Some(owner.clone()), Some(owner.clone())];
        let rows = self
            .text_rows(
                "SELECT table_name, 'TABLE' FROM all_tables WHERE owner = :1 AND nested = 'NO' AND iot_name IS NULL \
                   AND table_name NOT LIKE 'BIN$%' \
                 UNION ALL SELECT view_name, 'VIEW' FROM all_views WHERE owner = :2 ORDER BY 1",
                &p,
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| ObjectRef {
                database: database.to_string(),
                schema: Some(owner.clone()),
                name: cell(r, 0),
                kind: if cell(r, 1) == "VIEW" { ObjectKind::View } else { ObjectKind::Table },
            })
            .collect())
    }

    async fn table_structure(&mut self, obj: &ObjectRef) -> DbResult<TableStructure> {
        let owner = self.owner_of(obj);
        let p = [Some(owner.clone()), Some(obj.name.clone())];
        let pk_cols: Vec<String> = self
            .text_rows(
                "SELECT cc.column_name FROM all_constraints c \
                 JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
                 WHERE c.owner = :1 AND c.table_name = :2 AND c.constraint_type = 'P' ORDER BY cc.position",
                &p,
            )
            .await?
            .iter()
            .map(|r| cell(r, 0))
            .collect();
        let pk_index = self
            .text_rows(
                "SELECT index_name FROM all_constraints WHERE owner = :1 AND table_name = :2 AND constraint_type = 'P'",
                &p,
            )
            .await?
            .first()
            .map(|r| cell(r, 0));
        let columns: Vec<ColumnInfo> = self
            .text_rows(
                "SELECT c.column_name, c.data_type, c.data_length, c.char_length, c.char_used, c.data_precision, \
                        c.data_scale, c.nullable, c.data_default, c.identity_column, c.virtual_column, cc.comments \
                 FROM all_tab_cols c \
                 LEFT JOIN all_col_comments cc ON cc.owner = c.owner AND cc.table_name = c.table_name \
                   AND cc.column_name = c.column_name \
                 WHERE c.owner = :1 AND c.table_name = :2 AND c.hidden_column = 'NO' ORDER BY c.column_id",
                &p,
            )
            .await?
            .iter()
            .map(|r| {
                let identity = cell(r, 9) == "YES";
                let virtual_col = cell(r, 10) == "YES";
                let default = r.get(8).cloned().flatten().map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
                ColumnInfo {
                    primary_key: pk_cols.contains(&cell(r, 0)),
                    data_type: spell_type(
                        &cell(r, 1),
                        &cell(r, 2),
                        &cell(r, 3),
                        &cell(r, 4),
                        r.get(5).cloned().flatten().as_deref(),
                        r.get(6).cloned().flatten().as_deref(),
                    ),
                    nullable: cell(r, 7) == "Y",
                    default: if identity { None } else { default },
                    comment: r.get(11).cloned().flatten().filter(|c| !c.is_empty()),
                    extra: if identity {
                        Some("IDENTITY".into())
                    } else if virtual_col {
                        Some("VIRTUAL".into())
                    } else {
                        None
                    },
                    name: cell(r, 0),
                }
            })
            .collect();
        if columns.is_empty() {
            return Err(DbError::Invalid(format!("table not found: {}", obj.name)));
        }
        let mut indexes: Vec<IndexInfo> = Vec::new();
        for r in self
            .text_rows(
                "SELECT i.index_name, i.uniqueness, ic.column_name, ie.column_expression \
                 FROM all_indexes i \
                 JOIN all_ind_columns ic ON ic.index_owner = i.owner AND ic.index_name = i.index_name \
                 LEFT JOIN all_ind_expressions ie ON ie.index_owner = ic.index_owner AND ie.index_name = ic.index_name \
                   AND ie.column_position = ic.column_position \
                 WHERE i.table_owner = :1 AND i.table_name = :2 AND i.index_type <> 'LOB' \
                 ORDER BY i.index_name, ic.column_position",
                &p,
            )
            .await?
        {
            let name = cell(&r, 0);
            let column = r.get(3).cloned().flatten().map(|e| e.trim().to_string()).unwrap_or_else(|| cell(&r, 2));
            match indexes.last_mut() {
                Some(last) if last.name == name => last.columns.push(column),
                _ => indexes.push(IndexInfo {
                    primary: pk_index.as_deref() == Some(name.as_str()),
                    unique: cell(&r, 1) == "UNIQUE",
                    name,
                    columns: vec![column],
                }),
            }
        }
        // Primary key first, like the other engines.
        indexes.sort_by_key(|i| !i.primary);
        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        for r in self
            .text_rows(
                "SELECT c.constraint_name, cc.column_name, rc.owner, rc.table_name, rcc.column_name, c.delete_rule \
                 FROM all_constraints c \
                 JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
                 JOIN all_constraints rc ON rc.owner = c.r_owner AND rc.constraint_name = c.r_constraint_name \
                 JOIN all_cons_columns rcc ON rcc.owner = rc.owner AND rcc.constraint_name = rc.constraint_name \
                   AND rcc.position = cc.position \
                 WHERE c.owner = :1 AND c.table_name = :2 AND c.constraint_type = 'R' \
                 ORDER BY c.constraint_name, cc.position",
                &p,
            )
            .await?
        {
            let name = cell(&r, 0);
            let ref_table = if cell(&r, 2) == owner { cell(&r, 3) } else { format!("{}.{}", cell(&r, 2), cell(&r, 3)) };
            match foreign_keys.last_mut() {
                Some(last) if last.name == name => {
                    last.columns.push(cell(&r, 1));
                    last.ref_columns.push(cell(&r, 4));
                }
                _ => foreign_keys.push(ForeignKeyInfo {
                    name,
                    columns: vec![cell(&r, 1)],
                    ref_table,
                    ref_columns: vec![cell(&r, 4)],
                    on_update: "NO ACTION".into(),
                    on_delete: cell(&r, 5),
                }),
            }
        }
        Ok(TableStructure { columns, indexes, foreign_keys })
    }

    async fn ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let owner = self.owner_of(obj);
        let kind = if obj.kind == ObjectKind::View { "VIEW" } else { "TABLE" };
        let name = obj.name.clone();
        let (o, n) = (owner.clone(), name.clone());
        let main = self
            .with(move |c| {
                // Physical attributes only add noise to a script that is meant to be read and re-run.
                c.execute(
                    "BEGIN \
                       DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SEGMENT_ATTRIBUTES', FALSE); \
                       DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'STORAGE', FALSE); \
                       DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'TABLESPACE', FALSE); \
                       DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SQLTERMINATOR', TRUE); \
                     END;",
                    &[],
                )
                .map_err(map_err)?;
                let params = [Some(kind.to_string()), Some(n.clone()), Some(o.clone())];
                let found = query_bound(c, "SELECT DBMS_METADATA.GET_DDL(:1, :2, :3) FROM dual", &params).and_then(|cur| {
                    read_cursor(cur, &FetchLimits { max_rows: 1 })
                });
                let found = found.map_err(|e| match e {
                    DbError::Server { code: Some(c), .. } if c == "ORA-31603" => {
                        DbError::Invalid(format!("object not found: {n}"))
                    }
                    other => other,
                })?;
                found.rows.first().and_then(|r| r.first().cloned().flatten()).ok_or_else(|| DbError::Invalid(format!("no DDL for {n}")))
            })
            .await?;
        let mut parts = vec![main.trim().to_string()];
        if obj.kind == ObjectKind::Table {
            // Indexes behind the primary key / unique constraints are already part of the table's DDL.
            let p = [Some(owner.clone()), Some(name.clone()), Some(owner.clone()), Some(name)];
            let extra = self
                .text_rows(
                    "SELECT index_name FROM all_indexes WHERE table_owner = :1 AND table_name = :2 AND index_type <> 'LOB' \
                       AND index_name NOT IN (SELECT index_name FROM all_constraints WHERE owner = :3 AND table_name = :4 \
                                              AND index_name IS NOT NULL) ORDER BY index_name",
                    &p,
                )
                .await?;
            for r in extra {
                let (o, n) = (owner.clone(), cell(&r, 0));
                let text = self
                    .with(move |c| {
                        let params = [Some("INDEX".to_string()), Some(n), Some(o)];
                        let r = query_bound(c, "SELECT DBMS_METADATA.GET_DDL(:1, :2, :3) FROM dual", &params)
                            .and_then(|cur| read_cursor(cur, &FetchLimits { max_rows: 1 }))?;
                        Ok(r.rows.first().and_then(|r| r.first().cloned().flatten()))
                    })
                    .await?;
                parts.extend(text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()));
            }
        }
        Ok(parts.join("\n\n"))
    }

    fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::Oracle(OraCancel(self.shared.clone()))
    }

    fn current_database(&self) -> String {
        self.service.clone()
    }

    async fn close(self) -> DbResult<()> {
        let conn = self.conn;
        tokio::task::spawn_blocking(move || drop(conn)).await.map_err(|e| DbError::Io(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plsql_units_keep_their_semicolon_and_sql_loses_it() {
        assert_eq!(without_terminator("select 1 from dual;"), "select 1 from dual");
        assert_eq!(without_terminator("select 1 from dual ;; "), "select 1 from dual");
        assert_eq!(without_terminator("BEGIN NULL; END;"), "BEGIN NULL; END;");
        assert_eq!(without_terminator("declare x number; begin null; end;"), "declare x number; begin null; end;");
        assert_eq!(
            without_terminator("CREATE OR REPLACE PROCEDURE p AS BEGIN NULL; END;"),
            "CREATE OR REPLACE PROCEDURE p AS BEGIN NULL; END;"
        );
        assert_eq!(without_terminator("create table t (a number);"), "create table t (a number)");
        assert_eq!(
            without_terminator("CREATE TRIGGER t BEFORE INSERT ON x BEGIN NULL; END;"),
            "CREATE TRIGGER t BEFORE INSERT ON x BEGIN NULL; END;"
        );
    }

    #[test]
    fn transaction_keywords_are_recognised() {
        assert_eq!(keyword_statement("set transaction read write"), Some("begin"));
        assert_eq!(keyword_statement("COMMIT;"), Some("commit"));
        assert_eq!(keyword_statement(" rollback work "), Some("rollback"));
        assert_eq!(keyword_statement("rollback to savepoint a"), None);
        assert_eq!(keyword_statement("select 1 from dual"), None);
    }

    #[test]
    fn catalog_types_are_spelled_like_create_table() {
        assert_eq!(spell_type("VARCHAR2", "50", "50", "B", None, None), "VARCHAR2(50)");
        assert_eq!(spell_type("VARCHAR2", "200", "50", "C", None, None), "VARCHAR2(50 CHAR)");
        assert_eq!(spell_type("NUMBER", "22", "0", "B", Some("10"), Some("2")), "NUMBER(10,2)");
        assert_eq!(spell_type("NUMBER", "22", "0", "B", Some("10"), Some("0")), "NUMBER(10)");
        assert_eq!(spell_type("NUMBER", "22", "0", "B", None, Some("0")), "NUMBER(*,0)");
        assert_eq!(spell_type("NUMBER", "22", "0", "B", None, None), "NUMBER");
        assert_eq!(spell_type("RAW", "8", "0", "B", None, None), "RAW(8)");
        assert_eq!(spell_type("TIMESTAMP(6)", "11", "0", "B", None, None), "TIMESTAMP(6)");
        assert_eq!(spell_type("FLOAT", "22", "0", "B", Some("126"), None), "FLOAT");
    }
}
