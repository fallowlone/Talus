//! Integration suite run identically against every engine.
//! SQLite always runs. PostgreSQL / MySQL / SQL Server run when `TALUS_TEST_PG` / `TALUS_TEST_MYSQL` /
//! `TALUS_TEST_MSSQL`
//! are set to `host:port:user:password:database` (see `scripts/test-env.sh`).

use std::time::{Duration, Instant};

use talus_core::engine::{ScriptOptions, Session, create_database_file};
use talus_core::model::*;
use talus_core::{DbError, Secret};

struct Target {
    cfg: ConnectionConfig,
    password: Option<Secret>,
    /// Namespace (PG schema / MySQL database / SQLite attached name) created for this run.
    ns: String,
    slow_sql: &'static str,
}

impl Target {
    fn obj(&self, name: &str, kind: ObjectKind) -> ObjectRef {
        match self.cfg.engine {
            Engine::Postgres => ObjectRef {
                database: self.cfg.database.clone(),
                schema: Some(self.ns.clone()),
                name: name.into(),
                kind,
            },
            // SQL Server: one database per run, objects live in its `dbo` schema.
            Engine::SqlServer => {
                ObjectRef { database: self.ns.clone(), schema: Some("dbo".into()), name: name.into(), kind }
            }
            _ => ObjectRef { database: self.ns.clone(), schema: None, name: name.into(), kind },
        }
    }
    fn q(&self, name: &str) -> String {
        let d = talus_core::sql::Dialect(self.cfg.engine);
        d.qualified(&self.obj(name, ObjectKind::Table)).unwrap()
    }
}

fn unique(prefix: &str) -> String {
    format!("{prefix}_{}", &uuid::Uuid::new_v4().simple().to_string()[..10])
}

fn from_env(var: &str, engine: Engine) -> Option<(ConnectionConfig, Secret)> {
    let v = std::env::var(var).ok()?;
    let p: Vec<&str> = v.splitn(5, ':').collect();
    assert_eq!(p.len(), 5, "{var} must be host:port:user:password:database");
    let mut cfg = ConnectionConfig::new(engine, var);
    cfg.host = p[0].into();
    cfg.port = p[1].parse().unwrap();
    cfg.user = p[2].into();
    cfg.database = p[4].into();
    cfg.ssl_mode = SslMode::Disable;
    Some((cfg, Secret::new(p[3])))
}

async fn sqlite_target() -> Target {
    let dir = std::env::temp_dir().join(unique("talus-it"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.sqlite");
    create_database_file(&path).unwrap();
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "it-sqlite");
    cfg.database = path.to_string_lossy().into_owned();
    Target {
        cfg,
        password: None,
        ns: "main".into(),
        slow_sql: "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c",
    }
}

async fn setup(mut t: Target) -> (Target, Session) {
    let mut s = Session::connect(&t.cfg, t.password.as_ref(), None).await.expect("connect");
    if t.cfg.engine == Engine::SqlServer {
        // A database of its own: SQL Server schemas cannot be dropped while they hold objects.
        t.ns = unique("it");
        let r = s.run_script(&format!("CREATE DATABASE [{}]", t.ns), &ScriptOptions::default()).await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        s.close().await.unwrap();
        t.cfg.database = t.ns.clone();
        let s = Session::connect(&t.cfg, t.password.as_ref(), None).await.expect("connect to the new database");
        return (t, s);
    }
    let setup_sql = match t.cfg.engine {
        Engine::Sqlite => None,
        Engine::Postgres => {
            t.ns = unique("it");
            Some(format!("CREATE SCHEMA \"{}\"", t.ns))
        }
        Engine::MySql => {
            t.ns = unique("it");
            Some(format!("CREATE DATABASE `{}`", t.ns))
        }
        Engine::SqlServer => unreachable!("handled above"),
        Engine::Redis | Engine::MongoDb => unreachable!("not a SQL engine"),
    };
    if let Some(sql) = setup_sql {
        let r = s.run_script(&sql, &ScriptOptions::default()).await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
    }
    (t, s)
}

async fn teardown(t: &Target, s: &mut Session) {
    let sql = match t.cfg.engine {
        Engine::Sqlite => return,
        Engine::Postgres => format!("DROP SCHEMA \"{}\" CASCADE", t.ns),
        Engine::MySql => format!("DROP DATABASE `{}`", t.ns),
        Engine::SqlServer => format!(
            "USE [master]; ALTER DATABASE [{0}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{0}]",
            t.ns
        ),
        Engine::Redis | Engine::MongoDb => unreachable!("not a SQL engine"),
    };
    let r = s.run_script(&sql, &ScriptOptions::default()).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
}

fn rows(o: &Outcome) -> &QueryResult {
    match o {
        Outcome::Rows(r) => r,
        other => panic!("expected rows, got {other:?}"),
    }
}

async fn suite(t: Target) {
    let engine = t.cfg.engine;
    let (t, mut s) = setup(t).await;
    let opts = ScriptOptions::default();

    // Version
    let v = s.server_version().await.unwrap();
    assert!(!v.is_empty());

    // DDL + DML + multiple results in one script
    let (customers, orders) = (t.q("customers"), t.q("orders"));
    // SQLite forbids schema-qualified names in REFERENCES.
    let customers_ref = if engine == Engine::Sqlite { "customers".to_string() } else { customers.clone() };
    let orders_ref = if engine == Engine::Sqlite { "orders".to_string() } else { orders.clone() };
    let int_pk = if engine == Engine::Sqlite { "INTEGER PRIMARY KEY" } else { "INT PRIMARY KEY" };
    let script = format!(
        "CREATE TABLE {customers} (id {int_pk}, name VARCHAR(50) NOT NULL, note VARCHAR(50));\n\
         CREATE TABLE {orders} (id {int_pk}, customer_id INT NOT NULL, total DECIMAL(10,2), FOREIGN KEY (customer_id) REFERENCES {customers_ref}(id));\n\
         CREATE INDEX ix_orders_customer ON {orders_ref} (customer_id);\n\
         INSERT INTO {customers} (id, name, note) VALUES (1, 'Ann', NULL), (2, 'Bob', 'b'), (3, 'Cid', 'c');\n\
         INSERT INTO {orders} (id, customer_id, total) VALUES (10, 1, 5.50), (11, 2, 7.25);\n\
         CREATE VIEW {} AS SELECT name FROM {customers};\n\
         SELECT id, name, note FROM {customers} ORDER BY id;\n\
         SELECT count(*) AS n FROM {orders};",
        t.q("v_names")
    );
    let r = s.run_script(&script, &opts).await;
    assert!(r.errors.is_empty(), "{engine:?}: {:?}", r.errors);
    let selects: Vec<&QueryResult> = r
        .results
        .iter()
        .filter_map(|x| match &x.outcome {
            Outcome::Rows(q) => Some(q),
            _ => None,
        })
        .collect();
    assert_eq!(selects.len(), 2);
    assert_eq!(selects[0].columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "name", "note"]);
    assert_eq!(selects[0].rows[0], vec![Some("1".into()), Some("Ann".into()), None]);
    assert_eq!(selects[1].rows[0][0].as_deref(), Some("2"));
    let inserted: Vec<u64> = r
        .results
        .iter()
        .filter_map(|x| match x.outcome {
            Outcome::Affected(n) if x.sql.starts_with("INSERT") => Some(n),
            _ => None,
        })
        .collect();
    assert_eq!(inserted, vec![3, 2]);

    // Errors: stop on first error; continue_on_error runs the rest; line reported.
    let bad = "SELECT 1;\nSELEC oops;\nSELECT 2";
    let r = s.run_script(bad, &opts).await;
    assert_eq!(r.errors.len(), 1);
    assert_eq!(r.errors[0].line, 1);
    assert!(matches!(r.errors[0].error, DbError::Server { .. }), "{:?}", r.errors[0].error);
    assert_eq!(r.results.len(), 1);
    let r = s.run_script(bad, &ScriptOptions { continue_on_error: true, ..opts }).await;
    assert_eq!((r.results.len(), r.errors.len()), (2, 1));
    if engine == Engine::Postgres {
        // Position points at the missing relation (checked before columns).
        let r = s.run_script("SELECT nope FROM nowhere_xyz", &opts).await;
        assert!(matches!(r.errors[0].error, DbError::Server { position: Some(18), .. }), "{:?}", r.errors[0].error);
    }

    // Row cap
    let r = s
        .run_script(
            &format!("SELECT * FROM {customers}"),
            &ScriptOptions { limits: FetchLimits { max_rows: 2 }, ..opts },
        )
        .await;
    let q = rows(&r.results[0].outcome);
    assert_eq!(q.rows.len(), 2);
    assert!(q.truncated);
    // Session still usable after truncation (drained correctly)
    let r = s.run_script("SELECT 42", &opts).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("42"));

    // Metadata
    let dbs = s.list_databases().await.unwrap();
    let ns_db = if engine == Engine::Postgres { t.cfg.database.clone() } else { t.ns.clone() };
    assert!(dbs.contains(&ns_db), "{dbs:?}");
    if engine == Engine::Postgres {
        assert!(s.list_schemas().await.unwrap().contains(&t.ns));
    }
    let tbl = t.obj("customers", ObjectKind::Table);
    let objs = s.list_objects(&tbl.database, tbl.schema.as_deref()).await.unwrap();
    let names: Vec<(&str, ObjectKind)> = objs.iter().map(|o| (o.name.as_str(), o.kind)).collect();
    assert_eq!(
        names,
        vec![("customers", ObjectKind::Table), ("orders", ObjectKind::Table), ("v_names", ObjectKind::View)]
    );
    let st = s.table_structure(&tbl).await.unwrap();
    assert_eq!(st.primary_key(), vec!["id"]);
    assert!(!st.column("name").unwrap().nullable);
    assert!(st.column("note").unwrap().nullable);
    let ost = s.table_structure(&t.obj("orders", ObjectKind::Table)).await.unwrap();
    assert!(ost.indexes.iter().any(|i| i.columns == ["customer_id"] && !i.unique), "{:?}", ost.indexes);
    assert_eq!(ost.foreign_keys.len(), 1, "{:?}", ost.foreign_keys);
    assert_eq!(ost.foreign_keys[0].columns, ["customer_id"]);
    assert!(ost.foreign_keys[0].ref_table.ends_with("customers"));
    let ddl = s.ddl(&tbl).await.unwrap();
    assert!(ddl.to_uppercase().contains("CREATE TABLE"), "{ddl}");
    let vddl = s.ddl(&t.obj("v_names", ObjectKind::View)).await.unwrap();
    assert!(vddl.to_uppercase().contains("VIEW"), "{vddl}");

    // Data editor paging/filter/sort
    let mut page = PageQuery {
        table: tbl.clone(),
        filters: vec![],
        sort: vec![("name".into(), SortDir::Desc)],
        offset: 0,
        limit: 2,
    };
    let p1 = s.fetch_page(&page, &st).await.unwrap();
    assert_eq!(p1.rows.iter().map(|r| r[1].clone().unwrap()).collect::<Vec<_>>(), ["Cid", "Bob"]);
    page.offset = 2;
    assert_eq!(s.fetch_page(&page, &st).await.unwrap().rows.len(), 1);
    page.offset = 0;
    page.filters = vec![Filter { enabled: true, column: "name".into(), op: FilterOp::Contains, value: "o".into() }];
    assert_eq!(s.fetch_page(&page, &st).await.unwrap().rows.len(), 1);
    page.filters = vec![Filter { enabled: true, column: "note".into(), op: FilterOp::IsNull, value: String::new() }];
    let p = s.fetch_page(&page, &st).await.unwrap();
    assert_eq!(p.rows.len(), 1);
    page.filters = vec![Filter { enabled: true, column: "id".into(), op: FilterOp::Ge, value: "2".into() }];
    assert_eq!(s.fetch_page(&page, &st).await.unwrap().rows.len(), 2);
    // Injection attempt via value is just data
    page.filters =
        vec![Filter { enabled: true, column: "name".into(), op: FilterOp::Eq, value: "x' OR '1'='1".into() }];
    assert_eq!(s.fetch_page(&page, &st).await.unwrap().rows.len(), 0);

    // apply_changes: update + insert + delete in one transaction
    let key = |id: &str| RowKey(vec![("id".into(), Some(id.into()))]);
    let cs = ChangeSet {
        updates: vec![RowUpdate {
            key: key("2"),
            values: vec![("note".into(), None), ("name".into(), Some("Bobby".into()))],
        }],
        inserts: vec![vec![("id".into(), Some("4".into())), ("name".into(), Some("Dee".into()))]],
        deletes: vec![key("3")],
    };
    let rep = s.apply_changes(&tbl, &st, &cs).await.unwrap();
    assert_eq!((rep.updated, rep.inserted, rep.deleted), (1, 1, 1));
    // Updating to the same value must still count as one matched row (MySQL CLIENT_FOUND_ROWS).
    let same = ChangeSet {
        updates: vec![RowUpdate { key: key("2"), values: vec![("name".into(), Some("Bobby".into()))] }],
        ..Default::default()
    };
    s.apply_changes(&tbl, &st, &same).await.unwrap();
    let r = s.run_script(&format!("SELECT id, name, note FROM {customers} ORDER BY id"), &opts).await;
    let all = rows(&r.results[0].outcome);
    assert_eq!(
        all.rows,
        vec![
            vec![Some("1".into()), Some("Ann".into()), None],
            vec![Some("2".into()), Some("Bobby".into()), None],
            vec![Some("4".into()), Some("Dee".into()), None],
        ]
    );

    // Failing apply rolls back everything (second update targets a missing row)
    let cs = ChangeSet {
        updates: vec![
            RowUpdate { key: key("1"), values: vec![("name".into(), Some("CHANGED".into()))] },
            RowUpdate { key: key("999"), values: vec![("name".into(), Some("ghost".into()))] },
        ],
        ..Default::default()
    };
    let err = s.apply_changes(&tbl, &st, &cs).await.unwrap_err();
    assert!(err.to_string().contains("rolled back"), "{err}");
    let r = s.run_script(&format!("SELECT name FROM {customers} WHERE id = 1"), &opts).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("Ann"));
    // Constraint violation also rolls back
    let cs = ChangeSet {
        updates: vec![RowUpdate { key: key("1"), values: vec![("name".into(), Some("CHANGED".into()))] }],
        inserts: vec![vec![("id".into(), Some("1".into())), ("name".into(), Some("dup".into()))]],
        ..Default::default()
    };
    assert!(s.apply_changes(&tbl, &st, &cs).await.is_err());
    let r = s.run_script(&format!("SELECT name FROM {customers} WHERE id = 1"), &opts).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("Ann"));

    // Cancellation of a long-running statement
    let cancel = s.cancel_handle();
    let started = Instant::now();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        cancel.cancel().await
    });
    let r = tokio::time::timeout(Duration::from_secs(20), s.run_script(t.slow_sql, &opts))
        .await
        .expect("cancel took too long");
    canceller.await.unwrap().unwrap();
    assert!(r.cancelled, "{engine:?}: {:?} {:?}", r.errors, r.results);
    assert!(started.elapsed() < Duration::from_secs(10));
    // Session is usable after cancel
    let r = s.run_script("SELECT 1", &opts).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);

    // ER model: tables only, with the orders → customers foreign key.
    let model = s.schema_model(&tbl.database, tbl.schema.as_deref()).await.unwrap();
    let names: Vec<&str> = model.iter().map(|(o, _)| o.name.as_str()).collect();
    assert_eq!(names, ["customers", "orders"]);
    assert_eq!(model[1].1.foreign_keys.len(), 1);

    // Find in database: structure and data.
    let scope = t.obj("customers", ObjectKind::Table);
    let hits = s.find(&scope.database, scope.schema.as_deref(), "NOTE", false).await.unwrap();
    assert_eq!(
        hits.iter().map(|h| (h.obj.name.as_str(), h.column.as_deref())).collect::<Vec<_>>(),
        vec![("customers", Some("note"))]
    );
    let hits = s.find(&scope.database, scope.schema.as_deref(), "bobby", true).await.unwrap();
    assert_eq!(
        hits.iter().map(|h| (h.obj.name.as_str(), h.rows)).collect::<Vec<_>>(),
        vec![("customers", Some(1)), ("v_names", Some(1))]
    );
    assert!(s.find(&scope.database, scope.schema.as_deref(), "  ", true).await.is_err());

    designer(&t, &mut s).await;
    designer_constraints(&t, &mut s).await;
    data_sync(&t, &mut s).await;
    dump_roundtrip(&t, &mut s).await;
    programmability(&t, &mut s).await;
    accounts(&t, &mut s).await;
    query_builder(&t, &mut s).await;
    sqlserver_specifics(&t, &mut s).await;
    job_actions(&t, &mut s).await;
    profiling(&t, &mut s).await;
    data_generation(&t, &mut s).await;
    modeling(&t, &mut s).await;
    import(&t, &mut s).await;
    admin(&t, &mut s).await;

    teardown(&t, &mut s).await;
    s.close().await.unwrap();
}

