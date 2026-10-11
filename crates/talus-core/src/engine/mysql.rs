//! MySQL / MariaDB engine (mysql_async). Cancellation sends `KILL QUERY <id>` from a
//! short-lived side connection. `CLIENT_FOUND_ROWS` makes UPDATE report matched rows, which
//! the data editor relies on to verify that exactly one row was targeted.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mysql_async::consts::ColumnType;
use mysql_async::prelude::Queryable;
use mysql_async::{Column, Conn, Opts, OptsBuilder, Params, SslOpts, Value};

use super::{CancelHandle, EngineSession, hex};
use crate::error::{DbError, DbResult};
use crate::model::{
    ColumnInfo, ColumnMeta, ConnectionConfig, FetchLimits, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectRef, Outcome,
    QueryResult, SslMode, TableStructure,
};
use crate::secret::Secret;
use crate::sql::Dialect;

const D: Dialect = Dialect(crate::model::Engine::MySql);
/// MySQL "binary" character set id.
const BINARY_CHARSET: u16 = 63;

#[derive(Clone)]
pub struct MyCancel {
    opts: Opts,
    id: u32,
    /// Some statements (e.g. `BENCHMARK`, `SLEEP`) return normally when killed, so the
    /// session also checks this flag to report the statement as cancelled.
    requested: Arc<AtomicBool>,
}

impl MyCancel {
    pub async fn cancel(&self) -> DbResult<()> {
        self.requested.store(true, Ordering::SeqCst);
        let mut conn =
            Conn::new(self.opts.clone()).await.map_err(|e| DbError::Io(format!("cancel connection failed: {e}")))?;
        let r = conn.query_drop(format!("KILL QUERY {}", self.id)).await;
        let _ = conn.disconnect().await;
        r.map_err(|e| DbError::Io(format!("KILL QUERY failed: {e}")))
    }
}

pub struct MySession {
    conn: Conn,
    opts: Opts,
    database: String,
    cancel_requested: Arc<AtomicBool>,
}

fn map_err(e: mysql_async::Error) -> DbError {
    match e {
        mysql_async::Error::Server(s) if s.code == 1317 => DbError::Cancelled,
        mysql_async::Error::Server(s) => {
            DbError::Server { message: s.message, code: Some(format!("{} ({})", s.code, s.state)), position: None }
        }
        mysql_async::Error::Io(e) => DbError::ConnectionLost(e.to_string()),
        mysql_async::Error::Driver(mysql_async::DriverError::ConnectionClosed) => {
            DbError::ConnectionLost("connection closed".into())
        }
        other => DbError::server(other.to_string()),
    }
}

fn is_binary(c: &Column) -> bool {
    use ColumnType::*;
    c.character_set() == BINARY_CHARSET
        && matches!(
            c.column_type(),
            MYSQL_TYPE_TINY_BLOB
                | MYSQL_TYPE_MEDIUM_BLOB
                | MYSQL_TYPE_LONG_BLOB
                | MYSQL_TYPE_BLOB
                | MYSQL_TYPE_STRING
                | MYSQL_TYPE_VAR_STRING
                | MYSQL_TYPE_VARCHAR
                | MYSQL_TYPE_BIT
                | MYSQL_TYPE_GEOMETRY
        )
}

fn type_name(c: &Column) -> String {
    let t = format!("{:?}", c.column_type());
    let t = t.trim_start_matches("MYSQL_TYPE_");
    match (t, is_binary(c)) {
        ("BLOB" | "TINY_BLOB" | "MEDIUM_BLOB" | "LONG_BLOB", false) => "TEXT".into(),
        ("VAR_STRING", false) => "VARCHAR".into(),
        ("VAR_STRING", true) => "VARBINARY".into(),
        ("STRING", false) => "CHAR".into(),
        ("STRING", true) => "BINARY".into(),
        ("LONG", _) => "INT".into(),
        ("LONGLONG", _) => "BIGINT".into(),
        ("SHORT", _) => "SMALLINT".into(),
        ("TINY", _) => "TINYINT".into(),
        ("INT24", _) => "MEDIUMINT".into(),
        ("NEWDECIMAL", _) => "DECIMAL".into(),
        (other, _) => other.to_string(),
    }
}

