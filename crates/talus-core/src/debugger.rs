//! PL/pgSQL debugger on top of the server's pldebugger extension (`pldbgapi`, preloaded
//! `plugin_debugger`). Two sessions: the *target* runs the routine and blocks inside it at
//! breakpoints; the *control* session attaches to it (`pldbg_attach_to_port` after the target's
//! `PLDBGBREAK` notice), drives it (`pldbg_step_over`, `pldbg_continue`, …) and reads stack and
//! variables. A control call that is waiting when the routine returns is raced against the target's
//! completion, so the end of the run is always noticed.

use tokio::task::JoinHandle;

use crate::engine::{CancelHandle, ScriptOptions, ScriptOutcome, Session};
use crate::error::{DbError, DbResult};
use crate::model::{ConnectionConfig, Engine, Outcome};
use crate::secret::Secret;
use crate::sql::Dialect;

/// A routine as the debugger needs it: what to call and the source its line numbers refer to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DebugTarget {
    pub oid: u32,
    /// `schema.name(arg types)` for display.
    pub signature: String,
    /// Fully quoted `schema.name`, used to build the call.
    pub call_name: String,
    pub is_procedure: bool,
    /// Input parameters in order (`IN` / `INOUT` / `VARIADIC`).
    pub params: Vec<DebugParam>,
    /// Routine body; debugger line numbers count its lines from 1.
    pub source: String,
    /// Every parameter in call order, with its mode (Oracle builds a block with a variable for each).
    pub args: Vec<DebugArg>,
    /// Oracle function return type.
    pub returns: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DebugParam {
    pub name: String,
    pub type_name: String,
}

/// One parameter of an Oracle routine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugArg {
    pub name: String,
    pub type_name: String,
    /// `I` in, `O` out, `B` in/out.
    pub mode: char,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub level: i32,
    pub name: String,
    pub func: u32,
    pub line: i32,
    pub args: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    pub name: String,
    /// `A` argument, `L` local, `P` package (pldebugger's `varclass`).
    pub class: char,
    pub line: i32,
    pub type_name: String,
    pub value: Option<String>,
}

/// Where the routine is paused, with what the panes show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub func: u32,
    pub line: i32,
    pub stack: Vec<Frame>,
    pub variables: Vec<Variable>,
    /// Source of the routine in `func` (a callee when stepped into).
    pub source: String,
}

#[derive(Debug)]
pub enum DebugEvent {
    Paused(Snapshot),
    /// The routine returned (or failed); its result is the target session's outcome.
    Finished(ScriptOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Over,
    Into,
    Continue,
    /// Run until the current routine returns to its caller. pldebugger has no step-out, so this
    /// steps over until the stack is shallower (at most `STEP_OUT_LIMIT` steps); in the outermost
    /// routine it continues.
    Out,
}

/// Upper bound of single steps one Step Out may take (a long loop pauses after it).
const STEP_OUT_LIMIT: usize = 10_000;

impl Step {
    fn function(self) -> &'static str {
        match self {
            Step::Over | Step::Out => "pldbg_step_over",
            Step::Into => "pldbg_step_into",
            Step::Continue => "pldbg_continue",
        }
    }
}

type TargetRun = JoinHandle<(Session, ScriptOutcome)>;

/// Attach attempts per start. pldebugger's proxy treats any interrupted `select()` (e.g. by the
/// sinval catch-up signal that concurrent DDL on the server sends to idle backends) as a lost
/// target ("select() failed waiting for target"); a fresh attach usually succeeds.
const START_ATTEMPTS: usize = 3;

fn interrupted(e: &DbError) -> bool {
    e.to_string().contains("select() failed waiting for target")
}

pub struct PgDebugger {
    ctl: Session,
    sid: i64,
    target: Option<TargetRun>,
    target_cancel: CancelHandle,
    /// Stack depth at the last pause (for Step Out).
    depth: usize,
}

async fn rows(s: &mut Session, sql: &str) -> DbResult<Vec<Vec<Option<String>>>> {
    let out = s.run_script(sql, &ScriptOptions::default()).await;
    if let Some(e) = out.errors.into_iter().next() {
        return Err(e.error);
    }
    match out.results.into_iter().next().map(|r| r.outcome) {
        Some(Outcome::Rows(q)) => Ok(q.rows),
        _ => Ok(Vec::new()),
    }
}

fn cell(r: &[Option<String>], i: usize) -> String {
    r.get(i).cloned().flatten().unwrap_or_default()
}

fn int(r: &[Option<String>], i: usize) -> i64 {
    cell(r, i).trim().parse().unwrap_or(-1)
}