/// Database create/drop and the process monitor (network engines).
async fn admin(t: &Target, s: &mut Session) {
    use talus_core::sql::admin;
    let engine = t.cfg.engine;
    let Some(sql) = admin::process_list(engine) else {
        assert!(admin::create_database(engine, "x").is_err());
        return;
    };
    // A second session shows up in the process list and can be cancelled while busy.
    let mut other = Session::connect(&t.cfg, t.password.as_ref(), None).await.unwrap();
    let r = s.run_script(sql, &ScriptOptions::default()).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let q = rows(&r.results[0].outcome);
    assert_eq!(
        q.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["id", "user", "database", "client", "state", "seconds", "query"]
    );
    assert!(!q.rows.is_empty());
    let other_id = match engine {
        Engine::Postgres => other.run_script("SELECT pg_backend_pid()", &ScriptOptions::default()).await,
        Engine::SqlServer => other.run_script("SELECT @@SPID", &ScriptOptions::default()).await,
        _ => other.run_script("SELECT CONNECTION_ID()", &ScriptOptions::default()).await,
    };
    let other_id = rows(&other_id.results[0].outcome).rows[0][0].clone().unwrap();
    let slow = match engine {
        Engine::Postgres => "SELECT pg_sleep(30)",
        Engine::SqlServer => "WAITFOR DELAY '00:00:30'",
        _ => "SELECT BENCHMARK(2000000000, SHA2('x', 256))",
    };
    let busy = tokio::spawn(async move {
        let r = other.run_script(slow, &ScriptOptions::default()).await;
        (other, r)
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    // SQL Server cannot cancel one statement of another session: the whole session is ended.
    let r = s
        .run_script(&admin::kill(engine, &other_id, engine == Engine::SqlServer).unwrap(), &ScriptOptions::default())
        .await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let (other, done) =
        tokio::time::timeout(Duration::from_secs(10), busy).await.expect("killed query returns").unwrap();
    // PG reports cancellation as an error; MySQL BENCHMARK returns early (0); a killed SQL Server
    // session loses its connection.
    assert!(
        done.cancelled
            || done.connection_lost
            || done.results.first().is_some_and(|r| r.elapsed < Duration::from_secs(10)),
        "{done:?}"
    );
    let _ = other.close().await;

    let name = unique("db");
    s.execute_batch(&[admin::create_database(engine, &name).unwrap()], false).await.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&name));
    s.execute_batch(&[admin::drop_database(engine, &name).unwrap()], false).await.unwrap();
    assert!(!s.list_databases().await.unwrap().contains(&name));
}

