//! SQL Server engine (tiberius / TDS). A session is bound to one database (like PostgreSQL), and
//! statements run as plain SQL batches so `USE`, `BEGIN TRANSACTION` and temp tables behave as in
//! any client. Cancelling sends a TDS Attention to the running request.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

use futures_util::TryStreamExt;
use tiberius::{AuthMethod, Client, ColumnData, ColumnType, Config, EncryptionLevel, FromSql, Query, QueryItem};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::{CancelHandle, EngineSession, hex};
use crate::error::{DbError, DbResult};
use crate::model::{
    ColumnInfo, ColumnMeta, ConnectionConfig, FetchLimits, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectRef, Outcome,
    QueryResult, SslMode, TableStructure,
};
use crate::secret::Secret;
use crate::sql::Dialect;
use crate::sql::constraints::{FkDef, IndexDef, create_index, fk_clause};
use crate::sql::ddl::{ColumnDef, create_table_with};

const D: Dialect = Dialect(crate::model::Engine::SqlServer);

type SqlClient = Client<Compat<TcpStream>>;

/// State shared between a session and the handles that cancel it.
struct CancelShared {
    /// `@@SPID` of the live connection (changes when the session reconnects after a cancel).
    spid: AtomicI32,
    /// A KILL was issued: the connection is (about to be) gone and must be replaced.
    requested: AtomicBool,
    /// Held for the whole KILL so the session never reconnects half-way through one.
    kill_lock: tokio::sync::Mutex<()>,
    config: Config,
    timeout: Duration,
}

/// Cancels the running statement of the session it came from. SQL Server's Attention signal is not
/// reliable through the driver, so the session is ended with `KILL` from a side connection and the
/// session transparently reconnects (an open transaction is lost with it).
#[derive(Clone)]
pub struct MsCancel(Arc<CancelShared>);

impl MsCancel {
    pub async fn cancel(&self) -> DbResult<()> {
        let _guard = self.0.kill_lock.lock().await;
        self.0.requested.store(true, Ordering::SeqCst);
        let mut side = open_client(&self.0.config, self.0.timeout).await?;
        let spid = self.0.spid.load(Ordering::SeqCst);
        let r = side.simple_query(format!("KILL {spid}")).await.map_err(map_err);
        match r {
            Ok(s) => s.into_results().await.map(|_| ()).map_err(map_err),
            Err(e) => Err(e),
        }
        .map_err(|e| DbError::Io(format!("KILL failed: {e}")))
    }
}

pub struct MsSession {
    client: SqlClient,
    database: String,
    shared: Arc<CancelShared>,
}

async fn open_client(config: &Config, timeout: Duration) -> DbResult<SqlClient> {
    let connect = async {
        let tcp = TcpStream::connect(config.get_addr()).await.map_err(|e| DbError::Connect(e.to_string()))?;
        tcp.set_nodelay(true).ok();
        Client::connect(config.clone(), tcp.compat_write()).await.map_err(|e| match map_err(e) {
            DbError::Server { message, .. } => DbError::Connect(message),
            other => DbError::Connect(other.to_string()),
        })
    };
    tokio::time::timeout(timeout, connect)
        .await
        .map_err(|_| DbError::Connect(format!("timed out after {}s", timeout.as_secs())))?
}

async fn spid_of(client: &mut SqlClient) -> DbResult<i32> {
    let row = client
        .simple_query("SELECT CAST(@@SPID AS int)")
        .await
        .map_err(map_err)?
        .into_row()
        .await
        .map_err(map_err)?
        .ok_or_else(|| DbError::Connect("the server did not report a session id".into()))?;
    row.get::<i32, _>(0).ok_or_else(|| DbError::Connect("the server did not report a session id".into()))
}

