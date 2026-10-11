//! Redis session: key browsing (SCAN, never KEYS), typed value read/write, TTL and a raw command
//! console. Every value is handled as bytes and shown as text; binary content is flagged.

use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis::{AsyncConnectionConfig, Client, ConnectionAddr, IntoConnectionInfo, RedisConnectionInfo, Value};

use crate::error::{DbError, DbResult};
use crate::model::{ConnectionConfig, SslMode};
use crate::secret::Secret;

/// Elements read per collection key; larger collections are truncated (and say so).
pub const MAX_ELEMENTS: usize = 1000;

pub struct RedisSession {
    conn: MultiplexedConnection,
    db: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    pub key: String,
    /// `string`, `list`, `set`, `zset`, `hash`, `stream`, …
    pub ty: String,
    /// Remaining time to live; `None` = no expiry.
    pub ttl_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RedisValue {
    /// `binary` when the bytes are not valid UTF-8 (shown as hex).
    Text {
        text: String,
        binary: bool,
    },
    List(Vec<String>),
    Set(Vec<String>),
    ZSet(Vec<(String, f64)>),
    Hash(Vec<(String, String)>),
    Stream(Vec<(String, Vec<(String, String)>)>),
    /// A type this client only reports.
    Other(String),
    Missing,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValueView {
    pub value: RedisValue,
    /// Number of elements/bytes in the key (for the truncation notice).
    pub size: u64,
    pub truncated: bool,
}

fn map_err(e: redis::RedisError) -> DbError {
    if e.is_io_error() || e.is_connection_dropped() {
        DbError::ConnectionLost(e.to_string())
    } else if e.is_timeout() {
        DbError::Io("timed out waiting for Redis".into())
    } else {
        DbError::Server {
            message: e.detail().map_or_else(|| e.to_string(), str::to_string),
            code: Some(format!("{:?}", e.kind())),
            position: None,
        }
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::BulkString(b) => String::from_utf8(b.clone()).unwrap_or_else(|e| crate::engine::hex(e.as_bytes())),
        Value::SimpleString(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Okay => "OK".into(),
        Value::Nil => String::new(),
        Value::VerbatimString { text, .. } => text.clone(),
        other => format!("{other:?}"),
    }
}

fn is_binary(v: &Value) -> bool {
    matches!(v, Value::BulkString(b) if std::str::from_utf8(b).is_err())
}

fn items(v: Value) -> Vec<Value> {
    match v {
        Value::Array(a) | Value::Set(a) => a,
        _ => Vec::new(),
    }
}

/// `redis-cli` style rendering of any reply.
pub fn render_reply(v: &Value) -> String {
    fn go(v: &Value, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        match v {
            Value::Nil => out.push_str(&format!("{pad}(nil)\n")),
            Value::Int(i) => out.push_str(&format!("{pad}(integer) {i}\n")),
            Value::Array(a) | Value::Set(a) if a.is_empty() => out.push_str(&format!("{pad}(empty list)\n")),
            Value::Array(a) | Value::Set(a) => {
                for (i, x) in a.iter().enumerate() {
                    match x {
                        Value::Array(_) | Value::Set(_) | Value::Map(_) => {
                            out.push_str(&format!("{pad}{})\n", i + 1));
                            go(x, depth + 1, out);
                        }
                        _ => {
                            let mut s = String::new();
                            go(x, 0, &mut s);
                            out.push_str(&format!("{pad}{}) {}", i + 1, s));
                        }
                    }
                }
            }
            Value::Map(m) => {
                for (k, x) in m {
                    out.push_str(&format!("{pad}{} =>\n", text_of(k)));
                    go(x, depth + 1, out);
                }
            }
            Value::ServerError(e) => out.push_str(&format!("{pad}(error) {}\n", e.details().unwrap_or("error"))),
            other => out.push_str(&format!("{pad}\"{}\"\n", text_of(other))),
        }
    }
    let mut s = String::new();
    go(v, 0, &mut s);
    s
}

/// Splits a console line like `SET "my key" 'a b'` into arguments (backslash escapes inside quotes).
pub fn parse_command_line(line: &str) -> DbResult<Vec<String>> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), '\\') => match chars.next() {
                Some('n') => cur.push('\n'),
                Some('t') => cur.push('\t'),
                Some(other) => cur.push(other),
                None => return Err(DbError::Invalid("dangling backslash".into())),
            },
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_token = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_token {
                    args.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_token = true;
            }
        }
    }
    if quote.is_some() {
        return Err(DbError::Invalid("unterminated quote".into()));
    }
    if in_token {
        args.push(cur);
    }
    Ok(args)
}

