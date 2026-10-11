//! Engine-neutral data model shared by the engines, stores and the UI.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Engine {
    Sqlite,
    Postgres,
    MySql,
    SqlServer,
    Redis,
    MongoDb,
}

/// How a connection is used: SQL tabs and tables, a key/value browser or a document browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    Sql,
    KeyValue,
    Document,
}

impl Engine {
    pub const ALL: [Engine; 6] =
        [Engine::Sqlite, Engine::Postgres, Engine::MySql, Engine::SqlServer, Engine::Redis, Engine::MongoDb];

    pub fn kind(self) -> EngineKind {
        match self {
            Engine::Redis => EngineKind::KeyValue,
            Engine::MongoDb => EngineKind::Document,
            _ => EngineKind::Sql,
        }
    }

    pub fn is_sql(self) -> bool {
        self.kind() == EngineKind::Sql
    }

    pub fn label(self) -> &'static str {
        match self {
            Engine::Sqlite => "SQLite",
            Engine::Postgres => "PostgreSQL",
            Engine::MySql => "MySQL / MariaDB",
            Engine::SqlServer => "SQL Server",
            Engine::Redis => "Redis",
            Engine::MongoDb => "MongoDB",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Engine::Sqlite => 0,
            Engine::Postgres => 5432,
            Engine::MySql => 3306,
            Engine::SqlServer => 1433,
            Engine::Redis => 6379,
            Engine::MongoDb => 27017,
        }
    }

    /// Schema new objects land in when none is named (engines with schemas).
    pub fn default_schema(self) -> Option<&'static str> {
        match self {
            Engine::Postgres => Some("public"),
            Engine::SqlServer => Some("dbo"),
            Engine::Sqlite | Engine::MySql | Engine::Redis | Engine::MongoDb => None,
        }
    }

    /// Database a connection opens when none is configured.
    pub fn default_database(self) -> Option<&'static str> {
        match self {
            Engine::Postgres => Some("postgres"),
            Engine::SqlServer => Some("master"),
            Engine::Redis => Some("0"),
            Engine::MongoDb => Some("admin"),
            Engine::Sqlite | Engine::MySql => None,
        }
    }

    /// Statement that opens a transaction.
    pub fn begin_sql(self) -> &'static str {
        match self {
            Engine::MySql => "START TRANSACTION",
            Engine::SqlServer => "BEGIN TRANSACTION",
            Engine::Sqlite | Engine::Postgres | Engine::Redis | Engine::MongoDb => "BEGIN",
        }
    }

    pub fn capabilities(self) -> Capabilities {
        match self {
            Engine::Sqlite => Capabilities {
                schemas: false,
                session_per_database: false,
                server_cancel: true,
                transactional_ddl: true,
                network: false,
            },
            Engine::Postgres => Capabilities {
                schemas: true,
                session_per_database: true,
                server_cancel: true,
                transactional_ddl: true,
                network: true,
            },
            Engine::MySql => Capabilities {
                schemas: false,
                session_per_database: false,
                server_cancel: true,
                transactional_ddl: false,
                network: true,
            },
            Engine::SqlServer => Capabilities {
                schemas: true,
                session_per_database: true,
                server_cancel: true,
                transactional_ddl: true,
                network: true,
            },
            // Redis selects a database per connection (SELECT n).
            Engine::Redis => Capabilities {
                schemas: false,
                session_per_database: true,
                server_cancel: false,
                transactional_ddl: false,
                network: true,
            },
            Engine::MongoDb => Capabilities {
                schemas: false,
                session_per_database: false,
                server_cancel: false,
                transactional_ddl: false,
                network: true,
            },
        }
    }
}

/// Explicit engine capabilities so the UI never hard-codes engine checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Databases contain schemas (PostgreSQL) — adds a tree level.
    pub schemas: bool,
    /// A session is bound to one database; other databases need another session.
    pub session_per_database: bool,
    /// A running statement can be cancelled while it executes.
    pub server_cancel: bool,
    /// DDL participates in transactions (rollback undoes CREATE/ALTER).
    pub transactional_ddl: bool,
    /// Host/port/user apply (false for file databases).
    pub network: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    /// Encrypt, verify the certificate chain (system roots or `ssl_ca_file`) and the host name.
    VerifyFull,
}