async fn scalar(s: &mut Session, sql: &str) -> DbResult<String> {
    rows(s, sql).await?.first().map(|r| cell(r, 0)).ok_or_else(|| DbError::Invalid(format!("no result from {sql}")))
}

/// Dollar-quoted literal for user-supplied text: immune to `standard_conforming_strings`, the tag is
/// extended until it does not occur in the value.
fn literal(v: &str) -> String {
    let mut tag = String::from("sv");
    while v.contains(&format!("${tag}$")) {
        tag.push('x');
    }
    format!("${tag}${v}${tag}$")
}

/// The call that runs `t` with `args` (`None` = SQL NULL), each value cast to its declared type.
pub fn call_sql(t: &DebugTarget, args: &[Option<String>]) -> DbResult<String> {
    if args.len() != t.params.len() {
        return Err(DbError::Invalid(format!(
            "{} expects {} argument(s), got {}",
            t.signature,
            t.params.len(),
            args.len()
        )));
    }
    let list: Vec<String> = t
        .params
        .iter()
        .zip(args)
        .map(|(p, a)| match a {
            None => format!("NULL::{}", p.type_name),
            Some(v) => format!("{}::{}", literal(v), p.type_name),
        })
        .collect();
    Ok(if t.is_procedure {
        format!("CALL {}({})", t.call_name, list.join(", "))
    } else {
        format!("SELECT * FROM {}({})", t.call_name, list.join(", "))
    })
}

/// Fails with a setup hint when the server cannot debug (extension missing or not preloaded).
async fn require_debugger(s: &mut Session) -> DbResult<()> {
    let ext = scalar(s, "SELECT count(*) FROM pg_extension WHERE extname = 'pldbgapi'").await?;
    if ext.trim() != "1" {
        return Err(DbError::Invalid(
            "the PL/pgSQL debugger is not installed in this database: install pldebugger on the server, set \
             shared_preload_libraries = 'plugin_debugger', restart, then run CREATE EXTENSION pldbgapi"
                .into(),
        ));
    }
    Ok(())
}

/// Reads what the debugger needs about routine `oid` (a `pg_proc` OID, or an Oracle `OBJECT_ID`).
pub async fn describe(s: &mut Session, oid: u32) -> DbResult<DebugTarget> {
    match s.engine() {
        Engine::Postgres => describe_pg(s, oid).await,
        Engine::Oracle => crate::ora_debugger::describe(s, oid).await,
        e => Err(DbError::Invalid(format!("{} has no debugger", e.label()))),
    }
}

async fn describe_pg(s: &mut Session, oid: u32) -> DbResult<DebugTarget> {
    let r = rows(
        s,
        &format!(
            "SELECT n.nspname, p.proname, p.prokind::text, l.lanname, p.prosrc, \
                    pg_get_function_identity_arguments(p.oid), \
                    array_to_string(ARRAY(SELECT format_type(t, NULL) FROM unnest(p.proargtypes) t), ','), \
                    coalesce(array_to_string(p.proargnames, ',', ''), ''), \
                    coalesce(array_to_string(p.proargmodes, ','), '') \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace JOIN pg_language l ON l.oid = p.prolang \
             WHERE p.oid = {oid}"
        ),
    )
    .await?;
    let r = r.first().ok_or_else(|| DbError::Invalid(format!("routine {oid} no longer exists")))?;
    if cell(r, 3) != "plpgsql" {
        return Err(DbError::Invalid(format!(
            "only PL/pgSQL routines can be debugged ({} is {})",
            cell(r, 1),
            cell(r, 3)
        )));
    }
    let d = Dialect(Engine::Postgres);
    let call_name = format!("{}.{}", d.quote_ident(&cell(r, 0))?, d.quote_ident(&cell(r, 1))?);
    // proargtypes lists input parameters only; proargnames / proargmodes cover all of them (incl. OUT).
    let in_types: Vec<String> = cell(r, 6).split(',').filter(|t| !t.is_empty()).map(str::to_string).collect();
    let names: Vec<String> = cell(r, 7).split(',').map(str::to_string).collect();
    let modes: Vec<String> = cell(r, 8).split(',').filter(|m| !m.is_empty()).map(str::to_string).collect();
    let in_names: Vec<String> = if modes.is_empty() {
        names
    } else {
        names.into_iter().zip(&modes).filter(|(_, m)| matches!(m.as_str(), "i" | "b" | "v")).map(|(n, _)| n).collect()
    };
    let params = in_types
        .into_iter()
        .enumerate()
        .map(|(i, type_name)| DebugParam {
            name: in_names.get(i).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| format!("${}", i + 1)),
            type_name,
        })
        .collect();
    Ok(DebugTarget {
        oid,
        signature: format!("{}.{}({})", cell(r, 0), cell(r, 1), cell(r, 5)),
        call_name,
        is_procedure: cell(r, 2) == "p",
        params,
        source: cell(r, 4),
        ..Default::default()
    })
}

