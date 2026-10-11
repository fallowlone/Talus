//! PL/pgSQL debugger against the disposable PostgreSQL container built with pldebugger
//! (docker/postgres-debugger.Dockerfile). Runs when TALUS_TEST_PG is set.

use talus_core::Secret;
use talus_core::debugger::{DebugEvent, DebugTarget, PgDebugger, Snapshot, Step, describe};
use talus_core::engine::{ScriptOptions, Session};
use talus_core::model::{ConnectionConfig, Engine, Outcome, SslMode};

fn pg() -> Option<(ConnectionConfig, Secret)> {
    let v = std::env::var("TALUS_TEST_PG").ok()?;
    let p: Vec<&str> = v.splitn(5, ':').collect();
    let mut cfg = ConnectionConfig::new(Engine::Postgres, "dbg");
    cfg.host = p[0].into();
    cfg.port = p[1].parse().unwrap();
    cfg.user = p[2].into();
    cfg.database = p[4].into();
    cfg.ssl_mode = SslMode::Disable;
    Some((cfg, Secret::new(p[3])))
}

/// Test routine; `{name}` gives each test its own function.
const FUNC: &str = "CREATE OR REPLACE FUNCTION public.{name}(n integer) RETURNS integer LANGUAGE plpgsql AS $body$
DECLARE
  total integer := 0;
  i integer;
BEGIN
  FOR i IN 1..n LOOP
    total := total + i;
  END LOOP;
  RETURN total;
END
$body$";

/// The tests here run in parallel; setup is serialised (concurrent `CREATE EXTENSION` or
/// `CREATE OR REPLACE FUNCTION` fail with "tuple concurrently updated").
static SETUP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Creates routine `name`; returns an admin session and its debug target.
async fn routine(cfg: &ConnectionConfig, pw: &Secret, name: &str) -> (Session, DebugTarget) {
    let _guard = SETUP.lock().await;
    let mut admin = Session::connect(cfg, Some(pw), None).await.unwrap();
    let sql = format!("CREATE EXTENSION IF NOT EXISTS pldbgapi;\n{};", FUNC.replace("{name}", name));
    let r = admin.run_script(&sql, &ScriptOptions::default()).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let lookup = format!("SELECT 'public.{name}(integer)'::regprocedure::oid");
    let oid: u32 = match &admin.run_script(&lookup, &ScriptOptions::default()).await.results[0].outcome {
        Outcome::Rows(q) => q.rows[0][0].as_deref().unwrap().parse().unwrap(),
        o => panic!("{o:?}"),
    };
    let target = describe(&mut admin, oid).await.unwrap();
    (admin, target)
}

/// Line of the routine body (1-based, as the debugger counts) containing `text`.
fn line_of(source: &str, text: &str) -> i32 {
    source.lines().position(|l| l.contains(text)).expect("line in source") as i32 + 1
}

fn paused(e: DebugEvent) -> Snapshot {
    match e {
        DebugEvent::Paused(s) => s,
        DebugEvent::Finished(o) => panic!("expected a pause, the routine finished: {:?}", o.errors),
    }
}