impl SslMode {
    pub const ALL: [SslMode; 4] = [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyFull];
    pub fn label(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyFull => "verify-full",
        }
    }
}

/// Cloud vendor presets of the New Connection dialog: engine, port, TLS and a host pattern hint.
/// Managed services all require encryption, so the presets start at verify-full against system roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudPreset {
    pub vendor: &'static str,
    pub service: &'static str,
    pub engine: Engine,
    pub port: u16,
    pub host_hint: &'static str,
    pub ssl_mode: SslMode,
    pub default_database: &'static str,
}

const fn preset(
    vendor: &'static str,
    service: &'static str,
    engine: Engine,
    port: u16,
    host_hint: &'static str,
    default_database: &'static str,
) -> CloudPreset {
    CloudPreset { vendor, service, engine, port, host_hint, ssl_mode: SslMode::VerifyFull, default_database }
}

pub const CLOUD_PRESETS: &[CloudPreset] = &[
    preset(
        "Amazon AWS",
        "RDS for PostgreSQL",
        Engine::Postgres,
        5432,
        "<instance>.<id>.<region>.rds.amazonaws.com",
        "postgres",
    ),
    preset(
        "Amazon AWS",
        "Aurora PostgreSQL",
        Engine::Postgres,
        5432,
        "<cluster>.cluster-<id>.<region>.rds.amazonaws.com",
        "postgres",
    ),
    preset("Amazon AWS", "RDS for MySQL", Engine::MySql, 3306, "<instance>.<id>.<region>.rds.amazonaws.com", ""),
    preset("Amazon AWS", "Aurora MySQL", Engine::MySql, 3306, "<cluster>.cluster-<id>.<region>.rds.amazonaws.com", ""),
    preset(
        "Amazon AWS",
        "RDS for SQL Server",
        Engine::SqlServer,
        1433,
        "<instance>.<id>.<region>.rds.amazonaws.com",
        "master",
    ),
    preset("Amazon AWS", "Redshift", Engine::Postgres, 5439, "<cluster>.<id>.<region>.redshift.amazonaws.com", "dev"),
    preset("Amazon AWS", "ElastiCache for Redis", Engine::Redis, 6379, "<name>.<id>.cache.amazonaws.com", "0"),
    preset("Microsoft Azure", "Azure SQL Database", Engine::SqlServer, 1433, "<server>.database.windows.net", "master"),
    preset(
        "Microsoft Azure",
        "Database for PostgreSQL",
        Engine::Postgres,
        5432,
        "<server>.postgres.database.azure.com",
        "postgres",
    ),
    preset("Microsoft Azure", "Database for MySQL", Engine::MySql, 3306, "<server>.mysql.database.azure.com", ""),
    preset("Microsoft Azure", "Cache for Redis", Engine::Redis, 6380, "<name>.redis.cache.windows.net", "0"),
    preset(
        "Google Cloud",
        "Cloud SQL for PostgreSQL",
        Engine::Postgres,
        5432,
        "<public IP of the instance>",
        "postgres",
    ),
    preset("Google Cloud", "Cloud SQL for MySQL", Engine::MySql, 3306, "<public IP of the instance>", ""),
    preset(
        "Google Cloud",
        "Cloud SQL for SQL Server",
        Engine::SqlServer,
        1433,
        "<public IP of the instance>",
        "master",
    ),
    preset("Alibaba Cloud", "ApsaraDB RDS for MySQL", Engine::MySql, 3306, "<instance>.mysql.rds.aliyuncs.com", ""),
    preset(
        "Alibaba Cloud",
        "ApsaraDB RDS for PostgreSQL",
        Engine::Postgres,
        5432,
        "<instance>.pg.rds.aliyuncs.com",
        "postgres",
    ),
    preset("Supabase", "Postgres", Engine::Postgres, 5432, "db.<project>.supabase.co", "postgres"),
    preset("Neon", "Serverless Postgres", Engine::Postgres, 5432, "<endpoint>.<region>.aws.neon.tech", "neondb"),
    preset("PlanetScale", "MySQL", Engine::MySql, 3306, "<region>.connect.psdb.cloud", ""),
];

