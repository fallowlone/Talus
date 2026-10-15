//! Snowflake engine (`snowflake-connector-rs` over HTTPS; no ODBC/JDBC bridge, no local warehouse).
//!
//! * One session works on one database (`USE DATABASE`), like PostgreSQL: [`SfSession::connect`]
//!   takes the same `database` override. Unquoted identifiers fold to upper case; names coming from
//!   the catalog are exact and always quoted when used.
//! * Snowflake bills compute per query on the session's warehouse, and metadata reads are queries
//!   too, so catalog listings use single `INFORMATION_SCHEMA` statements.
//! * Cancellation uses the connector's query handle: the Stop button aborts the running query while
//!   the session (and its transaction) survives.
//! * The session pins `AUTOCOMMIT = TRUE` so statements outside an explicit `BEGIN … COMMIT` commit
//!   like on the other engines. A session that may not change the parameter (least privilege on an
//!   account with autocommit off) commits every statement explicitly instead.
//! * Text cells use fixed formats (`YYYY-MM-DD HH24:MI:SS[.ffffff]`, UTC offsets as `±HH:MM`) that
//!   parse back into the same types.

use std::sync::{Arc, Mutex};

use snowflake_connector_rs as sf;

use super::{CancelHandle, EngineSession, hex};
use crate::error::{DbError, DbResult};
use crate::model::{
    ColumnInfo, ColumnMeta, ConnectionConfig, FetchLimits, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectRef, Outcome,
    QueryResult, SnowflakeAuth, TableStructure,
};
use crate::secret::Secret;

/// Snowflake server errors start with the 6-digit code and the SQLSTATE: `001003 (42000): …`.
fn server_code(message: &str) -> Option<String> {
    let code = message.get(..6)?;
    code.bytes().all(|b| b.is_ascii_digit()).then(|| code.to_string())
}

/// … and often name the position: `… syntax error line 1 at position 7 unexpected 'FROM'`.
fn server_position(message: &str) -> Option<usize> {
    let i = message.rfind("position ")?;
    message[i + "position ".len()..].split_whitespace().next()?.parse().ok()
}

fn map_err(e: sf::Error) -> DbError {
    use sf::ErrorKind as K;
    match e.kind() {
        K::Auth => DbError::Connect(e.to_string()),
        K::Config | K::BindEncode => DbError::Invalid(e.to_string()),
        K::Network | K::SessionExpired => DbError::ConnectionLost(e.to_string()),
        K::Timeout => DbError::Io(format!("timed out: {e}")),
        K::Cancelled => DbError::Cancelled,
        K::Server => {
            let m = e.to_string();
            DbError::Server { code: server_code(&m), position: server_position(&m), message: m }
        }
        // Protocol, Decode, Internal, Other — and anything added later (the enum is non-exhaustive).
        _ => DbError::Io(e.to_string()),
    }
}

/// At connect time every transport/server failure is a connection failure (bad warehouse, role or
/// database included: those are connection settings).
fn map_connect_err(e: sf::Error) -> DbError {
    match map_err(e) {
        DbError::ConnectionLost(m) | DbError::Io(m) => DbError::Connect(m),
        DbError::Server { message, code, .. } => {
            DbError::Connect(code.map(|c| format!("{c}: {message}")).unwrap_or(message))
        }
        other => other,
    }
}