fn var(s: &Snapshot, name: &str) -> Option<String> {
    s.variables.iter().find(|v| v.name == name).and_then(|v| v.value.clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn step_inspect_set_value_breakpoints_and_finish() {
    let Some((cfg, pw)) = pg() else {
        eprintln!("TALUS_TEST_PG not set; skipping");
        return;
    };
    let (mut admin, target) = routine(&cfg, &pw, "dbg_sum").await;
    let oid = target.oid;
    assert_eq!(target.signature, "public.dbg_sum(n integer)");
    assert_eq!(target.params.len(), 1);
    assert_eq!((target.params[0].name.as_str(), target.params[0].type_name.as_str()), ("n", "integer"));
    let ret = line_of(&target.source, "RETURN total");

    // Run 1: pause at entry, inspect, step, change a variable, continue to the end.
    let (mut dbg, first) = PgDebugger::start(&cfg, Some(&pw), None, &target, &[Some("3".into())], &[]).await.unwrap();
    let s = paused(first);
    assert_eq!(s.func, oid);
    assert!(s.line > 0);
    assert_eq!(var(&s, "n").as_deref(), Some("3"));
    assert!(s.variables.iter().any(|v| v.name == "n" && v.class == 'A'), "{:?}", s.variables);
    assert!(s.source.contains("total := total + i"));
    assert_eq!(s.stack[0].name, "dbg_sum(integer)");
    let mut s = paused(dbg.step(Step::Over).await.unwrap());
    for _ in 0..20 {
        if var(&s, "total").as_deref() == Some("1") {
            break;
        }
        s = paused(dbg.step(Step::Over).await.unwrap());
    }
    assert_eq!(var(&s, "total").as_deref(), Some("1"), "stepping updates variables");
    let s = dbg.set_value("total", s.variables.iter().find(|v| v.name == "total").unwrap().line, "100").await.unwrap();
    assert_eq!(var(&s, "total").as_deref(), Some("100"));
    match dbg.step(Step::Continue).await.unwrap() {
        // 100 + 2 + 3
        DebugEvent::Finished(o) => match &o.results[0].outcome {
            Outcome::Rows(q) => assert_eq!(q.rows[0][0].as_deref(), Some("105")),
            o => panic!("{o:?}"),
        },
        DebugEvent::Paused(s) => panic!("expected the end, paused at line {}", s.line),
    }
    assert!(!dbg.is_running());
    assert!(dbg.step(Step::Over).await.is_err(), "a finished run cannot step");
    dbg.stop().await.unwrap();

    // Run 2: a breakpoint on RETURN; Continue stops there with the loop done.
    let (mut dbg, first) =
        PgDebugger::start(&cfg, Some(&pw), None, &target, &[Some("4".into())], &[ret]).await.unwrap();
    paused(first);
    let s = paused(dbg.step(Step::Continue).await.unwrap());
    assert_eq!(s.line, ret);
    assert_eq!(var(&s, "total").as_deref(), Some("10"));
    // Stop aborts the routine; the server stays usable.
    dbg.stop().await.unwrap();
    let r = admin.run_script("SELECT public.dbg_sum(2)", &ScriptOptions::default()).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    admin.close().await.unwrap();
}

/// pldebugger has 50 shared connection slots; the global-breakpoint mode leaked one per session.
/// More sessions than slots must keep working.
#[tokio::test(flavor = "multi_thread")]
async fn more_sessions_than_pldebugger_slots_keep_working() {
    let Some((cfg, pw)) = pg() else {
        eprintln!("TALUS_TEST_PG not set; skipping");
        return;
    };
    let (admin, target) = routine(&cfg, &pw, "dbg_sum_many").await;
    admin.close().await.unwrap();
    for i in 0..55 {
        let (mut dbg, first) =
            PgDebugger::start(&cfg, Some(&pw), None, &target, &[Some("2".into())], &[]).await.unwrap();
        paused(first);
        match dbg.step(Step::Continue).await.unwrap() {
            DebugEvent::Finished(o) => assert!(o.errors.is_empty(), "run {i}: {:?}", o.errors),
            DebugEvent::Paused(s) => panic!("run {i}: paused at {}", s.line),
        }
        dbg.stop().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn step_into_a_callee_and_out_again() {
    let Some((cfg, pw)) = pg() else {
        eprintln!("TALUS_TEST_PG not set; skipping");
        return;
    };
    let (admin, outer) = {
        let _guard = SETUP.lock().await;
        let mut admin = Session::connect(&cfg, Some(&pw), None).await.unwrap();
        let r = admin
            .run_script(
                "CREATE EXTENSION IF NOT EXISTS pldbgapi;
                 CREATE OR REPLACE FUNCTION public.dbg_inner(x integer) RETURNS integer LANGUAGE plpgsql AS $b$
                 DECLARE y integer;
                 BEGIN
                   y := x + 1;
                   y := y + 1;
                   RETURN y;
                 END $b$;
                 CREATE OR REPLACE FUNCTION public.dbg_outer(n integer) RETURNS integer LANGUAGE plpgsql AS $b$
                 DECLARE r integer;
                 BEGIN
                   r := public.dbg_inner(n);
                   r := r * 10;
                   RETURN r;
                 END $b$;",
                &ScriptOptions::default(),
            )
            .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let lookup = "SELECT 'public.dbg_outer(integer)'::regprocedure::oid";
        let oid: u32 = match &admin.run_script(lookup, &ScriptOptions::default()).await.results[0].outcome {
            Outcome::Rows(q) => q.rows[0][0].as_deref().unwrap().parse().unwrap(),
            o => panic!("{o:?}"),
        };
        let t = describe(&mut admin, oid).await.unwrap();
        (admin, t)
    };
    let (mut dbg, first) = PgDebugger::start(&cfg, Some(&pw), None, &outer, &[Some("1".into())], &[]).await.unwrap();
    let mut s = paused(first);
    // Step until the call line, then into the callee.
    for _ in 0..10 {
        if s.source.lines().nth((s.line - 1) as usize).is_some_and(|l| l.contains("dbg_inner(n)")) {
            break;
        }
        s = paused(dbg.step(Step::Over).await.unwrap());
    }
    let inner = paused(dbg.step(Step::Into).await.unwrap());
    assert_eq!(inner.stack.len(), 2, "{:?}", inner.stack);
    assert!(inner.stack[0].name.starts_with("dbg_inner"), "{:?}", inner.stack);
    let back = paused(dbg.step(Step::Out).await.unwrap());
    assert_eq!(back.stack.len(), 1, "{:?}", back.stack);
    assert_eq!(back.func, outer.oid);
    match dbg.step(Step::Out).await.unwrap() {
        // Out of the outermost routine runs it to the end: (1 + 2) * 10.
        DebugEvent::Finished(o) => match &o.results[0].outcome {
            Outcome::Rows(q) => assert_eq!(q.rows[0][0].as_deref(), Some("30")),
            o => panic!("{o:?}"),
        },
        DebugEvent::Paused(s) => panic!("expected the end, paused at line {}", s.line),
    }
    dbg.stop().await.unwrap();
    admin.close().await.unwrap();
}