impl CloudPreset {
    pub fn label(&self) -> String {
        format!("{} — {}", self.vendor, self.service)
    }

    /// Settings of a new connection for this service; the host stays empty for the user to fill.
    pub fn apply(&self, cfg: &ConnectionConfig) -> ConnectionConfig {
        ConnectionConfig {
            id: cfg.id,
            name: cfg.name.clone(),
            port: self.port,
            ssl_mode: self.ssl_mode,
            database: self.default_database.into(),
            ..ConnectionConfig::new(self.engine, "")
        }
    }
}

fn default_timeout() -> u64 {
    10
}

/// Persisted connection settings. Never contains secrets — see `store::secrets`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: Uuid,
    pub name: String,
    pub engine: Engine,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    /// Initial database for network engines; the database file path for SQLite.
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub save_password: bool,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub ssl_mode: SslMode,
    /// PEM file with the CA to trust for `verify-full` (empty = system roots).
    #[serde(default)]
    pub ssl_ca_file: String,
    #[serde(default = "default_timeout")]
    pub connect_timeout_secs: u64,
    /// Statements executed right after connecting (e.g. `SET search_path ...`).
    #[serde(default)]
    pub init_sql: String,
    /// SQLite only: the file is encrypted with SQLCipher; the password is the key.
    #[serde(default)]
    pub encrypted: bool,
    /// Optional SSH tunnel; `host`/`port` are then resolved on the SSH server's side.
    #[serde(default)]
    pub ssh: Option<SshConfig>,
    /// Optional HTTP tunnel (proxy `CONNECT`); `host`/`port` are then resolved by the proxy.
    /// Exclusive with `ssh`.
    #[serde(default)]
    pub http_proxy: Option<HttpProxyConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpProxyConfig {
    pub host: String,
    pub port: u16,
    /// Empty: no `Proxy-Authorization`. The password is the connection's tunnel secret.
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub save_secret: bool,
}

impl Default for HttpProxyConfig {
    fn default() -> Self {
        Self { host: String::new(), port: 3128, user: String::new(), save_secret: false }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SshAuth {
    /// Password stored in the Keychain as `SecretKind::TunnelSecret`.
    Password,
    /// Private key file; its passphrase (if any) is the SSH secret.
    PrivateKey { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    #[serde(default)]
    pub save_secret: bool,
    /// Accept and remember an unknown host key on first connect (TOFU). Changed keys are
    /// always rejected.
    #[serde(default)]
    pub trust_new_host_key: bool,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: SshAuth::Password,
            save_secret: false,
            trust_new_host_key: false,
        }
    }
}

impl ConnectionConfig {
    pub fn new(engine: Engine, name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            engine,
            host: if engine.capabilities().network { "localhost".into() } else { String::new() },
            port: engine.default_port(),
            user: String::new(),
            database: String::new(),
            save_password: false,
            group: None,
            ssl_mode: SslMode::default(),
            ssl_ca_file: String::new(),
            connect_timeout_secs: default_timeout(),
            init_sql: String::new(),
            encrypted: false,
            ssh: None,
            http_proxy: None,
        }
    }

    pub fn has_tunnel(&self) -> bool {
        self.ssh.is_some() || self.http_proxy.is_some()
    }

    /// For the tunnel's secret (SSH password / key passphrase, proxy password): whether it is saved
    /// in the Keychain and whether it must be asked for when it is not. `None` without a tunnel.
    pub fn tunnel_secret_policy(&self) -> Option<(bool, bool)> {
        match (&self.ssh, &self.http_proxy) {
            (Some(s), _) => Some((s.save_secret, matches!(s.auth, SshAuth::Password))),
            (None, Some(p)) => Some((p.save_secret, !p.user.is_empty())),
            (None, None) => None,
        }
    }