/// One cell as text, `None` for NULL.
fn cell_text(v: &sf::CellValue) -> Option<String> {
    use sf::CellValue as C;
    Some(match v {
        C::Null => return None,
        C::Boolean(b) => (if *b { "TRUE" } else { "FALSE" }).to_string(),
        C::Integer(n) => n.to_string(),
        C::Float(f) => f.to_string(),
        C::Decimal(d) => d.to_string(),
        C::String(s) => s.clone(),
        C::Date(d) => d.to_string(),
        C::Time(t) => t.to_string(),
        C::TimestampNtz(t) => t.to_string(),
        C::TimestampLtz(t) => t.format("%Y-%m-%d %H:%M:%S%.f %:z").to_string(),
        C::TimestampTz(t) => t.format("%Y-%m-%d %H:%M:%S%.f %:z").to_string(),
        C::Json(j) => j.to_string(),
        C::Binary(b) => hex(b.as_bytes()),
        // VECTOR is exotic in a grid; Debug is honest. The enum is non-exhaustive, so future
        // types take the same path rather than failing the row.
        v => format!("{v:?}"),
    })
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Passes a database name the way the server folds it: simple names go unquoted (folded to upper
/// case), anything else is quoted verbatim. Returns the SQL fragment and the tracked name.
fn database_ref(name: &str) -> (String, String) {
    let name = name.trim();
    if name.starts_with('"') && name.ends_with('"') && name.len() >= 2 {
        let inner = name[1..name.len() - 1].to_string();
        (quote_ident(&inner), inner)
    } else if !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
        && !name.bytes().next().is_some_and(|b| b.is_ascii_digit())
    {
        (name.to_string(), name.to_ascii_uppercase())
    } else {
        (quote_ident(name), name.to_string())
    }
}

fn keyword_statement(sql: &str) -> Option<&'static str> {
    let norm: Vec<String> =
        sql.trim().trim_end_matches(';').split_whitespace().map(|w| w.to_ascii_uppercase()).collect();
    match norm.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["BEGIN"] | ["BEGIN", "WORK"] | ["BEGIN", "TRANSACTION"] | ["START", "TRANSACTION"] => Some("begin"),
        ["COMMIT"] | ["COMMIT", "WORK"] => Some("commit"),
        ["ROLLBACK"] | ["ROLLBACK", "WORK"] => Some("rollback"),
        _ => None,
    }
}

/// Statements that answer with rows (anything tabular is shown, so unknown ones join them).
fn is_query(sql: &str) -> bool {
    !matches!(
        sql.split_whitespace().next().map(|w| w.to_ascii_uppercase()).as_deref(),
        Some(
            "ALTER"
                | "BEGIN"
                | "COMMIT"
                | "CREATE"
                | "DELETE"
                | "DROP"
                | "GRANT"
                | "INSERT"
                | "MERGE"
                | "REVOKE"
                | "ROLLBACK"
                | "SET"
                | "TRUNCATE"
                | "UNDROP"
                | "UPDATE"
                | "USE"
        )
    )
}

/// `USE [DATABASE] name` (a bare `USE name` means a database in Snowflake); the tracked name is
/// folded the way the server folds it. `USE SCHEMA / WAREHOUSE / ROLE` is not a database switch.
fn use_database_name(sql: &str) -> Option<String> {
    let mut words = sql.trim().trim_end_matches(';').split_whitespace();
    if !words.next().is_some_and(|w| w.eq_ignore_ascii_case("use")) {
        return None;
    }
    let second = words.next()?;
    let name = if second.eq_ignore_ascii_case("database") {
        words.next()?
    } else if second.eq_ignore_ascii_case("schema")
        || second.eq_ignore_ascii_case("warehouse")
        || second.eq_ignore_ascii_case("role")
        || second.eq_ignore_ascii_case("secondary")
    {
        return None;
    } else {
        second
    };
    if words.next().is_some() {
        return None;
    }
    Some(database_ref(name).1)
}

/// Column type as written in `CREATE TABLE`, from the catalog's separate length/precision columns.
fn spell_type(
    data_type: &str,
    char_max: Option<&str>,
    precision: Option<&str>,
    scale: Option<&str>,
    dt_precision: Option<&str>,
) -> String {
    match data_type {
        "TEXT" => char_max.map_or_else(|| "TEXT".into(), |n| format!("VARCHAR({n})")),
        "NUMBER" => match (precision, scale) {
            (Some(p), Some(s)) if s != "0" => format!("NUMBER({p},{s})"),
            (Some(p), _) => format!("NUMBER({p})"),
            _ => "NUMBER".into(),
        },
        "BINARY" => char_max.map_or_else(|| "BINARY".into(), |n| format!("BINARY({n})")),
        "TIME" => dt_precision.map_or_else(|| "TIME".into(), |p| format!("TIME({p})")),
        t if t.starts_with("TIMESTAMP") => dt_precision.map_or_else(|| t.into(), |p| format!("{t}({p})")),
        other => other.to_string(),
    }
}