/// CSV import: batched, one transaction, NULLs, rollback on a bad row.
async fn import(t: &Target, s: &mut Session) {
    use talus_core::import::{ImportSpec, auto_mapping, import_csv, preview_csv};
    let int = if t.cfg.engine == Engine::Sqlite { "INTEGER" } else { "int" };
    let table = t.q("imp");
    s.execute_batch(
        &[format!("CREATE TABLE {table} (id {int} PRIMARY KEY, name varchar(50) NOT NULL, qty {int})")],
        false,
    )
    .await
    .unwrap();
    let dir = std::env::temp_dir().join(unique("csv"));
    std::fs::create_dir_all(&dir).unwrap();
    let good = dir.join("good.csv");
    let mut text = String::from("name,ID,qty\n");
    for i in 1..=1200 {
        let qty = if i % 10 == 0 { String::new() } else { i.to_string() };
        text.push_str(&format!("\"item, {i}\",{i},{qty}\n"));
    }
    std::fs::write(&good, &text).unwrap();
    let pv = preview_csv(&good, b',', true, 5).unwrap();
    let obj = t.obj("imp", ObjectKind::Table);
    let st = s.table_structure(&obj).await.unwrap();
    let cols: Vec<String> = st.columns.iter().map(|c| c.name.clone()).collect();
    let mapping: Vec<(String, usize)> =
        auto_mapping(&pv.headers, &cols).into_iter().filter_map(|(c, i)| i.map(|i| (c, i))).collect();
    assert_eq!(mapping.len(), 3);
    let spec =
        ImportSpec { path: good, delimiter: b',', has_header: true, mapping: mapping.clone(), empty_as_null: true };
    let rep = import_csv(s, &obj, &st, &spec).await.unwrap();
    assert_eq!(rep.rows, 1200);
    let r =
        s.run_script(&format!("SELECT count(*), count(qty), min(name) FROM {table}"), &ScriptOptions::default()).await;
    let row = &rows(&r.results[0].outcome).rows[0];
    assert_eq!(row[0].as_deref(), Some("1200"));
    assert_eq!(row[1].as_deref(), Some("1080"), "empty cells became NULL");
    assert_eq!(row[2].as_deref(), Some("item, 1"));

    // Duplicate key in the second batch: everything is rolled back, error names the batch.
    let bad = dir.join("bad.csv");
    let mut text = String::from("name,ID,qty\n");
    for i in 2001..=2700 {
        text.push_str(&format!("n{i},{i},1\n"));
    }
    text.push_str("dup,5,1\n");
    std::fs::write(&bad, &text).unwrap();
    let spec = ImportSpec { path: bad, ..spec };
    let err = import_csv(s, &obj, &st, &spec).await.unwrap_err().to_string();
    assert!(err.contains("nothing was imported") && err.contains("rows 501–701"), "{err}");
    let r = s.run_script(&format!("SELECT count(*) FROM {table}"), &ScriptOptions::default()).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("1200"));

    // JSON: array of objects, keys matched case-insensitively, null / numbers / missing keys.
    let json = dir.join("rows.json");
    std::fs::write(
        &json,
        r#"[{"id":5001,"NAME":"j1","qty":3},{"id":5002,"name":"j2","qty":null},{"id":5003,"name":"j3"}]"#,
    )
    .unwrap();
    let rep = talus_core::import::import_json(s, &obj, &st, &json).await.unwrap();
    assert_eq!(rep.rows, 3);
    let r = s
        .run_script(&format!("SELECT name, qty FROM {table} WHERE id >= 5001 ORDER BY id"), &ScriptOptions::default())
        .await;
    let q = rows(&r.results[0].outcome);
    assert_eq!((q.rows[0][1].as_deref(), q.rows[1][1].as_deref(), q.rows[2][1].as_deref()), (Some("3"), None, None));
    std::fs::write(&json, r#"[{"id":6001,"name":"ok"},{"id":6002,"nope":1}]"#).unwrap();
    let err = talus_core::import::import_json(s, &obj, &st, &json).await.unwrap_err().to_string();
    assert!(err.contains("no column named \"nope\""), "{err}");
    std::fs::write(&json, r#"{"not":"an array"}"#).unwrap();
    assert!(talus_core::import::import_json(s, &obj, &st, &json).await.is_err());
    let r = s.run_script(&format!("SELECT count(*) FROM {table}"), &ScriptOptions::default()).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("1203"), "failed JSON imports wrote nothing");

    // XML round trip: export the table, load the file into an empty twin.
    let xml_path = dir.join("imp.xml");
    talus_core::export::export_table(s, &obj, talus_core::export::ExportFormat::Xml, &xml_path).await.unwrap();
    let twin = t.q("imp_xml");
    s.execute_batch(
        &[format!("CREATE TABLE {twin} (id {int} PRIMARY KEY, name varchar(50) NOT NULL, qty {int})")],
        false,
    )
    .await
    .unwrap();
    let twin_obj = t.obj("imp_xml", ObjectKind::Table);
    let twin_st = s.table_structure(&twin_obj).await.unwrap();
    let rep = talus_core::import::import_xml(s, &twin_obj, &twin_st, &xml_path).await.unwrap();
    assert_eq!(rep.rows, 1203);
    let r =
        s.run_script(&format!("SELECT count(*), count(qty), min(name) FROM {twin}"), &ScriptOptions::default()).await;
    let row = &rows(&r.results[0].outcome).rows[0];
    assert_eq!(
        (row[0].as_deref(), row[1].as_deref(), row[2].as_deref()),
        (Some("1203"), Some("1081"), Some("item, 1")),
        "NULLs survive the round trip"
    );
    std::fs::write(&xml_path, "<rows><row><col name=\"nope\">1</col></row></rows>").unwrap();
    assert!(talus_core::import::import_xml(s, &twin_obj, &twin_st, &xml_path).await.is_err());
}

/// Table designer plans applied for real: create, add/rename/modify/drop columns, rollback.
async fn designer(t: &Target, s: &mut Session) {
    use talus_core::sql::Dialect;
    use talus_core::sql::ddl::{ColumnDef, DesignColumn, alter_table, create_table};
    let engine = t.cfg.engine;
    let d = Dialect(engine);
    let tx = engine.capabilities().transactional_ddl;
    let obj = t.obj("designed", ObjectKind::Table);
    let int = if engine == Engine::Sqlite { "INTEGER" } else { "int" };
    let col = |n: &str, ty: &str, nullable: bool, pk: bool| ColumnDef {
        name: n.into(),
        data_type: ty.into(),
        nullable,
        primary_key: pk,
        ..Default::default()
    };
    let create = create_table(
        d,
        &obj,
        &[
            col("id", int, false, true),
            ColumnDef { default: Some("'n/a'".into()), ..col("label", "varchar(20)", false, false) },
        ],
    )
    .unwrap();
    s.execute_batch(&[create], tx).await.unwrap();
    s.execute_batch(&[format!("INSERT INTO {} (id) VALUES (1)", t.q("designed"))], false).await.unwrap();
    let st = s.table_structure(&obj).await.unwrap();
    assert_eq!(st.primary_key(), vec!["id"]);
    assert_eq!(
        st.column("label").unwrap().default.as_deref().map(|x| x.contains("n/a")),
        Some(true),
        "{:?}",
        st.column("label")
    );
    let original: Vec<ColumnDef> = st.columns.iter().map(ColumnDef::from).collect();

    // Rename + add (supported everywhere), modify (PG/MySQL only).
    let mut edited: Vec<DesignColumn> =
        original.iter().map(|c| DesignColumn { original: Some(c.name.clone()), def: c.clone() }).collect();
    edited[1].def.name = "title".into();
    if engine != Engine::Sqlite {
        edited[1].def.data_type = "varchar(40)".into();
        edited[1].def.nullable = true;
    }
    edited.push(DesignColumn { original: None, def: col("qty", int, true, false) });
    let plan = alter_table(d, &obj, &original, &edited).unwrap();
    s.execute_batch(&plan.statements, tx).await.unwrap();
    let st = s.table_structure(&obj).await.unwrap();
    let names: Vec<&str> = st.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "title", "qty"]);
    if engine != Engine::Sqlite {
        assert!(st.column("title").unwrap().nullable);
        assert!(st.column("title").unwrap().data_type.contains("40"), "{:?}", st.column("title"));
        // The literal default survived the MODIFY / ALTER TYPE.
        assert!(
            st.column("title").unwrap().default.as_deref().is_some_and(|x| x.contains("n/a")),
            "{:?}",
            st.column("title")
        );
    }
    let r = s.run_script(&format!("SELECT title FROM {}", t.q("designed")), &ScriptOptions::default()).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("n/a"), "data kept through rename/modify");

    // Drop column (destructive) is reported as such and applies.
    let original: Vec<ColumnDef> = st.columns.iter().map(ColumnDef::from).collect();
    let edited: Vec<DesignColumn> = original
        .iter()
        .filter(|c| c.name != "qty")
        .map(|c| DesignColumn { original: Some(c.name.clone()), def: c.clone() })
        .collect();
    let plan = alter_table(d, &obj, &original, &edited).unwrap();
    assert_eq!(plan.destructive.len(), 1);
    s.execute_batch(&plan.statements, tx).await.unwrap();
    assert_eq!(s.table_structure(&obj).await.unwrap().columns.len(), 2);

    // A failing batch on transactional-DDL engines leaves nothing behind.
    if tx {
        let bad = vec![
            format!("ALTER TABLE {} ADD COLUMN extra1 {int}", t.q("designed")),
            format!("ALTER TABLE {} ADD COLUMN id {int}", t.q("designed")),
        ];
        let err = s.execute_batch(&bad, true).await.unwrap_err();
        assert!(err.to_string().contains("rolled back"), "{err}");
        assert_eq!(s.table_structure(&obj).await.unwrap().columns.len(), 2, "first ALTER was rolled back");
    }
}

/// A model is forward-engineered into real tables (keys, FK, index, auto-increment) and reversed again.
async fn modeling(t: &Target, s: &mut Session) {
    use talus_core::model_design::{DesignModel, LType, MColumn, MFk, MIndex, MTable, ModelKind};
    let engine = t.cfg.engine;
    let col = |name: &str, ty: LType, nullable: bool| MColumn {
        name: name.into(),
        ty,
        nullable,
        primary_key: false,
        auto_increment: false,
        default: None,
    };
    let mut orders = MTable::new("md_orders", 0.0, 0.0);
    orders.columns.push(col("customer_id", LType::Integer, false));
    orders
        .columns
        .push(MColumn { default: Some("0".into()), ..col("total", LType::Decimal { precision: 10, scale: 2 }, true) });
    orders.foreign_keys.push(MFk {
        name: "fk_md_orders_customer".into(),
        columns: vec!["customer_id".into()],
        ref_table: "md_customers".into(),
        ref_columns: vec!["id".into()],
        on_update: "NO ACTION".into(),
        on_delete: "CASCADE".into(),
    });
    orders.indexes.push(MIndex {
        name: "ix_md_orders_customer".into(),
        columns: vec!["customer_id".into()],
        unique: false,
    });
    let mut customers = MTable::new("md_customers", 300.0, 0.0);
    customers.columns.push(col("email", LType::Varchar(80), false));
    let model = DesignModel { name: "shop".into(), kind: ModelKind::Physical, tables: vec![orders, customers] };

    let place = t.obj("md_orders", ObjectKind::Table);
    let statements = model.forward(engine, &place.database, place.schema.as_deref()).unwrap();
    s.execute_batch(&statements, engine.capabilities().transactional_ddl).await.unwrap();

    // The tables work: the generated key numbers itself, the foreign key and cascade are enforced.
    let opts = ScriptOptions::default();
    s.execute_batch(
        &[
            format!("INSERT INTO {} (email) VALUES ('a@b.test')", t.q("md_customers")),
            format!("INSERT INTO {} (customer_id) VALUES (1)", t.q("md_orders")),
        ],
        false,
    )
    .await
    .unwrap();
    let orphan = s.execute_batch(&[format!("INSERT INTO {} (customer_id) VALUES (99)", t.q("md_orders"))], false).await;
    assert!(orphan.is_err(), "{engine:?}: the foreign key rejects an orphan");
    s.execute_batch(&[format!("DELETE FROM {} WHERE id = 1", t.q("md_customers"))], false).await.unwrap();
    let r = s.run_script(&format!("SELECT count(*) FROM {}", t.q("md_orders")), &opts).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("0"), "ON DELETE CASCADE");

    // Reverse engineering gives the model back.
    let live = s.schema_model(&place.database, place.schema.as_deref()).await.unwrap();
    let mine: Vec<_> = live.into_iter().filter(|(o, _)| o.name.starts_with("md_")).collect();
    let back = DesignModel::reverse("shop", &mine);
    let find = |n: &str| back.tables.iter().find(|x| x.name == n).unwrap();
    let c = find("md_customers");
    assert_eq!(c.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "email"]);
    assert!(c.columns[0].primary_key && !c.columns[1].nullable, "{c:?}");
    assert!(matches!(c.columns[1].ty, LType::Varchar(80)), "{engine:?}: {:?}", c.columns[1].ty);
    let o = find("md_orders");
    assert_eq!(
        (o.foreign_keys.len(), o.foreign_keys[0].ref_table.as_str(), o.foreign_keys[0].on_delete.as_str()),
        (1, "md_customers", "CASCADE")
    );
    assert!(o.indexes.iter().any(|i| i.name == "ix_md_orders_customer"), "{o:?}");
    assert!(back.validate().is_empty(), "{:?}", back.validate());

    // Model vs database: nothing to do right after forward engineering ...
    let (db, schema) = (place.database.as_str(), place.schema.as_deref());
    let plan = model.diff(engine, db, schema, &mine, false).unwrap();
    assert!(plan.statements.is_empty(), "{engine:?}: {:?}", plan.statements);
    // ... and an edited model yields the statements that bring the database in line.
    let mut edited = model.clone();
    let cust = edited.tables.iter_mut().find(|t| t.name == "md_customers").unwrap();
    cust.columns.push(col("nickname", LType::Varchar(30), true));
    cust.indexes.push(MIndex { name: "ux_md_customers_email".into(), columns: vec!["email".into()], unique: true });
    let plan = edited.diff(engine, db, schema, &mine, false).unwrap();
    assert!(!plan.statements.is_empty() && plan.destructive.is_empty(), "{engine:?}: {plan:?}");
    s.execute_batch(&plan.statements, engine.capabilities().transactional_ddl).await.unwrap();
    let live = s.schema_model(db, schema).await.unwrap();
    let mine: Vec<_> = live.into_iter().filter(|(o, _)| o.name.starts_with("md_")).collect();
    let plan = edited.diff(engine, db, schema, &mine, false).unwrap();
    assert!(plan.statements.is_empty(), "{engine:?}: {:?}", plan.statements);
}