impl RedisSession {
    pub async fn connect(cfg: &ConnectionConfig, password: Option<&Secret>, database: Option<&str>) -> DbResult<Self> {
        let db: i64 = database
            .or(Some(cfg.database.as_str()))
            .map(|d| d.trim_start_matches("db"))
            .filter(|d| !d.is_empty())
            .map_or(Ok(0), str::parse)
            .map_err(|_| DbError::Invalid("the Redis database must be a number (0, 1, …)".into()))?;
        let addr = if cfg.ssl_mode == SslMode::Disable {
            ConnectionAddr::Tcp(cfg.host.clone(), cfg.port)
        } else {
            ConnectionAddr::TcpTls {
                host: cfg.host.clone(),
                port: cfg.port,
                insecure: cfg.ssl_mode != SslMode::VerifyFull,
                tls_params: None,
            }
        };
        let mut redis = RedisConnectionInfo::default().set_db(db);
        if !cfg.user.is_empty() && cfg.user != "default" {
            redis = redis.set_username(&cfg.user);
        } else if password.is_some() && !cfg.user.is_empty() {
            redis = redis.set_username("default");
        }
        if let Some(p) = password.filter(|p| !p.expose().is_empty()) {
            redis = redis.set_password(p.expose());
        }
        let info = addr.into_connection_info().map_err(|e| DbError::Connect(e.to_string()))?.set_redis_settings(redis);
        let client = Client::open(info).map_err(|e| DbError::Connect(e.to_string()))?;
        let timeout = Duration::from_secs(cfg.connect_timeout_secs.max(1));
        let conn_cfg = AsyncConnectionConfig::new()
            .set_connection_timeout(Some(timeout))
            .set_response_timeout(Some(Duration::from_secs(60)));
        let conn = client
            .get_multiplexed_async_connection_with_config(&conn_cfg)
            .await
            .map_err(|e| DbError::Connect(e.detail().map_or_else(|| e.to_string(), str::to_string)))?;
        Ok(Self { conn, db })
    }

    pub fn database(&self) -> i64 {
        self.db
    }

    async fn call(&mut self, args: &[&[u8]]) -> DbResult<Value> {
        let mut cmd = redis::cmd(std::str::from_utf8(args[0]).unwrap_or(""));
        for a in &args[1..] {
            cmd.arg(*a);
        }
        let v: Value = cmd.query_async(&mut self.conn).await.map_err(map_err)?;
        Ok(v)
    }

    async fn text_call(&mut self, args: &[&str]) -> DbResult<Value> {
        let bytes: Vec<&[u8]> = args.iter().map(|s| s.as_bytes()).collect();
        self.call(&bytes).await
    }

    pub async fn server_version(&mut self) -> DbResult<String> {
        let info = text_of(&self.text_call(&["INFO", "server"]).await?);
        let v = info.lines().find_map(|l| l.strip_prefix("redis_version:")).unwrap_or("?").trim().to_string();
        Ok(format!("Redis {v}"))
    }