fn cell(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

/// Cancels the running statement of the session it came from; the session survives.
#[derive(Clone)]
pub struct SfCancel(Arc<Mutex<Option<sf::QueryCanceller>>>);

impl SfCancel {
    pub async fn cancel(&self) -> DbResult<()> {
        let c = self.0.lock().map_err(|_| DbError::ConnectionLost("Snowflake cancel lock poisoned".into()))?.clone();
        match c {
            Some(c) => c.cancel().await.map(|_| ()).map_err(map_err),
            None => Ok(()),
        }
    }
}

pub struct SfSession {
    session: sf::Session,
    current: Arc<Mutex<Option<sf::QueryCanceller>>>,
    database: String,
    in_txn: bool,
    /// The session could not enable autocommit: every statement is committed explicitly.
    emulate_commit: bool,
}

impl SfSession {
    pub async fn connect(cfg: &ConnectionConfig, password: Option<&Secret>, database: Option<&str>) -> DbResult<Self> {
        let sc = cfg.snowflake_config();
        let secret = password.map(|s| s.expose().to_string()).unwrap_or_default();
        let auth = match sc.auth {
            SnowflakeAuth::Password => sf::AuthConfig::password(sf::PasswordConfig::new(secret)),
            SnowflakeAuth::OAuth => sf::AuthConfig::oauth(secret),
            SnowflakeAuth::KeyPair { ref key_file } => {
                let pem = std::fs::read_to_string(key_file)
                    .map_err(|e| DbError::Connect(format!("cannot read private key file {key_file}: {e}")))?;
                sf::AuthConfig::key_pair(sf::KeyPairConfig::from_pem(pem))
            }
        };
        let mut session_cfg = sf::SessionConfig::new();
        if !sc.warehouse.trim().is_empty() {
            session_cfg = session_cfg.with_warehouse(sc.warehouse.trim());
        }
        if !sc.role.trim().is_empty() {
            session_cfg = session_cfg.with_role(sc.role.trim());
        }
        if !sc.schema.trim().is_empty() {
            session_cfg = session_cfg.with_schema(sc.schema.trim());
        }
        let mut client_cfg = sf::ClientConfig::new(cfg.user.trim(), cfg.host.trim(), auth).with_session(session_cfg);
        if !sc.endpoint.trim().is_empty() {
            let url = url::Url::parse(sc.endpoint.trim())
                .map_err(|e| DbError::Connect(format!("invalid Snowflake endpoint: {e}")))?;
            client_cfg = client_cfg.with_endpoint(sf::EndpointConfig::custom_base_url(url));
        }
        let client = sf::Client::new(client_cfg).map_err(map_connect_err)?;
        let session = client.create_session().await.map_err(map_connect_err)?;
        let mut s =
            Self { session, current: Arc::default(), database: String::new(), in_txn: false, emulate_commit: false };
        // Best effort: a least-privilege session may not change parameters; `emulate_commit`
        // (decided below) keeps such sessions correct instead of failing the connect.
        let _ = s.run_plain("ALTER SESSION SET AUTOCOMMIT = TRUE").await;
        let _ = s.run_plain("ALTER SESSION SET QUERY_TAG = 'talus'").await;
        s.emulate_commit = !s.autocommit_on().await;
        let db =
            database.filter(|d| !d.trim().is_empty()).or(Some(cfg.database.as_str()).filter(|d| !d.trim().is_empty()));
        if let Some(db) = db {
            s.use_database(db).await?;
        }
        Ok(s)
    }

    /// Runs one statement with text binds, streaming rows up to the cap.
    async fn run(&mut self, sql: &str, params: &[Option<String>], limits: &FetchLimits) -> DbResult<QueryResult> {
        let mut stmt = sf::Statement::new_positional(sql.to_string());
        for p in params {
            stmt = stmt.bind(p.clone());
        }
        let handle = self.session.query_handle(stmt).map_err(map_err)?;
        *self.current.lock().map_err(|_| DbError::ConnectionLost("Snowflake cancel lock poisoned".into()))? =
            Some(handle.canceller());
        let cursor = handle.execute().await;
        *self.current.lock().map_err(|_| DbError::ConnectionLost("Snowflake cancel lock poisoned".into()))? = None;
        let mut cursor = cursor.map_err(map_err)?;
        let mut out = QueryResult {
            columns: cursor
                .schema()
                .columns()
                .iter()
                .map(|c| ColumnMeta {
                    name: c.name().to_string(),
                    type_name: c.ty().to_string(),
                    binary: matches!(c.ty(), sf::ColumnType::Binary { .. }),
                })
                .collect(),
            ..Default::default()
        };
        while out.rows.len() < limits.max_rows {
            let Some(table) = cursor.next_table().await.map_err(map_err)? else { break };
            for row in table.rows::<sf::DynamicRow>().map_err(map_err)? {
                if out.rows.len() >= limits.max_rows {
                    out.truncated = true;
                    break;
                }
                out.rows.push(row.map_err(map_err)?.values().iter().map(cell_text).collect());
            }
        }
        if !out.truncated && out.rows.len() >= limits.max_rows && !cursor.is_exhausted() {
            out.truncated = true;
        }
        Ok(out)
    }

    async fn run_plain(&mut self, sql: &str) -> DbResult<QueryResult> {
        self.run(sql, &[], &FetchLimits { max_rows: 10_000 }).await
    }

    async fn text_rows(&mut self, sql: &str, params: &[Option<String>]) -> DbResult<Vec<Vec<Option<String>>>> {
        Ok(self.run(sql, params, &FetchLimits { max_rows: usize::MAX }).await?.rows)
    }

    /// `SHOW …` output reduced to the named columns (matched case-insensitively).
    fn show_cols(result: &QueryResult, names: &[&str]) -> Vec<Vec<String>> {
        let idx: Vec<Option<usize>> =
            names.iter().map(|w| result.columns.iter().position(|c| c.name.eq_ignore_ascii_case(w))).collect();
        result
            .rows
            .iter()
            .map(|r| {
                idx.iter()
                    .map(|i| i.map(|i| r.get(i).cloned().flatten().unwrap_or_default()).unwrap_or_default())
                    .collect()
            })
            .collect()
    }

    async fn autocommit_on(&mut self) -> bool {
        match self.run_plain("SHOW PARAMETERS LIKE 'AUTOCOMMIT' IN SESSION").await {
            Ok(r) => Self::show_cols(&r, &["value"]).first().is_some_and(|row| {
                row.first().is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "on" | "1"))
            }),
            Err(_) => true,
        }
    }

    async fn use_database(&mut self, db: &str) -> DbResult<()> {
        let (fragment, tracked) = database_ref(db);
        self.run_plain(&format!("USE DATABASE {fragment}")).await?;
        self.database = tracked;
        Ok(())
    }

    async fn current_schema(&mut self) -> DbResult<String> {
        let s = self
            .text_rows("SELECT CURRENT_SCHEMA()", &[])
            .await?
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .unwrap_or_default();
        if s.is_empty() { Err(DbError::Invalid("no current schema".into())) } else { Ok(s) }
    }
}