    pub fn set_tunnel_secret_saved(&mut self, saved: bool) {
        if let Some(s) = self.ssh.as_mut() {
            s.save_secret = saved;
        } else if let Some(p) = self.http_proxy.as_mut() {
            p.save_secret = saved;
        }
    }

    pub fn needs_password(&self) -> bool {
        self.engine.capabilities().network || (self.engine == Engine::Sqlite && self.encrypted)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("Connection name is required".into());
        }
        match self.engine {
            Engine::Sqlite if self.database.trim().is_empty() => Err("Database file is required".into()),
            e if e.capabilities().network && self.host.trim().is_empty() => Err("Host is required".into()),
            e if e.capabilities().network && self.port == 0 => Err("Port must be between 1 and 65535".into()),
            // Redis and MongoDB can run without authentication.
            Engine::Postgres | Engine::MySql | Engine::SqlServer if self.user.trim().is_empty() => {
                Err("User name is required".into())
            }
            _ if self.ssh.is_some() && self.http_proxy.is_some() => {
                Err("Use either an SSH tunnel or an HTTP tunnel, not both".into())
            }
            _ if self.http_proxy.as_ref().is_some_and(|p| p.host.trim().is_empty()) => {
                Err("HTTP proxy host is required".into())
            }
            _ if self.http_proxy.as_ref().is_some_and(|p| p.port == 0) => {
                Err("HTTP proxy port must be between 1 and 65535".into())
            }
            _ => match &self.ssh {
                Some(s) if s.host.trim().is_empty() => Err("SSH host is required".into()),
                Some(s) if s.user.trim().is_empty() => Err("SSH user name is required".into()),
                Some(s) if s.port == 0 => Err("SSH port must be between 1 and 65535".into()),
                Some(SshConfig { auth: SshAuth::PrivateKey { path }, .. }) if path.trim().is_empty() => {
                    Err("SSH private key file is required".into())
                }
                _ => Ok(()),
            },
        }
    }
}

