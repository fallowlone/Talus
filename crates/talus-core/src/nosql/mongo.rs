//! MongoDB session: databases, collections, filtered/sorted document pages, document edits by
//! `_id`, indexes and raw commands. Documents travel as relaxed Extended JSON text so the UI
//! never has to know BSON.

use std::time::Duration;

use futures_util::TryStreamExt;
use mongodb::bson::{self, Bson, Document};
use mongodb::options::{ClientOptions, Credential, ServerAddress, Tls, TlsOptions};
use mongodb::{Client, IndexModel};

use crate::error::{DbError, DbResult};
use crate::model::{ConnectionConfig, SslMode};
use crate::secret::Secret;

pub struct MongoSession {
    client: Client,
    /// Database used when none is named.
    default_db: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionInfo {
    pub name: String,
    /// `collection`, `view` or `timeseries`.
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfoDoc {
    pub name: String,
    /// Key pattern as JSON, e.g. `{"a":1}`.
    pub keys: String,
    pub unique: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FindSpec {
    /// Extended JSON object; empty = match everything.
    pub filter: String,
    pub sort: String,
    pub skip: u64,
    pub limit: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocPage {
    /// Each document as pretty-printed relaxed Extended JSON.
    pub docs: Vec<String>,
    /// `_id` of each document as Extended JSON (`"x"`, `{"$oid":"…"}`, …).
    pub ids: Vec<String>,
    pub total: u64,
}

fn map_err(e: mongodb::error::Error) -> DbError {
    use mongodb::error::ErrorKind as K;
    match &*e.kind {
        K::Io(_) | K::ConnectionPoolCleared { .. } => DbError::ConnectionLost(e.to_string()),
        K::ServerSelection { .. } => DbError::Connect(e.to_string()),
        K::Authentication { message, .. } => DbError::Connect(message.clone()),
        K::Command(c) => DbError::Server {
            message: c.message.clone(),
            code: Some(format!("{} ({})", c.code, c.code_name)),
            position: None,
        },
        K::Write(w) => DbError::server(format!("{w:?}")),
        _ => DbError::server(e.to_string()),
    }
}

/// Parses `{…}` Extended JSON into a BSON document; empty text is an empty document.
pub fn parse_doc(text: &str) -> DbResult<Document> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(Document::new());
    }
    let v: serde_json::Value = serde_json::from_str(t).map_err(|e| DbError::Invalid(format!("invalid JSON: {e}")))?;
    match Bson::try_from(v).map_err(|e| DbError::Invalid(format!("invalid Extended JSON: {e}")))? {
        Bson::Document(d) => Ok(d),
        _ => Err(DbError::Invalid("expected a JSON object { … }".into())),
    }
}

fn to_json(b: Bson) -> String {
    serde_json::to_string_pretty(&b.into_relaxed_extjson()).unwrap_or_default()
}

fn compact(b: Bson) -> String {
    serde_json::to_string(&b.into_relaxed_extjson()).unwrap_or_default()
}

impl MongoSession {
    pub async fn connect(cfg: &ConnectionConfig, password: Option<&Secret>, _database: Option<&str>) -> DbResult<Self> {
        let timeout = Duration::from_secs(cfg.connect_timeout_secs.max(1));
        let mut opts = ClientOptions::default();
        opts.hosts = vec![ServerAddress::Tcp { host: cfg.host.clone(), port: Some(cfg.port) }];
        opts.app_name = Some("Talus".into());
        opts.connect_timeout = Some(timeout);
        opts.server_selection_timeout = Some(timeout);
        // One connection is enough for a desktop client and keeps session counts honest.
        opts.max_pool_size = Some(4);
        opts.direct_connection = Some(true);
        let auth_db = if cfg.database.is_empty() { "admin".to_string() } else { cfg.database.clone() };
        if !cfg.user.is_empty() {
            opts.credential = Some(
                Credential::builder()
                    .username(cfg.user.clone())
                    .password(password.map(|p| p.expose().to_string()))
                    .source(auth_db.clone())
                    .build(),
            );
        }
        if cfg.ssl_mode != SslMode::Disable {
            let mut tls = TlsOptions::default();
            if cfg.ssl_mode != SslMode::VerifyFull {
                tls.allow_invalid_certificates = Some(true);
            } else if !cfg.ssl_ca_file.is_empty() {
                tls.ca_file_path = Some(cfg.ssl_ca_file.clone().into());
            }
            opts.tls = Some(Tls::Enabled(tls));
        }
        let client = Client::with_options(opts).map_err(|e| DbError::Connect(e.to_string()))?;
        // Clients connect lazily: ping proves the server, credentials and TLS right now.
        client.database("admin").run_command(bson::doc! { "ping": 1 }).await.map_err(|e| match map_err(e) {
            DbError::Server { message, .. } => DbError::Connect(message),
            DbError::Connect(m) => DbError::Connect(m),
            other => DbError::Connect(other.to_string()),
        })?;
        Ok(Self { client, default_db: auth_db })
    }

    pub fn default_database(&self) -> &str {
        &self.default_db
    }

    pub async fn server_version(&mut self) -> DbResult<String> {
        let d = self.client.database("admin").run_command(bson::doc! { "buildInfo": 1 }).await.map_err(map_err)?;
        Ok(format!("MongoDB {}", d.get_str("version").unwrap_or("?")))
    }

    pub async fn databases(&mut self) -> DbResult<Vec<String>> {
        let mut names = self.client.list_database_names().await.map_err(map_err)?;
        names.sort();
        Ok(names)
    }

    pub async fn collections(&mut self, db: &str) -> DbResult<Vec<CollectionInfo>> {
        let mut specs: Vec<CollectionInfo> = self
            .client
            .database(db)
            .list_collections()
            .await
            .map_err(map_err)?
            .try_collect::<Vec<_>>()
            .await
            .map_err(map_err)?
            .into_iter()
            .map(|c| CollectionInfo { name: c.name, kind: format!("{:?}", c.collection_type).to_lowercase() })
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(specs)
    }

    pub async fn find(&mut self, db: &str, coll: &str, spec: &FindSpec) -> DbResult<DocPage> {
        let c = self.client.database(db).collection::<Document>(coll);
        let filter = parse_doc(&spec.filter)?;
        let total = c.count_documents(filter.clone()).await.map_err(map_err)?;
        let mut q = c.find(filter).skip(spec.skip).limit(spec.limit.max(1));
        let sort = parse_doc(&spec.sort)?;
        if !sort.is_empty() {
            q = q.sort(sort);
        }
        let docs: Vec<Document> = q.await.map_err(map_err)?.try_collect().await.map_err(map_err)?;
        let ids = docs.iter().map(|d| d.get("_id").cloned().map_or_else(String::new, compact)).collect();
        let docs = docs.into_iter().map(|d| to_json(Bson::Document(d))).collect();
        Ok(DocPage { docs, ids, total })
    }

    /// Inserts one document; returns its `_id` as Extended JSON.
    pub async fn insert(&mut self, db: &str, coll: &str, json: &str) -> DbResult<String> {
        let doc = parse_doc(json)?;
        let r = self.client.database(db).collection::<Document>(coll).insert_one(doc).await.map_err(map_err)?;
        Ok(compact(r.inserted_id))
    }

    /// Replaces the document whose `_id` is `id_json` with `json` (which must keep that `_id`).
    pub async fn replace(&mut self, db: &str, coll: &str, id_json: &str, json: &str) -> DbResult<()> {
        let id = id_value(id_json)?;
        let doc = parse_doc(json)?;
        if let Some(new_id) = doc.get("_id")
            && *new_id != id
        {
            return Err(DbError::Invalid("the _id of a document cannot be changed".into()));
        }
        let r = self
            .client
            .database(db)
            .collection::<Document>(coll)
            .replace_one(bson::doc! { "_id": id }, doc)
            .await
            .map_err(map_err)?;
        if r.matched_count != 1 {
            return Err(DbError::Invalid("the document no longer exists (it was deleted or its _id changed)".into()));
        }
        Ok(())
    }

    pub async fn delete(&mut self, db: &str, coll: &str, id_json: &str) -> DbResult<bool> {
        let id = id_value(id_json)?;
        let r = self
            .client
            .database(db)
            .collection::<Document>(coll)
            .delete_one(bson::doc! { "_id": id })
            .await
            .map_err(map_err)?;
        Ok(r.deleted_count == 1)
    }

    pub async fn create_collection(&mut self, db: &str, name: &str) -> DbResult<()> {
        self.client.database(db).create_collection(name).await.map_err(map_err)
    }

    pub async fn drop_collection(&mut self, db: &str, name: &str) -> DbResult<()> {
        self.client.database(db).collection::<Document>(name).drop().await.map_err(map_err)
    }

    pub async fn drop_database(&mut self, db: &str) -> DbResult<()> {
        self.client.database(db).drop().await.map_err(map_err)
    }

    pub async fn indexes(&mut self, db: &str, coll: &str) -> DbResult<Vec<IndexInfoDoc>> {
        let list: Vec<IndexModel> = self
            .client
            .database(db)
            .collection::<Document>(coll)
            .list_indexes()
            .await
            .map_err(map_err)?
            .try_collect()
            .await
            .map_err(map_err)?;
        Ok(list
            .into_iter()
            .map(|m| IndexInfoDoc {
                name: m.options.as_ref().and_then(|o| o.name.clone()).unwrap_or_default(),
                keys: compact(Bson::Document(m.keys)),
                unique: m.options.as_ref().and_then(|o| o.unique).unwrap_or(false),
            })
            .collect())
    }

    pub async fn create_index(&mut self, db: &str, coll: &str, keys_json: &str, unique: bool) -> DbResult<String> {
        let keys = parse_doc(keys_json)?;
        if keys.is_empty() {
            return Err(DbError::Invalid("an index needs at least one key, e.g. {\"field\": 1}".into()));
        }
        let model = IndexModel::builder()
            .keys(keys)
            .options(mongodb::options::IndexOptions::builder().unique(Some(unique)).build())
            .build();
        let r = self.client.database(db).collection::<Document>(coll).create_index(model).await.map_err(map_err)?;
        Ok(r.index_name)
    }

    pub async fn drop_index(&mut self, db: &str, coll: &str, name: &str) -> DbResult<()> {
        self.client.database(db).collection::<Document>(coll).drop_index(name).await.map_err(map_err)
    }

    /// Runs a database command given as Extended JSON; returns the reply as JSON.
    pub async fn command(&mut self, db: &str, json: &str) -> DbResult<String> {
        let cmd = parse_doc(json)?;
        if cmd.is_empty() {
            return Err(DbError::Invalid("enter a command document, e.g. {\"ping\": 1}".into()));
        }
        let r = self.client.database(db).run_command(cmd).await.map_err(map_err)?;
        Ok(to_json(Bson::Document(r)))
    }

    pub async fn close(self) -> DbResult<()> {
        Ok(())
    }
}

/// `_id` as Extended JSON text (`"abc"`, `5`, `{"$oid":"…"}`) → BSON.
fn id_value(json: &str) -> DbResult<Bson> {
    let v: serde_json::Value =
        serde_json::from_str(json.trim()).map_err(|e| DbError::Invalid(format!("invalid _id: {e}")))?;
    Bson::try_from(v).map_err(|e| DbError::Invalid(format!("invalid _id: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_parse_extended_json_and_reject_non_objects() {
        let d = parse_doc(
            r#"{"_id": {"$oid": "507f1f77bcf86cd799439011"}, "n": 3, "d": {"$date": "2024-01-02T03:04:05Z"}}"#,
        )
        .unwrap();
        assert!(matches!(d.get("_id"), Some(Bson::ObjectId(_))));
        assert!(matches!(d.get("d"), Some(Bson::DateTime(_))));
        assert!(parse_doc("").unwrap().is_empty());
        assert!(parse_doc("[1]").is_err());
        assert!(parse_doc("{oops").is_err());
    }

    #[test]
    fn ids_roundtrip_through_json() {
        let oid = Bson::ObjectId(bson::oid::ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap());
        let text = compact(oid.clone());
        assert_eq!(text, r#"{"$oid":"507f1f77bcf86cd799439011"}"#);
        assert_eq!(id_value(&text).unwrap(), oid);
        assert_eq!(id_value("\"abc\"").unwrap(), Bson::String("abc".into()));
        assert_eq!(id_value("5").unwrap(), Bson::Int32(5));
    }
}