fn value_to_text(v: Value, col: &Column) -> Option<String> {
    match v {
        Value::NULL => None,
        Value::Bytes(b) if is_binary(col) => Some(hex(&b)),
        Value::Bytes(b) => Some(String::from_utf8(b).unwrap_or_else(|e| hex(e.as_bytes()))),
        Value::Int(i) => Some(i.to_string()),
        Value::UInt(u) => Some(u.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Double(d) => Some(d.to_string()),
        Value::Date(y, m, d, h, mi, s, us) => Some(if col.column_type() == ColumnType::MYSQL_TYPE_DATE {
            format!("{y:04}-{m:02}-{d:02}")
        } else if us > 0 {
            format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}")
        } else {
            format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
        }),
        Value::Time(neg, days, h, mi, s, us) => {
            let hours = u64::from(days) * 24 + u64::from(h);
            let sign = if neg { "-" } else { "" };
            Some(if us > 0 {
                format!("{sign}{hours:02}:{mi:02}:{s:02}.{us:06}")
            } else {
                format!("{sign}{hours:02}:{mi:02}:{s:02}")
            })
        }
    }
}

/// information_schema stores literal defaults unquoted; turn them back into SQL.
fn default_as_sql(value: &str, extra: &str, data_type: &str) -> String {
    if extra.contains("DEFAULT_GENERATED") {
        let upper = value.to_ascii_uppercase();
        return if upper.starts_with("CURRENT_TIMESTAMP") || upper.starts_with("NOW(") || upper.starts_with('(') {
            value.to_string()
        } else {
            format!("({value})")
        };
    }
    let numeric = matches!(
        data_type.to_ascii_lowercase().as_str(),
        "tinyint"
            | "smallint"
            | "mediumint"
            | "int"
            | "integer"
            | "bigint"
            | "decimal"
            | "numeric"
            | "float"
            | "double"
            | "real"
    );
    if numeric && value.parse::<f64>().is_ok() { value.to_string() } else { D.string_literal(value) }
}

fn to_params(params: &[Option<String>]) -> Params {
    if params.is_empty() {
        return Params::Empty;
    }
    Params::Positional(
        params.iter().map(|p| p.as_ref().map_or(Value::NULL, |s| Value::Bytes(s.clone().into_bytes()))).collect(),
    )
}