impl EngineSession for SfSession {
    async fn server_version(&mut self) -> DbResult<String> {
        let v = self
            .text_rows("SELECT CURRENT_VERSION()", &[])
            .await?
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .unwrap_or_default();
        Ok(if v.is_empty() { "Snowflake".into() } else { format!("Snowflake {v}") })
    }

    async fn execute(&mut self, sql: &str, limits: &FetchLimits) -> DbResult<Vec<Outcome>> {
        let sql = sql.trim().trim_end_matches(';').trim().to_string();
        if sql.is_empty() {
            return Ok(vec![Outcome::Affected(0)]);
        }
        if let Some(kw) = keyword_statement(&sql) {
            self.run_plain(&sql).await?;
            self.in_txn = kw == "begin";
            return Ok(vec![Outcome::Affected(0)]);
        }
        if let Some(db) = use_database_name(&sql) {
            let r = self.run_plain(&sql).await?;
            self.database = db;
            return Ok(vec![Outcome::Rows(r)]);
        }
        if is_query(&sql) {
            return Ok(vec![Outcome::Rows(self.run(&sql, &[], limits).await?)]);
        }
        let r = self.run(&sql, &[], &FetchLimits { max_rows: 100 }).await?;
        if self.emulate_commit && !self.in_txn {
            self.run_plain("COMMIT").await?;
        }
        // DML answers with the affected count as its only cell ("number of rows updated").
        if r.rows.len() == 1
            && r.columns.len() == 1
            && let Some(n) = r.rows[0].first().and_then(|c| c.as_deref()).and_then(|t| t.trim().parse::<i64>().ok())
        {
            return Ok(vec![Outcome::Affected(n.max(0) as u64)]);
        }
        if r.rows.is_empty() {
            return Ok(vec![Outcome::Affected(0)]);
        }
        Ok(vec![Outcome::Rows(r)])
    }