/// A running debug session of either engine.
pub enum Debugger {
    Pg(PgDebugger),
    Oracle(Box<crate::ora_debugger::OraDebugger>),
}

impl Debugger {
    /// Starts `target` with `args` and pauses at its first statement.
    pub async fn start(
        cfg: &ConnectionConfig,
        password: Option<&Secret>,
        database: Option<&str>,
        target: &DebugTarget,
        args: &[Option<String>],
        breakpoints: &[i32],
    ) -> DbResult<(Self, DebugEvent)> {
        match cfg.engine {
            Engine::Oracle => {
                let (d, ev) =
                    crate::ora_debugger::OraDebugger::start(cfg, password, database, target, args, breakpoints).await?;
                Ok((Debugger::Oracle(Box::new(d)), ev))
            }
            _ => {
                let (d, ev) = PgDebugger::start(cfg, password, database, target, args, breakpoints).await?;
                Ok((Debugger::Pg(d), ev))
            }
        }
    }

    pub fn target_cancel(&self) -> CancelHandle {
        match self {
            Debugger::Pg(d) => d.target_cancel(),
            Debugger::Oracle(d) => d.target_cancel(),
        }
    }

    pub fn is_running(&self) -> bool {
        match self {
            Debugger::Pg(d) => d.is_running(),
            Debugger::Oracle(d) => d.is_running(),
        }
    }

    pub async fn step(&mut self, step: Step) -> DbResult<DebugEvent> {
        match self {
            Debugger::Pg(d) => d.step(step).await,
            Debugger::Oracle(d) => d.step(step).await,
        }
    }

    pub async fn set_breakpoint(&mut self, func: u32, line: i32, on: bool) -> DbResult<()> {
        match self {
            Debugger::Pg(d) => d.set_breakpoint(func, line, on).await,
            Debugger::Oracle(d) => d.set_breakpoint(func, line, on).await,
        }
    }

    pub async fn set_value(&mut self, name: &str, line: i32, value: &str) -> DbResult<Snapshot> {
        match self {
            Debugger::Pg(d) => d.set_value(name, line, value).await,
            Debugger::Oracle(d) => d.set_value(name, line, value).await,
        }
    }

    pub async fn stop(self) -> DbResult<()> {
        match self {
            Debugger::Pg(d) => d.stop().await,
            Debugger::Oracle(d) => d.stop().await,
        }
    }
}

impl PgDebugger {
    /// Starts `target` with `args` and pauses at its first statement; `breakpoints` (line numbers of
    /// the routine body) are set before it continues.
    pub async fn start(
        cfg: &ConnectionConfig,
        password: Option<&Secret>,
        database: Option<&str>,
        target: &DebugTarget,
        args: &[Option<String>],
        breakpoints: &[i32],
    ) -> DbResult<(Self, DebugEvent)> {
        if cfg.engine != Engine::Postgres {
            return Err(DbError::Invalid("the debugger supports PostgreSQL PL/pgSQL routines".into()));
        }
        let call = call_sql(target, args)?;
        let mut last = None;
        for _ in 0..START_ATTEMPTS {
            match Self::try_start(cfg, password, database, target, &call, breakpoints).await {
                Err(e) if interrupted(&e) => last = Some(e),
                r => return r,
            }
        }
        Err(last.unwrap_or_else(|| DbError::Invalid("the debugger could not attach to the routine".into())))
    }

    /// One attach attempt; on failure both sessions are closed (the routine is aborted).
    async fn try_start(
        cfg: &ConnectionConfig,
        password: Option<&Secret>,
        database: Option<&str>,
        target: &DebugTarget,
        call: &str,
        breakpoints: &[i32],
    ) -> DbResult<(Self, DebugEvent)> {
        let call = call.to_string();
        let mut ctl = Session::connect(cfg, password, database).await?;
        require_debugger(&mut ctl).await?;
        let mut tgt = Session::connect(cfg, password, database).await?;
        let mut notices =
            tgt.take_pg_notices().ok_or_else(|| DbError::Invalid("the target session has no notice stream".into()))?;
        let target_cancel = tgt.cancel_handle();
        // In-context debugging: a local breakpoint on the routine's entry for the target's own
        // backend. When it is hit, the target listens for a debugger and announces itself with a
        // `PLDBGBREAK:<backend>` notice; the control session then attaches to it. (The global
        // breakpoint mode, where the target connects to a listener, leaks pldebugger's 50 shared
        // connection slots and eventually stops working until the server restarts.)
        scalar(&mut tgt, &format!("SELECT pldbg_oid_debug({})", target.oid)).await?;
        let run: TargetRun = tokio::spawn(async move {
            let out = tgt.run_script(&call, &ScriptOptions::default()).await;
            (tgt, out)
        });
        let mut dbg = PgDebugger { ctl, sid: -1, target: Some(run), target_cancel, depth: 0 };
        match dbg.attach(&mut notices, target.oid, breakpoints).await {
            Ok(event) => Ok((dbg, event)),
            Err(e) => {
                let _ = dbg.stop().await;
                Err(e)
            }
        }
    }