fn cell(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

/// Reads the current result set of `result` into a `QueryResult`, honouring the row cap
/// (remaining rows are drained so the connection stays usable).
async fn read_set<P: mysql_async::prelude::Protocol>(
    result: &mut mysql_async::QueryResult<'_, '_, P>,
    cols: &[Column],
    limits: &FetchLimits,
) -> DbResult<QueryResult> {
    let mut out = QueryResult {
        columns: cols
            .iter()
            .map(|c| ColumnMeta { name: c.name_str().into_owned(), type_name: type_name(c), binary: is_binary(c) })
            .collect(),
        ..Default::default()
    };
    while let Some(row) = result.next().await.map_err(map_err)? {
        if out.rows.len() >= limits.max_rows {
            out.truncated = true;
            continue;
        }
        out.rows.push(row.unwrap().into_iter().zip(cols).map(|(v, c)| value_to_text(v, c)).collect());
    }
    Ok(out)
}

impl MySession {
    fn build_opts(cfg: &ConnectionConfig, password: Option<&Secret>, database: Option<&str>, tls: bool) -> Opts {
        let db = database
            .filter(|d| !d.is_empty())
            .or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()))
            .map(String::from);
        let ssl = tls.then(|| {
            if cfg.ssl_mode == SslMode::VerifyFull {
                let ca = Some(cfg.ssl_ca_file.as_str()).filter(|p| !p.is_empty());
                SslOpts::default().with_root_certs(ca.map(|p| vec![PathBuf::from(p).into()]).unwrap_or_default())
            } else {
                SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true)
            }
        });
        OptsBuilder::default()
            .ip_or_hostname(cfg.host.clone())
            .tcp_port(cfg.port)
            .user(Some(cfg.user.clone()))
            .pass(password.map(|p| p.expose().to_string()))
            .db_name(db)
            .prefer_socket(false)
            .client_found_rows(true)
            .ssl_opts(ssl)
            .into()
    }

    pub async fn connect(cfg: &ConnectionConfig, password: Option<&Secret>, database: Option<&str>) -> DbResult<Self> {
        let timeout = Duration::from_secs(cfg.connect_timeout_secs.max(1));
        let attempt = |tls: bool| {
            let opts = Self::build_opts(cfg, password, database, tls);
            async move {
                match tokio::time::timeout(timeout, Conn::new(opts.clone())).await {
                    Err(_) => Err(DbError::Connect(format!("timed out after {}s", timeout.as_secs()))),
                    Ok(Err(mysql_async::Error::Server(s))) => {
                        Err(DbError::Connect(format!("{} ({})", s.message, s.code)))
                    }
                    Ok(Err(e)) => Err(DbError::Connect(e.to_string())),
                    Ok(Ok(conn)) => Ok((conn, opts)),
                }
            }
        };
        let (conn, opts) = match cfg.ssl_mode {
            SslMode::Disable => attempt(false).await?,
            SslMode::Require | SslMode::VerifyFull => attempt(true).await?,
            // Like libmysqlclient's PREFERRED: fall back to plain TCP only when TLS itself fails,
            // never after an authentication error from the server.
            SslMode::Prefer => match attempt(true).await {
                Ok(v) => v,
                Err(DbError::Connect(m))
                    if m.contains("SSL") || m.contains("TLS") || m.contains("ssl") || m.contains("tls") =>
                {
                    attempt(false).await?
                }
                Err(e) => return Err(e),
            },
        };
        let database = opts.db_name().unwrap_or_default().to_string();
        Ok(Self { conn, opts, database, cancel_requested: Arc::new(AtomicBool::new(false)) })
    }

    /// Wraps one round-trip: clears a stale cancel request first and converts a request made
    /// while the statement ran into `Cancelled`.
    fn begin_op(&self) {
        self.cancel_requested.store(false, Ordering::SeqCst);
    }

    fn end_op<T>(&self, r: DbResult<T>) -> DbResult<T> {
        if self.cancel_requested.swap(false, Ordering::SeqCst) {
            return Err(DbError::Cancelled);
        }
        r
    }

    async fn text_rows(&mut self, sql: &str, params: &[Option<String>]) -> DbResult<Vec<Vec<Option<String>>>> {
        Ok(self.query_params(sql, params, &FetchLimits { max_rows: usize::MAX }).await?.rows)
    }
}

impl EngineSession for MySession {
    async fn server_version(&mut self) -> DbResult<String> {
        let rows = self.text_rows("SELECT VERSION()", &[]).await?;
        Ok(format!("MySQL {}", rows.first().map(|r| cell(r, 0)).unwrap_or_default()))
    }

    async fn execute(&mut self, sql: &str, limits: &FetchLimits) -> DbResult<Vec<Outcome>> {
        self.begin_op();
        let r = async {
            let mut result = self.conn.query_iter(sql).await.map_err(map_err)?;
            let mut outcomes = Vec::new();
            while let Some(cols) = result.columns() {
                if cols.is_empty() {
                    outcomes.push(Outcome::Affected(result.affected_rows()));
                    // Consumes the OK packet and advances to the next set, if any.
                    result.next().await.map_err(map_err)?;
                } else {
                    outcomes.push(Outcome::Rows(read_set(&mut result, &cols, limits).await?));
                }
            }
            Ok(outcomes)
        }
        .await;
        self.end_op(r)
    }

    async fn query_params(
        &mut self,
        sql: &str,
        params: &[Option<String>],
        limits: &FetchLimits,
    ) -> DbResult<QueryResult> {
        self.begin_op();
        let r = async {
            let mut result = self.conn.exec_iter(sql, to_params(params)).await.map_err(map_err)?;
            let cols = result.columns().unwrap_or_else(|| Vec::new().into());
            let out = read_set(&mut result, &cols, limits).await?;
            result.drop_result().await.map_err(map_err)?;
            Ok(out)
        }
        .await;
        self.end_op(r)
    }

    async fn exec_params(&mut self, sql: &str, params: &[Option<String>]) -> DbResult<u64> {
        self.begin_op();
        let r = self.conn.exec_drop(sql, to_params(params)).await.map_err(map_err).map(|()| self.conn.affected_rows());
        self.end_op(r)
    }

