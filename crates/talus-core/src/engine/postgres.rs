//! PostgreSQL engine (tokio-postgres). Editor statements use the simple-query protocol
//! (text results, any statement type); parameterised data-editor queries cast to text.

use std::time::Duration;

use futures_util::{StreamExt, pin_mut};
use postgres_native_tls::MakeTlsConnector;
use tokio_postgres::error::{ErrorPosition, SqlState};
use tokio_postgres::types::ToSql;
use tokio_postgres::{CancelToken, Client, NoTls, SimpleQueryMessage};

use super::{CancelHandle, EngineSession};
use crate::error::{DbError, DbResult};
use crate::model::{
    ColumnInfo, ColumnMeta, ConnectionConfig, FetchLimits, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectRef, Outcome,
    QueryResult, SslMode, TableStructure,
};
use crate::secret::Secret;
use crate::sql::Dialect;

const D: Dialect = Dialect(crate::model::Engine::Postgres);
/// Unit separator used to pack arrays into one text cell in catalog queries.
const SEP: char = '\u{1f}';

#[derive(Clone)]
pub struct PgCancel {
    token: CancelToken,
    tls: Option<MakeTlsConnector>,
}

impl PgCancel {
    pub async fn cancel(&self) -> DbResult<()> {
        let r = match &self.tls {
            Some(tls) => self.token.cancel_query(tls.clone()).await,
            None => self.token.cancel_query(NoTls).await,
        };
        r.map_err(|e| DbError::Io(format!("cancel request failed: {e}")))
    }
}

pub struct PgSession {
    client: Client,
    cancel: PgCancel,
    database: String,
    /// Server notices (`RAISE NOTICE`, pldebugger's `PLDBGBREAK:n`) until someone takes them.
    notices: Option<tokio::sync::mpsc::Receiver<String>>,
}

/// Notices kept for a reader; older ones are dropped when nobody reads (a `RAISE NOTICE` in a
/// loop must not grow memory).
const NOTICE_BUFFER: usize = 256;

/// Drives the connection and forwards notices to `tx` (dropped when the buffer is full).
async fn drive_connection<S, T>(mut conn: tokio_postgres::Connection<S, T>, tx: tokio::sync::mpsc::Sender<String>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let messages = futures_util::stream::poll_fn(move |cx| conn.poll_message(cx));
    pin_mut!(messages);
    while let Some(m) = messages.next().await {
        match m {
            Ok(tokio_postgres::AsyncMessage::Notice(n)) => {
                let _ = tx.try_send(n.message().to_string());
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(error = %e, "postgres connection ended");
                break;
            }
        }
    }
}

fn map_err(e: tokio_postgres::Error) -> DbError {
    if let Some(db) = e.as_db_error() {
        if db.code() == &SqlState::QUERY_CANCELED {
            return DbError::Cancelled;
        }
        let mut message = format!("{}: {}", db.severity(), db.message());
        if let Some(d) = db.detail() {
            message.push_str(&format!("\nDETAIL: {d}"));
        }
        if let Some(h) = db.hint() {
            message.push_str(&format!("\nHINT: {h}"));
        }
        let position = match db.position() {
            Some(ErrorPosition::Original(p)) => Some(*p as usize),
            _ => None,
        };
        return DbError::Server { message, code: Some(db.code().code().to_string()), position };
    }
    if e.is_closed() {
        return DbError::ConnectionLost(e.to_string());
    }
    DbError::server(e.to_string())
}

fn tls_connector(cfg: &ConnectionConfig) -> DbResult<MakeTlsConnector> {
    let mut b = native_tls::TlsConnector::builder();
    if cfg.ssl_mode == SslMode::VerifyFull {
        if !cfg.ssl_ca_file.is_empty() {
            let pem = std::fs::read(&cfg.ssl_ca_file)
                .map_err(|e| DbError::Connect(format!("cannot read CA file {}: {e}", cfg.ssl_ca_file)))?;
            let cert = native_tls::Certificate::from_pem(&pem)
                .map_err(|e| DbError::Connect(format!("invalid CA file {}: {e}", cfg.ssl_ca_file)))?;
            b.add_root_certificate(cert);
        }
    } else {
        // libpq `prefer`/`require` semantics: encrypt without certificate verification.
        b.danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true);
    }
    let c = b.build().map_err(|e| DbError::Connect(format!("TLS setup failed: {e}")))?;
    Ok(MakeTlsConnector::new(c))
}