/// A cell rendered as text; `None` is SQL NULL (distinct from the empty string).
pub type Cell = Option<String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    pub type_name: String,
    /// Value is binary and shown as `0x…` hex — not editable as text.
    pub binary: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Cell>>,
    /// More rows existed than `FetchLimits::max_rows`.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Rows(QueryResult),
    Affected(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatementResult {
    pub sql: String,
    pub outcome: Outcome,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct FetchLimits {
    pub max_rows: usize,
}

impl Default for FetchLimits {
    fn default() -> Self {
        Self { max_rows: 10_000 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectKind {
    Table,
    View,
}

impl ObjectKind {
    pub fn label(self) -> &'static str {
        match self {
            ObjectKind::Table => "Table",
            ObjectKind::View => "View",
        }
    }
}

/// Fully identifies a relation. `database` selects the session for engines with
/// `session_per_database`; for MySQL it is the schema qualifier; for SQLite the attached db name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObjectRef {
    pub database: String,
    pub schema: Option<String>,
    pub name: String,
    pub kind: ObjectKind,
}

impl ObjectRef {
    pub fn display_name(&self) -> String {
        match &self.schema {
            Some(s) => format!("{s}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub primary_key: bool,
    pub comment: Option<String>,
    /// Engine-specific attributes that must survive a column redefinition
    /// (MySQL `auto_increment`, `on update CURRENT_TIMESTAMP`, generated markers).
    pub extra: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableStructure {
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
}

impl TableStructure {
    pub fn primary_key(&self) -> Vec<&str> {
        self.columns.iter().filter(|c| c.primary_key).map(|c| c.name.as_str()).collect()
    }
    pub fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.iter().find(|c| c.name == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    NotContains,
    BeginsWith,
    IsNull,
    IsNotNull,
}

impl FilterOp {
    pub const ALL: [FilterOp; 11] = [
        FilterOp::Eq,
        FilterOp::Ne,
        FilterOp::Lt,
        FilterOp::Le,
        FilterOp::Gt,
        FilterOp::Ge,
        FilterOp::Contains,
        FilterOp::NotContains,
        FilterOp::BeginsWith,
        FilterOp::IsNull,
        FilterOp::IsNotNull,
    ];
    pub fn label(self) -> &'static str {
        match self {
            FilterOp::Eq => "=",
            FilterOp::Ne => "<>",
            FilterOp::Lt => "<",
            FilterOp::Le => "<=",
            FilterOp::Gt => ">",
            FilterOp::Ge => ">=",
            FilterOp::Contains => "contains",
            FilterOp::NotContains => "does not contain",
            FilterOp::BeginsWith => "begins with",
            FilterOp::IsNull => "is null",
            FilterOp::IsNotNull => "is not null",
        }
    }
    pub fn takes_value(self) -> bool {
        !matches!(self, FilterOp::IsNull | FilterOp::IsNotNull)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub enabled: bool,
    pub column: String,
    pub op: FilterOp,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PageQuery {
    pub table: ObjectRef,
    pub filters: Vec<Filter>,
    pub sort: Vec<(String, SortDir)>,
    pub offset: u64,
    pub limit: u64,
}

/// Original values of the key columns that identify one row.
#[derive(Debug, Clone, PartialEq)]
pub struct RowKey(pub Vec<(String, Cell)>);

#[derive(Debug, Clone, PartialEq)]
pub struct RowUpdate {
    pub key: RowKey,
    pub values: Vec<(String, Cell)>,
}

/// Pending grid edits applied atomically by `Session::apply_changes`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChangeSet {
    pub updates: Vec<RowUpdate>,
    pub inserts: Vec<Vec<(String, Cell)>>,
    pub deletes: Vec<RowKey>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.inserts.is_empty() && self.deletes.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub updated: u64,
    pub inserted: u64,
    pub deleted: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnels_are_exclusive_and_the_proxy_needs_a_host() {
        let mut c = ConnectionConfig::new(Engine::Postgres, "pg");
        c.user = "u".into();
        c.http_proxy = Some(HttpProxyConfig::default());
        assert_eq!(c.validate().unwrap_err(), "HTTP proxy host is required");
        c.http_proxy = Some(HttpProxyConfig { host: "proxy".into(), ..HttpProxyConfig::default() });
        assert!(c.validate().is_ok());
        assert_eq!(c.tunnel_secret_policy(), Some((false, false)), "no proxy user: nothing to ask");
        c.ssh = Some(SshConfig { host: "bastion".into(), user: "u".into(), ..SshConfig::default() });
        assert!(c.validate().unwrap_err().contains("not both"));
        c.http_proxy = None;
        c.set_tunnel_secret_saved(true);
        assert_eq!(c.tunnel_secret_policy(), Some((true, true)));
    }

    #[test]
    fn cloud_presets_use_network_engines_with_tls() {
        for p in CLOUD_PRESETS {
            assert!(p.engine.capabilities().network && p.port > 0 && p.ssl_mode != SslMode::Disable, "{}", p.label());
            let base = ConnectionConfig::new(Engine::Sqlite, "mine");
            let c = p.apply(&base);
            assert_eq!((c.id, c.name.as_str(), c.engine, c.port), (base.id, "mine", p.engine, p.port));
        }
    }

    #[test]
    fn config_roundtrip_and_validation() {
        let mut c = ConnectionConfig::new(Engine::Postgres, "pg");
        assert!(c.validate().is_err(), "user required");
        c.user = "u".into();
        assert!(c.validate().is_ok());
        let json = serde_json::to_string(&c).unwrap();
        let back: ConnectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
        let s = ConnectionConfig::new(Engine::Sqlite, "s");
        assert_eq!(s.validate().unwrap_err(), "Database file is required");
    }
}

/// One hit of "Find in Database".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindHit {
    pub obj: ObjectRef,
    /// Matching column (structure search), if the hit is a column.
    pub column: Option<String>,
    /// Number of matching rows (data search).
    pub rows: Option<u64>,
}
