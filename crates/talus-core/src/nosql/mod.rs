//! Non-SQL engines: a Redis key browser and a MongoDB document browser. `NoSqlSession` gives the
//! app one entry point (like `engine::Session` for SQL); operations are plain data so they can
//! cross the worker channel.

pub mod mongo;
pub mod redis;

use crate::error::{DbError, DbResult};
use crate::model::{ConnectionConfig, Engine, ObjectKind, ObjectRef};
use crate::secret::Secret;
use mongo::{CollectionInfo, DocPage, FindSpec, IndexInfoDoc, MongoSession};
use redis::{KeyInfo, RedisSession, ValueView};

pub enum NoSqlSession {
    Redis(RedisSession),
    Mongo(MongoSession),
}

#[derive(Debug, Clone)]
pub enum NoSqlOp {
    RedisScan { cursor: u64, pattern: String, count: usize },
    RedisGet(String),
    RedisSetString { key: String, value: String },
    RedisDelete(String),
    RedisRename { key: String, new_key: String },
    RedisSetTtl { key: String, seconds: Option<u64> },
    RedisHashSet { key: String, field: String, value: String },
    RedisHashDelete { key: String, field: String },
    RedisListPush { key: String, value: String },
    RedisListSet { key: String, index: u64, value: String },
    RedisListRemove { key: String, value: String },
    RedisSetAdd { key: String, member: String },
    RedisSetRemove { key: String, member: String },
    RedisZAdd { key: String, member: String, score: f64 },
    RedisZRemove { key: String, member: String },
    RedisCommand(String),
    RedisDbSize,
    RedisFlushDb,
    MongoFind { db: String, coll: String, spec: FindSpec },
    MongoInsert { db: String, coll: String, json: String },
    MongoReplace { db: String, coll: String, id: String, json: String },
    MongoDelete { db: String, coll: String, id: String },
    MongoCreateCollection { db: String, name: String },
    MongoDropCollection { db: String, name: String },
    MongoDropDatabase(String),
    MongoIndexes { db: String, coll: String },
    MongoCreateIndex { db: String, coll: String, keys: String, unique: bool },
    MongoDropIndex { db: String, coll: String, name: String },
    MongoCommand { db: String, json: String },
}

#[derive(Debug, Clone)]
pub enum NoSqlOut {
    Done,
    Flag(bool),
    Text(String),
    Count(u64),
    Scan { cursor: u64, keys: Vec<KeyInfo> },
    Value(Box<ValueView>),
    Docs(DocPage),
    Indexes(Vec<IndexInfoDoc>),
}

/// `RedisScan { … }` → `RedisScan`.
fn op_name(op: &NoSqlOp) -> String {
    format!("{op:?}").split(['{', '(']).next().unwrap_or("this operation").trim().to_string()
}

fn wrong_engine(op: &str, engine: Engine) -> DbError {
    DbError::Invalid(format!("{op} is not available for {}", engine.label()))
}

impl NoSqlSession {
    pub async fn connect(cfg: &ConnectionConfig, password: Option<&Secret>, database: Option<&str>) -> DbResult<Self> {
        match cfg.engine {
            Engine::Redis => Ok(Self::Redis(RedisSession::connect(cfg, password, database).await?)),
            Engine::MongoDb => Ok(Self::Mongo(MongoSession::connect(cfg, password, database).await?)),
            other => Err(DbError::Invalid(format!("{} is a SQL engine", other.label()))),
        }
    }

    pub fn engine(&self) -> Engine {
        match self {
            Self::Redis(_) => Engine::Redis,
            Self::Mongo(_) => Engine::MongoDb,
        }
    }

    pub async fn server_version(&mut self) -> DbResult<String> {
        match self {
            Self::Redis(r) => r.server_version().await,
            Self::Mongo(m) => m.server_version().await,
        }
    }

    pub async fn databases(&mut self) -> DbResult<Vec<String>> {
        match self {
            Self::Redis(r) => r.databases().await,
            Self::Mongo(m) => m.databases().await,
        }
    }

