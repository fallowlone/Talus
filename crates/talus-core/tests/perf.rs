//! Performance probes (run with `cargo test --release -p talus-core --test perf -- --ignored --nocapture`).
//! They print timings and assert generous upper bounds so regressions are caught.

use std::time::{Duration, Instant};

use talus_core::engine::{ScriptOptions, Session, create_database_file};
use talus_core::export::{ExportFormat, export_table};
use talus_core::model::*;
use talus_core::sql::split::split;

async fn big_sqlite(rows: u64) -> (Session, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("talus-perf-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("big.sqlite");
    create_database_file(&db).unwrap();
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "perf");
    cfg.database = db.to_string_lossy().into_owned();
    let mut s = Session::connect(&cfg, None, None).await.unwrap();
    let r = s
        .run_script(
            &format!(
                "CREATE TABLE big (id INTEGER PRIMARY KEY, name TEXT, amount REAL, note TEXT);
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {rows})
                 INSERT INTO big SELECT i, 'name ' || i, i * 0.5, 'lorem ipsum dolor sit amet ' || (i % 97) FROM n;"
            ),
            &ScriptOptions::default(),
        )
        .await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    (s, dir)
}

fn timed<T>(label: &str, start: Instant, limit: Duration, v: T) -> T {
    let e = start.elapsed();
    println!("{label}: {:.1} ms", e.as_secs_f64() * 1000.0);
    assert!(e < limit, "{label} took {e:?} (limit {limit:?})");
    v
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn perf_one_million_rows() {
    let (mut s, dir) = big_sqlite(1_000_000).await;
    let obj = ObjectRef { database: "main".into(), schema: None, name: "big".into(), kind: ObjectKind::Table };

    let t = Instant::now();
    let r = s
        .run_script(
            "SELECT * FROM big",
            &ScriptOptions { limits: FetchLimits { max_rows: 10_000 }, ..Default::default() },
        )
        .await;
    timed("editor SELECT * (cap 10k of 1M)", t, Duration::from_secs(5), ());
    let Outcome::Rows(q) = &r.results[0].outcome else { panic!() };
    assert!(q.truncated && q.rows.len() == 10_000);

    let st = s.table_structure(&obj).await.unwrap();
    let t = Instant::now();
    let page = s
        .fetch_page(&PageQuery { table: obj.clone(), filters: vec![], sort: vec![], offset: 900_000, limit: 1000 }, &st)
        .await
        .unwrap();
    timed("data page at offset 900k", t, Duration::from_secs(2), ());
    assert_eq!(page.rows[0][0].as_deref(), Some("900001"));

    let t = Instant::now();
    let filtered = s
        .fetch_page(
            &PageQuery {
                table: obj.clone(),
                filters: vec![Filter {
                    enabled: true,
                    column: "note".into(),
                    op: FilterOp::Contains,
                    value: "amet 42".into(),
                }],
                sort: vec![("amount".into(), SortDir::Desc)],
                offset: 0,
                limit: 1000,
            },
            &st,
        )
        .await
        .unwrap();
    timed("filtered + sorted page over 1M", t, Duration::from_secs(5), ());
    assert_eq!(filtered.rows.len(), 1000);

    let out = dir.join("big.csv");
    let t = Instant::now();
    let n = export_table(&mut s, &obj, ExportFormat::Csv, &out).await.unwrap();
    timed("streaming CSV export of 1M rows", t, Duration::from_secs(60), ());
    assert_eq!(n, 1_000_000);
    println!("export file: {:.1} MB", std::fs::metadata(&out).unwrap().len() as f64 / 1e6);
    s.close().await.unwrap();
}

#[test]
#[ignore]
fn perf_split_large_script() {
    let mut script = String::new();
    while script.len() < 5_000_000 {
        script.push_str("INSERT INTO t (a, b) VALUES (1, 'it''s; fine') -- comment; here\n;\n");
    }
    let t = Instant::now();
    let n = split(&script, Engine::Postgres).len();
    timed("split 5 MB script", t, Duration::from_secs(3), ());
    assert!(n > 60_000);
    let t = Instant::now();
    let f = talus_core::sql::format::format_sql(&script, Engine::Postgres);
    timed("format 5 MB script", t, Duration::from_secs(5), ());
    assert!(f.len() > 4_000_000);
}