/// Generated test data respects foreign keys, uniqueness, lengths and survives a second run.
async fn data_generation(t: &Target, s: &mut Session) {
    use talus_core::datagen::fill_table;
    let engine = t.cfg.engine;
    let int = if engine == Engine::Sqlite { "INTEGER" } else { "int" };
    let parent_ref = if engine == Engine::Sqlite { "dg_parent".to_string() } else { t.q("dg_parent") };
    s.execute_batch(
        &[
            format!("CREATE TABLE {} (id {int} PRIMARY KEY, label varchar(20))", t.q("dg_parent")),
            format!(
                "CREATE TABLE {} (id {int} PRIMARY KEY, parent_id {int} NOT NULL, email varchar(40) NOT NULL UNIQUE, name varchar(25), qty {int}, price decimal(10,2), FOREIGN KEY (parent_id) REFERENCES {parent_ref} (id))",
                t.q("dg_child")
            ),
            format!("INSERT INTO {} VALUES (1, 'a'), (2, 'b'), (3, 'c')", t.q("dg_parent")),
        ],
        false,
    )
    .await
    .unwrap();
    let opts = ScriptOptions::default();
    let child = t.obj("dg_child", ObjectKind::Table);
    assert_eq!(fill_table(s, &child, 300, 42).await.unwrap(), 300);
    assert_eq!(fill_table(s, &child, 50, 43).await.unwrap(), 50, "a second run continues after the first");
    let q = |sql: &str| sql.replace("{T}", &t.q("dg_child")).replace("{P}", &t.q("dg_parent"));
    let r = s
        .run_script(&q("SELECT count(*), count(DISTINCT email), count(DISTINCT id), min(id), max(id) FROM {T}"), &opts)
        .await;
    let row = &rows(&r.results[0].outcome).rows[0];
    assert_eq!(
        row.iter().map(|c| c.as_deref().unwrap()).collect::<Vec<_>>(),
        ["350", "350", "350", "1", "350"],
        "{row:?}"
    );
    let r = s.run_script(&q("SELECT count(*) FROM {T} WHERE parent_id NOT IN (SELECT id FROM {P})"), &opts).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("0"), "every child has a parent");
    // Required columns without a way to generate them are reported before anything is written.
    let empty = t.obj("dg_parent", ObjectKind::Table);
    assert_eq!(fill_table(s, &empty, 4, 1).await.unwrap(), 4);
    assert!(fill_table(s, &child, 0, 1).await.is_err());
}

/// Column profiles: counts, NULLs, distinct values, min / max / average and top values.
async fn profiling(t: &Target, s: &mut Session) {
    use talus_core::profile::profile_table;
    let int = if t.cfg.engine == Engine::Sqlite { "INTEGER" } else { "int" };
    s.execute_batch(
        &[
            format!("CREATE TABLE {} (id {int} PRIMARY KEY, name varchar(20), score {int})", t.q("pf")),
            format!(
                "INSERT INTO {} VALUES (1, 'a', 10), (2, 'a', 20), (3, 'b', 20), (4, NULL, 30), (5, 'a', NULL)",
                t.q("pf")
            ),
        ],
        false,
    )
    .await
    .unwrap();
    let p = profile_table(s, &t.obj("pf", ObjectKind::Table), 2).await.unwrap();
    let by = |n: &str| p.iter().find(|c| c.name == n).unwrap();
    let name = by("name");
    assert_eq!((name.rows, name.nulls, name.distinct), (5, 1, Some(2)), "{name:?}");
    assert_eq!((name.min.as_deref(), name.max.as_deref()), (Some("a"), Some("b")));
    assert_eq!(name.top[0], (Some("a".to_string()), 3));
    assert_eq!(name.top.len(), 2);
    let score = by("score");
    assert_eq!((score.nulls, score.distinct), (1, Some(3)), "{score:?}");
    assert_eq!((score.min.as_deref(), score.max.as_deref()), (Some("10"), Some("30")));
    assert!(score.avg.as_deref().is_some_and(|a| a.starts_with("20")), "{:?}", score.avg);
    assert_eq!(by("id").distinct, Some(5));
    assert!(profile_table(s, &t.obj("no_such_table", ObjectKind::Table), 2).await.is_err());
}

/// Scheduled-job actions: SQL, table export and database dump through one open session.
async fn job_actions(t: &Target, s: &mut Session) {
    use talus_core::jobs::{JobAction, execute_action};
    let now = chrono::NaiveDate::from_ymd_opt(2025, 3, 5).unwrap().and_hms_opt(10, 30, 0).unwrap();
    let ok =
        execute_action(s, &JobAction::RunSql { sql: "SELECT 1; SELECT 2".into(), database: None }, now).await.unwrap();
    assert!(ok.starts_with("2 statement result(s)"), "{ok}");
    let err = execute_action(s, &JobAction::RunSql { sql: "SELECT 1; SELEKT nope".into(), database: None }, now)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("statement 2 failed"), "{err}");

    let customers = t.obj("customers", ObjectKind::Table);
    let dir = std::env::temp_dir().join(unique("job"));
    std::fs::create_dir_all(&dir).unwrap();
    let export = JobAction::ExportTable {
        database: customers.database.clone(),
        schema: customers.schema.clone(),
        table: "customers".into(),
        format: "csv".into(),
        path: format!("{}/c_{{date}}.csv", dir.display()),
    };
    let msg = execute_action(s, &export, now).await.unwrap();
    assert!(msg.contains("exported 3 row(s)"), "{msg}");
    let text = std::fs::read_to_string(dir.join("c_20250305.csv")).unwrap();
    assert!(text.lines().count() == 4 && text.contains("Ann"), "{text}");
    let bad_fmt = JobAction::ExportTable {
        database: customers.database.clone(),
        schema: customers.schema.clone(),
        table: "customers".into(),
        format: "docx".into(),
        path: format!("{}/bad.out", dir.display()),
    };
    assert!(execute_action(s, &bad_fmt, now).await.is_err());

    let dump = JobAction::DumpDatabase {
        database: customers.database.clone(),
        schema: customers.schema.clone(),
        path: format!("{}/d_{{datetime}}.sql", dir.display()),
        include_data: false,
    };
    let msg = execute_action(s, &dump, now).await.unwrap();
    assert!(msg.contains("table(s)"), "{msg}");
    let dumped = std::fs::read_to_string(dir.join("d_20250305-103000.sql")).unwrap();
    assert!(dumped.contains("customers"), "{dumped:.300}");
    let _ = std::fs::remove_dir_all(dir);
}