fn params_ref(params: &[Option<String>]) -> Vec<&(dyn ToSql + Sync)> {
    params.iter().map(|p| p as &(dyn ToSql + Sync)).collect()
}

fn cell(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn regclass(obj: &ObjectRef) -> DbResult<Option<String>> {
    D.qualified(obj).map(Some)
}

fn fk_action(code: &str) -> String {
    match code {
        "r" => "RESTRICT",
        "c" => "CASCADE",
        "n" => "SET NULL",
        "d" => "SET DEFAULT",
        _ => "NO ACTION",
    }
    .into()
}

impl PgSession {
    pub async fn connect(cfg: &ConnectionConfig, password: Option<&Secret>, database: Option<&str>) -> DbResult<Self> {
        let db = database
            .filter(|d| !d.is_empty())
            .unwrap_or(if cfg.database.is_empty() { "postgres" } else { &cfg.database })
            .to_string();
        let mut pc = tokio_postgres::Config::new();
        pc.host(&cfg.host)
            .port(cfg.port)
            .user(&cfg.user)
            .dbname(&db)
            .application_name("Talus")
            .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs.max(1)));
        if let Some(p) = password {
            pc.password(p.expose());
        }
        pc.ssl_mode(match cfg.ssl_mode {
            SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            SslMode::Require | SslMode::VerifyFull => tokio_postgres::config::SslMode::Require,
        });
        let connect_err = |e: tokio_postgres::Error| match map_err(e) {
            DbError::Server { message, .. } => DbError::Connect(message),
            other => DbError::Connect(other.to_string()),
        };
        let (tx, notices) = tokio::sync::mpsc::channel(NOTICE_BUFFER);
        let (client, tls) = if cfg.ssl_mode == SslMode::Disable {
            let (client, conn) = pc.connect(NoTls).await.map_err(connect_err)?;
            tokio::spawn(drive_connection(conn, tx));
            (client, None)
        } else {
            let tls = tls_connector(cfg)?;
            let (client, conn) = pc.connect(tls.clone()).await.map_err(connect_err)?;
            tokio::spawn(drive_connection(conn, tx));
            (client, Some(tls))
        };
        let cancel = PgCancel { token: client.cancel_token(), tls };
        Ok(Self { client, cancel, database: db, notices: Some(notices) })
    }

    /// The session's notice stream (once).
    pub fn take_notices(&mut self) -> Option<tokio::sync::mpsc::Receiver<String>> {
        self.notices.take()
    }

    async fn text_rows(&self, sql: &str, params: &[Option<String>]) -> DbResult<Vec<Vec<Option<String>>>> {
        let rows = self.client.query(sql, &params_ref(params)).await.map_err(map_err)?;
        rows.iter()
            .map(|r| (0..r.len()).map(|i| r.try_get::<_, Option<String>>(i).map_err(map_err)).collect())
            .collect()
    }

    async fn table_ddl(&self, name: &str, rc: &[Option<String>]) -> DbResult<String> {
        let s = self.table_structure_inner(rc).await?;
        let mut lines: Vec<String> = Vec::new();
        for c in &s.columns {
            let mut l = format!("  {} {}", D.quote_ident(&c.name)?, c.data_type);
            if let Some(d) = &c.default {
                l.push_str(&format!(" DEFAULT {d}"));
            }
            if !c.nullable {
                l.push_str(" NOT NULL");
            }
            lines.push(l);
        }
        let cons = self
            .text_rows(
                "SELECT conname::text, pg_get_constraintdef(oid, true) FROM pg_constraint WHERE conrelid = $1::text::regclass ORDER BY contype <> 'p', conname",
                rc,
            )
            .await?;
        for r in &cons {
            lines.push(format!("  CONSTRAINT {} {}", D.quote_ident(&cell(r, 0))?, cell(r, 1)));
        }
        let mut out = format!("CREATE TABLE {name} (\n{}\n);", lines.join(",\n"));
        let idx = self
            .text_rows(
                "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i WHERE i.indrelid = $1::text::regclass \
                 AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.indexrelid) ORDER BY 1",
                rc,
            )
            .await?;
        for r in &idx {
            out.push_str(&format!("\n\n{};", cell(r, 0)));
        }
        let comment = self.text_rows("SELECT obj_description($1::text::regclass, 'pg_class')", rc).await?;
        if let Some(cm) = comment.first().and_then(|r| r.first().cloned().flatten()) {
            out.push_str(&format!("\n\nCOMMENT ON TABLE {name} IS {};", D.string_literal(&cm)));
        }
        for c in &s.columns {
            if let Some(cm) = &c.comment {
                out.push_str(&format!(
                    "\n\nCOMMENT ON COLUMN {name}.{} IS {};",
                    D.quote_ident(&c.name)?,
                    D.string_literal(cm)
                ));
            }
        }
        Ok(out)
    }

    async fn table_structure_inner(&self, rc: &[Option<String>]) -> DbResult<TableStructure> {
        let columns: Vec<ColumnInfo> = self
            .text_rows(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), (NOT a.attnotnull)::text, \
                 pg_get_expr(d.adbin, d.adrelid), \
                 (EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = a.attrelid AND i.indisprimary AND a.attnum = ANY(i.indkey)))::text, \
                 col_description(a.attrelid, a.attnum) \
                 FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE a.attrelid = $1::text::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
                rc,
            )
            .await?
            .iter()
            .map(|r| ColumnInfo {
                name: cell(r, 0),
                data_type: cell(r, 1),
                nullable: cell(r, 2) == "true",
                default: r.get(3).cloned().flatten(),
                primary_key: cell(r, 4) == "true",
                comment: r.get(5).cloned().flatten(),
                extra: None,
            })
            .collect();
        let indexes = self
            .text_rows(
                "SELECT ic.relname::text, i.indisunique::text, i.indisprimary::text, \
                 array_to_string(ARRAY(SELECT pg_get_indexdef(i.indexrelid, k + 1, true) FROM generate_subscripts(i.indkey, 1) k ORDER BY k), E'\\x1f') \
                 FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid WHERE i.indrelid = $1::text::regclass ORDER BY i.indisprimary DESC, 1",
                rc,
            )
            .await?
            .iter()
            .map(|r| IndexInfo {
                name: cell(r, 0),
                unique: cell(r, 1) == "true",
                primary: cell(r, 2) == "true",
                columns: cell(r, 3).split(SEP).filter(|s| !s.is_empty()).map(String::from).collect(),
            })
            .collect();
        let foreign_keys = self
            .text_rows(
                "SELECT con.conname::text, \
                 (SELECT string_agg(a.attname, E'\\x1f' ORDER BY k.ord) FROM unnest(con.conkey) WITH ORDINALITY k(attnum, ord) \
                    JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum), \
                 con.confrelid::regclass::text, \
                 (SELECT string_agg(a.attname, E'\\x1f' ORDER BY k.ord) FROM unnest(con.confkey) WITH ORDINALITY k(attnum, ord) \
                    JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.attnum), \
                 con.confupdtype::text, con.confdeltype::text \
                 FROM pg_constraint con WHERE con.conrelid = $1::text::regclass AND con.contype = 'f' ORDER BY 1",
                rc,
            )
            .await?
            .iter()
            .map(|r| ForeignKeyInfo {
                name: cell(r, 0),
                columns: cell(r, 1).split(SEP).map(String::from).collect(),
                ref_table: cell(r, 2),
                ref_columns: cell(r, 3).split(SEP).map(String::from).collect(),
                on_update: fk_action(&cell(r, 4)),
                on_delete: fk_action(&cell(r, 5)),
            })
            .collect();
        Ok(TableStructure { columns, indexes, foreign_keys })
    }
}