    /// What the navigator lists under a database: Redis has one pseudo-object (its keys), MongoDB
    /// its collections and views.
    pub async fn objects(&mut self, database: &str) -> DbResult<Vec<ObjectRef>> {
        match self {
            Self::Redis(_) => Ok(vec![ObjectRef {
                database: database.to_string(),
                schema: None,
                name: "keys".into(),
                kind: ObjectKind::Table,
            }]),
            Self::Mongo(m) => Ok(m
                .collections(database)
                .await?
                .into_iter()
                .map(|CollectionInfo { name, kind }| ObjectRef {
                    database: database.to_string(),
                    schema: None,
                    name,
                    kind: if kind == "view" { ObjectKind::View } else { ObjectKind::Table },
                })
                .collect()),
        }
    }

    pub async fn run(&mut self, op: NoSqlOp) -> DbResult<NoSqlOut> {
        use NoSqlOp::*;
        let engine = self.engine();
        match (self, op) {
            (Self::Redis(r), op) => Ok(match op {
                RedisScan { cursor, pattern, count } => {
                    let (cursor, keys) = r.scan(cursor, &pattern, count).await?;
                    NoSqlOut::Scan { cursor, keys }
                }
                RedisGet(k) => NoSqlOut::Value(Box::new(r.get(&k).await?)),
                RedisSetString { key, value } => done(r.set_string(&key, &value).await)?,
                RedisDelete(k) => NoSqlOut::Flag(r.delete(&k).await?),
                RedisRename { key, new_key } => done(r.rename(&key, &new_key).await)?,
                RedisSetTtl { key, seconds } => done(r.set_ttl(&key, seconds).await)?,
                RedisHashSet { key, field, value } => done(r.hash_set(&key, &field, &value).await)?,
                RedisHashDelete { key, field } => done(r.hash_delete(&key, &field).await)?,
                RedisListPush { key, value } => done(r.list_push(&key, &value).await)?,
                RedisListSet { key, index, value } => done(r.list_set(&key, index, &value).await)?,
                RedisListRemove { key, value } => done(r.list_remove(&key, &value).await)?,
                RedisSetAdd { key, member } => done(r.set_add(&key, &member).await)?,
                RedisSetRemove { key, member } => done(r.set_remove(&key, &member).await)?,
                RedisZAdd { key, member, score } => done(r.zset_add(&key, &member, score).await)?,
                RedisZRemove { key, member } => done(r.zset_remove(&key, &member).await)?,
                RedisCommand(line) => NoSqlOut::Text(r.command(&line).await?),
                RedisDbSize => NoSqlOut::Count(r.dbsize().await?),
                RedisFlushDb => done(r.flush_db().await)?,
                other => {
                    return Err(wrong_engine(&op_name(&other), engine));
                }
            }),
            (Self::Mongo(m), op) => Ok(match op {
                MongoFind { db, coll, spec } => NoSqlOut::Docs(m.find(&db, &coll, &spec).await?),
                MongoInsert { db, coll, json } => NoSqlOut::Text(m.insert(&db, &coll, &json).await?),
                MongoReplace { db, coll, id, json } => done(m.replace(&db, &coll, &id, &json).await)?,
                MongoDelete { db, coll, id } => NoSqlOut::Flag(m.delete(&db, &coll, &id).await?),
                MongoCreateCollection { db, name } => done(m.create_collection(&db, &name).await)?,
                MongoDropCollection { db, name } => done(m.drop_collection(&db, &name).await)?,
                MongoDropDatabase(db) => done(m.drop_database(&db).await)?,
                MongoIndexes { db, coll } => NoSqlOut::Indexes(m.indexes(&db, &coll).await?),
                MongoCreateIndex { db, coll, keys, unique } => {
                    NoSqlOut::Text(m.create_index(&db, &coll, &keys, unique).await?)
                }
                MongoDropIndex { db, coll, name } => done(m.drop_index(&db, &coll, &name).await)?,
                MongoCommand { db, json } => NoSqlOut::Text(m.command(&db, &json).await?),
                other => {
                    return Err(wrong_engine(&op_name(&other), engine));
                }
            }),
        }
    }

    pub async fn close(self) -> DbResult<()> {
        match self {
            Self::Redis(r) => r.close().await,
            Self::Mongo(m) => m.close().await,
        }
    }
}

fn done(r: DbResult<()>) -> DbResult<NoSqlOut> {
    r.map(|()| NoSqlOut::Done)
}