    /// `db0 … dbN` as configured on the server (16 when CONFIG is not allowed).
    pub async fn databases(&mut self) -> DbResult<Vec<String>> {
        let n = match self.text_call(&["CONFIG", "GET", "databases"]).await {
            Ok(v) => items(v).get(1).map(text_of).and_then(|s| s.parse::<usize>().ok()).unwrap_or(16),
            Err(_) => 16,
        };
        Ok((0..n).map(|i| format!("db{i}")).collect())
    }

    pub async fn dbsize(&mut self) -> DbResult<u64> {
        match self.text_call(&["DBSIZE"]).await? {
            Value::Int(n) => Ok(n.max(0) as u64),
            other => Err(DbError::server(format!("unexpected DBSIZE reply: {other:?}"))),
        }
    }

    /// One SCAN step: `(next cursor, keys)`; cursor 0 ends the scan.
    pub async fn scan(&mut self, cursor: u64, pattern: &str, count: usize) -> DbResult<(u64, Vec<KeyInfo>)> {
        let cur = cursor.to_string();
        let cnt = count.to_string();
        let pat = if pattern.trim().is_empty() { "*" } else { pattern };
        let reply = self.text_call(&["SCAN", &cur, "MATCH", pat, "COUNT", &cnt]).await?;
        let mut parts = items(reply).into_iter();
        let next: u64 = parts.next().map(|v| text_of(&v)).and_then(|s| s.parse().ok()).unwrap_or(0);
        let keys: Vec<String> = items(parts.next().unwrap_or(Value::Nil)).iter().map(text_of).collect();
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let ty = text_of(&self.call(&[b"TYPE", key.as_bytes()]).await?);
            let ttl = match self.call(&[b"PTTL", key.as_bytes()]).await? {
                Value::Int(n) if n >= 0 => Some(n),
                _ => None,
            };
            out.push(KeyInfo { key, ty, ttl_ms: ttl });
        }
        Ok((next, out))
    }

    pub async fn get(&mut self, key: &str) -> DbResult<ValueView> {
        let ty = text_of(&self.call(&[b"TYPE", key.as_bytes()]).await?);
        let k = key.as_bytes();
        let cap = (MAX_ELEMENTS - 1).to_string();
        let n = MAX_ELEMENTS.to_string();
        let (value, size) = match ty.as_str() {
            "none" => (RedisValue::Missing, 0),
            "string" => {
                let v = self.call(&[b"GET", k]).await?;
                let binary = is_binary(&v);
                let len = match &v {
                    Value::BulkString(b) => b.len() as u64,
                    _ => 0,
                };
                (RedisValue::Text { text: text_of(&v), binary }, len)
            }
            "list" => {
                let size = self.len_of(b"LLEN", k).await?;
                let v = self.call(&[b"LRANGE", k, b"0", cap.as_bytes()]).await?;
                (RedisValue::List(items(v).iter().map(text_of).collect()), size)
            }
            "set" => {
                let size = self.len_of(b"SCARD", k).await?;
                let v = self.call(&[b"SSCAN", k, b"0", b"COUNT", n.as_bytes()]).await?;
                let members = items(items(v).into_iter().nth(1).unwrap_or(Value::Nil));
                let mut m: Vec<String> = members.iter().map(text_of).collect();
                m.sort();
                m.truncate(MAX_ELEMENTS);
                (RedisValue::Set(m), size)
            }
            "zset" => {
                let size = self.len_of(b"ZCARD", k).await?;
                let v = self.call(&[b"ZRANGE", k, b"0", cap.as_bytes(), b"WITHSCORES"]).await?;
                let flat = items(v);
                let mut out = Vec::new();
                match flat.first() {
                    // RESP3 returns [[member, score], …]; RESP2 a flat list.
                    Some(Value::Array(_)) => {
                        for pair in flat {
                            let p = items(pair);
                            if let (Some(m), Some(s)) = (p.first(), p.get(1)) {
                                out.push((text_of(m), text_of(s).parse().unwrap_or(0.0)));
                            }
                        }
                    }
                    _ => {
                        for pair in flat.chunks(2) {
                            if let [m, s] = pair {
                                out.push((text_of(m), text_of(s).parse().unwrap_or(0.0)));
                            }
                        }
                    }
                }
                (RedisValue::ZSet(out), size)
            }
            "hash" => {
                let size = self.len_of(b"HLEN", k).await?;
                let v = self.call(&[b"HSCAN", k, b"0", b"COUNT", n.as_bytes()]).await?;
                let flat = items(items(v).into_iter().nth(1).unwrap_or(Value::Nil));
                let mut out: Vec<(String, String)> = flat
                    .chunks(2)
                    .filter_map(|p| if let [f, v] = p { Some((text_of(f), text_of(v))) } else { None })
                    .collect();
                out.sort();
                out.truncate(MAX_ELEMENTS);
                (RedisValue::Hash(out), size)
            }
            "stream" => {
                let size = self.len_of(b"XLEN", k).await?;
                let v = self.call(&[b"XRANGE", k, b"-", b"+", b"COUNT", n.as_bytes()]).await?;
                let entries = items(v)
                    .into_iter()
                    .map(|e| {
                        let mut it = items(e).into_iter();
                        let id = it.next().map(|v| text_of(&v)).unwrap_or_default();
                        let fields = items(it.next().unwrap_or(Value::Nil));
                        let kv = fields
                            .chunks(2)
                            .filter_map(|p| if let [f, v] = p { Some((text_of(f), text_of(v))) } else { None })
                            .collect();
                        (id, kv)
                    })
                    .collect();
                (RedisValue::Stream(entries), size)
            }
            other => (RedisValue::Other(other.to_string()), 0),
        };
        let shown = match &value {
            RedisValue::List(v) | RedisValue::Set(v) => v.len() as u64,
            RedisValue::ZSet(v) => v.len() as u64,
            RedisValue::Hash(v) => v.len() as u64,
            RedisValue::Stream(v) => v.len() as u64,
            _ => size,
        };
        Ok(ValueView { truncated: shown < size, value, size })
    }

    async fn len_of(&mut self, cmd: &[u8], key: &[u8]) -> DbResult<u64> {
        Ok(match self.call(&[cmd, key]).await? {
            Value::Int(n) => n.max(0) as u64,
            _ => 0,
        })
    }

    /// SET keeping the TTL; creates the key when missing.
    pub async fn set_string(&mut self, key: &str, value: &str) -> DbResult<()> {
        self.call(&[b"SET", key.as_bytes(), value.as_bytes(), b"KEEPTTL"]).await.map(|_| ())
    }

    pub async fn delete(&mut self, key: &str) -> DbResult<bool> {
        Ok(matches!(self.call(&[b"UNLINK", key.as_bytes()]).await?, Value::Int(n) if n > 0))
    }

    pub async fn rename(&mut self, key: &str, new_key: &str) -> DbResult<()> {
        // NX variant: renaming onto an existing key would silently destroy it.
        match self.call(&[b"RENAMENX", key.as_bytes(), new_key.as_bytes()]).await? {
            Value::Int(1) => Ok(()),
            _ => Err(DbError::Invalid(format!("a key named {new_key:?} already exists"))),
        }
    }

    /// Sets the expiry in seconds; `None` removes it.
    pub async fn set_ttl(&mut self, key: &str, seconds: Option<u64>) -> DbResult<()> {
        let r = match seconds {
            Some(s) => self.call(&[b"EXPIRE", key.as_bytes(), s.to_string().as_bytes()]).await?,
            None => self.call(&[b"PERSIST", key.as_bytes()]).await?,
        };
        let _ = r;
        Ok(())
    }

    /// Element-level edits for collections (the UI offers add / remove per row).
    pub async fn hash_set(&mut self, key: &str, field: &str, value: &str) -> DbResult<()> {
        self.call(&[b"HSET", key.as_bytes(), field.as_bytes(), value.as_bytes()]).await.map(|_| ())
    }
    pub async fn hash_delete(&mut self, key: &str, field: &str) -> DbResult<()> {
        self.call(&[b"HDEL", key.as_bytes(), field.as_bytes()]).await.map(|_| ())
    }
    pub async fn list_push(&mut self, key: &str, value: &str) -> DbResult<()> {
        self.call(&[b"RPUSH", key.as_bytes(), value.as_bytes()]).await.map(|_| ())
    }
    pub async fn list_set(&mut self, key: &str, index: u64, value: &str) -> DbResult<()> {
        self.call(&[b"LSET", key.as_bytes(), index.to_string().as_bytes(), value.as_bytes()]).await.map(|_| ())
    }
    /// Removes one occurrence of `value` (LREM count 1).
    pub async fn list_remove(&mut self, key: &str, value: &str) -> DbResult<()> {
        self.call(&[b"LREM", key.as_bytes(), b"1", value.as_bytes()]).await.map(|_| ())
    }
    pub async fn set_add(&mut self, key: &str, member: &str) -> DbResult<()> {
        self.call(&[b"SADD", key.as_bytes(), member.as_bytes()]).await.map(|_| ())
    }
    pub async fn set_remove(&mut self, key: &str, member: &str) -> DbResult<()> {
        self.call(&[b"SREM", key.as_bytes(), member.as_bytes()]).await.map(|_| ())
    }
    pub async fn zset_add(&mut self, key: &str, member: &str, score: f64) -> DbResult<()> {
        self.call(&[b"ZADD", key.as_bytes(), score.to_string().as_bytes(), member.as_bytes()]).await.map(|_| ())
    }
    pub async fn zset_remove(&mut self, key: &str, member: &str) -> DbResult<()> {
        self.call(&[b"ZREM", key.as_bytes(), member.as_bytes()]).await.map(|_| ())
    }

    /// Runs a console line and renders the reply like `redis-cli`.
    pub async fn command(&mut self, line: &str) -> DbResult<String> {
        let args = parse_command_line(line)?;
        let Some(first) = args.first() else { return Err(DbError::Invalid("enter a command".into())) };
        // These change the connection's meaning under the session pool.
        if ["SELECT", "SUBSCRIBE", "PSUBSCRIBE", "MONITOR", "SHUTDOWN", "QUIT"]
            .iter()
            .any(|c| first.eq_ignore_ascii_case(c))
        {
            return Err(DbError::Invalid(format!("{} is not available in the console", first.to_uppercase())));
        }
        let bytes: Vec<&[u8]> = args.iter().map(|s| s.as_bytes()).collect();
        let v = self.call(&bytes).await?;
        Ok(render_reply(&v))
    }

    pub async fn flush_db(&mut self) -> DbResult<()> {
        self.call(&[b"FLUSHDB"]).await.map(|_| ())
    }

    pub async fn close(self) -> DbResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_split_like_a_shell() {
        assert_eq!(parse_command_line("SET k v").unwrap(), ["SET", "k", "v"]);
        assert_eq!(parse_command_line(r#"SET "my key" 'a b' "q\"x""#).unwrap(), ["SET", "my key", "a b", "q\"x"]);
        assert_eq!(parse_command_line("  GET   x  ").unwrap(), ["GET", "x"]);
        assert_eq!(parse_command_line(r#"SET e """#).unwrap(), ["SET", "e", ""]);
        assert!(parse_command_line("SET 'oops").is_err());
        assert!(parse_command_line("").unwrap().is_empty());
    }

    #[test]
    fn replies_render_like_redis_cli() {
        let v = Value::Array(vec![Value::BulkString(b"a".to_vec()), Value::Int(2), Value::Nil]);
        assert_eq!(render_reply(&v), "1) \"a\"\n2) (integer) 2\n3) (nil)\n");
        assert_eq!(render_reply(&Value::Array(vec![])), "(empty list)\n");
    }
}