    async fn query_params(
        &mut self,
        sql: &str,
        params: &[Option<String>],
        limits: &FetchLimits,
    ) -> DbResult<QueryResult> {
        let sql = sql.trim().trim_end_matches(';').trim().to_string();
        self.run(&sql, params, limits).await
    }

    async fn exec_params(&mut self, sql: &str, params: &[Option<String>]) -> DbResult<u64> {
        let sql = sql.trim().trim_end_matches(';').trim().to_string();
        let r = self.run(&sql, params, &FetchLimits { max_rows: 2 }).await?;
        if self.emulate_commit && !self.in_txn {
            self.run_plain("COMMIT").await?;
        }
        r.rows
            .first()
            .and_then(|row| row.first().cloned().flatten())
            .and_then(|t| t.trim().parse::<i64>().ok())
            .map(|n| n.max(0) as u64)
            .ok_or_else(|| {
                DbError::Invalid(format!(
                    "unexpected result for {}: {:?}",
                    sql.split_whitespace().next().unwrap_or("statement"),
                    r.rows
                ))
            })
    }

    async fn list_databases(&mut self) -> DbResult<Vec<String>> {
        let r = self.run_plain("SHOW DATABASES").await?;
        Ok(Self::show_cols(&r, &["name"])
            .into_iter()
            .filter_map(|row| row.into_iter().next().filter(|n| !n.is_empty()))
            .collect())
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<String>> {
        if self.database.is_empty() {
            return Ok(Vec::new());
        }
        let db = self.database.clone();
        let r = self.run_plain(&format!("SHOW SCHEMAS IN DATABASE {}", quote_ident(&db))).await?;
        Ok(Self::show_cols(&r, &["name"])
            .into_iter()
            .filter_map(|row| row.into_iter().next().filter(|n| !n.is_empty()))
            .collect())
    }

    async fn list_objects(&mut self, database: &str, schema: Option<&str>) -> DbResult<Vec<ObjectRef>> {
        let from = format!("{}.INFORMATION_SCHEMA.TABLES", quote_ident(database));
        let (sql, params) = match schema {
            Some(s) => (
                format!("SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM {from} WHERE TABLE_SCHEMA = ? ORDER BY 2"),
                vec![Some(s.to_string())],
            ),
            None => (format!("SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM {from} ORDER BY 1, 2"), Vec::new()),
        };
        Ok(self
            .text_rows(&sql, &params)
            .await?
            .iter()
            .map(|r| ObjectRef {
                database: database.to_string(),
                schema: Some(cell(r, 0)),
                name: cell(r, 1),
                kind: if cell(r, 2) == "BASE TABLE" { ObjectKind::Table } else { ObjectKind::View },
            })
            .collect())
    }

    async fn table_structure(&mut self, obj: &ObjectRef) -> DbResult<TableStructure> {
        let schema = match obj.schema.clone() {
            Some(s) => s,
            None => self.current_schema().await?,
        };
        let db = quote_ident(&obj.database);
        let p = [Some(schema.clone()), Some(obj.name.clone())];
        let pk_cols: Vec<String> = self
            .text_rows(
                &format!(
                    "SELECT kcu.COLUMN_NAME FROM {db}.INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc \
                     JOIN {db}.INFORMATION_SCHEMA.KEY_COLUMN_USAGE kcu \
                       ON kcu.CONSTRAINT_CATALOG = tc.CONSTRAINT_CATALOG \
                       AND kcu.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                       AND kcu.CONSTRAINT_NAME = tc.CONSTRAINT_NAME \
                     WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? AND tc.CONSTRAINT_TYPE = 'PRIMARY KEY' \
                     ORDER BY kcu.ORDINAL_POSITION"
                ),
                &p,
            )
            .await?
            .iter()
            .map(|r| cell(r, 0))
            .collect();
        let pk_name = self
            .text_rows(
                &format!(
                    "SELECT CONSTRAINT_NAME FROM {db}.INFORMATION_SCHEMA.TABLE_CONSTRAINTS \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND CONSTRAINT_TYPE = 'PRIMARY KEY'"
                ),
                &p,
            )
            .await?
            .first()
            .map(|r| cell(r, 0));
        let columns: Vec<ColumnInfo> = self
            .text_rows(
                &format!(
                    "SELECT COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, NUMERIC_SCALE, \
                            DATETIME_PRECISION, IS_NULLABLE, COLUMN_DEFAULT, COMMENT \
                     FROM {db}.INFORMATION_SCHEMA.COLUMNS \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION"
                ),
                &p,
            )
            .await?
            .iter()
            .map(|r| {
                let default = r.get(7).cloned().flatten().map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
                // `AUTOINCREMENT(…)` / `IDENTITY(…)` show up as the default; the grid offers no
                // default to edit for them.
                let identity = default.as_deref().is_some_and(|d| {
                    let u = d.to_ascii_uppercase();
                    u.contains("AUTOINCREMENT") || u.contains("IDENTITY")
                });
                ColumnInfo {
                    primary_key: pk_cols.contains(&cell(r, 0)),
                    data_type: spell_type(
                        &cell(r, 1),
                        r.get(2).cloned().flatten().as_deref(),
                        r.get(3).cloned().flatten().as_deref(),
                        r.get(4).cloned().flatten().as_deref(),
                        r.get(5).cloned().flatten().as_deref(),
                    ),
                    nullable: cell(r, 6) == "YES",
                    default: if identity { None } else { default },
                    comment: r.get(8).cloned().flatten().filter(|c| !c.is_empty()),
                    extra: if identity { Some("AUTOINCREMENT".into()) } else { None },
                    name: cell(r, 0),
                }
            })
            .collect();
        if columns.is_empty() {
            return Err(DbError::Invalid(format!("table not found: {}", obj.name)));
        }
        // Snowflake has no secondary indexes; primary keys and unique constraints are the whole list.
        let mut indexes: Vec<IndexInfo> = Vec::new();
        if !pk_cols.is_empty() {
            indexes.push(IndexInfo {
                primary: true,
                unique: true,
                name: pk_name.clone().unwrap_or_else(|| format!("pk_{}", obj.name)),
                columns: pk_cols,
            });
        }
        for r in self
            .text_rows(
                &format!(
                    "SELECT tc.CONSTRAINT_NAME, kcu.COLUMN_NAME \
                     FROM {db}.INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc \
                     JOIN {db}.INFORMATION_SCHEMA.KEY_COLUMN_USAGE kcu \
                       ON kcu.CONSTRAINT_CATALOG = tc.CONSTRAINT_CATALOG \
                       AND kcu.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                       AND kcu.CONSTRAINT_NAME = tc.CONSTRAINT_NAME \
                     WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? AND tc.CONSTRAINT_TYPE = 'UNIQUE' \
                     ORDER BY tc.CONSTRAINT_NAME, kcu.ORDINAL_POSITION"
                ),
                &p,
            )
            .await?
        {
            let name = cell(&r, 0);
            match indexes.last_mut() {
                Some(last) if last.name == name && !last.primary => last.columns.push(cell(&r, 1)),
                _ => indexes.push(IndexInfo { primary: false, unique: true, name, columns: vec![cell(&r, 1)] }),
            }
        }
        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        for r in self
            .text_rows(
                &format!(
                    "SELECT tc.CONSTRAINT_NAME, kcu.COLUMN_NAME, kcu2.TABLE_SCHEMA, kcu2.TABLE_NAME, \
                            kcu2.COLUMN_NAME, rc.DELETE_RULE, rc.UPDATE_RULE \
                     FROM {db}.INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc \
                     JOIN {db}.INFORMATION_SCHEMA.KEY_COLUMN_USAGE kcu \
                       ON kcu.CONSTRAINT_CATALOG = tc.CONSTRAINT_CATALOG \
                       AND kcu.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                       AND kcu.CONSTRAINT_NAME = tc.CONSTRAINT_NAME \
                     JOIN {db}.INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS rc \
                       ON rc.CONSTRAINT_CATALOG = tc.CONSTRAINT_CATALOG \
                       AND rc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                       AND rc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME \
                     JOIN {db}.INFORMATION_SCHEMA.KEY_COLUMN_USAGE kcu2 \
                       ON kcu2.CONSTRAINT_CATALOG = rc.UNIQUE_CONSTRAINT_CATALOG \
                       AND kcu2.CONSTRAINT_SCHEMA = rc.UNIQUE_CONSTRAINT_SCHEMA \
                       AND kcu2.CONSTRAINT_NAME = rc.UNIQUE_CONSTRAINT_NAME \
                       AND kcu2.ORDINAL_POSITION = kcu.ORDINAL_POSITION \
                     WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? AND tc.CONSTRAINT_TYPE = 'FOREIGN KEY' \
                     ORDER BY tc.CONSTRAINT_NAME, kcu.ORDINAL_POSITION"
                ),
                &p,
            )
            .await?
        {
            let name = cell(&r, 0);
            let ref_table =
                if cell(&r, 2) == schema { cell(&r, 3) } else { format!("{}.{}", cell(&r, 2), cell(&r, 3)) };
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
                    on_update: cell(&r, 6),
                    on_delete: cell(&r, 5),
                }),
            }
        }
        Ok(TableStructure { columns, indexes, foreign_keys })
    }

    async fn ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let schema = match obj.schema.clone() {
            Some(s) => s,
            None => self.current_schema().await?,
        };
        let kind = if obj.kind == ObjectKind::View { "VIEW" } else { "TABLE" };
        let full = format!("{}.{}.{}", quote_ident(&obj.database), quote_ident(&schema), quote_ident(&obj.name));
        // Bound, never interpolated: an object name may contain a single quote.
        let rows = self.text_rows("SELECT GET_DDL(?, ?)", &[Some(kind.to_string()), Some(full)]).await?;
        rows.first()
            .and_then(|r| r.first().cloned().flatten())
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .ok_or_else(|| DbError::Invalid(format!("no DDL for {}", obj.name)))
    }

    fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::Snowflake(SfCancel(self.current.clone()))
    }

    fn current_database(&self) -> String {
        self.database.clone()
    }

    async fn close(self) -> DbResult<()> {
        // The connector exposes no session close; server-side sessions expire on their own.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_keywords_are_recognised() {
        assert_eq!(keyword_statement("BEGIN"), Some("begin"));
        assert_eq!(keyword_statement("begin work;"), Some("begin"));
        assert_eq!(keyword_statement("START TRANSACTION"), Some("begin"));
        assert_eq!(keyword_statement("COMMIT;"), Some("commit"));
        assert_eq!(keyword_statement(" rollback work "), Some("rollback"));
        assert_eq!(keyword_statement("rollback to savepoint a"), None);
        assert_eq!(keyword_statement("select 1"), None);
    }

    #[test]
    fn queries_and_commands_are_told_apart() {
        for q in [
            "SELECT 1",
            "  with x as (select 1) select * from x",
            "SHOW TABLES",
            "DESCRIBE TABLE t",
            "DESC t",
            "EXPLAIN SELECT 1",
            "VALUES (1), (2)",
            "CALL p()",
            "LIST @stage",
            "SELECT GET_DDL('TABLE', 'x')",
        ] {
            assert!(is_query(q), "{q}");
        }
        for c in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET a = 1",
            "CREATE TABLE t (a INT)",
            "ALTER TABLE t ADD COLUMN b TEXT",
            "DROP TABLE t",
            "TRUNCATE TABLE t",
            "GRANT SELECT ON t TO r",
            "USE DATABASE d",
            "BEGIN",
            "COMMIT",
        ] {
            assert!(!is_query(c), "{c}");
        }
    }

    #[test]
    fn use_database_names_fold_like_the_server() {
        assert_eq!(use_database_name("USE DATABASE mydb"), Some("MYDB".into()));
        assert_eq!(use_database_name("use mydb;"), Some("MYDB".into()));
        assert_eq!(use_database_name("USE \"lowerdb\""), Some("lowerdb".into()));
        assert_eq!(use_database_name("USE DATABASE \"lowerdb\""), Some("lowerdb".into()));
        assert_eq!(use_database_name("USE SCHEMA s"), None);
        assert_eq!(use_database_name("USE WAREHOUSE w"), None);
        assert_eq!(use_database_name("USE ROLE r"), None);
        assert_eq!(use_database_name("SELECT 1"), None);
    }

    #[test]
    fn database_refs_quote_only_what_needs_it() {
        assert_eq!(database_ref("mydb"), ("mydb".into(), "MYDB".into()));
        assert_eq!(database_ref("\"lowerdb\""), ("\"lowerdb\"".into(), "lowerdb".into()));
        assert_eq!(database_ref("my-db"), ("\"my-db\"".into(), "my-db".into()));
        assert_eq!(database_ref("9lives"), ("\"9lives\"".into(), "9lives".into()));
    }

    #[test]
    fn catalog_types_are_spelled_like_create_table() {
        assert_eq!(spell_type("TEXT", Some("16"), None, None, None), "VARCHAR(16)");
        assert_eq!(spell_type("TEXT", None, None, None, None), "TEXT");
        assert_eq!(spell_type("NUMBER", None, Some("10"), Some("2"), None), "NUMBER(10,2)");
        assert_eq!(spell_type("NUMBER", None, Some("38"), Some("0"), None), "NUMBER(38)");
        assert_eq!(spell_type("NUMBER", None, None, None, None), "NUMBER");
        assert_eq!(spell_type("FLOAT", None, None, None, None), "FLOAT");
        assert_eq!(spell_type("BOOLEAN", None, None, None, None), "BOOLEAN");
        assert_eq!(spell_type("DATE", None, None, None, None), "DATE");
        assert_eq!(spell_type("TIME", None, None, None, Some("3")), "TIME(3)");
        assert_eq!(spell_type("TIMESTAMP_NTZ", None, None, None, Some("9")), "TIMESTAMP_NTZ(9)");
        assert_eq!(spell_type("TIMESTAMP_LTZ", None, None, None, None), "TIMESTAMP_LTZ");
        assert_eq!(spell_type("BINARY", Some("64"), None, None, None), "BINARY(64)");
        assert_eq!(spell_type("VARIANT", None, None, None, None), "VARIANT");
    }

    #[test]
    fn server_codes_and_positions_are_read_from_messages() {
        assert_eq!(server_code("001003 (42000): SQL compilation error"), Some("001003".into()));
        assert_eq!(server_code("connection refused"), None);
        assert_eq!(server_position("001003 (42000): syntax error line 1 at position 7 unexpected 'FROM'"), Some(7));
        assert_eq!(server_position("001003 (42000): SQL compilation error"), None);
    }

    #[test]
    fn cells_render_as_text_and_parse_back() {
        use sf::CellValue as C;
        assert_eq!(cell_text(&C::Null), None);
        assert_eq!(cell_text(&C::Boolean(true)).as_deref(), Some("TRUE"));
        assert_eq!(cell_text(&C::Boolean(false)).as_deref(), Some("FALSE"));
        assert_eq!(cell_text(&C::Integer(-42)).as_deref(), Some("-42"));
        assert_eq!(cell_text(&C::Float(1.5)).as_deref(), Some("1.5"));
        assert_eq!(cell_text(&C::String("x".into())).as_deref(), Some("x"));
        assert_eq!(
            cell_text(&C::Date(chrono::NaiveDate::from_ymd_opt(2026, 3, 4).unwrap())).as_deref(),
            Some("2026-03-04")
        );
        assert_eq!(
            cell_text(&C::TimestampNtz(
                chrono::NaiveDate::from_ymd_opt(2026, 3, 4).unwrap().and_hms_opt(5, 6, 7).unwrap()
            ))
            .as_deref(),
            Some("2026-03-04 05:06:07")
        );
        assert_eq!(cell_text(&C::Json(serde_json::json!({"a": [1]}))).as_deref(), Some(r#"{"a":[1]}"#));
    }
}
