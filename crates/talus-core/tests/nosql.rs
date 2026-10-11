//! Redis and MongoDB sessions against real servers (see `scripts/test-env.sh`).

use talus_core::model::{ConnectionConfig, Engine, SslMode};
use talus_core::nosql::redis::{RedisSession, RedisValue};
use talus_core::{DbError, Secret};

fn target(var: &str, engine: Engine) -> Option<(ConnectionConfig, Secret)> {
    let v = std::env::var(var).ok()?;
    let p: Vec<&str> = v.splitn(5, ':').collect();
    let mut cfg = ConnectionConfig::new(engine, var);
    cfg.host = p[0].into();
    cfg.port = p[1].parse().unwrap();
    cfg.user = p[2].into();
    cfg.database = p[4].into();
    cfg.ssl_mode = SslMode::Disable;
    Some((cfg, Secret::new(p[3])))
}

fn unique(prefix: &str) -> String {
    format!("{prefix}_{}", &uuid::Uuid::new_v4().simple().to_string()[..10])
}

#[tokio::test(flavor = "multi_thread")]
async fn redis_browse_edit_and_console() {
    let Some((cfg, pw)) = target("TALUS_TEST_REDIS", Engine::Redis) else {
        eprintln!("TALUS_TEST_REDIS not set; skipping");
        return;
    };
    // A scratch database of its own keeps the shared server clean.
    let mut r = RedisSession::connect(&cfg, Some(&pw), Some("db9")).await.unwrap();
    assert_eq!(r.database(), 9);
    r.flush_db().await.unwrap();
    assert!(r.server_version().await.unwrap().starts_with("Redis 7"));
    assert!(r.databases().await.unwrap().contains(&"db0".to_string()));

    let p = unique("k");
    for line in [
        format!("SET {p}:str 'héllo wörld'"),
        format!("RPUSH {p}:list a b c"),
        format!("SADD {p}:set x y z"),
        format!("ZADD {p}:zset 1.5 one 2 two"),
        format!("HSET {p}:hash f1 v1 f2 v2"),
        format!("XADD {p}:stream * temp 21"),
    ] {
        r.command(&line).await.unwrap();
    }
    assert_eq!(r.dbsize().await.unwrap(), 6);

    // SCAN with a pattern, typed keys, no TTL.
    let mut all = Vec::new();
    let mut cur = 0;
    loop {
        let (next, mut keys) = r.scan(cur, &format!("{p}:*"), 100).await.unwrap();
        all.append(&mut keys);
        if next == 0 {
            break;
        }
        cur = next;
    }
    all.sort_by(|a, b| a.key.cmp(&b.key));
    let types: Vec<(&str, &str)> = all.iter().map(|k| (k.key.rsplit(':').next().unwrap(), k.ty.as_str())).collect();
    assert_eq!(
        types,
        [("hash", "hash"), ("list", "list"), ("set", "set"), ("str", "string"), ("stream", "stream"), ("zset", "zset")]
    );
    assert!(all.iter().all(|k| k.ttl_ms.is_none()));

    let v = r.get(&format!("{p}:str")).await.unwrap();
    assert_eq!(v.value, RedisValue::Text { text: "héllo wörld".into(), binary: false });
    assert_eq!(
        r.get(&format!("{p}:list")).await.unwrap().value,
        RedisValue::List(vec!["a".into(), "b".into(), "c".into()])
    );
    assert_eq!(
        r.get(&format!("{p}:set")).await.unwrap().value,
        RedisValue::Set(vec!["x".into(), "y".into(), "z".into()])
    );
    assert_eq!(
        r.get(&format!("{p}:zset")).await.unwrap().value,
        RedisValue::ZSet(vec![("one".into(), 1.5), ("two".into(), 2.0)])
    );
    assert_eq!(
        r.get(&format!("{p}:hash")).await.unwrap().value,
        RedisValue::Hash(vec![("f1".into(), "v1".into()), ("f2".into(), "v2".into())])
    );
    match r.get(&format!("{p}:stream")).await.unwrap().value {
        RedisValue::Stream(e) => assert_eq!(e[0].1, vec![("temp".to_string(), "21".to_string())]),
        other => panic!("{other:?}"),
    }
    assert_eq!(r.get("missing-key").await.unwrap().value, RedisValue::Missing);

    // Edits: strings keep their TTL, collections change per element.
    let s = format!("{p}:str");
    r.set_ttl(&s, Some(3600)).await.unwrap();
    r.set_string(&s, "changed").await.unwrap();
    let (_, keys) = r.scan(0, &s, 100).await.unwrap();
    assert!(keys[0].ttl_ms.is_some_and(|t| t > 3_000_000), "TTL survived SET: {keys:?}");
    r.set_ttl(&s, None).await.unwrap();
    let (_, keys) = r.scan(0, &s, 100).await.unwrap();
    assert!(keys[0].ttl_ms.is_none());

    let h = format!("{p}:hash");
    r.hash_set(&h, "f3", "v3").await.unwrap();
    r.hash_delete(&h, "f1").await.unwrap();
    assert_eq!(
        r.get(&h).await.unwrap().value,
        RedisValue::Hash(vec![("f2".into(), "v2".into()), ("f3".into(), "v3".into())])
    );
    let l = format!("{p}:list");
    r.list_push(&l, "d").await.unwrap();
    r.list_set(&l, 0, "A").await.unwrap();
    r.list_remove(&l, "b").await.unwrap();
    assert_eq!(r.get(&l).await.unwrap().value, RedisValue::List(vec!["A".into(), "c".into(), "d".into()]));
    let st = format!("{p}:set");
    r.set_add(&st, "w").await.unwrap();
    r.set_remove(&st, "x").await.unwrap();
    assert_eq!(r.get(&st).await.unwrap().value, RedisValue::Set(vec!["w".into(), "y".into(), "z".into()]));
    let z = format!("{p}:zset");
    r.zset_add(&z, "three", 3.0).await.unwrap();
    r.zset_remove(&z, "one").await.unwrap();
    assert_eq!(r.get(&z).await.unwrap().value, RedisValue::ZSet(vec![("two".into(), 2.0), ("three".into(), 3.0)]));

    // Rename refuses to overwrite; delete reports whether something went away.
    assert!(r.rename(&s, &l).await.is_err());
    r.rename(&s, &format!("{p}:renamed")).await.unwrap();
    assert!(r.delete(&format!("{p}:renamed")).await.unwrap());
    assert!(!r.delete(&format!("{p}:renamed")).await.unwrap());

    // Console: replies render like redis-cli, risky commands are refused, errors are reported.
    assert_eq!(r.command(&format!("LRANGE {l} 0 -1")).await.unwrap(), "1) \"A\"\n2) \"c\"\n3) \"d\"\n");
    assert!(r.command("SELECT 3").await.is_err());
    assert!(matches!(r.command("NOSUCHCOMMAND").await, Err(DbError::Server { .. })));
    assert!(r.command("   ").await.is_err());
    r.flush_db().await.unwrap();
    assert_eq!(r.dbsize().await.unwrap(), 0);

    // Wrong password is a clean connect error that does not echo the secret.
    let err = RedisSession::connect(&cfg, Some(&Secret::new("definitely-wrong")), None).await.err().expect("must fail");
    assert!(matches!(err, DbError::Connect(_)), "{err:?}");
    assert!(!err.to_string().contains("definitely-wrong"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mongo_documents_indexes_and_commands() {
    use talus_core::nosql::mongo::{FindSpec, MongoSession};
    let Some((cfg, pw)) = target("TALUS_TEST_MONGO", Engine::MongoDb) else {
        eprintln!("TALUS_TEST_MONGO not set; skipping");
        return;
    };
    let mut m = MongoSession::connect(&cfg, Some(&pw), None).await.unwrap();
    assert!(m.server_version().await.unwrap().starts_with("MongoDB 7"));
    let db = unique("talus_it");
    let coll = "people";
    m.create_collection(&db, coll).await.unwrap();
    assert!(m.databases().await.unwrap().contains(&db) || m.collections(&db).await.unwrap().len() == 1);

    for doc in [
        r#"{"_id": 1, "name": "Ann", "age": 31, "tags": ["a", "b"]}"#,
        r#"{"_id": 2, "name": "Bob", "age": 25, "address": {"city": "Oslo"}}"#,
        r#"{"_id": 3, "name": "Cyd", "age": 40, "born": {"$date": "1984-05-06T00:00:00Z"}}"#,
        r#"{"name": "Dee", "age": 19}"#,
    ] {
        m.insert(&db, coll, doc).await.unwrap();
    }
    assert!(m.insert(&db, coll, r#"{"_id": 1, "name": "dup"}"#).await.is_err(), "duplicate _id is refused");

    let page = m
        .find(
            &db,
            coll,
            &FindSpec { filter: r#"{"age": {"$gte": 25}}"#.into(), sort: r#"{"age": -1}"#.into(), skip: 0, limit: 2 },
        )
        .await
        .unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(page.ids, ["3", "1"]);
    assert!(page.docs[0].contains("\"name\": \"Cyd\"") && page.docs[0].contains("1984-05-06"), "{}", page.docs[0]);
    let all = m.find(&db, coll, &FindSpec { limit: 10, ..Default::default() }).await.unwrap();
    assert_eq!(all.docs.len(), 4);
    assert!(all.ids[3].starts_with("{\"$oid\":"), "generated ObjectId: {}", all.ids[3]);
    let paged = m
        .find(&db, coll, &FindSpec { sort: r#"{"_id": 1}"#.into(), skip: 3, limit: 10, ..Default::default() })
        .await
        .unwrap();
    assert_eq!(paged.docs.len(), 1);
    assert!(m.find(&db, coll, &FindSpec { filter: "{oops".into(), limit: 5, ..Default::default() }).await.is_err());

    // Edit by _id; the _id itself is immutable; a vanished document is reported.
    m.replace(&db, coll, "2", r#"{"_id": 2, "name": "Robert", "age": 26}"#).await.unwrap();
    let one =
        m.find(&db, coll, &FindSpec { filter: r#"{"_id": 2}"#.into(), limit: 1, ..Default::default() }).await.unwrap();
    assert!(one.docs[0].contains("Robert") && !one.docs[0].contains("Oslo"));
    assert!(m.replace(&db, coll, "2", r#"{"_id": 9, "name": "x"}"#).await.is_err());
    assert!(m.replace(&db, coll, "99", r#"{"name": "ghost"}"#).await.is_err());
    assert!(m.delete(&db, coll, "1").await.unwrap());
    assert!(!m.delete(&db, coll, "1").await.unwrap());

    // Indexes.
    let name = m.create_index(&db, coll, r#"{"name": 1}"#, true).await.unwrap();
    let idx = m.indexes(&db, coll).await.unwrap();
    assert!(
        idx.iter().any(|i| i.name == "_id_")
            && idx.iter().any(|i| i.name == name && i.unique && i.keys == r#"{"name":1}"#),
        "{idx:?}"
    );
    assert!(m.insert(&db, coll, r#"{"_id": 50, "name": "Robert"}"#).await.is_err(), "unique index enforced");
    m.drop_index(&db, coll, &name).await.unwrap();
    assert!(m.create_index(&db, coll, "{}", false).await.is_err());

    // Commands.
    let reply = m.command(&db, r#"{"count": "people"}"#).await.unwrap();
    assert!(reply.contains("\"n\": 3"), "{reply}");
    assert!(m.command(&db, r#"{"definitelyNotACommand": 1}"#).await.is_err());
    assert!(m.command(&db, "[]").await.is_err());

    let kinds = m.collections(&db).await.unwrap();
    assert_eq!((kinds[0].name.as_str(), kinds[0].kind.as_str()), ("people", "collection"));
    m.drop_collection(&db, coll).await.unwrap();
    m.drop_database(&db).await.unwrap();

    let err = MongoSession::connect(&cfg, Some(&Secret::new("definitely-wrong")), None).await.err().expect("must fail");
    assert!(matches!(err, DbError::Connect(_)), "{err:?}");
    assert!(!err.to_string().contains("definitely-wrong"));
}