fn map_err(e: tiberius::error::Error) -> DbError {
    use tiberius::error::Error as E;
    match e {
        // The session was ended with KILL (from the monitor or another client).
        E::Server(t) if t.code() == 596 => DbError::ConnectionLost(t.message().to_string()),
        E::Server(t) => DbError::Server {
            message: t.message().to_string(),
            code: Some(format!("{} (state {})", t.code(), t.state())),
            position: None,
        },
        E::Io { message, .. } => DbError::ConnectionLost(message),
        E::Tls(m) => DbError::Connect(format!("TLS: {m}")),
        other => DbError::server(other.to_string()),
    }
}

fn type_name(t: ColumnType) -> String {
    match t {
        ColumnType::Bit | ColumnType::Bitn => "bit".into(),
        ColumnType::Int1 => "tinyint".into(),
        ColumnType::Int2 => "smallint".into(),
        ColumnType::Int4 | ColumnType::Intn => "int".into(),
        ColumnType::Int8 => "bigint".into(),
        ColumnType::Float4 => "real".into(),
        ColumnType::Float8 | ColumnType::Floatn => "float".into(),
        ColumnType::Money | ColumnType::Money4 => "money".into(),
        ColumnType::Datetime | ColumnType::Datetimen => "datetime".into(),
        ColumnType::Datetime4 => "smalldatetime".into(),
        ColumnType::Guid => "uniqueidentifier".into(),
        ColumnType::Decimaln | ColumnType::Numericn => "numeric".into(),
        ColumnType::BigVarChar => "varchar".into(),
        ColumnType::BigChar => "char".into(),
        ColumnType::NVarchar => "nvarchar".into(),
        ColumnType::NChar => "nchar".into(),
        ColumnType::BigVarBin => "varbinary".into(),
        ColumnType::BigBinary => "binary".into(),
        ColumnType::Daten => "date".into(),
        ColumnType::Timen => "time".into(),
        ColumnType::Datetime2 => "datetime2".into(),
        ColumnType::DatetimeOffsetn => "datetimeoffset".into(),
        other => format!("{other:?}").to_lowercase(),
    }
}

fn is_binary(t: ColumnType) -> bool {
    matches!(t, ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image)
}

/// Text form of one value, as the grid and exports show it.
fn cell_text(d: &ColumnData<'static>) -> Option<String> {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
    match d {
        ColumnData::U8(v) => v.map(|v| v.to_string()),
        ColumnData::I16(v) => v.map(|v| v.to_string()),
        ColumnData::I32(v) => v.map(|v| v.to_string()),
        ColumnData::I64(v) => v.map(|v| v.to_string()),
        ColumnData::F32(v) => v.map(|v| v.to_string()),
        ColumnData::F64(v) => v.map(|v| v.to_string()),
        ColumnData::Bit(v) => v.map(|b| if b { "1" } else { "0" }.to_string()),
        ColumnData::String(v) => v.as_ref().map(|s| s.to_string()),
        ColumnData::Guid(v) => v.map(|g| g.to_string().to_uppercase()),
        ColumnData::Binary(v) => v.as_ref().map(|b| hex(b)),
        ColumnData::Numeric(v) => v.map(|n| n.to_string()),
        ColumnData::Xml(v) => v.as_ref().map(|x| x.to_string()),
        ColumnData::Date(_) => NaiveDate::from_sql(d).ok().flatten().map(|v| v.format("%Y-%m-%d").to_string()),
        ColumnData::Time(_) => NaiveTime::from_sql(d).ok().flatten().map(|v| v.format("%H:%M:%S%.f").to_string()),
        ColumnData::DateTime(_) | ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => {
            NaiveDateTime::from_sql(d).ok().flatten().map(|v| v.format("%Y-%m-%d %H:%M:%S%.f").to_string())
        }
        ColumnData::DateTimeOffset(_) => DateTime::<FixedOffset>::from_sql(d)
            .ok()
            .flatten()
            .map(|v| v.format("%Y-%m-%d %H:%M:%S%.f %:z").to_string()),
    }
}