    async fn attach(
        &mut self,
        notices: &mut tokio::sync::mpsc::Receiver<String>,
        oid: u32,
        breakpoints: &[i32],
    ) -> DbResult<DebugEvent> {
        enum Announced {
            Backend(String),
            Finished(Result<(Session, ScriptOutcome), tokio::task::JoinError>),
            Closed,
        }
        let announced = {
            let Some(run) = self.target.as_mut() else {
                return Err(DbError::Invalid("the routine has already finished; start it again".into()));
            };
            loop {
                tokio::select! {
                    n = notices.recv() => match n {
                        Some(text) => match text.strip_prefix("PLDBGBREAK:") {
                            Some(id) => break Announced::Backend(id.trim().to_string()),
                            None => continue,
                        },
                        None => break Announced::Closed,
                    },
                    done = &mut *run => break Announced::Finished(done),
                }
            }
        };
        let backend = match announced {
            Announced::Backend(id) => {
                id.parse::<i64>().map_err(|_| DbError::Invalid(format!("unexpected debugger notice {id}")))?
            }
            Announced::Finished(done) => {
                self.target = None;
                let (tgt, out) = done.map_err(|e| DbError::Invalid(format!("debug target task failed: {e}")))?;
                let _ = tgt.close().await;
                return Ok(DebugEvent::Finished(out));
            }
            Announced::Closed => return Err(DbError::ConnectionLost("the routine's session closed".into())),
        };
        self.sid = scalar(&mut self.ctl, &format!("SELECT pldbg_attach_to_port({backend})"))
            .await?
            .trim()
            .parse()
            .unwrap_or(-1);
        // Read the target's first breakpoint report before sending any other command: a command sent
        // earlier would read that report as its answer and desynchronise the session.
        if let Some(out) = self.wait(&format!("SELECT * FROM pldbg_wait_for_breakpoint({})", self.sid)).await? {
            return Ok(DebugEvent::Finished(out));
        }
        for line in breakpoints {
            self.set_breakpoint(oid, *line, true).await?;
        }
        Ok(DebugEvent::Paused(self.snapshot().await?))
    }

    /// Cancels the routine's statement from outside, e.g. while a step waits for a breakpoint that
    /// never comes; the waiting call then reports `Finished`.
    pub fn target_cancel(&self) -> CancelHandle {
        self.target_cancel.clone()
    }

    pub fn is_running(&self) -> bool {
        self.target.is_some()
    }

    pub async fn step(&mut self, step: Step) -> DbResult<DebugEvent> {
        if step != Step::Out {
            return self.step_once(step).await;
        }
        let depth = self.depth;
        if depth <= 1 {
            return self.step_once(Step::Continue).await;
        }
        let mut last = None;
        for _ in 0..STEP_OUT_LIMIT {
            match self.step_once(Step::Over).await? {
                DebugEvent::Paused(s) if s.stack.len() >= depth => last = Some(DebugEvent::Paused(s)),
                done => return Ok(done),
            }
        }
        last.ok_or_else(|| DbError::Invalid("step out made no progress".into()))
    }

    async fn step_once(&mut self, step: Step) -> DbResult<DebugEvent> {
        let sql = format!("SELECT * FROM {}({})", step.function(), self.sid);
        self.drive(&sql).await.map_err(|e| {
            if interrupted(&e) {
                DbError::Invalid(
                    "the server interrupted the debugging session (pldebugger lost the target); stop and start again"
                        .into(),
                )
            } else {
                e
            }
        })
    }

    /// Runs a control call, then snapshots the pause it ends in (or reports the finished run).
    async fn drive(&mut self, sql: &str) -> DbResult<DebugEvent> {
        Ok(match self.wait(sql).await? {
            Some(out) => DebugEvent::Finished(out),
            None => DebugEvent::Paused(self.snapshot().await?),
        })
    }