/// T-SQL behaviours that need their own checks: GO batches, SHOWPLAN, types, IDENTITY, dump.
async fn sqlserver_specifics(t: &Target, s: &mut Session) {
    if t.cfg.engine != Engine::SqlServer {
        return;
    }
    let opts = ScriptOptions::default();
    // GO splits batches; a proc body with semicolons stays in one piece.
    let script = "CREATE TABLE ss_t (id int IDENTITY(1,1) PRIMARY KEY, name nvarchar(40), flag bit, at datetime2(3), g uniqueidentifier, amount decimal(10,2))\nGO\n\
                  CREATE PROCEDURE ss_p AS BEGIN SELECT 1; SELECT 2; END\nGO\n\
                  INSERT INTO ss_t (name, flag, at, g, amount) VALUES (N'Ünï «çødé» 日本', 1, '2024-05-06 07:08:09.123', '6F9619FF-8B86-D011-B42D-00C04FC964FF', 12.50)\nGO\n\
                  EXEC ss_p\nGO";
    let r = s.run_script(script, &opts).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let sets: Vec<_> =
        r.results.iter().filter_map(|x| if let Outcome::Rows(q) = &x.outcome { Some(q) } else { None }).collect();
    assert_eq!(sets.len(), 2, "EXEC returns both result sets: {:?}", r.results);
    assert!(
        r.results.iter().any(|x| matches!(x.outcome, Outcome::Affected(1))),
        "INSERT reports 1 row: {:?}",
        r.results
    );

    let r = s.run_script("SELECT name, flag, at, g, amount FROM ss_t", &opts).await;
    let q = rows(&r.results[0].outcome);
    assert_eq!(
        q.rows[0].iter().map(|c| c.as_deref()).collect::<Vec<_>>(),
        vec![
            Some("Ünï «çødé» 日本"),
            Some("1"),
            Some("2024-05-06 07:08:09.123"),
            Some("6F9619FF-8B86-D011-B42D-00C04FC964FF"),
            Some("12.50")
        ]
    );
    assert_eq!(
        q.columns.iter().map(|c| c.type_name.as_str()).collect::<Vec<_>>(),
        ["nvarchar", "bit", "datetime2", "uniqueidentifier", "numeric"]
    );

    // Execution plan mode is switched on and off within one script.
    let r = s.run_script("SET SHOWPLAN_TEXT ON; SELECT * FROM ss_t; SET SHOWPLAN_TEXT OFF", &opts).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let plan = r.results.iter().find_map(|x| if let Outcome::Rows(q) = &x.outcome { Some(q) } else { None }).unwrap();
    assert!(plan.rows[0][0].as_deref().is_some_and(|p| p.contains("SELECT")), "{plan:?}");
    let r = s.run_script("SELECT count(*) FROM ss_t", &opts).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("1"), "plan mode is off again");

    // Identity column metadata, and data survives a dump → wipe → restore.
    let obj = t.obj("ss_t", ObjectKind::Table);
    let st = s.table_structure(&obj).await.unwrap();
    assert_eq!(st.column("id").unwrap().extra.as_deref(), Some("IDENTITY(1,1)"));
    assert_eq!(st.column("amount").unwrap().data_type, "decimal(10,2)");
    assert_eq!(st.column("at").unwrap().data_type, "datetime2(3)");
    let path = std::env::temp_dir().join(format!("{}.sql", unique("ssdump")));
    let rep = talus_core::dump::dump_schema(s, &t.ns, Some("dbo"), &path, true).await.unwrap();
    assert!(rep.rows >= 1);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("SET IDENTITY_INSERT [dbo].[ss_t] ON"), "{text}");
    s.execute_batch(&["DROP TABLE ss_t".to_string(), "DROP PROCEDURE ss_p".to_string()], false).await.ok();
    s.execute_batch(&["DROP TABLE dbo.ss_t".to_string()], false).await.ok();
    let wiped: Vec<String> = s
        .list_objects(&t.ns, Some("dbo"))
        .await
        .unwrap()
        .into_iter()
        .filter(|o| o.name == "ss_t")
        .map(|o| o.name)
        .collect();
    if !wiped.is_empty() {
        s.execute_batch(&["DROP TABLE [dbo].[ss_t]".to_string()], false).await.unwrap();
    }
    // Restoring recreates only ss_t (other suite tables still exist; their CREATEs fail), so run just its part.
    let part: Vec<String> = talus_core::sql::split::split(&text, Engine::SqlServer)
        .iter()
        .map(|x| x.text.to_string())
        .filter(|x| x.contains("ss_t") && !x.starts_with("--"))
        .collect();
    s.execute_batch(&part, true).await.unwrap();
    let r = s.run_script("SELECT id, name FROM ss_t", &opts).await;
    let q = rows(&r.results[0].outcome);
    assert_eq!((q.rows[0][0].as_deref(), q.rows[0][1].as_deref()), (Some("1"), Some("Ünï «çødé» 日本")));
    let _ = std::fs::remove_file(path);
}

/// SQL from the visual builder runs on every engine and filters / joins / orders as specified.
async fn query_builder(t: &Target, s: &mut Session) {
    use talus_core::sql::Dialect;
    use talus_core::sql::builder::{ColRef, Condition, Join, JoinKind, Op, QuerySpec, TableRef, build};
    let int = if t.cfg.engine == Engine::Sqlite { "INTEGER" } else { "int" };
    s.execute_batch(
        &[
            format!("CREATE TABLE {} (id {int} PRIMARY KEY, name varchar(20))", t.q("qb_c")),
            format!("CREATE TABLE {} (id {int} PRIMARY KEY, cid {int}, total {int})", t.q("qb_o")),
            format!("INSERT INTO {} VALUES (1, 'Ann'), (2, 'Bob''s'), (3, 'Cy')", t.q("qb_c")),
            format!("INSERT INTO {} VALUES (10, 1, 5), (11, 1, 50), (12, 2, 70), (13, NULL, 90)", t.q("qb_o")),
        ],
        false,
    )
    .await
    .unwrap();
    let c = |a: &str, n: &str| ColRef { alias: a.into(), column: n.into() };
    let spec = QuerySpec {
        base: TableRef { obj: t.obj("qb_o", ObjectKind::Table), alias: "o".into() },
        joins: vec![Join {
            kind: JoinKind::Inner,
            table: TableRef { obj: t.obj("qb_c", ObjectKind::Table), alias: "c".into() },
            left: c("o", "cid"),
            right: c("c", "id"),
        }],
        columns: vec![c("o", "id"), c("c", "name")],
        conditions: vec![
            Condition { col: c("o", "total"), op: Op::Ge, value: "10".into() },
            Condition { col: c("c", "name"), op: Op::NotLike, value: "Cy".into() },
        ],
        order: vec![(c("o", "id"), talus_core::model::SortDir::Desc)],
        limit: Some(10),
        distinct: false,
    };
    let sql = build(Dialect(t.cfg.engine), &spec).unwrap();
    let r = s.run_script(&sql, &ScriptOptions::default()).await;
    assert!(r.errors.is_empty(), "{sql}\n{:?}", r.errors);
    let q = rows(&r.results[0].outcome);
    let got: Vec<(Option<&str>, Option<&str>)> = q.rows.iter().map(|r| (r[0].as_deref(), r[1].as_deref())).collect();
    assert_eq!(got, vec![(Some("12"), Some("Bob's")), (Some("11"), Some("Ann"))], "{sql}");
}

/// Accounts: create, list, grants, drop (server engines only).
async fn accounts(t: &Target, s: &mut Session) {
    use talus_core::sql::users::{create_user_sql, drop_user_sql, grants, list_users};
    let engine = t.cfg.engine;
    if engine == Engine::Sqlite {
        assert!(list_users(s).await.is_err());
        return;
    }
    let name = unique("acct");
    let (create, shown) = create_user_sql(engine, &name, "%", "Pw-1234'x").unwrap();
    assert!(!shown.contains("Pw-1234"));
    s.execute_batch(&[create], false).await.unwrap();
    let users = list_users(s).await.unwrap();
    let me = users.iter().find(|u| u.name == name).unwrap_or_else(|| panic!("{name} missing from {users:?}"));
    if engine == Engine::Postgres {
        assert!(me.attrs.contains(&"login".to_string()), "{me:?}");
    }
    let table = t.q("acct_t");
    let int = "int";
    s.execute_batch(&[format!("CREATE TABLE {table} (a {int})")], false).await.unwrap();
    if engine == Engine::SqlServer {
        // A login only gets table permissions through a user of the database.
        s.execute_batch(&[format!("CREATE USER [{name}] FOR LOGIN [{name}]")], false).await.unwrap();
    }
    let target = talus_core::sql::users::GrantTarget::Table(t.obj("acct_t", ObjectKind::Table));
    let grant = talus_core::sql::users::grant_sql(engine, me, &["SELECT".to_string()], &target, true).unwrap();
    s.execute_batch(&[grant], false).await.unwrap();
    let g = grants(s, me).await.unwrap();
    assert!(g.iter().any(|l| l.contains("SELECT") && (l.contains("acct_t") || l.contains("`acct_t`"))), "{g:?}");
    let revoke = talus_core::sql::users::grant_sql(engine, me, &["SELECT".to_string()], &target, false).unwrap();
    s.execute_batch(&[revoke], false).await.unwrap();
    let g = grants(s, me).await.unwrap();
    assert!(!g.iter().any(|l| l.contains("SELECT") && l.contains("acct_t")), "revoked: {g:?}");
    // The role / login can only go once nothing refers to it (SQL Server: drop its database user first).
    let mut cleanup = Vec::new();
    if engine == Engine::SqlServer {
        cleanup.push(format!("DROP USER [{name}]"));
    }
    cleanup.push(drop_user_sql(engine, me).unwrap());
    s.execute_batch(&cleanup, false).await.unwrap();
    assert!(!list_users(s).await.unwrap().iter().any(|u| u.name == name));
}

/// Triggers / functions / procedures are listed with their definitions.
async fn programmability(t: &Target, s: &mut Session) {
    use talus_core::programmability::{ProgKind, list};
    let engine = t.cfg.engine;
    let int = if engine == Engine::Sqlite { "INTEGER" } else { "int" };
    let ns = t.ns.clone();
    let setup: Vec<String> = match engine {
        Engine::Sqlite => vec![
            format!("CREATE TABLE {} (a {int})", t.q("pr_t")),
            "CREATE TRIGGER trg_t AFTER INSERT ON pr_t BEGIN SELECT 1; END".into(),
        ],
        Engine::Postgres => vec![
            format!("CREATE TABLE {} (a {int})", t.q("pr_t")),
            format!("CREATE FUNCTION \"{ns}\".pr_fn() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$"),
            format!("CREATE PROCEDURE \"{ns}\".pr_proc() LANGUAGE sql AS $$ SELECT 1 $$"),
            format!(
                "CREATE TRIGGER trg_t BEFORE INSERT ON {} FOR EACH ROW EXECUTE FUNCTION \"{ns}\".pr_fn()",
                t.q("pr_t")
            ),
        ],
        Engine::MySql => vec![
            format!("CREATE TABLE {} (a {int})", t.q("pr_t")),
            format!("CREATE TRIGGER `{ns}`.trg_t BEFORE INSERT ON {} FOR EACH ROW SET NEW.a = 1", t.q("pr_t")),
            format!("CREATE FUNCTION `{ns}`.pr_fn() RETURNS int DETERMINISTIC RETURN 1"),
            format!("CREATE PROCEDURE `{ns}`.pr_proc() SELECT 1"),
        ],
        Engine::SqlServer => vec![
            format!("CREATE TABLE {} (a {int})", t.q("pr_t")),
            format!("CREATE TRIGGER dbo.trg_t ON {} AFTER INSERT AS BEGIN SET NOCOUNT ON; END", t.q("pr_t")),
            "CREATE FUNCTION dbo.pr_fn() RETURNS int AS BEGIN RETURN 1 END".to_string(),
            "CREATE PROCEDURE dbo.pr_proc AS SELECT 1".to_string(),
        ],
        Engine::Redis | Engine::MongoDb => unreachable!("not a SQL engine"),
    };
    s.execute_batch(&setup, false).await.unwrap();
    let schema = match engine {
        Engine::Postgres => Some(ns.as_str()),
        Engine::SqlServer => Some("dbo"),
        _ => None,
    };
    let items = list(s, &ns, schema).await.unwrap();
    let find = |kind: ProgKind| items.iter().filter(|i| i.kind == kind).collect::<Vec<_>>();
    let trig = find(ProgKind::Trigger);
    assert_eq!(trig.len(), 1, "{items:?}");
    assert_eq!((trig[0].name.as_str(), trig[0].table.as_deref()), ("trg_t", Some("pr_t")));
    assert!(trig[0].definition.to_lowercase().contains("trigger"), "{:?}", trig[0]);
    if engine != Engine::Sqlite {
        assert_eq!(find(ProgKind::Function).len(), 1, "{items:?}");
        assert_eq!(find(ProgKind::Procedure).len(), 1, "{items:?}");
        assert!(items.iter().all(|i| !i.definition.is_empty()), "{items:?}");
    }
}

