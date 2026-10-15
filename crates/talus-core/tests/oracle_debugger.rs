//! PL/SQL debugger (DBMS_DEBUG) against the disposable Oracle Free container
//! (`docker-compose.test.yml`, profile `oracle`). Runs when `TALUS_TEST_ORACLE` is set; the test account
//! needs DEBUG CONNECT SESSION and DEBUG ANY PROCEDURE (`docker/oracle-init.sql` grants them).

use talus_core::Secret;
use talus_core::debugger::{DebugEvent, DebugTarget, Debugger, Snapshot, Step, describe};
use talus_core::engine::{ScriptOptions, Session};
use talus_core::model::{ConnectionConfig, Engine, Outcome, SslMode};

fn oracle() -> Option<(ConnectionConfig, Secret)> {
    let v = std::env::var("TALUS_TEST_ORACLE").ok()?;
    let p: Vec<&str> = v.splitn(5, ':').collect();
    let mut cfg = ConnectionConfig::new(Engine::Oracle, "dbg");
    cfg.host = p[0].into();
    cfg.port = p[1].parse().unwrap();
    cfg.user = p[2].into();
    cfg.database = p[4].into();
    cfg.ssl_mode = SslMode::Disable;
    Some((cfg, Secret::new(p[3])))
}

fn unique(prefix: &str) -> String {
    format!("{prefix}_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]).to_uppercase()
}

fn user_of(cfg: &ConnectionConfig) -> String {
    cfg.user.to_uppercase()
}

async fn run(s: &mut Session, sql: &str) {
    let r = s.run_script(sql, &ScriptOptions::default()).await;
    assert!(r.errors.is_empty(), "{sql}\n{:?}", r.errors);
}

async fn object_id(s: &mut Session, owner: &str, name: &str) -> u32 {
    let sql = format!("SELECT object_id FROM all_objects WHERE owner = '{owner}' AND object_name = '{name}'");
    match &s.run_script(&sql, &ScriptOptions::default()).await.results[0].outcome {
        Outcome::Rows(q) => q.rows[0][0].as_deref().unwrap().parse().unwrap(),
        o => panic!("{o:?}"),
    }
}

/// Creates function `name` (debug-compiled unless `debug` is false) and returns its debug target.
async fn function(cfg: &ConnectionConfig, pw: &Secret, name: &str, body: &str, debug: bool) -> (Session, DebugTarget) {
    let mut admin = Session::connect(cfg, Some(pw), None).await.unwrap();
    run(&mut admin, &format!("CREATE OR REPLACE {body}")).await;
    if debug {
        run(
            &mut admin,
            &format!(
                "ALTER {} {name} COMPILE DEBUG",
                if body.starts_with("PROCEDURE") { "PROCEDURE" } else { "FUNCTION" }
            ),
        )
        .await;
    }
    let id = object_id(&mut admin, &user_of(cfg), name).await;
    let target = describe(&mut admin, id).await.unwrap();
    (admin, target)
}

fn sum_function(name: &str) -> String {
    format!(
        "FUNCTION {name}(n IN NUMBER, label IN VARCHAR2 DEFAULT 'x') RETURN NUMBER AS\n  total NUMBER := 0;\nBEGIN\n  FOR i IN 1..n LOOP\n    total := total + i;\n  END LOOP;\n  RETURN total;\nEND;"
    )
}

/// 1-based line of the source containing `text`, as the debugger counts.
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

/// What the target printed through DBMS_OUTPUT.
fn printed(o: &talus_core::engine::ScriptOutcome) -> Vec<String> {
    o.results
        .iter()
        .filter_map(|r| match &r.outcome {
            Outcome::Rows(q) if q.columns[0].name == "DBMS_OUTPUT" => Some(q),
            _ => None,
        })
        .flat_map(|q| q.rows.iter().filter_map(|r| r[0].clone()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn step_inspect_set_value_breakpoints_and_finish() {
    let Some((cfg, pw)) = oracle() else {
        eprintln!("TALUS_TEST_ORACLE not set; skipping");
        return;
    };
    let name = unique("DBG_SUM");
    let (mut admin, target) = function(&cfg, &pw, &name, &sum_function(&name), true).await;
    let oid = target.oid;
    assert_eq!(target.signature, format!("{}.{name}(N NUMBER, LABEL VARCHAR2)", user_of(&cfg)));
    assert_eq!(target.params.len(), 2);
    assert_eq!(target.returns.as_deref(), Some("NUMBER"));
    let ret = line_of(&target.source, "RETURN total");

    // Run 1: pause at entry, inspect, step, change a variable, continue to the end.
    let args = [Some("3".into()), Some("hello".into())];
    let (mut dbg, first) = Debugger::start(&cfg, Some(&pw), None, &target, &args, &[]).await.unwrap();
    let s = paused(first);
    assert_eq!(s.func, oid);
    assert!(s.line > 0);
    assert_eq!(var(&s, "N").as_deref(), Some("3"));
    assert_eq!(var(&s, "LABEL").as_deref(), Some("hello"));
    assert!(s.source.contains("total := total + i"), "{}", s.source);
    assert_eq!(s.stack.len(), 1);
    assert!(s.stack[0].name.ends_with(&name), "{:?}", s.stack);
    let mut s = paused(dbg.step(Step::Over).await.unwrap());
    for _ in 0..20 {
        if var(&s, "TOTAL").as_deref() == Some("1") {
            break;
        }
        s = paused(dbg.step(Step::Over).await.unwrap());
    }
    assert_eq!(var(&s, "TOTAL").as_deref(), Some("1"), "stepping updates variables: {:?}", s.variables);
    let s = dbg.set_value("TOTAL", 0, "100").await.unwrap();
    assert_eq!(var(&s, "TOTAL").as_deref(), Some("100"));
    match dbg.step(Step::Continue).await.unwrap() {
        // 100 + 2 + 3
        DebugEvent::Finished(o) => assert_eq!(printed(&o), vec!["result = 105".to_string()], "{:?}", o.results),
        DebugEvent::Paused(s) => panic!("expected the end, paused at line {}", s.line),
    }
    assert!(!dbg.is_running());
    assert!(dbg.step(Step::Over).await.is_err(), "a finished run cannot step");
    dbg.stop().await.unwrap();

    // Run 2: a breakpoint on RETURN; Continue stops there with the loop done; the breakpoint can be dropped.
    let (mut dbg, first) =
        Debugger::start(&cfg, Some(&pw), None, &target, &[Some("4".into()), None], &[ret]).await.unwrap();
    paused(first);
    let s = paused(dbg.step(Step::Continue).await.unwrap());
    assert_eq!(s.line, ret);
    assert_eq!(var(&s, "TOTAL").as_deref(), Some("10"));
    assert_eq!(var(&s, "LABEL"), None, "a NULL argument has no value");
    dbg.set_breakpoint(oid, ret, false).await.unwrap();
    match dbg.step(Step::Continue).await.unwrap() {
        DebugEvent::Finished(o) => assert_eq!(printed(&o), vec!["result = 10".to_string()]),
        DebugEvent::Paused(s) => panic!("paused at {}", s.line),
    }
    dbg.stop().await.unwrap();

    // Run 3: stopping a paused routine ends it; the accounts' sessions are gone afterwards.
    let (dbg, first) = Debugger::start(&cfg, Some(&pw), None, &target, &[Some("2".into()), None], &[]).await.unwrap();
    paused(first);
    tokio::time::timeout(std::time::Duration::from_secs(20), dbg.stop()).await.expect("stop returns").unwrap();

    run(&mut admin, &format!("DROP FUNCTION {name}")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn step_into_a_callee_and_out_again() {
    let Some((cfg, pw)) = oracle() else {
        eprintln!("TALUS_TEST_ORACLE not set; skipping");
        return;
    };
    let inner = unique("DBG_INNER");
    let outer = unique("DBG_OUTER");
    let (mut admin, _) = function(&cfg, &pw, &inner, &sum_function(&inner), true).await;
    let outer_src = format!(
        "FUNCTION {outer}(n IN NUMBER) RETURN NUMBER AS\n  r NUMBER;\nBEGIN\n  r := {inner}(n) * 2;\n  RETURN r;\nEND;"
    );
    let (_, target) = function(&cfg, &pw, &outer, &outer_src, true).await;
    let call_line = line_of(&target.source, "r := ");

    // The run pauses on entry; stepping over reaches the call.
    let (mut dbg, first) = Debugger::start(&cfg, Some(&pw), None, &target, &[Some("3".into())], &[]).await.unwrap();
    let mut at_call = paused(first);
    for _ in 0..5 {
        if at_call.line == call_line {
            break;
        }
        at_call = paused(dbg.step(Step::Over).await.unwrap());
    }
    assert_eq!(at_call.line, call_line);
    assert_eq!(at_call.func, target.oid);

    // Into: the callee's source, one frame deeper, the caller's line kept in the stack.
    let s = paused(
        tokio::time::timeout(std::time::Duration::from_secs(15), dbg.step(Step::Into))
            .await
            .expect("into returns")
            .unwrap(),
    );
    assert_ne!(s.func, target.oid);
    assert!(s.source.contains("total := total + i"), "{}", s.source);
    assert_eq!(s.stack.len(), 2, "{:?}", s.stack);
    assert!(s.stack[0].name.ends_with(&inner) && s.stack[1].name.ends_with(&outer), "{:?}", s.stack);
    assert_eq!(s.stack[1].line, call_line);
    // Values are readable once the callee is running its body.
    let s = paused(dbg.step(Step::Over).await.unwrap());
    assert_eq!(var(&s, "N").as_deref(), Some("3"), "{:?}", s.variables);

    // Out: back in the caller, after the call returned.
    let s = paused(
        tokio::time::timeout(std::time::Duration::from_secs(15), dbg.step(Step::Out))
            .await
            .expect("out returns")
            .unwrap(),
    );
    assert_eq!(s.func, target.oid, "{:?}", s.stack);
    assert_eq!(s.stack.len(), 1);
    match dbg.step(Step::Continue).await.unwrap() {
        DebugEvent::Finished(o) => assert_eq!(printed(&o), vec!["result = 12".to_string()]),
        DebugEvent::Paused(s) => panic!("paused at {}", s.line),
    }
    dbg.stop().await.unwrap();
    run(&mut admin, &format!("DROP FUNCTION {outer}")).await;
    run(&mut admin, &format!("DROP FUNCTION {inner}")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_routines_without_debug_info_and_reports_failures() {
    let Some((cfg, pw)) = oracle() else {
        eprintln!("TALUS_TEST_ORACLE not set; skipping");
        return;
    };
    // Compiled without debug information: a clear message, the object is not touched.
    let plain = unique("DBG_PLAIN");
    let (mut admin, target) = function(&cfg, &pw, &plain, &sum_function(&plain), false).await;
    let err =
        Debugger::start(&cfg, Some(&pw), None, &target, &[Some("1".into()), None], &[]).await.err().expect("refused");
    assert!(err.to_string().contains("COMPILE DEBUG"), "{err}");
    let r = admin
        .run_script(
            &format!("SELECT plsql_debug FROM user_plsql_object_settings WHERE name = '{plain}'"),
            &ScriptOptions::default(),
        )
        .await;
    match &r.results[0].outcome {
        Outcome::Rows(q) => assert_eq!(q.rows[0][0].as_deref(), Some("FALSE"), "left as it was"),
        o => panic!("{o:?}"),
    }
    // Wrong argument count and unsupported kinds are rejected before anything starts.
    assert!(Debugger::start(&cfg, Some(&pw), None, &target, &[], &[]).await.is_err());
    run(&mut admin, &format!("CREATE OR REPLACE PACKAGE {plain}_PK AS PROCEDURE p; END;")).await;
    let pk = object_id(&mut admin, &user_of(&cfg), &format!("{plain}_PK")).await;
    assert!(describe(&mut admin, pk).await.is_err(), "packages are not offered");

    // An exception inside the routine ends the run with the error in its outcome.
    let boom = unique("DBG_BOOM");
    let src = format!("FUNCTION {boom}(n IN NUMBER) RETURN NUMBER AS\nBEGIN\n  RETURN 1 / n;\nEND;");
    let (_, target) = function(&cfg, &pw, &boom, &src, true).await;
    let (mut dbg, first) = Debugger::start(&cfg, Some(&pw), None, &target, &[Some("0".into())], &[]).await.unwrap();
    paused(first);
    match dbg.step(Step::Continue).await.unwrap() {
        DebugEvent::Finished(o) => {
            assert!(o.errors.iter().any(|e| e.error.to_string().contains("divisor is equal to zero")), "{:?}", o.errors)
        }
        DebugEvent::Paused(s) => panic!("paused at {}", s.line),
    }
    dbg.stop().await.unwrap();

    run(&mut admin, &format!("DROP PACKAGE {plain}_PK")).await;
    run(&mut admin, &format!("DROP FUNCTION {plain}")).await;
    run(&mut admin, &format!("DROP FUNCTION {boom}")).await;
}