/// `((0))` / `('x')` → `0` / `'x'`: the catalog wraps stored defaults in redundant parentheses.
fn strip_default_parens(def: &str) -> String {
    let mut s = def.trim();
    loop {
        let bytes = s.as_bytes();
        if bytes.len() < 2 || bytes[0] != b'(' || bytes[bytes.len() - 1] != b')' {
            return s.to_string();
        }
        // The outer pair must wrap the whole text, not just `(a) + (b)`.
        let mut depth = 0i32;
        let mut in_str = false;
        let mut closes_early = false;
        for (i, c) in s.char_indices() {
            match c {
                '\'' => in_str = !in_str,
                '(' if !in_str => depth += 1,
                ')' if !in_str => {
                    depth -= 1;
                    if depth == 0 && i != s.len() - 1 {
                        closes_early = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        if closes_early {
            return s.to_string();
        }
        s = s[1..s.len() - 1].trim();
    }
}

fn cell(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn flag(row: &[Option<String>], i: usize) -> bool {
    matches!(cell(row, i).as_str(), "1" | "true")
}

/// `[schema].[name]` as a string for `OBJECT_ID`.
fn object_id_arg(obj: &ObjectRef) -> DbResult<String> {
    D.qualified(obj)
}

impl MsSession {
    pub async fn connect(cfg: &ConnectionConfig, password: Option<&Secret>, database: Option<&str>) -> DbResult<Self> {
        let db = database
            .filter(|d| !d.is_empty())
            .unwrap_or(if cfg.database.is_empty() { "master" } else { &cfg.database })
            .to_string();
        let mut c = Config::new();
        c.host(&cfg.host);
        c.port(cfg.port);
        c.database(&db);
        c.application_name("Talus");
        c.authentication(AuthMethod::sql_server(&cfg.user, password.map_or("", |p| p.expose())));
        match cfg.ssl_mode {
            SslMode::Disable => {
                // The login packet is always TLS-wrapped; without trust the handshake needs a valid chain.
                c.encryption(EncryptionLevel::Off);
                c.trust_cert();
            }
            SslMode::Prefer => {
                c.encryption(EncryptionLevel::On);
                c.trust_cert();
            }
            SslMode::Require => {
                c.encryption(EncryptionLevel::Required);
                c.trust_cert();
            }
            SslMode::VerifyFull => {
                c.encryption(EncryptionLevel::Required);
                if !cfg.ssl_ca_file.is_empty() {
                    c.trust_cert_ca(&cfg.ssl_ca_file);
                }
            }
        }
        let timeout = Duration::from_secs(cfg.connect_timeout_secs.max(1));
        let mut client = open_client(&c, timeout).await?;
        let spid = spid_of(&mut client).await?;
        let shared = Arc::new(CancelShared {
            spid: AtomicI32::new(spid),
            requested: AtomicBool::new(false),
            kill_lock: tokio::sync::Mutex::new(()),
            config: c,
            timeout,
        });
        Ok(Self { client, database: db, shared })
    }

    /// Runs `op`. When a cancel (KILL) hit this session meanwhile, the connection is replaced and
    /// a failed statement is reported as `Cancelled`.
    async fn cancellable<T>(&mut self, op: impl AsyncFnOnce(&mut SqlClient) -> DbResult<T>) -> DbResult<T> {
        let result = op(&mut self.client).await;
        if self.shared.requested.load(Ordering::SeqCst) {
            // Wait for the KILL to finish before reusing the session id space.
            let _guard = self.shared.kill_lock.lock().await;
            self.shared.requested.store(false, Ordering::SeqCst);
            let mut client = open_client(&self.shared.config, self.shared.timeout).await?;
            self.shared.spid.store(spid_of(&mut client).await?, Ordering::SeqCst);
            self.client = client;
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
}

/// Reads every result set of a stream, keeping at most `limits.max_rows` rows per set.
async fn read_sets(mut stream: tiberius::QueryStream<'_>, limits: &FetchLimits) -> DbResult<Vec<QueryResult>> {
    let mut sets: Vec<QueryResult> = Vec::new();
    while let Some(item) = stream.try_next().await.map_err(map_err)? {
        match item {
            QueryItem::Metadata(m) => sets.push(QueryResult {
                columns: m
                    .columns()
                    .iter()
                    .map(|c| ColumnMeta {
                        name: c.name().to_string(),
                        type_name: type_name(c.column_type()),
                        binary: is_binary(c.column_type()),
                    })
                    .collect(),
                ..Default::default()
            }),
            QueryItem::Row(r) => {
                let Some(set) = sets.last_mut() else { continue };
                if set.rows.len() >= limits.max_rows {
                    set.truncated = true;
                    continue;
                }
                set.rows.push(r.cells().map(|(_, d)| cell_text(d)).collect());
            }
        }
    }
    Ok(sets)
}

fn bound<'a>(sql: &'a str, params: &[Option<String>]) -> Query<'a> {
    let mut q = Query::new(sql);
    for p in params {
        q.bind(p.clone());
    }
    q
}

impl EngineSession for MsSession {
    async fn server_version(&mut self) -> DbResult<String> {
        let r = self
            .text_rows(
                "SELECT CAST(SERVERPROPERTY('ProductVersion') AS varchar(64)) + ' ' + CAST(SERVERPROPERTY('Edition') AS varchar(128))",
                &[],
            )
            .await?;
        Ok(format!("SQL Server {}", r.first().map(|r| cell(r, 0)).unwrap_or_default()))
    }

    async fn execute(&mut self, sql: &str, limits: &FetchLimits) -> DbResult<Vec<Outcome>> {
        let limits = *limits;
        let sets = self
            .cancellable(async |c| {
                let stream = c.simple_query(sql).await.map_err(map_err)?;
                read_sets(stream, &limits).await
            })
            .await?;
        if !sets.is_empty() {
            return Ok(sets.into_iter().map(Outcome::Rows).collect());
        }
        // No result set: report how many rows the statement touched (0 for DDL).
        let n = self
            .cancellable(async |c| {
                let stream = c.simple_query("SELECT @@ROWCOUNT").await.map_err(map_err)?;
                read_sets(stream, &FetchLimits { max_rows: 1 }).await
            })
            .await?;
        let affected = n
            .first()
            .and_then(|s| s.rows.first())
            .and_then(|r| r.first().cloned().flatten())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        Ok(vec![Outcome::Affected(affected)])
    }

    async fn query_params(
        &mut self,
        sql: &str,
        params: &[Option<String>],
        limits: &FetchLimits,
    ) -> DbResult<QueryResult> {
        let limits = *limits;
        let sets = self
            .cancellable(async |c| {
                let stream = bound(sql, params).query(c).await.map_err(map_err)?;
                read_sets(stream, &limits).await
            })
            .await?;
        Ok(sets.into_iter().next().unwrap_or_default())
    }

    async fn exec_params(&mut self, sql: &str, params: &[Option<String>]) -> DbResult<u64> {
        self.cancellable(async |c| Ok(bound(sql, params).execute(c).await.map_err(map_err)?.total())).await
    }

    async fn list_databases(&mut self) -> DbResult<Vec<String>> {
        let rows = self
            .text_rows("SELECT name FROM sys.databases WHERE state = 0 AND HAS_DBACCESS(name) = 1 ORDER BY name", &[])
            .await?;
        Ok(rows.iter().map(|r| cell(r, 0)).collect())
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<String>> {
        let rows = self
            .text_rows(
                "SELECT name FROM sys.schemas WHERE schema_id < 16384 AND name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest') ORDER BY name",
                &[],
            )
            .await?;
        Ok(rows.iter().map(|r| cell(r, 0)).collect())
    }

    async fn list_objects(&mut self, database: &str, schema: Option<&str>) -> DbResult<Vec<ObjectRef>> {
        let schema = schema.unwrap_or("dbo").to_string();
        let rows = self
            .text_rows(
                "SELECT o.name, o.type FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
                 WHERE s.name = @P1 AND o.type IN ('U', 'V') AND o.is_ms_shipped = 0 ORDER BY o.name",
                &[Some(schema.clone())],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| ObjectRef {
                database: database.to_string(),
                schema: Some(schema.clone()),
                name: cell(r, 0),
                kind: if cell(r, 1).trim() == "V" { ObjectKind::View } else { ObjectKind::Table },
            })
            .collect())
    }

    async fn table_structure(&mut self, obj: &ObjectRef) -> DbResult<TableStructure> {
        let id = Some(object_id_arg(obj)?);
        let cols = self
            .text_rows(
                "SELECT c.name, \
                   CASE WHEN t.name IN ('varchar', 'char', 'varbinary', 'binary') \
                          THEN t.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length AS varchar(10)) END + ')' \
                        WHEN t.name IN ('nvarchar', 'nchar') \
                          THEN t.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length / 2 AS varchar(10)) END + ')' \
                        WHEN t.name IN ('decimal', 'numeric') \
                          THEN t.name + '(' + CAST(c.precision AS varchar(10)) + ',' + CAST(c.scale AS varchar(10)) + ')' \
                        WHEN t.name IN ('datetime2', 'time', 'datetimeoffset') \
                          THEN t.name + '(' + CAST(c.scale AS varchar(10)) + ')' \
                        ELSE t.name END, \
                   c.is_nullable, dc.definition, c.is_identity, c.is_computed, \
                   CASE WHEN EXISTS (SELECT 1 FROM sys.index_columns ic JOIN sys.indexes i \
                        ON i.object_id = ic.object_id AND i.index_id = ic.index_id \
                        WHERE i.is_primary_key = 1 AND ic.object_id = c.object_id AND ic.column_id = c.column_id) THEN 1 ELSE 0 END, \
                   CAST(ep.value AS nvarchar(4000)) \
                 FROM sys.columns c JOIN sys.types t ON t.user_type_id = c.user_type_id \
                 LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
                 LEFT JOIN sys.extended_properties ep ON ep.major_id = c.object_id AND ep.minor_id = c.column_id \
                      AND ep.class = 1 AND ep.name = 'MS_Description' \
                 WHERE c.object_id = OBJECT_ID(@P1) ORDER BY c.column_id",
                std::slice::from_ref(&id),
            )
            .await?;
        if cols.is_empty() {
            return Err(DbError::Invalid(format!("object not found: {}", obj.display_name())));
        }
        let columns: Vec<ColumnInfo> = cols
            .iter()
            .map(|r| {
                let extra = if flag(r, 4) {
                    Some("IDENTITY(1,1)".to_string())
                } else if flag(r, 5) {
                    Some("COMPUTED".to_string())
                } else {
                    None
                };
                ColumnInfo {
                    name: cell(r, 0),
                    data_type: cell(r, 1),
                    nullable: flag(r, 2),
                    default: r.get(3).cloned().flatten().map(|d| strip_default_parens(&d)),
                    primary_key: flag(r, 6),
                    comment: r.get(7).cloned().flatten().filter(|c| !c.is_empty()),
                    extra,
                }
            })
            .collect();
        let idx_rows = self
            .text_rows(
                "SELECT i.name, i.is_unique, i.is_primary_key, c.name \
                 FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                 JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                 WHERE i.object_id = OBJECT_ID(@P1) AND i.type > 0 AND ic.is_included_column = 0 AND ic.key_ordinal > 0 \
                 ORDER BY i.is_primary_key DESC, i.name, ic.key_ordinal",
                std::slice::from_ref(&id),
            )
            .await?;
        let mut indexes: Vec<IndexInfo> = Vec::new();
        for r in &idx_rows {
            let name = cell(r, 0);
            match indexes.last_mut().filter(|i| i.name == name) {
                Some(i) => i.columns.push(cell(r, 3)),
                None => {
                    indexes.push(IndexInfo { name, columns: vec![cell(r, 3)], unique: flag(r, 1), primary: flag(r, 2) })
                }
            }
        }
        let fk_rows = self
            .text_rows(
                "SELECT fk.name, pc.name, SCHEMA_NAME(rt.schema_id) + '.' + rt.name, rc.name, \
                        fk.update_referential_action_desc, fk.delete_referential_action_desc \
                 FROM sys.foreign_keys fk \
                 JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                 JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                 JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id \
                 JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                 WHERE fk.parent_object_id = OBJECT_ID(@P1) ORDER BY fk.name, fkc.constraint_column_id",
                std::slice::from_ref(&id),
            )
            .await?;
        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        for r in &fk_rows {
            let name = cell(r, 0);
            match foreign_keys.last_mut().filter(|f| f.name == name) {
                Some(f) => {
                    f.columns.push(cell(r, 1));
                    f.ref_columns.push(cell(r, 3));
                }
                None => foreign_keys.push(ForeignKeyInfo {
                    name,
                    columns: vec![cell(r, 1)],
                    ref_table: cell(r, 2),
                    ref_columns: vec![cell(r, 3)],
                    on_update: cell(r, 4).replace('_', " "),
                    on_delete: cell(r, 5).replace('_', " "),
                }),
            }
        }
        Ok(TableStructure { columns, indexes, foreign_keys })
    }

    async fn ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let id = Some(object_id_arg(obj)?);
        let kind = self
            .text_rows("SELECT type FROM sys.objects WHERE object_id = OBJECT_ID(@P1)", std::slice::from_ref(&id))
            .await?;
        match kind.first().map(|r| cell(r, 0)).as_deref().map(str::trim) {
            Some("V") => {
                let def = self
                    .text_rows(
                        "SELECT definition FROM sys.sql_modules WHERE object_id = OBJECT_ID(@P1)",
                        std::slice::from_ref(&id),
                    )
                    .await?;
                match def.first().map(|r| cell(r, 0)).filter(|d| !d.is_empty()) {
                    Some(d) => Ok(d),
                    None => Err(DbError::Invalid("the view definition is encrypted or hidden".into())),
                }
            }
            Some(_) => {
                let st = self.table_structure(obj).await?;
                let cols: Vec<ColumnDef> = st.columns.iter().map(ColumnDef::from).collect();
                let fks: Vec<FkDef> = FkDef::from_info(&st.foreign_keys);
                let mut out = create_table_with(D, obj, &cols, &fks)?;
                for ix in IndexDef::from_info(&st.indexes) {
                    out.push_str(";\n");
                    out.push_str(&create_index(D, obj, &ix)?);
                }
                let _ = fk_clause;
                Ok(out)
            }
            None => Err(DbError::Invalid(format!("object not found: {}", obj.display_name()))),
        }
    }

    fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::SqlServer(MsCancel(self.shared.clone()))
    }

    fn current_database(&self) -> String {
        self.database.clone()
    }

    async fn close(self) -> DbResult<()> {
        self.client.close().await.map_err(map_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_parentheses_are_stripped_only_when_redundant() {
        assert_eq!(strip_default_parens("((0))"), "0");
        assert_eq!(strip_default_parens("('x')"), "'x'");
        assert_eq!(strip_default_parens("(getdate())"), "getdate()");
        assert_eq!(strip_default_parens("((1)+(2))"), "(1)+(2)");
        assert_eq!(strip_default_parens("(N'a)b')"), "N'a)b'");
    }

    #[test]
    fn type_names_are_sql_server_spellings() {
        assert_eq!(type_name(ColumnType::Int4), "int");
        assert_eq!(type_name(ColumnType::NVarchar), "nvarchar");
        assert_eq!(type_name(ColumnType::Guid), "uniqueidentifier");
    }
}
