//! Accounts, sessions, workspaces and shared connection definitions, persisted as one JSON file.
//! Every operation checks authorization itself, so no endpoint can forget to.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use ring::digest::{SHA256, digest};
use ring::pbkdf2::{PBKDF2_HMAC_SHA256, derive, verify};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const MIN_PASSWORD_LEN: usize = 12;
/// OWASP's minimum for PBKDF2-HMAC-SHA256.
const PBKDF2_ITERATIONS: u32 = 600_000;
const TOKEN_TTL_SECS: u64 = 30 * 24 * 3600;
const MAX_CONNECTION_BYTES: usize = 16 * 1024;
const MAX_WORKSPACES_PER_OWNER: usize = 50;
const MAX_CONNECTIONS_PER_WORKSPACE: usize = 500;
const MAX_MEMBERS_PER_WORKSPACE: usize = 200;
const LOCKOUT_FAILURES: u32 = 5;
const LOCKOUT_SECS: u64 = 60;
const MAX_SESSIONS_PER_USER: usize = 20;
const MAX_TRACKED_FAILURES: usize = 10_000;
const MAX_JSON_DEPTH: usize = 8;

/// Declaration order is the privilege order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Editor,
    Owner,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    /// The server's current copy of a connection a write conflicted with.
    pub current: Option<Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), current: None }
    }
    fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }
    fn missing(what: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, format!("{what} not found"))
    }
    fn internal(context: &str, e: impl std::fmt::Display) -> Self {
        // The detail stays on the server; the client only learns that it failed.
        eprintln!("talus-server: {context}: {e}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct User {
    name: String,
    salt: String,
    hash: String,
    iterations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    user: String,
    expires: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Connection {
    rev: u64,
    data: Value,
    updated_by: String,
    updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Workspace {
    name: String,
    members: BTreeMap<String, Role>,
    connections: BTreeMap<String, Connection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    users: BTreeMap<String, User>,
    /// SHA-256 of the bearer token; the token itself is never stored.
    sessions: HashMap<String, Session>,
    workspaces: BTreeMap<String, Workspace>,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceSummary {
    pub id: String,
    pub name: String,
    pub role: Role,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceInfo {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub members: BTreeMap<String, Role>,
}

#[derive(Debug, Serialize)]
pub struct SharedConnection {
    pub id: String,
    pub rev: u64,
    pub data: Value,
    pub updated_by: String,
    pub updated_at: u64,
}

pub struct Store {
    path: Option<PathBuf>,
    state: Mutex<State>,
    /// Failed logins per user name: (count, time of the last one). In memory only.
    failures: Mutex<HashMap<String, (u32, u64)>>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn random_hex(len: usize) -> Result<String, ApiError> {
    let mut buf = vec![0u8; len];
    SystemRandom::new().fill(&mut buf).map_err(|_| ApiError::internal("random", "system RNG failed"))?;
    Ok(hex(&buf))
}

fn token_key(token: &str) -> String {
    hex(digest(&SHA256, token.as_bytes()).as_ref())
}

/// User names are case-insensitive: `Alice` and `alice` are one account.
pub fn normalize_user(name: &str) -> Result<String, ApiError> {
    let n = name.trim().to_ascii_lowercase();
    let ok = !n.is_empty()
        && n.len() <= 64
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'));
    if ok { Ok(n) } else { Err(ApiError::bad("user name: 1-64 characters of letters, digits and . _ - @")) }
}

fn check_text(label: &str, s: &str) -> Result<String, ApiError> {
    let s = s.trim();
    if s.is_empty() || s.chars().count() > 80 || s.chars().any(char::is_control) {
        return Err(ApiError::bad(format!("{label}: 1-80 characters, no control characters")));
    }
    Ok(s.to_string())
}

fn parse_connection_id(id: &str) -> Result<String, ApiError> {
    Uuid::parse_str(id).map(|u| u.to_string()).map_err(|_| ApiError::bad("connection id must be a UUID"))
}

/// Defense in depth: even if a client is buggy, secrets and machine-local paths never reach the
/// server, because they would leak to every member (or mean nothing on their machines).
fn check_connection_data(data: &Value) -> Result<(), ApiError> {
    fn walk(v: &Value, depth: usize) -> Result<(), ApiError> {
        if depth > MAX_JSON_DEPTH {
            return Err(ApiError::bad("connection definition is nested too deeply"));
        }
        match v {
            Value::Object(m) => m.iter().try_for_each(|(k, v)| {
                let k = k.to_ascii_lowercase();
                let secret = ["password", "secret", "passphrase", "token", "private"].iter().any(|w| k.contains(w));
                let local = matches!(k.as_str(), "path" | "key_file" | "ssl_ca_file" | "init_sql");
                if secret || local {
                    return Err(ApiError::bad(format!("field \"{k}\" is not shareable (secret or machine-local)")));
                }
                walk(v, depth + 1)
            }),
            Value::Array(a) => a.iter().try_for_each(|v| walk(v, depth + 1)),
            _ => Ok(()),
        }
    }
    if !data.is_object() {
        return Err(ApiError::bad("connection definition must be a JSON object"));
    }
    if data.to_string().len() > MAX_CONNECTION_BYTES {
        return Err(ApiError::bad("connection definition is too large"));
    }
    walk(data, 0)
}

fn derive_hash(password: &str, salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = NonZeroU32::new(iterations).unwrap_or(NonZeroU32::MIN);
    derive(PBKDF2_HMAC_SHA256, n, salt, password.as_bytes(), &mut out);
    out
}

/// What `login` verifies a password against. Unknown users get a dummy so the work, and hence the
/// timing, does not reveal whether an account exists.
pub struct Credentials {
    salt: Vec<u8>,
    hash: Vec<u8>,
    iterations: u32,
    pub exists: bool,
}

impl Credentials {
    pub fn check(&self, password: &str) -> bool {
        let n = NonZeroU32::new(self.iterations).unwrap_or(NonZeroU32::MIN);
        let ok = verify(PBKDF2_HMAC_SHA256, n, &self.salt, password.as_bytes(), &self.hash).is_ok();
        ok && self.exists
    }
}

impl Store {
    pub fn in_memory() -> Self {
        Self { path: None, state: Mutex::new(State::default()), failures: Mutex::default() }
    }

    pub fn open(dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        }
        let path = dir.join("state.json");
        let state = match fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{} is corrupt: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        Ok(Self { path: Some(path), state: Mutex::new(state), failures: Mutex::default() })
    }

    fn save(&self, st: &State) -> Result<(), ApiError> {
        let Some(path) = &self.path else { return Ok(()) };
        let bytes = serde_json::to_vec(st).map_err(|e| ApiError::internal("serialize", e))?;
        let tmp = path.with_extension("tmp");
        let write = || -> std::io::Result<()> {
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            let mut f = opts.open(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            fs::rename(&tmp, path)
        };
        write().map_err(|e| ApiError::internal("save state", e))
    }

    /// Runs `f` on a copy and keeps the result only if it succeeded and was saved.
    // ponytail: copies the whole state per write, fine for a team-sized server; a database if it grows.
    fn mutate<T>(&self, f: impl FnOnce(&mut State) -> Result<T, ApiError>) -> Result<T, ApiError> {
        let mut guard = self.state.lock().map_err(|_| ApiError::internal("lock", "poisoned"))?;
        let mut next = guard.clone();
        let out = f(&mut next)?;
        self.save(&next)?;
        *guard = next;
        Ok(out)
    }

    fn read<T>(&self, f: impl FnOnce(&State) -> Result<T, ApiError>) -> Result<T, ApiError> {
        let guard = self.state.lock().map_err(|_| ApiError::internal("lock", "poisoned"))?;
        f(&guard)
    }

    // ---------- accounts and sessions ----------

    /// Administrator operation (command line): creates the account or resets its password.
    pub fn set_user(&self, name: &str, password: &str) -> Result<(), ApiError> {
        let name = normalize_user(name)?;
        if password.chars().count() < MIN_PASSWORD_LEN {
            return Err(ApiError::bad(format!("password must be at least {MIN_PASSWORD_LEN} characters")));
        }
        let mut salt = [0u8; 16];
        SystemRandom::new().fill(&mut salt).map_err(|_| ApiError::internal("random", "system RNG failed"))?;
        let hash = derive_hash(password, &salt, PBKDF2_ITERATIONS);
        let user = User { name: name.clone(), salt: hex(&salt), hash: hex(&hash), iterations: PBKDF2_ITERATIONS };
        self.mutate(|st| {
            st.users.insert(name.clone(), user);
            // A reset password ends every session of that account.
            st.sessions.retain(|_, s| s.user != name);
            Ok(())
        })
    }

    pub fn credentials(&self, name: &str) -> Credentials {
        let found =
            normalize_user(name).ok().and_then(|n| self.read(|st| Ok(st.users.get(&n).cloned())).ok().flatten());
        match found.and_then(|u| Some((unhex(&u.salt)?, unhex(&u.hash)?, u.iterations))) {
            Some((salt, hash, iterations)) => Credentials { salt, hash, iterations, exists: true },
            None => Credentials { salt: vec![0; 16], hash: vec![0; 32], iterations: PBKDF2_ITERATIONS, exists: false },
        }
    }

    /// `Err(429)` while the name is locked out after repeated failures.
    pub fn check_lockout(&self, name: &str) -> Result<(), ApiError> {
        let key = name.trim().to_ascii_lowercase();
        let failures = self.failures.lock().map_err(|_| ApiError::internal("lock", "poisoned"))?;
        match failures.get(&key) {
            Some((n, at)) if *n >= LOCKOUT_FAILURES && now() < at + LOCKOUT_SECS => {
                Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "too many failed logins, try again later"))
            }
            _ => Ok(()),
        }
    }

    pub fn record_failure(&self, name: &str) {
        let key = name.trim().to_ascii_lowercase();
        let t = now();
        let Ok(mut failures) = self.failures.lock() else { return };
        if failures.len() >= MAX_TRACKED_FAILURES {
            failures.retain(|_, (_, at)| t < *at + LOCKOUT_SECS);
        }
        let e = failures.entry(key).or_insert((0, t));
        // An old streak does not count against a new attempt.
        if t >= e.1 + LOCKOUT_SECS {
            *e = (0, t);
        }
        *e = (e.0 + 1, t);
    }

    pub fn clear_failures(&self, name: &str) {
        if let Ok(mut f) = self.failures.lock() {
            f.remove(&name.trim().to_ascii_lowercase());
        }
    }

    pub fn issue_token(&self, name: &str) -> Result<String, ApiError> {
        let user = normalize_user(name)?;
        let token = random_hex(32)?;
        let key = token_key(&token);
        let t = now();
        self.mutate(|st| {
            st.sessions.retain(|_, s| s.expires > t);
            // Each login adds a session; the oldest go first so they cannot pile up.
            let mut mine: Vec<(u64, String)> =
                st.sessions.iter().filter(|(_, s)| s.user == user).map(|(k, s)| (s.expires, k.clone())).collect();
            mine.sort();
            let surplus = (mine.len() + 1).saturating_sub(MAX_SESSIONS_PER_USER);
            for (_, k) in mine.into_iter().take(surplus) {
                st.sessions.remove(&k);
            }
            st.sessions.insert(key, Session { user, expires: t + TOKEN_TTL_SECS });
            Ok(())
        })?;
        Ok(token)
    }

    pub fn authenticate(&self, token: &str) -> Result<String, ApiError> {
        let unauth = || ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid token");
        let key = token_key(token);
        self.read(|st| match st.sessions.get(&key) {
            Some(s) if s.expires > now() && st.users.contains_key(&s.user) => Ok(s.user.clone()),
            _ => Err(unauth()),
        })
    }

    pub fn logout(&self, token: &str) -> Result<(), ApiError> {
        let key = token_key(token);
        self.mutate(|st| {
            st.sessions.remove(&key);
            Ok(())
        })
    }

    // ---------- workspaces ----------

    pub fn list_workspaces(&self, user: &str) -> Result<Vec<WorkspaceSummary>, ApiError> {
        self.read(|st| {
            Ok(st
                .workspaces
                .iter()
                .filter_map(|(id, w)| {
                    Some(WorkspaceSummary { id: id.clone(), name: w.name.clone(), role: *w.members.get(user)? })
                })
                .collect())
        })
    }

    pub fn create_workspace(&self, user: &str, name: &str) -> Result<WorkspaceSummary, ApiError> {
        let name = check_text("workspace name", name)?;
        let id = random_hex(16)?;
        self.mutate(|st| {
            let owned = st.workspaces.values().filter(|w| w.members.get(user) == Some(&Role::Owner)).count();
            if owned >= MAX_WORKSPACES_PER_OWNER {
                return Err(ApiError::bad("workspace limit reached"));
            }
            let w = Workspace {
                name: name.clone(),
                members: BTreeMap::from([(user.to_string(), Role::Owner)]),
                connections: BTreeMap::new(),
            };
            st.workspaces.insert(id.clone(), w);
            Ok(WorkspaceSummary { id, name, role: Role::Owner })
        })
    }

    /// The member's role, or 404: whether a workspace exists is nobody's business but its members'.
    fn role_in(st: &State, user: &str, ws: &str) -> Result<Role, ApiError> {
        st.workspaces.get(ws).and_then(|w| w.members.get(user).copied()).ok_or_else(|| ApiError::missing("workspace"))
    }

    fn require(st: &State, user: &str, ws: &str, min: Role) -> Result<Role, ApiError> {
        let role = Self::role_in(st, user, ws)?;
        if role < min {
            return Err(ApiError::forbidden(format!("this needs the {min:?} role").to_lowercase()));
        }
        Ok(role)
    }

    pub fn workspace_info(&self, user: &str, ws: &str) -> Result<WorkspaceInfo, ApiError> {
        self.read(|st| {
            let role = Self::role_in(st, user, ws)?;
            let w = &st.workspaces[ws];
            Ok(WorkspaceInfo { id: ws.to_string(), name: w.name.clone(), role, members: w.members.clone() })
        })
    }

    pub fn delete_workspace(&self, user: &str, ws: &str) -> Result<(), ApiError> {
        self.mutate(|st| {
            Self::require(st, user, ws, Role::Owner)?;
            st.workspaces.remove(ws);
            Ok(())
        })
    }

    pub fn set_member(&self, actor: &str, ws: &str, member: &str, role: Role) -> Result<(), ApiError> {
        let member = normalize_user(member)?;
        self.mutate(|st| {
            Self::require(st, actor, ws, Role::Owner)?;
            if !st.users.contains_key(&member) {
                return Err(ApiError::missing("user"));
            }
            let w = st.workspaces.get_mut(ws).ok_or_else(|| ApiError::missing("workspace"))?;
            if !w.members.contains_key(&member) && w.members.len() >= MAX_MEMBERS_PER_WORKSPACE {
                return Err(ApiError::bad("member limit reached"));
            }
            let previous = w.members.insert(member.clone(), role);
            if previous == Some(Role::Owner) && role != Role::Owner && !w.members.values().any(|r| *r == Role::Owner) {
                return Err(ApiError::bad("a workspace needs at least one owner"));
            }
            Ok(())
        })
    }

    /// Owners remove anyone; every member may leave. The last owner cannot go.
    pub fn remove_member(&self, actor: &str, ws: &str, member: &str) -> Result<(), ApiError> {
        let member = normalize_user(member)?;
        self.mutate(|st| {
            if member == actor {
                Self::role_in(st, actor, ws)?;
            } else {
                Self::require(st, actor, ws, Role::Owner)?;
            }
            let w = st.workspaces.get_mut(ws).ok_or_else(|| ApiError::missing("workspace"))?;
            let removed = w.members.remove(&member).ok_or_else(|| ApiError::missing("member"))?;
            if removed == Role::Owner && !w.members.values().any(|r| *r == Role::Owner) {
                return Err(ApiError::bad("a workspace needs at least one owner"));
            }
            Ok(())
        })
    }

    // ---------- shared connections ----------

    pub fn list_connections(&self, user: &str, ws: &str) -> Result<Vec<SharedConnection>, ApiError> {
        self.read(|st| {
            Self::require(st, user, ws, Role::Viewer)?;
            Ok(st.workspaces[ws]
                .connections
                .iter()
                .map(|(id, c)| SharedConnection {
                    id: id.clone(),
                    rev: c.rev,
                    data: c.data.clone(),
                    updated_by: c.updated_by.clone(),
                    updated_at: c.updated_at,
                })
                .collect())
        })
    }

    fn conflict(c: &Connection, id: &str) -> ApiError {
        let mut e = ApiError::new(StatusCode::CONFLICT, "the connection changed on the server");
        e.current = Some(serde_json::json!({ "id": id, "rev": c.rev, "data": c.data }));
        e
    }

    /// Creates (`base_rev == 0`) or replaces a connection; replacing needs the revision the caller
    /// last saw, so two editors never silently overwrite each other.
    pub fn put_connection(&self, user: &str, ws: &str, id: &str, base_rev: u64, data: Value) -> Result<u64, ApiError> {
        let id = parse_connection_id(id)?;
        check_connection_data(&data)?;
        self.mutate(|st| {
            Self::require(st, user, ws, Role::Editor)?;
            let w = st.workspaces.get_mut(ws).ok_or_else(|| ApiError::missing("workspace"))?;
            let current = w.connections.get(&id);
            let rev = match current {
                Some(c) if c.rev != base_rev => return Err(Self::conflict(c, &id)),
                None if base_rev != 0 => return Err(ApiError::missing("connection")),
                Some(c) => c.rev + 1,
                None => {
                    if w.connections.len() >= MAX_CONNECTIONS_PER_WORKSPACE {
                        return Err(ApiError::bad("connection limit reached"));
                    }
                    1
                }
            };
            w.connections.insert(id, Connection { rev, data, updated_by: user.to_string(), updated_at: now() });
            Ok(rev)
        })
    }

    pub fn delete_connection(&self, user: &str, ws: &str, id: &str, base_rev: u64) -> Result<(), ApiError> {
        let id = parse_connection_id(id)?;
        self.mutate(|st| {
            Self::require(st, user, ws, Role::Editor)?;
            let w = st.workspaces.get_mut(ws).ok_or_else(|| ApiError::missing("workspace"))?;
            match w.connections.get(&id) {
                None => Err(ApiError::missing("connection")),
                Some(c) if c.rev != base_rev => Err(Self::conflict(c, &id)),
                Some(_) => {
                    w.connections.remove(&id);
                    Ok(())
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PW: &str = "correct horse battery";

    #[test]
    fn state_survives_a_restart_and_tokens_are_stored_hashed() {
        let dir = std::env::temp_dir().join(format!("talus-server-test-{}", Uuid::new_v4()));
        let store = Store::open(&dir).unwrap();
        store.set_user("Alice", PW).unwrap();
        let token = store.issue_token("alice").unwrap();
        let ws = store.create_workspace("alice", "Team").unwrap();
        let on_disk = fs::read_to_string(dir.join("state.json")).unwrap();
        assert!(!on_disk.contains(&token) && !on_disk.contains(PW), "neither token nor password is stored");

        let again = Store::open(&dir).unwrap();
        assert_eq!(again.authenticate(&token).unwrap(), "alice");
        assert_eq!(again.list_workspaces("alice").unwrap()[0].id, ws.id);
        assert!(again.credentials("ALICE").check(PW), "names are case-insensitive");
        assert!(!again.credentials("alice").check("wrong password!!"));
        assert!(!again.credentials("nobody").check(PW));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expired_tokens_and_reset_passwords_end_sessions() {
        let store = Store::in_memory();
        store.set_user("bob", PW).unwrap();
        let token = store.issue_token("bob").unwrap();
        assert!(store.authenticate(&token).is_ok());
        store
            .mutate(|st| {
                st.sessions.values_mut().for_each(|s| s.expires = 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(store.authenticate(&token).unwrap_err().status, StatusCode::UNAUTHORIZED);

        let token = store.issue_token("bob").unwrap();
        store.set_user("bob", "a brand new password").unwrap();
        assert_eq!(store.authenticate(&token).unwrap_err().status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn old_sessions_are_dropped_beyond_the_per_user_cap() {
        let store = Store::in_memory();
        store.set_user("dave", PW).unwrap();
        let tokens: Vec<String> = (0..MAX_SESSIONS_PER_USER).map(|_| store.issue_token("dave").unwrap()).collect();
        assert_eq!(store.read(|st| Ok(st.sessions.len())).unwrap(), MAX_SESSIONS_PER_USER);
        assert!(store.authenticate(tokens.last().unwrap()).is_ok(), "the newest session always survives");
    }

    #[test]
    fn weak_passwords_and_odd_names_are_refused() {
        let store = Store::in_memory();
        assert!(store.set_user("carol", "short").is_err());
        assert!(store.set_user("bad name", PW).is_err());
        assert!(store.set_user("", PW).is_err());
    }
}