    async fn list_databases(&mut self) -> DbResult<Vec<String>> {
        Ok(self
            .text_rows("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY 1", &[])
            .await?
            .iter()
            .map(|r| cell(r, 0))
            .collect())
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<String>> {
        Ok(Vec::new())
    }

    async fn list_objects(&mut self, database: &str, _schema: Option<&str>) -> DbResult<Vec<ObjectRef>> {
        Ok(self
            .text_rows(
                "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? ORDER BY 1",
                &[Some(database.to_string())],
            )
            .await?
            .iter()
            .map(|r| ObjectRef {
                database: database.to_string(),
                schema: None,
                name: cell(r, 0),
                kind: if cell(r, 1).contains("VIEW") { ObjectKind::View } else { ObjectKind::Table },
            })
            .collect())
    }

    async fn table_structure(&mut self, obj: &ObjectRef) -> DbResult<TableStructure> {
        let p = [Some(obj.database.clone()), Some(obj.name.clone())];
        let columns: Vec<ColumnInfo> = self
            .text_rows(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, COLUMN_KEY, COLUMN_COMMENT, EXTRA, DATA_TYPE \
                 FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
                &p,
            )
            .await?
            .iter()
            .map(|r| {
                let extra = cell(r, 6);
                ColumnInfo {
                    name: cell(r, 0),
                    data_type: cell(r, 1),
                    nullable: cell(r, 2) == "YES",
                    default: r.get(3).cloned().flatten().map(|v| default_as_sql(&v, &extra, &cell(r, 7))),
                    primary_key: cell(r, 4) == "PRI",
                    comment: r.get(5).cloned().flatten().filter(|c| !c.is_empty()),
                    extra: Some(extra).filter(|e| !e.is_empty()),
                }
            })
            .collect();
        if columns.is_empty() {
            return Err(DbError::Invalid(format!("table not found: {}", obj.name)));
        }
        let mut indexes: Vec<IndexInfo> = Vec::new();
        for r in self
            .text_rows(
                "SELECT INDEX_NAME, NON_UNIQUE, COALESCE(COLUMN_NAME, '<expr>') FROM information_schema.STATISTICS \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME, SEQ_IN_INDEX",
                &p,
            )
            .await?
        {
            let name = cell(&r, 0);
            match indexes.last_mut() {
                Some(last) if last.name == name => last.columns.push(cell(&r, 2)),
                _ => indexes.push(IndexInfo { primary: name == "PRIMARY", unique: cell(&r, 1) == "0", name, columns: vec![cell(&r, 2)] }),
            }
        }
        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        for r in self
            .text_rows(
                "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, \
                 r.UPDATE_RULE, r.DELETE_RULE FROM information_schema.KEY_COLUMN_USAGE k \
                 JOIN information_schema.REFERENTIAL_CONSTRAINTS r ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA \
                   AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME AND r.TABLE_NAME = k.TABLE_NAME \
                 WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ? AND k.REFERENCED_TABLE_NAME IS NOT NULL \
                 ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
                &p,
            )
            .await?
        {
            let name = cell(&r, 0);
            let ref_table = if cell(&r, 2) == obj.database { cell(&r, 3) } else { format!("{}.{}", cell(&r, 2), cell(&r, 3)) };
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
                    on_update: cell(&r, 5),
                    on_delete: cell(&r, 6),
                }),
            }
        }
        Ok(TableStructure { columns, indexes, foreign_keys })
    }

    async fn ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let sql = format!("SHOW CREATE TABLE {}", D.qualified(obj)?);
        let outcomes = self.execute(&sql, &FetchLimits { max_rows: 1 }).await?;
        match outcomes.into_iter().next() {
            Some(Outcome::Rows(r)) => r
                .rows
                .first()
                .and_then(|row| row.get(1).cloned().flatten())
                .map(|s| s + ";")
                .ok_or_else(|| DbError::Invalid("no DDL returned".into())),
            _ => Err(DbError::Invalid("no DDL returned".into())),
        }
    }

    fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::MySql(MyCancel {
            opts: self.opts.clone(),
            id: self.conn.id(),
            requested: self.cancel_requested.clone(),
        })
    }

    fn current_database(&self) -> String {
        self.database.clone()
    }

    async fn close(self) -> DbResult<()> {
        self.conn.disconnect().await.map_err(map_err)
    }
}