impl EngineSession for PgSession {
    async fn server_version(&mut self) -> DbResult<String> {
        let rows = self.text_rows("SELECT version()", &[]).await?;
        Ok(rows.first().map(|r| cell(r, 0)).unwrap_or_default())
    }

    async fn execute(&mut self, sql: &str, limits: &FetchLimits) -> DbResult<Vec<Outcome>> {
        let stream = self.client.simple_query_raw(sql).await.map_err(map_err)?;
        pin_mut!(stream);
        let mut outcomes = Vec::new();
        let mut current: Option<QueryResult> = None;
        while let Some(msg) = stream.next().await {
            match msg.map_err(map_err)? {
                SimpleQueryMessage::RowDescription(cols) => {
                    current = Some(QueryResult {
                        columns: cols
                            .iter()
                            .map(|c| ColumnMeta { name: c.name().to_string(), type_name: String::new(), binary: false })
                            .collect(),
                        ..Default::default()
                    });
                }
                SimpleQueryMessage::Row(row) => {
                    if let Some(r) = current.as_mut() {
                        if r.rows.len() < limits.max_rows {
                            r.rows.push((0..row.len()).map(|i| row.get(i).map(str::to_string)).collect());
                        } else {
                            // Keep draining so the connection stays usable.
                            r.truncated = true;
                        }
                    }
                }
                SimpleQueryMessage::CommandComplete(n) => match current.take() {
                    Some(r) => outcomes.push(Outcome::Rows(r)),
                    None => outcomes.push(Outcome::Affected(n)),
                },
                _ => {}
            }
        }
        if let Some(r) = current.take() {
            outcomes.push(Outcome::Rows(r));
        }
        Ok(outcomes)
    }