/// Dump a namespace to a file, wipe it, run the file back: structure, data and view return.
async fn dump_roundtrip(t: &Target, shared: &mut Session) {
    use talus_core::dump::{dump_schema, execute_sql_file};
    let engine = t.cfg.engine;
    // SQLite has no namespaces: dump a database file of its own, not the suite's `main`.
    let mut own;
    let s = if engine == Engine::Sqlite {
        let file = std::env::temp_dir().join(format!("{}.sqlite", unique("dumpdb")));
        create_database_file(&file).unwrap();
        let mut cfg = t.cfg.clone();
        cfg.database = file.to_string_lossy().into_owned();
        own = Session::connect(&cfg, None, None).await.unwrap();
        &mut own
    } else {
        shared
    };
    let tx = engine.capabilities().transactional_ddl;
    let int = if engine == Engine::Sqlite { "INTEGER" } else { "int" };
    let (ns, db, schema) = match engine {
        Engine::Sqlite => ("main".to_string(), "main".to_string(), None),
        Engine::Postgres => {
            let ns = unique("dmp");
            s.execute_batch(&[format!("CREATE SCHEMA \"{ns}\"")], false).await.unwrap();
            (ns.clone(), t.cfg.database.clone(), Some(ns))
        }
        Engine::MySql => {
            let ns = unique("dmp");
            s.execute_batch(&[format!("CREATE DATABASE `{ns}`"), format!("USE `{ns}`")], false).await.unwrap();
            (ns.clone(), ns, None)
        }
        Engine::SqlServer => {
            let ns = unique("dmp");
            s.execute_batch(&[format!("CREATE SCHEMA [{ns}]")], false).await.unwrap();
            (ns.clone(), t.ns.clone(), Some(ns))
        }
        Engine::Redis | Engine::MongoDb => unreachable!("not a SQL engine"),
    };
    let q = |n: &str| match engine {
        Engine::Postgres => format!("\"{ns}\".\"{n}\""),
        Engine::SqlServer => format!("[{ns}].[{n}]"),
        _ => format!("\"{n}\"").replace('"', if engine == Engine::MySql { "`" } else { "\"" }),
    };
    let setup = vec![
        format!("CREATE TABLE {} (id {int} PRIMARY KEY, label varchar(30))", q("dp_parent")),
        format!(
            "CREATE TABLE {} (id {int} PRIMARY KEY, pid {int} NOT NULL, note varchar(40), FOREIGN KEY (pid) REFERENCES {} (id))",
            q("dp_child"),
            q("dp_parent")
        ),
        format!("INSERT INTO {} VALUES (1, 'it''s'), (2, NULL)", q("dp_parent")),
        format!("INSERT INTO {} VALUES (10, 1, 'line1\nline2'), (11, 2, 'semi;colon')", q("dp_child")),
        format!("CREATE VIEW {} AS SELECT id FROM {}", q("dp_view"), q("dp_parent")),
    ];
    s.execute_batch(&setup, false).await.unwrap();
    let path = std::env::temp_dir().join(format!("{}.sql", unique("dump")));
    let rep = dump_schema(s, &db, schema.as_deref(), &path, true).await.unwrap();
    assert_eq!((rep.tables, rep.views, rep.rows), (2, 1, 4), "{rep:?}");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.find("dp_parent").unwrap() < text.find("dp_child").unwrap(), "parents first");

    let wipe = vec![
        format!("DROP VIEW {}", q("dp_view")),
        format!("DROP TABLE {}", q("dp_child")),
        format!("DROP TABLE {}", q("dp_parent")),
    ];
    s.execute_batch(&wipe, false).await.unwrap();
    let applied = execute_sql_file(s, &path, tx).await.unwrap();
    assert!(applied >= 6, "{applied}");
    let opts = ScriptOptions::default();
    let r = s.run_script(&format!("SELECT note FROM {} WHERE id = 10", q("dp_child")), &opts).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("line1\nline2"));
    let r = s.run_script(&format!("SELECT label FROM {} ORDER BY id", q("dp_parent")), &opts).await;
    let q2 = rows(&r.results[0].outcome);
    assert_eq!((q2.rows[0][0].as_deref(), q2.rows[1][0].as_deref()), (Some("it's"), None));
    let r = s.run_script(&format!("SELECT count(*) FROM {}", q("dp_view")), &opts).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("2"));
    // A broken file reports the failure instead of half-applying on transactional engines.
    let bad = std::env::temp_dir().join(format!("{}.sql", unique("bad")));
    std::fs::write(
        &bad,
        format!("INSERT INTO {} VALUES (99, 'x');\nINSERT INTO nope_missing VALUES (1);", q("dp_parent")),
    )
    .unwrap();
    assert!(execute_sql_file(s, &bad, tx).await.is_err());
    if tx {
        let r = s.run_script(&format!("SELECT count(*) FROM {} WHERE id = 99", q("dp_parent")), &opts).await;
        assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("0"), "rolled back");
    }
    let _ = (std::fs::remove_file(&path), std::fs::remove_file(&bad));
    // Leave nothing behind in the shared test server.
    let cleanup: Vec<String> = match engine {
        Engine::Sqlite => Vec::new(),
        Engine::Postgres => vec![format!("DROP SCHEMA \"{ns}\" CASCADE")],
        Engine::MySql => vec![format!("USE `{}`", t.ns), format!("DROP DATABASE `{ns}`")],
        Engine::SqlServer => vec![
            format!("DROP VIEW {}", q("dp_view")),
            format!("DROP TABLE {}", q("dp_child")),
            format!("DROP TABLE {}", q("dp_parent")),
            format!("DROP SCHEMA [{ns}]"),
        ],
        Engine::Redis | Engine::MongoDb => unreachable!("not a SQL engine"),
    };
    shared_cleanup(s, &cleanup).await;
}

async fn shared_cleanup(s: &mut Session, stmts: &[String]) {
    if !stmts.is_empty() {
        s.execute_batch(stmts, false).await.unwrap();
    }
}

/// Data sync between two sessions: dry run changes nothing, apply converges, second run is a no-op.
async fn data_sync(t: &Target, s: &mut Session) {
    use talus_core::data_sync::sync_table;
    let int = if t.cfg.engine == Engine::Sqlite { "INTEGER" } else { "int" };
    let src = t.obj("ds_src", ObjectKind::Table);
    let dst = t.obj("ds_dst", ObjectKind::Table);
    let mut setup = Vec::new();
    for n in ["ds_src", "ds_dst"] {
        setup.push(format!("CREATE TABLE {} (id {int} PRIMARY KEY, name varchar(20), qty {int})", t.q(n)));
    }
    setup.push(format!(
        "INSERT INTO {} (id, name, qty) VALUES (1, 'a', 1), (2, 'b', NULL), (3, 'c', 3), (4, 'd', 4), (5, 'e', 5)",
        t.q("ds_src")
    ));
    setup.push(format!(
        "INSERT INTO {} (id, name, qty) VALUES (3, 'CHANGED', 3), (4, 'd', 4), (9, 'extra', 9)",
        t.q("ds_dst")
    ));
    s.execute_batch(&setup, false).await.unwrap();
    let mut other = Session::connect(&t.cfg, t.password.as_ref(), None).await.unwrap();

    let dry = sync_table(s, &src, &mut other, &dst, true, false).await.unwrap();
    assert_eq!((dry.inserted, dry.updated, dry.deleted, dry.unchanged), (3, 1, 1, 1), "{dry:?}");
    assert!(!dry.applied && !dry.preview.is_empty());
    let r = s.run_script(&format!("SELECT count(*) FROM {}", t.q("ds_dst")), &ScriptOptions::default()).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("3"), "dry run must not write");

    // Without delete_missing the extra row stays.
    let keep = sync_table(s, &src, &mut other, &dst, false, true).await.unwrap();
    assert_eq!((keep.inserted, keep.updated, keep.deleted), (3, 1, 0), "{keep:?}");
    let r = s.run_script(&format!("SELECT count(*) FROM {}", t.q("ds_dst")), &ScriptOptions::default()).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("6"));
    let full = sync_table(s, &src, &mut other, &dst, true, true).await.unwrap();
    assert_eq!((full.inserted, full.updated, full.deleted), (0, 0, 1), "{full:?}");
    let again = sync_table(s, &src, &mut other, &dst, true, true).await.unwrap();
    assert_eq!(again.differences(), 0);
    let r = s
        .run_script(
            &format!(
                "SELECT s.id FROM {} s JOIN {} d ON d.id = s.id AND d.name = s.name AND (d.qty = s.qty OR (d.qty IS NULL AND s.qty IS NULL))",
                t.q("ds_src"),
                t.q("ds_dst")
            ),
            &ScriptOptions::default(),
        )
        .await;
    assert_eq!(rows(&r.results[0].outcome).rows.len(), 5, "tables identical after sync");

    // A table without a primary key is refused.
    s.execute_batch(&[format!("CREATE TABLE {} (a {int})", t.q("ds_nopk"))], false).await.unwrap();
    let nopk = t.obj("ds_nopk", ObjectKind::Table);
    assert!(matches!(sync_table(s, &nopk, &mut other, &nopk, false, false).await, Err(DbError::Invalid(_))));
}