    /// Runs a control call that returns when the target pauses (`None`); if the target finishes
    /// first, returns its outcome.
    async fn wait(&mut self, sql: &str) -> DbResult<Option<ScriptOutcome>> {
        let Some(run) = self.target.as_mut() else {
            return Err(DbError::Invalid("the routine has already finished; start it again".into()));
        };
        tokio::select! {
            r = rows(&mut self.ctl, sql) => r.map(|_| None),
            done = run => {
                self.target = None;
                let (tgt, out) = done.map_err(|e| DbError::Invalid(format!("debug target task failed: {e}")))?;
                let _ = tgt.close().await;
                Ok(Some(out))
            }
        }
    }

    async fn snapshot(&mut self) -> DbResult<Snapshot> {
        let sid = self.sid;
        let stack: Vec<Frame> = rows(
            &mut self.ctl,
            &format!("SELECT level, targetname, func, linenumber, args FROM pldbg_get_stack({sid})"),
        )
        .await?
        .iter()
        .map(|r| Frame {
            level: int(r, 0) as i32,
            name: cell(r, 1),
            func: int(r, 2) as u32,
            line: int(r, 3) as i32,
            args: cell(r, 4),
        })
        .collect();
        let top = stack.first().cloned().ok_or_else(|| DbError::Invalid("the debugger returned no stack".into()))?;
        let variables = rows(
            &mut self.ctl,
            &format!(
                "SELECT name, varclass, linenumber, format_type(dtype, NULL), value FROM pldbg_get_variables({sid})"
            ),
        )
        .await?
        .iter()
        .map(|r| Variable {
            name: cell(r, 0),
            class: cell(r, 1).chars().next().unwrap_or('L'),
            line: int(r, 2) as i32,
            type_name: cell(r, 3),
            value: r.get(4).cloned().flatten(),
        })
        .collect();
        let source = scalar(&mut self.ctl, &format!("SELECT pldbg_get_source({sid}, {})", top.func)).await?;
        self.depth = stack.len();
        Ok(Snapshot { func: top.func, line: top.line, stack, variables, source })
    }

    pub async fn set_breakpoint(&mut self, func: u32, line: i32, on: bool) -> DbResult<()> {
        let f = if on { "pldbg_set_breakpoint" } else { "pldbg_drop_breakpoint" };
        scalar(&mut self.ctl, &format!("SELECT {f}({}, {func}, {line})", self.sid)).await.map(|_| ())
    }

    /// Assigns `value` to variable `name` (declared at `line`) in the current frame.
    pub async fn set_value(&mut self, name: &str, line: i32, value: &str) -> DbResult<Snapshot> {
        let ok = scalar(
            &mut self.ctl,
            &format!("SELECT pldbg_deposit_value({}, {}, {line}, {})", self.sid, literal(name), literal(value)),
        )
        .await?;
        if ok != "t" {
            return Err(DbError::Invalid(format!("the server rejected the value for {name}")));
        }
        self.snapshot().await
    }

    /// Aborts the routine (its transaction rolls back) and closes both sessions.
    pub async fn stop(mut self) -> DbResult<()> {
        if let Some(run) = self.target.take() {
            let _ = rows(&mut self.ctl, &format!("SELECT * FROM pldbg_abort_target({})", self.sid)).await;
            if let Ok(Ok((tgt, _))) = tokio::time::timeout(std::time::Duration::from_secs(5), run).await {
                let _ = tgt.close().await;
            }
        }
        self.ctl.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(is_procedure: bool) -> DebugTarget {
        DebugTarget {
            oid: 1,
            signature: "public.f(integer, text)".into(),
            call_name: "\"public\".\"f\"".into(),
            is_procedure,
            params: vec![
                DebugParam { name: "n".into(), type_name: "integer".into() },
                DebugParam { name: "label".into(), type_name: "text".into() },
            ],
            source: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn call_casts_quotes_and_checks_arity() {
        let sql = call_sql(&target(false), &[Some("3".into()), Some("it's \\".into())]).unwrap();
        assert_eq!(sql, "SELECT * FROM \"public\".\"f\"($sv$3$sv$::integer, $sv$it's \\$sv$::text)");
        let sql = call_sql(&target(true), &[None, Some("a$sv$b".into())]).unwrap();
        assert_eq!(sql, "CALL \"public\".\"f\"(NULL::integer, $svx$a$sv$b$svx$::text)");
        assert!(call_sql(&target(false), &[None]).unwrap_err().to_string().contains("expects 2"));
    }
}