    async fn query_params(
        &mut self,
        sql: &str,
        params: &[Option<String>],
        limits: &FetchLimits,
    ) -> DbResult<QueryResult> {
        let stmt = self.client.prepare(sql).await.map_err(map_err)?;
        let columns = stmt
            .columns()
            .iter()
            .map(|c| ColumnMeta { name: c.name().to_string(), type_name: c.type_().name().to_string(), binary: false })
            .collect();
        let rows = self.client.query(&stmt, &params_ref(params)).await.map_err(map_err)?;
        let truncated = rows.len() > limits.max_rows;
        let rows = rows
            .iter()
            .take(limits.max_rows)
            .map(|r| (0..r.len()).map(|i| r.try_get::<_, Option<String>>(i).map_err(map_err)).collect())
            .collect::<DbResult<Vec<_>>>()?;
        Ok(QueryResult { columns, rows, truncated })
    }

    async fn exec_params(&mut self, sql: &str, params: &[Option<String>]) -> DbResult<u64> {
        self.client.execute(sql, &params_ref(params)).await.map_err(map_err)
    }

    async fn list_databases(&mut self) -> DbResult<Vec<String>> {
        Ok(self
            .text_rows("SELECT datname::text FROM pg_database WHERE NOT datistemplate AND datallowconn ORDER BY 1", &[])
            .await?
            .iter()
            .map(|r| cell(r, 0))
            .collect())
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<String>> {
        Ok(self
            .text_rows(
                "SELECT nspname::text FROM pg_namespace WHERE nspname NOT LIKE 'pg!_%' ESCAPE '!' AND nspname <> 'information_schema' ORDER BY 1",
                &[],
            )
            .await?
            .iter()
            .map(|r| cell(r, 0))
            .collect())
    }

    async fn list_objects(&mut self, database: &str, schema: Option<&str>) -> DbResult<Vec<ObjectRef>> {
        let schema = schema.unwrap_or("public").to_string();
        Ok(self
            .text_rows(
                "SELECT c.relname::text, c.relkind::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind IN ('r','p','f','v','m') ORDER BY 1",
                &[Some(schema.clone())],
            )
            .await?
            .iter()
            .map(|r| ObjectRef {
                database: database.to_string(),
                schema: Some(schema.clone()),
                name: cell(r, 0),
                kind: if matches!(cell(r, 1).as_str(), "v" | "m") { ObjectKind::View } else { ObjectKind::Table },
            })
            .collect())
    }

    async fn table_structure(&mut self, obj: &ObjectRef) -> DbResult<TableStructure> {
        self.table_structure_inner(&[regclass(obj)?]).await
    }

    async fn ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let rc = [regclass(obj)?];
        let name = D.qualified(obj)?;
        let kind = self.text_rows("SELECT relkind::text FROM pg_class WHERE oid = $1::text::regclass", &rc).await?;
        match kind.first().map(|r| cell(r, 0)).as_deref() {
            Some("v") | Some("m") => {
                let def = self.text_rows("SELECT pg_get_viewdef($1::text::regclass, true)", &rc).await?;
                let body = def.first().map(|r| cell(r, 0)).unwrap_or_default();
                let head = if kind[0][0].as_deref() == Some("m") {
                    "CREATE MATERIALIZED VIEW"
                } else {
                    "CREATE OR REPLACE VIEW"
                };
                Ok(format!("{head} {name} AS\n{}", body.trim_end()))
            }
            Some(_) => self.table_ddl(&name, &rc).await,
            None => Err(DbError::Invalid(format!("object not found: {name}"))),
        }
    }

    fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::Postgres(Box::new(self.cancel.clone()))
    }

    fn current_database(&self) -> String {
        self.database.clone()
    }

    async fn close(self) -> DbResult<()> {
        // Dropping the client terminates the connection task gracefully.
        drop(self.client);
        Ok(())
    }
}