/// Indexes and foreign keys through the designer plans: create with the table, diff, drop.
async fn designer_constraints(t: &Target, s: &mut Session) {
    use std::collections::HashMap;
    use talus_core::sql::Dialect;
    use talus_core::sql::constraints::{self, FkDef, IndexDef};
    use talus_core::sql::ddl::{ColumnDef, create_table_with};
    let engine = t.cfg.engine;
    let d = Dialect(engine);
    let tx = engine.capabilities().transactional_ddl;
    let int = if engine == Engine::Sqlite { "INTEGER" } else { "int" };
    let col = |n: &str, ty: &str, pk: bool| ColumnDef {
        name: n.into(),
        data_type: ty.into(),
        nullable: !pk,
        primary_key: pk,
        ..Default::default()
    };
    let parents = t.obj("dc_parents", ObjectKind::Table);
    let children = t.obj("dc_children", ObjectKind::Table);
    s.execute_batch(&[create_table_with(d, &parents, &[col("id", int, true)], &[]).unwrap()], tx).await.unwrap();

    let ref_table = match engine {
        Engine::Sqlite => "dc_parents".to_string(),
        Engine::SqlServer => "dbo.dc_parents".to_string(),
        _ => format!("{}.dc_parents", t.ns),
    };
    let fk = FkDef {
        name: "fk_dc_parent".into(),
        columns: vec!["pid".into()],
        ref_table,
        ref_columns: vec!["id".into()],
        on_update: "NO ACTION".into(),
        on_delete: "CASCADE".into(),
        original: None,
    };
    let ix = |name: &str, cols: &[&str], unique: bool| IndexDef {
        original: None,
        name: name.into(),
        columns: cols.iter().map(|c| c.to_string()).collect(),
        unique,
    };
    let cols = [col("id", int, true), col("pid", int, false), col("code", "varchar(20)", false)];
    let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
    let indexes = [ix("ix_dc_pid", &["pid"], false)];
    constraints::validate_new(d, &names, &indexes, std::slice::from_ref(&fk)).unwrap();
    let mut create = vec![create_table_with(d, &children, &cols, std::slice::from_ref(&fk)).unwrap()];
    create.push(constraints::create_index(d, &children, &indexes[0]).unwrap());
    s.execute_batch(&create, tx).await.unwrap();

    let st = s.table_structure(&children).await.unwrap();
    assert_eq!(st.foreign_keys.len(), 1, "{:?}", st.foreign_keys);
    assert_eq!(st.foreign_keys[0].columns, ["pid"]);
    assert_eq!(st.foreign_keys[0].on_delete, "CASCADE");
    assert!(st.indexes.iter().any(|i| i.name == "ix_dc_pid" && !i.unique && i.columns == ["pid"]), "{:?}", st.indexes);

    // Diff: nothing changed -> empty plan; then swap the index for a unique one and drop the FK.
    let loaded_ix = IndexDef::from_info(&st.indexes);
    let loaded_fk = FkDef::from_info(&st.foreign_keys);
    let same = constraints::diff(
        d,
        &children,
        &names,
        &HashMap::new(),
        (&st.indexes, &st.foreign_keys),
        (&loaded_ix, &loaded_fk),
    )
    .unwrap();
    assert!(same.drops.is_empty() && same.creates.is_empty(), "{same:?}");

    let mut new_ix: Vec<IndexDef> = loaded_ix.iter().filter(|i| i.name != "ix_dc_pid").cloned().collect();
    new_ix.push(ix("ux_dc_code", &["code"], true));
    let new_fk: Vec<FkDef> = if engine == Engine::Sqlite { loaded_fk.clone() } else { Vec::new() };
    let plan =
        constraints::diff(d, &children, &names, &HashMap::new(), (&st.indexes, &st.foreign_keys), (&new_ix, &new_fk))
            .unwrap();
    let stmts: Vec<String> = plan.drops.iter().chain(&plan.creates).cloned().collect();
    s.execute_batch(&stmts, tx).await.unwrap();
    let st = s.table_structure(&children).await.unwrap();
    assert!(!st.indexes.iter().any(|i| i.name == "ix_dc_pid"), "{:?}", st.indexes);
    assert!(st.indexes.iter().any(|i| i.name == "ux_dc_code" && i.unique), "{:?}", st.indexes);
    if engine != Engine::Sqlite {
        assert!(st.foreign_keys.is_empty(), "{:?}", st.foreign_keys);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_suite() {
    suite(sqlite_target().await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_suite() {
    let Some((cfg, pw)) = from_env("TALUS_TEST_PG", Engine::Postgres) else {
        eprintln!("TALUS_TEST_PG not set; skipping");
        return;
    };
    suite(Target { cfg, password: Some(pw), ns: String::new(), slow_sql: "SELECT pg_sleep(30)" }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_suite() {
    let Some((cfg, pw)) = from_env("TALUS_TEST_MYSQL", Engine::MySql) else {
        eprintln!("TALUS_TEST_MYSQL not set; skipping");
        return;
    };
    suite(Target {
        cfg,
        password: Some(pw),
        ns: String::new(),
        slow_sql: "SELECT BENCHMARK(2000000000, SHA2('talus', 256))",
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlserver_suite() {
    let Some((cfg, pw)) = from_env("TALUS_TEST_MSSQL", Engine::SqlServer) else {
        eprintln!("TALUS_TEST_MSSQL not set; skipping");
        return;
    };
    suite(Target { cfg, password: Some(pw), ns: String::new(), slow_sql: "WAITFOR DELAY '00:00:30'" }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_failures_are_reported() {
    // Missing SQLite file
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "missing");
    cfg.database = "/nonexistent/dir/x.sqlite".into();
    assert!(matches!(Session::connect(&cfg, None, None).await, Err(DbError::Connect(_))));
    // Not a database
    let f = std::env::temp_dir().join(unique("notdb"));
    std::fs::write(&f, b"this is not a database file at all, definitely not").unwrap();
    cfg.database = f.to_string_lossy().into_owned();
    assert!(matches!(Session::connect(&cfg, None, None).await, Err(DbError::Connect(_))));
    // Unreachable port (connection refused quickly)
    let mut pg = ConnectionConfig::new(Engine::Postgres, "refused");
    pg.user = "x".into();
    pg.port = 1;
    pg.host = "127.0.0.1".into();
    pg.connect_timeout_secs = 3;
    assert!(matches!(Session::connect(&pg, None, None).await, Err(DbError::Connect(_))));
    let mut my = pg.clone();
    my.engine = Engine::MySql;
    assert!(matches!(Session::connect(&my, None, None).await, Err(DbError::Connect(_))));
    // Wrong password against real servers
    for (var, engine) in [
        ("TALUS_TEST_PG", Engine::Postgres),
        ("TALUS_TEST_MYSQL", Engine::MySql),
        ("TALUS_TEST_MSSQL", Engine::SqlServer),
    ] {
        if let Some((cfg, _)) = from_env(var, engine) {
            let err = Session::connect(&cfg, Some(&Secret::new("definitely-wrong")), None)
                .await
                .err()
                .expect("wrong password must fail");
            assert!(matches!(err, DbError::Connect(_)), "{err:?}");
            assert!(!err.to_string().contains("definitely-wrong"), "secret leaked into error: {err}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn init_sql_runs_after_connect() {
    let t = sqlite_target().await;
    let mut cfg = t.cfg.clone();
    cfg.init_sql = "PRAGMA user_version = 7;".into();
    let mut s = Session::connect(&cfg, None, None).await.unwrap();
    let r = s.run_script("PRAGMA user_version", &ScriptOptions::default()).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("7"));
    cfg.init_sql = "SELECT * FROM missing_table".into();
    assert!(
        matches!(Session::connect(&cfg, None, None).await, Err(DbError::Connect(m)) if m.contains("initial query"))
    );
}

/// `verify-full` must refuse an untrusted server certificate (MySQL 8.4 ships a self-signed one,
/// the PG test image has no TLS at all) and report an unreadable CA file instead of ignoring it.
#[tokio::test(flavor = "multi_thread")]
async fn verify_full_rejects_untrusted_server() {
    for (var, engine) in [
        ("TALUS_TEST_PG", Engine::Postgres),
        ("TALUS_TEST_MYSQL", Engine::MySql),
        ("TALUS_TEST_MSSQL", Engine::SqlServer),
    ] {
        let Some((mut cfg, pw)) = from_env(var, engine) else { continue };
        cfg.ssl_mode = SslMode::VerifyFull;
        let err = Session::connect(&cfg, Some(&pw), None).await.err().expect("untrusted server must fail");
        assert!(matches!(err, DbError::Connect(_)), "{engine:?}: {err:?}");
        cfg.ssl_ca_file = "/nonexistent/ca.pem".into();
        let err = Session::connect(&cfg, Some(&pw), None).await.err().expect("missing CA file must fail");
        assert!(matches!(err, DbError::Connect(_)), "{engine:?}: {err:?}");
        if engine == Engine::Postgres {
            assert!(err.to_string().contains("cannot read CA file"), "{err}");
        }
    }
}

/// A backend killed from outside is reported as `connection_lost` (not a plain SQL error)
/// and a fresh session works right after.
#[tokio::test(flavor = "multi_thread")]
async fn killed_backend_is_reported_as_connection_lost() {
    for (var, engine, pid_sql, kill) in [
        ("TALUS_TEST_PG", Engine::Postgres, "SELECT pg_backend_pid()::text", "SELECT pg_terminate_backend({})"),
        ("TALUS_TEST_MYSQL", Engine::MySql, "SELECT CONNECTION_ID()", "KILL {}"),
        ("TALUS_TEST_MSSQL", Engine::SqlServer, "SELECT CAST(@@SPID AS varchar(10))", "KILL {}"),
    ] {
        let Some((cfg, pw)) = from_env(var, engine) else { continue };
        let opts = ScriptOptions::default();
        let mut victim = Session::connect(&cfg, Some(&pw), None).await.unwrap();
        let r = victim.run_script(pid_sql, &opts).await;
        let pid = rows(&r.results[0].outcome).rows[0][0].clone().unwrap();
        let mut killer = Session::connect(&cfg, Some(&pw), None).await.unwrap();
        let r = killer.run_script(&kill.replace("{}", &pid), &opts).await;
        assert!(r.errors.is_empty(), "{engine:?}: {:?}", r.errors);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let r = victim.run_script("SELECT 1", &opts).await;
        assert!(r.connection_lost, "{engine:?}: expected connection_lost, got {:?}", r.errors);
        let mut fresh = Session::connect(&cfg, Some(&pw), None).await.unwrap();
        let r = fresh.run_script("SELECT 1", &opts).await;
        assert!(r.errors.is_empty() && !r.connection_lost, "{engine:?}");
    }
}

/// SQLite rebuild: type / nullability / default / PK / FK edits keep data and indexes.
#[tokio::test(flavor = "multi_thread")]
async fn sqlite_table_rebuild_keeps_data_and_indexes() {
    use talus_core::sql::Dialect;
    use talus_core::sql::constraints::{FkDef, IndexDef};
    use talus_core::sql::ddl::{ColumnDef, DesignColumn, rebuild_sqlite};
    let t = sqlite_target().await;
    let (t, mut s) = setup(t).await;
    let d = Dialect(Engine::Sqlite);
    let opts = ScriptOptions::default();
    s.execute_batch(
        &[
            "CREATE TABLE rb_parent (id INTEGER PRIMARY KEY)".into(),
            "CREATE TABLE rb_child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES rb_parent(id) ON DELETE CASCADE)"
                .into(),
            "CREATE TABLE rb (id INTEGER PRIMARY KEY, n TEXT, m INTEGER)".into(),
            "CREATE INDEX rb_m ON rb (m)".into(),
            "INSERT INTO rb_parent VALUES (1)".into(),
            "INSERT INTO rb_child VALUES (1, 1)".into(),
            "INSERT INTO rb (id, n, m) VALUES (1, 'a', 10), (2, 'b', 20)".into(),
        ],
        true,
    )
    .await
    .unwrap();
    let obj = t.obj("rb", ObjectKind::Table);
    let st = s.table_structure(&obj).await.unwrap();
    let mut edited: Vec<DesignColumn> =
        st.columns.iter().map(|c| DesignColumn { original: Some(c.name.clone()), def: ColumnDef::from(c) }).collect();
    edited[1].def.data_type = "VARCHAR(30)".into();
    edited[1].def.nullable = false;
    edited[1].def.default = Some("'z'".into());
    edited[2].def.name = "m2".into();
    edited.push(DesignColumn {
        original: None,
        def: ColumnDef { name: "extra".into(), data_type: "TEXT".into(), nullable: true, ..Default::default() },
    });
    let indexes = vec![IndexDef { original: None, name: "rb_m".into(), columns: vec!["m2".into()], unique: false }];
    let fks: Vec<FkDef> = Vec::new();
    let plan = rebuild_sqlite(d, &obj, &edited, &fks, &indexes).unwrap();
    s.execute_batch(&plan.statements, true).await.unwrap();

    let st = s.table_structure(&obj).await.unwrap();
    assert_eq!(st.column("n").unwrap().data_type, "VARCHAR(30)");
    assert!(!st.column("n").unwrap().nullable);
    assert!(st.column("m2").is_some() && st.column("extra").is_some());
    assert!(st.indexes.iter().any(|i| i.name == "rb_m" && i.columns == ["m2"]), "{:?}", st.indexes);
    let r = s.run_script("SELECT id, n, m2 FROM rb ORDER BY id", &opts).await;
    let q = rows(&r.results[0].outcome);
    assert_eq!(q.rows[1][1].as_deref(), Some("b"));
    assert_eq!(q.rows[1][2].as_deref(), Some("20"));

    // Rebuilding a table that other tables reference must not cascade-delete their rows,
    // and foreign-key enforcement is back on afterwards.
    let parent = t.obj("rb_parent", ObjectKind::Table);
    let pst = s.table_structure(&parent).await.unwrap();
    let pcols: Vec<DesignColumn> =
        pst.columns.iter().map(|c| DesignColumn { original: Some(c.name.clone()), def: ColumnDef::from(c) }).collect();
    let plan = rebuild_sqlite(d, &parent, &pcols, &[], &[]).unwrap();
    s.execute_batch(&plan.statements, true).await.unwrap();
    let r = s.run_script("SELECT count(*) FROM rb_child; PRAGMA foreign_keys", &opts).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("1"), "child rows survived");
    assert_eq!(rows(&r.results[1].outcome).rows[0][0].as_deref(), Some("1"), "foreign_keys restored");

    // A failing rebuild (NOT NULL over existing NULLs) rolls back and restores the pragma.
    s.execute_batch(&["INSERT INTO rb (id, n, m2) VALUES (3, 'c', NULL)".into()], true).await.unwrap();
    let mut bad: Vec<DesignColumn> =
        st.columns.iter().map(|c| DesignColumn { original: Some(c.name.clone()), def: ColumnDef::from(c) }).collect();
    bad.iter_mut().find(|c| c.def.name == "m2").unwrap().def.nullable = false;
    let plan = rebuild_sqlite(d, &obj, &bad, &[], &[]).unwrap();
    assert!(s.execute_batch(&plan.statements, true).await.is_err());
    let r = s.run_script("SELECT count(*) FROM rb; PRAGMA foreign_keys", &opts).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("3"));
    assert_eq!(rows(&r.results[1].outcome).rows[0][0].as_deref(), Some("1"));
}

/// SQLCipher: the file is unreadable without the key, wrong keys are refused, the right key works.
#[tokio::test(flavor = "multi_thread")]
async fn sqlite_encrypted_file_needs_the_key() {
    use talus_core::engine::create_database_file_with_key;
    let dir = std::env::temp_dir().join(unique("enc"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("secret.sqlite");
    let key = Secret::new("correct horse");
    create_database_file_with_key(&path, Some(&key)).unwrap();
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "enc");
    cfg.database = path.to_string_lossy().into_owned();
    cfg.encrypted = true;
    assert!(cfg.needs_password());

    let mut s = Session::connect(&cfg, Some(&key), None).await.unwrap();
    s.execute_batch(&["CREATE TABLE t (a TEXT)".into(), "INSERT INTO t VALUES ('plain-marker-12345')".into()], false)
        .await
        .unwrap();
    s.close().await.unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert!(!bytes.starts_with(b"SQLite format 3"), "header must be encrypted");
    assert!(!bytes.windows(18).any(|w| w == b"plain-marker-12345"), "data must not appear in clear text");

    let wrong = Session::connect(&cfg, Some(&Secret::new("nope")), None).await.err().expect("wrong key");
    assert!(matches!(wrong, DbError::Connect(_)) && wrong.to_string().contains("wrong password"), "{wrong}");
    assert!(!wrong.to_string().contains("nope"));
    let none = Session::connect(&cfg, None, None).await.err().expect("no key");
    assert!(matches!(none, DbError::Connect(_)), "{none}");

    let mut s = Session::connect(&cfg, Some(&key), None).await.unwrap();
    let r = s.run_script("SELECT a FROM t", &ScriptOptions::default()).await;
    assert_eq!(rows(&r.results[0].outcome).rows[0][0].as_deref(), Some("plain-marker-12345"));

    // A plain (unencrypted) connection config keeps ignoring any password.
    cfg.encrypted = false;
    let plain = dir.join("plain.sqlite");
    create_database_file(&plain).unwrap();
    cfg.database = plain.to_string_lossy().into_owned();
    Session::connect(&cfg, Some(&key), None).await.unwrap();
}

/// View synchronization between two databases that share schema names.
#[tokio::test(flavor = "multi_thread")]
async fn view_sync_creates_and_replaces_views() {
    use talus_core::sql::sync::view_sync;
    // (source session, target session, objects' database / schema, cleanup)
    async fn run(
        engine: Engine,
        mut src: Session,
        mut dst: Session,
        db: &str,
        schema: Option<&str>,
        q: &dyn Fn(&str) -> String,
    ) {
        let opts = ScriptOptions::default();
        for s in [&mut src, &mut dst] {
            s.execute_batch(&[format!("CREATE TABLE {} (a int, b int)", q("vt"))], false).await.unwrap();
        }
        let v = q("vv");
        src.execute_batch(&[format!("CREATE VIEW {v} AS SELECT a FROM {}", q("vt"))], false).await.unwrap();
        dst.execute_batch(&[format!("CREATE VIEW {} AS SELECT 1 AS stale", q("gone"))], false).await.unwrap();
        for round in 0..2 {
            let a = src.views_with_ddl(db, schema).await.unwrap();
            let b = dst.views_with_ddl(db, schema).await.unwrap();
            let plan = view_sync(engine, &a, &b, round == 0, true).unwrap();
            assert!(!plan.statements.is_empty(), "{engine:?} round {round}: {plan:?}");
            dst.execute_batch(&plan.statements, false).await.unwrap();
            if round == 0 {
                assert!(
                    dst.views_with_ddl(db, schema).await.unwrap().iter().all(|(o, _)| o.name != "gone"),
                    "dropped on request"
                );
                // Change the definition on the source and sync again.
                let drop = if engine == Engine::Sqlite { vec![format!("DROP VIEW {v}")] } else { vec![] };
                src.execute_batch(&drop, false).await.unwrap();
                let create = if engine == Engine::Sqlite { "CREATE VIEW" } else { "CREATE OR REPLACE VIEW" };
                src.execute_batch(&[format!("{create} {v} AS SELECT a, b FROM {}", q("vt"))], false).await.unwrap();
            }
        }
        let r = dst.run_script(&format!("SELECT * FROM {v}"), &opts).await;
        assert_eq!(
            rows(&r.results[0].outcome).columns.len(),
            2,
            "{engine:?}: the replaced definition reached the target"
        );
        let a = src.views_with_ddl(db, schema).await.unwrap();
        let b = dst.views_with_ddl(db, schema).await.unwrap();
        assert!(
            view_sync(engine, &a, &b, false, true).unwrap().statements.is_empty(),
            "{engine:?}: in sync afterwards"
        );
    }

    // SQLite: two files, both called `main`.
    let mk = |n: &str| {
        let f = std::env::temp_dir().join(format!("{}.sqlite", unique(n)));
        create_database_file(&f).unwrap();
        let mut cfg = ConnectionConfig::new(Engine::Sqlite, n);
        cfg.database = f.to_string_lossy().into_owned();
        cfg
    };
    let (a, b) = (mk("vsrc"), mk("vdst"));
    let q = |n: &str| format!("\"main\".\"{n}\"");
    run(
        Engine::Sqlite,
        Session::connect(&a, None, None).await.unwrap(),
        Session::connect(&b, None, None).await.unwrap(),
        "main",
        None,
        &q,
    )
    .await;

    // PostgreSQL: two databases, both with schema `public`.
    if let Some((cfg, pw)) = from_env("TALUS_TEST_PG", Engine::Postgres) {
        let mut admin = Session::connect(&cfg, Some(&pw), None).await.unwrap();
        let (n1, n2) = (unique("vs_a"), unique("vs_b"));
        admin
            .execute_batch(&[format!("CREATE DATABASE \"{n1}\""), format!("CREATE DATABASE \"{n2}\"")], false)
            .await
            .unwrap();
        let s1 = Session::connect(&cfg, Some(&pw), Some(&n1)).await.unwrap();
        let s2 = Session::connect(&cfg, Some(&pw), Some(&n2)).await.unwrap();
        let q = |n: &str| format!("\"public\".\"{n}\"");
        run(Engine::Postgres, s1, s2, &n1, Some("public"), &q).await;
        // `run` consumed both sessions; give the server a moment to release the databases.
        tokio::time::sleep(Duration::from_millis(300)).await;
        admin
            .execute_batch(
                &[format!("DROP DATABASE \"{n1}\" WITH (FORCE)"), format!("DROP DATABASE \"{n2}\" WITH (FORCE)")],
                false,
            )
            .await
            .unwrap();
    }
}
