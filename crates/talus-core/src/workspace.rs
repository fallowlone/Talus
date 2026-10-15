//! Shared workspaces: connection definitions kept on a self-hosted `talus-server`.
//!
//! What is shared is the *definition* of a connection (engine, host, port, user, database, TLS
//! mode, tunnel hosts). Never shared: passwords and tunnel secrets (they stay in each member's
//! Keychain), machine-local paths (CA file, SSH / Snowflake key files), `init_sql` (it would run
//! on every member's session) and the SSH trust-on-first-use flag. SQLite files are local to one
//! machine and are not shareable at all.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::{Method, RequestBuilder, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::model::{ConnectionConfig, Engine};
use crate::secret::Secret;

/// Where the session token lives in the secret store: the nil id belongs to no connection.
pub const TOKEN_SLOT: (Uuid, crate::store::secrets::SecretKind) =
    (Uuid::nil(), crate::store::secrets::SecretKind::TunnelSecret);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Error, Clone, PartialEq)]
pub enum WorkspaceError {
    #[error("{0}")]
    Invalid(String),
    #[error("cannot reach the workspace server: {0}")]
    Network(String),
    #[error("{message}")]
    Server { status: u16, message: String },
    /// Someone changed the connection since this client last saw it; `current` is the server's copy.
    #[error("the connection changed on the server; pull first")]
    Conflict { current: Option<Box<RemoteConnection>> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Editor,
    Owner,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Editor => "editor",
            Role::Owner => "owner",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteWorkspace {
    pub id: String,
    pub name: String,
    pub role: Role,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WorkspaceDetails {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub members: BTreeMap<String, Role>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RemoteConnection {
    pub id: String,
    pub rev: u64,
    pub data: Value,
    #[serde(default)]
    pub updated_by: String,
}

/// Which workspace this installation is linked to, and the revision of every shared connection
/// it last pushed or pulled. The session token lives in the secret store, not here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceLink {
    pub server: String,
    pub user: String,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub workspace_name: String,
    #[serde(default)]
    pub revs: BTreeMap<Uuid, u64>,
}

// ---------- definitions: what leaves and what comes back ----------

/// The shareable form of a connection (see the module docs for what is dropped).
pub fn to_shared(cfg: &ConnectionConfig) -> Result<Value, WorkspaceError> {
    if cfg.engine == Engine::Sqlite {
        return Err(WorkspaceError::Invalid("SQLite databases are files on this machine and cannot be shared".into()));
    }
    let mut v = serde_json::to_value(cfg).map_err(|e| WorkspaceError::Invalid(e.to_string()))?;
    let o = v.as_object_mut().ok_or_else(|| WorkspaceError::Invalid("connection is not an object".into()))?;
    // `save_password` / `save_secret` are each member's own Keychain preference.
    for local in ["ssl_ca_file", "init_sql", "save_password"] {
        o.remove(local);
    }
    strip_secret_flags(o);
    if let Some(ssh) = o.get_mut("ssh").and_then(Value::as_object_mut) {
        ssh.remove("trust_new_host_key");
        if ssh.get("auth").is_some_and(|a| a.get("PrivateKey").is_some()) {
            ssh.insert("auth".into(), json!("KeyFile"));
        }
    }
    if let Some(sf) = o.get_mut("snowflake").and_then(Value::as_object_mut)
        && sf.get("auth").is_some_and(|a| a.get("KeyPair").is_some())
    {
        sf.insert("auth".into(), json!("KeyPair"));
    }
    Ok(v)
}

/// `save_secret` only says "remember my tunnel secret in my Keychain"; it is a local preference.
fn strip_secret_flags(o: &mut serde_json::Map<String, Value>) {
    for tunnel in ["ssh", "php_tunnel", "http_proxy"] {
        if let Some(t) = o.get_mut(tunnel).and_then(Value::as_object_mut) {
            t.remove("save_secret");
        }
    }
}

/// Turns a server copy into a local connection. Machine-local settings come from `local` (the
/// connection this one replaces), never from the server; a new connection gets empty ones.
pub fn from_shared(
    id: Uuid,
    data: &Value,
    local: Option<&ConnectionConfig>,
) -> Result<ConnectionConfig, WorkspaceError> {
    let mut v = data.clone();
    let o = v.as_object_mut().ok_or_else(|| WorkspaceError::Invalid("definition is not an object".into()))?;
    // Whatever the server holds, none of these may take effect on this machine.
    for local_only in ["ssl_ca_file", "init_sql", "save_password"] {
        o.remove(local_only);
    }
    strip_secret_flags(o);
    let key_path = local.and_then(|l| match l.ssh.as_ref().map(|s| &s.auth) {
        Some(crate::model::SshAuth::PrivateKey { path }) => Some(path.clone()),
        _ => None,
    });
    if let Some(ssh) = o.get_mut("ssh").and_then(Value::as_object_mut) {
        ssh.remove("trust_new_host_key");
        if ssh.get("auth") == Some(&json!("KeyFile")) {
            ssh.insert("auth".into(), json!({ "PrivateKey": { "path": key_path.unwrap_or_default() } }));
        }
    }
    let key_file = local.and_then(|l| match l.snowflake.as_ref().map(|s| &s.auth) {
        Some(crate::model::SnowflakeAuth::KeyPair { key_file }) => Some(key_file.clone()),
        _ => None,
    });
    if let Some(sf) = o.get_mut("snowflake").and_then(Value::as_object_mut)
        && sf.get("auth") == Some(&json!("KeyPair"))
    {
        sf.insert("auth".into(), json!({ "KeyPair": { "key_file": key_file.unwrap_or_default() } }));
    }
    let mut cfg: ConnectionConfig =
        serde_json::from_value(v).map_err(|e| WorkspaceError::Invalid(format!("unreadable definition: {e}")))?;
    if cfg.engine == Engine::Sqlite {
        return Err(WorkspaceError::Invalid("SQLite connections cannot be shared".into()));
    }
    cfg.id = id;
    if let Some(l) = local {
        cfg.ssl_ca_file = l.ssl_ca_file.clone();
        cfg.init_sql = l.init_sql.clone();
        cfg.save_password = l.save_password;
        if let (Some(new), Some(old)) = (cfg.ssh.as_mut(), l.ssh.as_ref()) {
            new.trust_new_host_key = old.trust_new_host_key;
            new.save_secret = old.save_secret;
        }
        if let (Some(new), Some(old)) = (cfg.php_tunnel.as_mut(), l.php_tunnel.as_ref()) {
            new.save_secret = old.save_secret;
        }
        if let (Some(new), Some(old)) = (cfg.http_proxy.as_mut(), l.http_proxy.as_ref()) {
            new.save_secret = old.save_secret;
        }
    }
    Ok(cfg)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PullReport {
    pub added: usize,
    pub updated: usize,
    /// Linked connections the workspace no longer has; they stay on this machine, unlinked.
    pub unshared: Vec<String>,
    /// Remote definitions that could not be applied: (name or id, reason).
    pub skipped: Vec<(String, String)>,
}

/// Applies the server's connections to the local list and records their revisions in `link`.
pub fn apply_pull(
    local: &mut Vec<ConnectionConfig>,
    link: &mut WorkspaceLink,
    remote: &[RemoteConnection],
) -> PullReport {
    let mut report = PullReport::default();
    for r in remote {
        let Ok(id) = Uuid::parse_str(&r.id) else {
            report.skipped.push((r.id.clone(), "not a UUID".into()));
            continue;
        };
        let existing = local.iter().position(|c| c.id == id);
        match from_shared(id, &r.data, existing.map(|i| &local[i])) {
            Ok(cfg) => {
                match existing {
                    Some(i) => {
                        local[i] = cfg;
                        report.updated += 1;
                    }
                    None => {
                        local.push(cfg);
                        report.added += 1;
                    }
                }
                link.revs.insert(id, r.rev);
            }
            Err(e) => report.skipped.push((r.id.clone(), e.to_string())),
        }
    }
    let gone: Vec<Uuid> = link
        .revs
        .keys()
        .filter(|id| !remote.iter().any(|r| Uuid::parse_str(&r.id).is_ok_and(|rid| rid == **id)))
        .copied()
        .collect();
    for id in gone {
        link.revs.remove(&id);
        report.unshared.push(local.iter().find(|c| c.id == id).map_or_else(|| id.to_string(), |c| c.name.clone()));
    }
    report
}

// ---------- client ----------

/// `https://…`, or `http://` only to this machine (a plain-text session token must not cross a network).
pub fn parse_server_url(raw: &str) -> Result<Url, WorkspaceError> {
    let url = Url::parse(raw.trim()).map_err(|e| WorkspaceError::Invalid(format!("server address: {e}")))?;
    let loopback = match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(a)) => a.is_loopback(),
        Some(url::Host::Ipv6(a)) => a.is_loopback(),
        None => false,
    };
    match url.scheme() {
        "https" => Ok(url),
        "http" if loopback => Ok(url),
        _ => Err(WorkspaceError::Invalid(
            "the server address must start with https:// (http:// only for localhost)".into(),
        )),
    }
}

pub struct WorkspaceClient {
    http: reqwest::Client,
    base: Url,
    token: Option<Secret>,
}

impl WorkspaceClient {
    pub fn new(server: &str, token: Option<Secret>) -> Result<Self, WorkspaceError> {
        let base = parse_server_url(server)?;
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // A redirect would carry the bearer token to wherever it points.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| WorkspaceError::Network(e.to_string()))?;
        Ok(Self { http, base, token })
    }

    pub fn token(&self) -> Option<&Secret> {
        self.token.as_ref()
    }

    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        if let Ok(mut s) = url.path_segments_mut() {
            s.pop_if_empty().extend(segments);
        }
        url
    }

    fn request(&self, method: Method, segments: &[&str]) -> RequestBuilder {
        let b = self.http.request(method, self.url(segments));
        match &self.token {
            Some(t) => b.bearer_auth(t.expose()),
            None => b,
        }
    }

    async fn send(&self, req: RequestBuilder) -> Result<Value, WorkspaceError> {
        let resp = req.send().await.map_err(|e| WorkspaceError::Network(e.to_string()))?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| WorkspaceError::Network(e.to_string()))?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(WorkspaceError::Invalid("the server's answer is too large".into()));
        }
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(body);
        }
        if status == StatusCode::CONFLICT {
            let current = body.get("current").and_then(|c| serde_json::from_value(c.clone()).ok()).map(Box::new);
            return Err(WorkspaceError::Conflict { current });
        }
        let message = body.get("error").and_then(Value::as_str).unwrap_or("the server refused the request");
        Err(WorkspaceError::Server { status: status.as_u16(), message: message.to_string() })
    }

    fn field<T: serde::de::DeserializeOwned>(body: &Value, name: &str) -> Result<T, WorkspaceError> {
        body.get(name)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .ok_or_else(|| WorkspaceError::Invalid(format!("unexpected answer from the server (no \"{name}\")")))
    }

    /// Signs in; the returned client carries the new session token.
    pub async fn login(server: &str, user: &str, password: &Secret) -> Result<Self, WorkspaceError> {
        let mut c = Self::new(server, None)?;
        let body = c
            .send(
                c.request(Method::POST, &["v1", "login"]).json(&json!({ "user": user, "password": password.expose() })),
            )
            .await?;
        let token: String = Self::field(&body, "token")?;
        c.token = Some(Secret::new(token));
        Ok(c)
    }

    pub async fn logout(&self) -> Result<(), WorkspaceError> {
        self.send(self.request(Method::POST, &["v1", "logout"])).await.map(|_| ())
    }

    pub async fn workspaces(&self) -> Result<Vec<RemoteWorkspace>, WorkspaceError> {
        let body = self.send(self.request(Method::GET, &["v1", "workspaces"])).await?;
        Self::field(&body, "workspaces")
    }

    pub async fn create_workspace(&self, name: &str) -> Result<RemoteWorkspace, WorkspaceError> {
        let body = self.send(self.request(Method::POST, &["v1", "workspaces"]).json(&json!({ "name": name }))).await?;
        serde_json::from_value(body).map_err(|e| WorkspaceError::Invalid(e.to_string()))
    }

    pub async fn details(&self, ws: &str) -> Result<WorkspaceDetails, WorkspaceError> {
        let body = self.send(self.request(Method::GET, &["v1", "workspaces", ws])).await?;
        serde_json::from_value(body).map_err(|e| WorkspaceError::Invalid(e.to_string()))
    }

    pub async fn delete_workspace(&self, ws: &str) -> Result<(), WorkspaceError> {
        self.send(self.request(Method::DELETE, &["v1", "workspaces", ws])).await.map(|_| ())
    }

    pub async fn set_member(&self, ws: &str, user: &str, role: Role) -> Result<(), WorkspaceError> {
        let req = self.request(Method::PUT, &["v1", "workspaces", ws, "members", user]).json(&json!({ "role": role }));
        self.send(req).await.map(|_| ())
    }

    pub async fn remove_member(&self, ws: &str, user: &str) -> Result<(), WorkspaceError> {
        self.send(self.request(Method::DELETE, &["v1", "workspaces", ws, "members", user])).await.map(|_| ())
    }

    pub async fn connections(&self, ws: &str) -> Result<Vec<RemoteConnection>, WorkspaceError> {
        let body = self.send(self.request(Method::GET, &["v1", "workspaces", ws, "connections"])).await?;
        Self::field(&body, "connections")
    }

    /// Shares (`base_rev == 0`) or updates a connection; returns its new revision.
    pub async fn push(&self, ws: &str, cfg: &ConnectionConfig, base_rev: u64) -> Result<u64, WorkspaceError> {
        let data = to_shared(cfg)?;
        let id = cfg.id.to_string();
        let req = self
            .request(Method::PUT, &["v1", "workspaces", ws, "connections", &id])
            .json(&json!({ "base_rev": base_rev, "data": data }));
        let body = self.send(req).await?;
        Self::field(&body, "rev")
    }

    pub async fn unshare(&self, ws: &str, id: Uuid, base_rev: u64) -> Result<(), WorkspaceError> {
        let req = self
            .request(Method::DELETE, &["v1", "workspaces", ws, "connections", &id.to_string()])
            .query(&[("base_rev", base_rev)]);
        self.send(req).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SshAuth, SshConfig};

    fn pg() -> ConnectionConfig {
        let mut c = ConnectionConfig::new(Engine::Postgres, "prod");
        c.host = "db.example.com".into();
        c.port = 5432;
        c.user = "app".into();
        c.save_password = true;
        c.ssl_ca_file = "/Users/me/ca.pem".into();
        c.init_sql = "SET search_path = x".into();
        c.ssh = Some(SshConfig {
            host: "bastion".into(),
            port: 22,
            user: "me".into(),
            auth: SshAuth::PrivateKey { path: "/Users/me/.ssh/id".into() },
            save_secret: true,
            trust_new_host_key: true,
        });
        c
    }

    #[test]
    fn shared_form_has_no_local_paths_or_executable_sql() {
        let v = to_shared(&pg()).unwrap();
        let text = v.to_string();
        for leak in ["/Users/me", "search_path", "trust_new_host_key", "PrivateKey"] {
            assert!(!text.contains(leak), "{leak} leaked: {text}");
        }
        assert!(v.get("save_password").is_none());
        assert_eq!(v["host"], json!("db.example.com"));
        assert!(to_shared(&ConnectionConfig::new(Engine::Sqlite, "f")).is_err());
    }

    #[test]
    fn import_keeps_local_settings_and_ignores_hostile_remote_ones() {
        let local = pg();
        let mut remote = to_shared(&local).unwrap();
        remote["host"] = json!("new.example.com");
        // A hostile or buggy copy tries to bring its own SQL, CA and TOFU flag along.
        remote["init_sql"] = json!("DROP TABLE users");
        remote["ssl_ca_file"] = json!("/tmp/evil.pem");
        remote["save_password"] = json!(true);
        remote["ssh"]["trust_new_host_key"] = json!(true);
        let mut me = local.clone();
        me.init_sql = "SET mine = 1".into();
        me.ssh.as_mut().unwrap().trust_new_host_key = false;
        let got = from_shared(local.id, &remote, Some(&me)).unwrap();
        assert_eq!(got.host, "new.example.com");
        assert_eq!(got.init_sql, "SET mine = 1");
        assert_eq!(got.ssl_ca_file, "/Users/me/ca.pem");
        assert!(got.save_password, "the local preference is kept");
        assert!(!got.ssh.as_ref().unwrap().trust_new_host_key);
        assert_eq!(got.ssh.unwrap().auth, SshAuth::PrivateKey { path: "/Users/me/.ssh/id".into() });
        // Brand new on this machine: nothing local to inherit.
        let fresh = from_shared(local.id, &remote, None).unwrap();
        assert!(fresh.init_sql.is_empty() && fresh.ssl_ca_file.is_empty() && !fresh.save_password);
        assert_eq!(fresh.ssh.unwrap().auth, SshAuth::PrivateKey { path: String::new() });
    }

    #[test]
    fn pull_adds_updates_and_unlinks() {
        let a = pg();
        let mut b = pg();
        b.id = Uuid::new_v4();
        b.name = "gone".into();
        let mut local = vec![b.clone()];
        let mut link = WorkspaceLink::default();
        link.revs.insert(b.id, 3);
        let remote = vec![RemoteConnection {
            id: a.id.to_string(),
            rev: 7,
            data: to_shared(&a).unwrap(),
            updated_by: "x".into(),
        }];
        let r = apply_pull(&mut local, &mut link, &remote);
        assert_eq!((r.added, r.updated), (1, 0));
        assert_eq!(r.unshared, vec!["gone".to_string()]);
        assert_eq!(link.revs.get(&a.id), Some(&7));
        assert!(!link.revs.contains_key(&b.id) && local.len() == 2, "an unshared connection stays local");
        let bad = vec![RemoteConnection { id: "nope".into(), rev: 1, data: json!({}), updated_by: String::new() }];
        assert_eq!(apply_pull(&mut local, &mut link, &bad).skipped.len(), 1);
    }

    #[test]
    fn plain_http_is_only_for_this_machine() {
        assert!(parse_server_url("https://talus.example.com").is_ok());
        assert!(parse_server_url("http://127.0.0.1:8787").is_ok());
        assert!(parse_server_url("http://localhost:8787").is_ok());
        assert!(parse_server_url("http://talus.example.com").is_err());
        assert!(parse_server_url("ftp://x").is_err());
    }
}
