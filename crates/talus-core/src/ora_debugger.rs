//! PL/SQL debugger on Oracle's `DBMS_DEBUG` package (the interface behind Probe / the classic SQL
//! Developer debugger). Two sessions, like the PL/pgSQL debugger: the *target* initialises a debug
//! session id, turns debugging on and runs the routine, which then blocks at each stop; the *control*
//! session attaches to that id (`ATTACH_SESSION`), waits for the target (`SYNCHRONIZE`), sets
//! breakpoints and drives it (`CONTINUE` with step flags, `GET_VALUE`, `SET_VALUE`).
//!
//! What it needs from the server and the account:
//! * `DEBUG CONNECT SESSION` for the account (and `DEBUG ANY PROCEDURE` for routines of other owners);
//! * the routine compiled with debug information (`ALTER PROCEDURE p COMPILE DEBUG`) — the debugger never
//!   recompiles it for you;
//! * a stored procedure or function. Anonymous blocks, packages and triggers are not offered.
//!
//! `DBMS_DEBUG` cannot list a frame's variables, so the panes show the routine's parameters and the
//! names declared in its source (a token scan of the declaration part and `FOR` loop indexes); a name
//! that is not in scope at the stop is simply left out.

use std::collections::HashMap;

use crate::debugger::{DebugArg, DebugEvent, DebugParam, DebugTarget, Frame, Snapshot, Step, Variable};
use crate::engine::{CancelHandle, ScriptOptions, ScriptOutcome, Session};
use crate::error::{DbError, DbResult};
use crate::model::{ConnectionConfig, Engine, FetchLimits, Outcome};
use crate::secret::Secret;
use crate::sql::Dialect;
use crate::sql::lexer::{TokenKind, tokenize};

const D: Dialect = Dialect(Engine::Oracle);

// DBMS_DEBUG constants (see the package specification).
const BREAK_ANY_CALL: i32 = 12;
const BREAK_NEXT_LINE: i32 = 32;
const BREAK_ANY_RETURN: i32 = 512;
const ABORT_EXECUTION: i32 = 8192;
/// `info_getStackDepth + info_getLineinfo`.
const INFO: i32 = 2 + 8;
const REASON_EXIT: i32 = 15;
const REASON_KNL_EXIT: i32 = 25;
const REASON_ABORT: i32 = 21;
/// Variables read per round trip.
const VARS_PER_CALL: usize = 10;
/// Stops skipped in a row while the run is in the calling block (no source to show).
const MAX_BLOCK_STOPS: usize = 4;

type TargetRun = tokio::task::JoinHandle<(Session, ScriptOutcome)>;

#[derive(Debug, Clone)]
struct Unit {
    owner: String,
    name: String,
    /// `PROCEDURE`, `FUNCTION`, `PACKAGE BODY`, `TRIGGER`, …
    kind: String,
    source: String,
    /// Names whose values the panes try to show, with their declared types.
    names: Vec<(String, String)>,
}

impl Unit {
    /// `DBMS_DEBUG.namespace_*` of the unit type.
    fn namespace(&self) -> i32 {
        match self.kind.as_str() {
            "PACKAGE BODY" | "TYPE BODY" => 2,
            "TRIGGER" => 3,
            _ => 1,
        }
    }
}

pub struct OraDebugger {
    ctl: Session,
    target: Option<TargetRun>,
    target_cancel: CancelHandle,
    units: HashMap<u32, Unit>,
    /// `(owner, name)` → unit id, for stops reported by name.
    by_name: HashMap<(String, String), u32>,
    /// Breakpoint numbers by `(unit, line)`.
    breakpoints: HashMap<(u32, i32), i32>,
    /// Frames from the calling block (not listed) to the stop: `(unit, line)`.
    frames: Vec<(u32, i32)>,
    next_unit_id: u32,
    /// `(owner, name, namespace)` of the last stop; `None` in the calling block.
    last_program: Option<(String, String, i32)>,
}

fn cell(r: &[Option<String>], i: usize) -> String {
    r.get(i).cloned().flatten().unwrap_or_default()
}

async fn rows(s: &mut Session, sql: &str) -> DbResult<Vec<Vec<Option<String>>>> {
    let out =
        s.run_script(sql, &ScriptOptions { limits: FetchLimits { max_rows: 200_000 }, ..Default::default() }).await;
    if let Some(e) = out.errors.into_iter().next() {
        return Err(e.error);
    }
    match out.results.into_iter().next().map(|r| r.outcome) {
        Some(Outcome::Rows(q)) => Ok(q.rows),
        _ => Ok(Vec::new()),
    }
}

/// PL/SQL string literal.
fn lit(v: &str) -> String {
    D.string_literal(v)
}

/// Declaration of a variable that can hold a parameter of `data_type` (an `ALL_ARGUMENTS` spelling).
fn declared_type(data_type: &str) -> Option<String> {
    let t = data_type.to_ascii_uppercase();
    Some(match t.as_str() {
        "VARCHAR2" | "VARCHAR" | "CHAR" | "STRING" => "VARCHAR2(32767)".into(),
        "NVARCHAR2" | "NCHAR" => "NVARCHAR2(16383)".into(),
        "NUMBER" | "FLOAT" | "INTEGER" | "DECIMAL" | "NUMERIC" => "NUMBER".into(),
        "BINARY_INTEGER" | "PLS_INTEGER" | "NATURAL" | "POSITIVE" => "PLS_INTEGER".into(),
        "BINARY_FLOAT" | "BINARY_DOUBLE" | "DATE" | "CLOB" | "NCLOB" => t,
        "RAW" => "RAW(32767)".into(),
        "PL/SQL BOOLEAN" | "BOOLEAN" => "BOOLEAN".into(),
        _ if t.starts_with("TIMESTAMP") || t.starts_with("INTERVAL") => t,
        _ => return None,
    })
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '#' | '.'))
}

/// Lists the routine as the debug tab needs it.
pub async fn describe(s: &mut Session, object_id: u32) -> DbResult<DebugTarget> {
    let r = rows(
        s,
        &format!(
            "SELECT owner, object_name, object_type FROM all_objects \
             WHERE object_id = {object_id} AND object_type IN ('PROCEDURE', 'FUNCTION')"
        ),
    )
    .await?;
    let r = r.first().ok_or_else(|| {
        DbError::Invalid(
            "only stored procedures and functions can be debugged (packages, triggers and anonymous blocks cannot)"
                .into(),
        )
    })?;
    let (owner, name, kind) = (cell(r, 0), cell(r, 1), cell(r, 2));
    let args = rows(
        s,
        &format!(
            "SELECT argument_name, in_out, data_type, position FROM all_arguments \
             WHERE owner = {} AND object_name = {} AND package_name IS NULL AND data_level = 0 ORDER BY position, sequence",
            lit(&owner),
            lit(&name)
        ),
    )
    .await?;
    let mut all = Vec::new();
    let mut returns = None;
    for a in &args {
        let (arg, mode, ty) = (cell(a, 0), cell(a, 1), cell(a, 2));
        if arg.is_empty() {
            // Position 0 without a name is a function's return value.
            returns = Some(ty);
            continue;
        }
        if ty.is_empty() || declared_type(&ty).is_none() {
            return Err(DbError::Invalid(format!(
                "parameter {arg} has the type {} which the debugger cannot supply",
                if ty.is_empty() { "?" } else { &ty }
            )));
        }
        let mode = match mode.as_str() {
            "OUT" => 'O',
            "IN/OUT" => 'B',
            _ => 'I',
        };
        all.push(DebugArg { name: arg, type_name: ty, mode });
    }
    if let Some(t) = &returns
        && declared_type(t).is_none()
    {
        return Err(DbError::Invalid(format!("the return type {t} is not supported by the debugger")));
    }
    let source = unit_source(s, &owner, &name, &kind).await?;
    let params = all
        .iter()
        .filter(|a| a.mode != 'O')
        .map(|a| DebugParam { name: a.name.clone(), type_name: a.type_name.clone() })
        .collect();
    let list = all.iter().map(|a| format!("{} {}", a.name, a.type_name)).collect::<Vec<_>>().join(", ");
    Ok(DebugTarget {
        oid: object_id,
        signature: format!("{owner}.{name}({list})"),
        call_name: format!("{}.{}", D.quote_ident(&owner)?, D.quote_ident(&name)?),
        is_procedure: kind == "PROCEDURE",
        params,
        source,
        args: all,
        returns,
    })
}

async fn unit_source(s: &mut Session, owner: &str, name: &str, kind: &str) -> DbResult<String> {
    let lines = rows(
        s,
        &format!(
            "SELECT text FROM all_source WHERE owner = {} AND name = {} AND type = {} ORDER BY line",
            lit(owner),
            lit(name),
            lit(kind)
        ),
    )
    .await?;
    Ok(lines.iter().map(|r| cell(r, 0)).collect())
}

/// The anonymous block that runs the routine with `args` and prints what it returns through
/// `DBMS_OUTPUT` (the target outcome's second result set).
pub fn call_block(t: &DebugTarget, values: &[Option<String>]) -> DbResult<String> {
    if values.len() != t.params.len() {
        return Err(DbError::Invalid(format!(
            "{} expects {} argument(s), got {}",
            t.signature,
            t.params.len(),
            values.len()
        )));
    }
    let mut decls = String::new();
    let mut sets = String::new();
    let mut prints = String::new();
    let mut call_args = Vec::new();
    let mut given = values.iter();
    for (i, a) in t.args.iter().enumerate() {
        let var = format!("l_a{i}");
        let ty = declared_type(&a.type_name)
            .ok_or_else(|| DbError::Invalid(format!("parameter {} has an unsupported type", a.name)))?;
        decls.push_str(&format!("  {var} {ty};\n"));
        if a.mode != 'O' {
            let v = given.next().cloned().flatten();
            sets.push_str(&format!("  {var} := {};\n", assignment_value(&a.type_name, v.as_deref())));
        }
        if a.mode != 'I' {
            prints.push_str(&format!("  DBMS_OUTPUT.PUT_LINE({});\n", print_expr(&a.name, &a.type_name, &var)));
        }
        call_args.push(var);
    }
    let call = format!("{}({})", t.call_name, call_args.join(", "));
    let invoke = match &t.returns {
        Some(ret) => {
            let ty = declared_type(ret).ok_or_else(|| DbError::Invalid("unsupported return type".into()))?;
            decls.push_str(&format!("  l_ret {ty};\n"));
            prints.push_str(&format!("  DBMS_OUTPUT.PUT_LINE({});\n", print_expr("result", ret, "l_ret")));
            format!("  l_ret := {call};\n")
        }
        None => format!("  {call};\n"),
    };
    // Debugging was switched on by the call before; it is switched off again before anything else runs: any
    // PL/SQL the session executes while it is on (for instance reading DBMS_OUTPUT afterwards) would wait for a
    // debugger forever.
    Ok(format!(
        "DECLARE\n{decls}BEGIN\n{sets}{invoke}  DBMS_DEBUG.DEBUG_OFF;\n{prints}\
         EXCEPTION WHEN OTHERS THEN\n  DBMS_DEBUG.DEBUG_OFF;\n  RAISE;\nEND;"
    ))
}

fn assignment_value(type_name: &str, v: Option<&str>) -> String {
    let Some(v) = v else { return "NULL".into() };
    match type_name.to_ascii_uppercase().as_str() {
        "PL/SQL BOOLEAN" | "BOOLEAN" => match v.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "t" | "y" | "yes" => "TRUE".into(),
            "false" | "0" | "f" | "n" | "no" => "FALSE".into(),
            _ => "NULL".into(),
        },
        "RAW" => format!("HEXTORAW({})", lit(v.trim().trim_start_matches("0x"))),
        _ => lit(v),
    }
}

fn print_expr(name: &str, type_name: &str, var: &str) -> String {
    let shown = match type_name.to_ascii_uppercase().as_str() {
        "PL/SQL BOOLEAN" | "BOOLEAN" => {
            format!("CASE WHEN {var} THEN 'TRUE' WHEN NOT {var} THEN 'FALSE' END")
        }
        "CLOB" | "NCLOB" => format!("SUBSTR({var}, 1, 2000)"),
        "RAW" => format!("RAWTOHEX({var})"),
        _ => format!("TO_CHAR({var})"),
    };
    format!("{} || {shown}", lit(&format!("{name} = ")))
}

/// Names declared in a unit's source: parameters come from the catalog, the rest from a token scan of
/// the declaration part (up to the first `BEGIN`) and of `FOR <index> IN` loops.
fn declared_names(source: &str) -> Vec<(String, String)> {
    let tokens: Vec<_> = tokenize(source, Engine::Oracle).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let word = |i: usize| -> Option<String> {
        tokens.get(i).filter(|t| t.kind == TokenKind::Word).map(|t| t.text(source).to_ascii_uppercase())
    };
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen_begin = false;
    let mut in_decl = false;
    let mut at_start = false;
    for i in 0..tokens.len() {
        let w = word(i);
        if !seen_begin && !in_decl && matches!(w.as_deref(), Some("IS" | "AS")) {
            in_decl = true;
            at_start = true;
            continue;
        }
        match w.as_deref() {
            Some("DECLARE") => {
                in_decl = true;
                at_start = true;
                continue;
            }
            Some("BEGIN") => {
                seen_begin = true;
                in_decl = false;
                continue;
            }
            Some("FOR") if word(i + 2).as_deref() == Some("IN") => {
                if let Some(n) = word(i + 1) {
                    out.push((n, "loop index".into()));
                }
            }
            _ => {}
        }
        if in_decl {
            if tokens[i].text(source) == ";" {
                at_start = true;
                continue;
            }
            if at_start {
                at_start = false;
                if let Some(n) = &w
                    && !matches!(
                        n.as_str(),
                        "CURSOR" | "TYPE" | "SUBTYPE" | "PROCEDURE" | "FUNCTION" | "PRAGMA" | "END"
                    )
                {
                    // The declared type: everything up to `;`, `:=` or DEFAULT.
                    let mut ty = Vec::new();
                    let mut j = i + 1;
                    while j < tokens.len() {
                        let text = tokens[j].text(source);
                        if text == ";" || text == ":" || word(j).as_deref() == Some("DEFAULT") {
                            break;
                        }
                        if word(j).as_deref() != Some("CONSTANT") {
                            ty.push(text.to_string());
                        }
                        j += 1;
                    }
                    out.push((n.clone(), ty.join(" ")));
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|(n, _)| seen.insert(n.clone()));
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunInfo {
    ret: i32,
    line: i32,
    reason: i32,
    depth: i32,
    terminated: bool,
}

impl OraDebugger {
    /// Starts `target` with `args` and pauses at its first statement; `breakpoints` (line numbers of
    /// the routine's source) are set before it runs.
    pub async fn start(
        cfg: &ConnectionConfig,
        password: Option<&Secret>,
        database: Option<&str>,
        target: &DebugTarget,
        args: &[Option<String>],
        breakpoints: &[i32],
    ) -> DbResult<(Self, DebugEvent)> {
        if cfg.engine != Engine::Oracle {
            return Err(DbError::Invalid("this debugger supports Oracle PL/SQL".into()));
        }
        let block = call_block(target, args)?;
        let mut ctl = Session::connect(cfg, password, database).await?;
        let mut tgt = Session::connect(cfg, password, database).await?;
        let target_cancel = tgt.cancel_handle();
        async {
            Self::require_debug_info(&mut ctl, target).await?;
            let id = format!("TALUS_{}", uuid::Uuid::new_v4().simple());
            // Debugging goes on in a call of its own: the *next* top-level PL/SQL the session runs is the one
            // that waits for the debugger. (`plsql_out` runs just this block: the usual statement path would read
            // DBMS_OUTPUT afterwards, which is PL/SQL too and would wait for a debugger that is not attached yet.)
            if let Err(e) = tgt
                .plsql_out(
                    &format!(
                        "DECLARE s VARCHAR2(200); BEGIN s := DBMS_DEBUG.INITIALIZE({}); DBMS_DEBUG.DEBUG_ON(TRUE, FALSE); END;",
                        lit(&id)
                    ),
                    0,
                )
                .await
            {
                return Err(hint(e));
            }
            // The routine blocks inside the database at each stop: it runs on a task of its own.
            let run: TargetRun = tokio::spawn(async move {
                let out = tgt.run_script(&block, &ScriptOptions::default()).await;
                (tgt, out)
            });
            let mut dbg = OraDebugger {
                ctl,
                target: Some(run),
                target_cancel,
                units: HashMap::new(),
                by_name: HashMap::new(),
                breakpoints: HashMap::new(),
                frames: Vec::new(),
                next_unit_id: u32::MAX,
                last_program: None,
            };
            match dbg.attach(&id, target, breakpoints).await {
                Ok(ev) => Ok((dbg, ev)),
                Err(e) => {
                    let _ = dbg.stop().await;
                    Err(e)
                }
            }
        }
        .await
    }

    /// The routine must carry debug information: the debugger never changes the user's objects.
    async fn require_debug_info(ctl: &mut Session, t: &DebugTarget) -> DbResult<()> {
        let (owner, name) = t.signature.split('(').next().and_then(|s| s.split_once('.')).unwrap_or(("", ""));
        let r = rows(
            ctl,
            &format!(
                "SELECT plsql_debug FROM all_plsql_object_settings WHERE owner = {} AND name = {}",
                lit(owner),
                lit(name)
            ),
        )
        .await?;
        if r.first().map(|r| cell(r, 0)).as_deref() == Some("FALSE") {
            return Err(DbError::Invalid(format!(
                "{owner}.{name} was compiled without debug information: run ALTER {} {}.{} COMPILE DEBUG and start again",
                if t.is_procedure { "PROCEDURE" } else { "FUNCTION" },
                D.quote_ident(owner)?,
                D.quote_ident(name)?
            )));
        }
        Ok(())
    }

    async fn attach(&mut self, id: &str, target: &DebugTarget, breakpoints: &[i32]) -> DbResult<DebugEvent> {
        self.control_call(&format!("BEGIN DBMS_DEBUG.ATTACH_SESSION({}); END;", lit(id)), 0).await?;
        // Waits for the routine to reach its first PL/SQL (the calling block's interpreter starting).
        if let Some(out) = self.run_info("SYNCHRONIZE(ri, 2 + 8)").await?.1 {
            return Ok(DebugEvent::Finished(out));
        }
        // The unit under debug is known by its id.
        let (owner, name) = target.signature.split('(').next().and_then(|s| s.split_once('.')).unwrap_or(("", ""));
        let unit = Unit {
            owner: owner.to_string(),
            name: name.to_string(),
            kind: if target.is_procedure { "PROCEDURE" } else { "FUNCTION" }.into(),
            names: self.names_for(&target.source, target),
            source: target.source.clone(),
        };
        self.by_name.insert((unit.owner.clone(), unit.name.clone()), target.oid);
        self.units.insert(target.oid, unit);
        for line in breakpoints {
            self.set_breakpoint(target.oid, *line, true).await?;
        }
        // Break on the first entry: the routine's first line.
        self.drive(BREAK_ANY_CALL).await
    }

    fn names_for(&self, source: &str, t: &DebugTarget) -> Vec<(String, String)> {
        let mut names: Vec<(String, String)> =
            t.args.iter().map(|a| (a.name.to_ascii_uppercase(), a.type_name.clone())).collect();
        for (n, ty) in declared_names(source) {
            if !names.iter().any(|(x, _)| *x == n) {
                names.push((n, ty));
            }
        }
        names
    }

    /// A control call (no result needed beyond success).
    async fn control_call(&mut self, block: &str, outs: usize) -> DbResult<Vec<Option<String>>> {
        self.ctl.plsql_out(block, outs).await
    }

    /// Runs a `DBMS_DEBUG` function that fills `ri` (`SYNCHRONIZE`, `CONTINUE`): either the stop it ends in
    /// or — if the target finished first — the target's outcome.
    async fn run_info(&mut self, call: &str) -> DbResult<(RunInfo, Option<ScriptOutcome>)> {
        let block = format!(
            "DECLARE ri DBMS_DEBUG.runtime_info; r BINARY_INTEGER; BEGIN r := DBMS_DEBUG.{call}; \
             :1 := r; :2 := ri.line#; :3 := ri.reason; :4 := ri.stackdepth; :5 := ri.terminated; \
             :6 := ri.program.name; :7 := ri.program.owner; :8 := ri.program.namespace; END;"
        );
        let Some(run) = self.target.as_mut() else {
            return Err(DbError::Invalid("the routine has already finished; start it again".into()));
        };
        let got = tokio::select! {
            r = self.ctl.plsql_out(&block, 8) => Ok(r),
            done = run => Err(done),
        };
        match got {
            Ok(r) => {
                let v = r?;
                let int = |i: usize| v[i].as_deref().and_then(|x| x.trim().parse::<i32>().ok()).unwrap_or(0);
                let info =
                    RunInfo { ret: int(0), line: int(1), reason: int(2), depth: int(3), terminated: int(4) == 1 };
                self.last_program = None;
                if let (Some(name), Some(owner)) = (v[5].clone(), v[6].clone()) {
                    self.last_program = Some((owner, name, int(7)));
                }
                if info.ret != 0 {
                    return Err(DbError::Invalid(format!("DBMS_DEBUG returned error {}", info.ret)));
                }
                if info.terminated || matches!(info.reason, REASON_EXIT | REASON_KNL_EXIT | REASON_ABORT) {
                    return Ok((info, Some(self.finish().await?)));
                }
                Ok((info, None))
            }
            Err(done) => {
                self.target = None;
                let (tgt, out) = done.map_err(|e| DbError::Invalid(format!("debug target task failed: {e}")))?;
                let _ = tgt.close().await;
                Ok((RunInfo { ret: 0, line: 0, reason: REASON_EXIT, depth: 0, terminated: true }, Some(out)))
            }
        }
    }

    /// Waits for the target's run to end and returns its outcome.
    async fn finish(&mut self) -> DbResult<ScriptOutcome> {
        let Some(run) = self.target.take() else {
            return Ok(ScriptOutcome::default());
        };
        let (tgt, out) = tokio::time::timeout(std::time::Duration::from_secs(30), run)
            .await
            .map_err(|_| DbError::Invalid("the routine did not finish after the debugger released it".into()))?
            .map_err(|e| DbError::Invalid(format!("debug target task failed: {e}")))?;
        let _ = tgt.close().await;
        Ok(out)
    }

    /// Cancels the routine's session from outside, e.g. while a step waits for a breakpoint that never comes.
    pub fn target_cancel(&self) -> CancelHandle {
        self.target_cancel.clone()
    }

    pub fn is_running(&self) -> bool {
        self.target.is_some()
    }

    pub async fn step(&mut self, step: Step) -> DbResult<DebugEvent> {
        self.drive(match step {
            Step::Over => BREAK_NEXT_LINE,
            Step::Into => BREAK_ANY_CALL,
            Step::Out => BREAK_ANY_RETURN,
            Step::Continue => 0,
        })
        .await
    }

    /// `CONTINUE` with `flags`, then the stop it ends in.
    async fn drive(&mut self, flags: i32) -> DbResult<DebugEvent> {
        let mut flags = flags;
        for _ in 0..=MAX_BLOCK_STOPS {
            let (info, finished) = self.run_info(&format!("CONTINUE(ri, {flags}, {INFO})")).await?;
            if let Some(out) = finished {
                return Ok(DebugEvent::Finished(out));
            }
            match self.resolve_unit().await? {
                Some(unit) => return Ok(DebugEvent::Paused(self.snapshot(unit, info).await?)),
                // Stopped in the calling block (after the routine returned): nothing to show, run on.
                None => flags = 0,
            }
        }
        Err(DbError::Invalid("the debugger stopped outside the routine".into()))
    }

    /// The unit the last stop is in, loading its source on first sight.
    async fn resolve_unit(&mut self) -> DbResult<Option<u32>> {
        let Some((owner, name, ns)) = self.last_program.clone() else { return Ok(None) };
        if let Some(id) = self.by_name.get(&(owner.clone(), name.clone())) {
            return Ok(Some(*id));
        }
        let kinds = match ns {
            2 => "'PACKAGE BODY', 'TYPE BODY'",
            3 => "'TRIGGER'",
            _ => "'PROCEDURE', 'FUNCTION', 'PACKAGE', 'TYPE'",
        };
        let r = rows(
            &mut self.ctl,
            &format!(
                "SELECT object_id, object_type FROM all_objects WHERE owner = {} AND object_name = {} AND object_type IN ({kinds})",
                lit(&owner),
                lit(&name)
            ),
        )
        .await?;
        let Some(r) = r.first() else { return Ok(None) };
        let id: u32 = cell(r, 0).parse().unwrap_or_else(|_| {
            self.next_unit_id -= 1;
            self.next_unit_id
        });
        let kind = cell(r, 1);
        let source = unit_source(&mut self.ctl, &owner, &name, &kind).await?;
        let mut names: Vec<(String, String)> = Vec::new();
        if matches!(kind.as_str(), "PROCEDURE" | "FUNCTION") {
            let args = rows(
                &mut self.ctl,
                &format!(
                    "SELECT argument_name, data_type FROM all_arguments WHERE owner = {} AND object_name = {} \
                     AND package_name IS NULL AND data_level = 0 AND argument_name IS NOT NULL ORDER BY position",
                    lit(&owner),
                    lit(&name)
                ),
            )
            .await?;
            names.extend(args.iter().map(|a| (cell(a, 0), cell(a, 1))));
        }
        for (n, ty) in declared_names(&source) {
            if !names.iter().any(|(x, _)| *x == n) {
                names.push((n, ty));
            }
        }
        self.by_name.insert((owner.clone(), name.clone()), id);
        self.units.insert(id, Unit { owner, name, kind, source, names });
        Ok(Some(id))
    }

    async fn snapshot(&mut self, func: u32, info: RunInfo) -> DbResult<Snapshot> {
        // Frame 1 is the calling block; the routine under debug starts at depth 2.
        let level = usize::try_from(info.depth - 2).unwrap_or(0);
        self.frames.truncate(level);
        while self.frames.len() < level {
            self.frames.push((func, 0));
        }
        self.frames.push((func, info.line));
        let stack: Vec<Frame> = self
            .frames
            .iter()
            .rev()
            .enumerate()
            .map(|(i, (f, line))| Frame {
                level: i as i32,
                name: self.units.get(f).map(|u| format!("{}.{}", u.owner, u.name)).unwrap_or_default(),
                func: *f,
                line: *line,
                args: String::new(),
            })
            .collect();
        let variables = self.variables(func).await?;
        let source = self.units.get(&func).map(|u| u.source.clone()).unwrap_or_default();
        Ok(Snapshot { func, line: info.line, stack, variables, source })
    }

    /// Values of the unit's candidate names that are in scope at this stop.
    async fn variables(&mut self, func: u32) -> DbResult<Vec<Variable>> {
        let names = self.units.get(&func).map(|u| u.names.clone()).unwrap_or_default();
        let mut out = Vec::new();
        for chunk in names.chunks(VARS_PER_CALL) {
            let mut body = String::new();
            for (i, (n, _)) in chunk.iter().enumerate() {
                body.push_str(&format!(
                    "v := NULL; r := DBMS_DEBUG.GET_VALUE({}, 0, v, NULL); :{} := r; :{} := v; ",
                    lit(n),
                    2 * i + 1,
                    2 * i + 2
                ));
            }
            let block = format!("DECLARE v VARCHAR2(4000); r BINARY_INTEGER; BEGIN {body}END;");
            let got = self.ctl.plsql_out(&block, chunk.len() * 2).await?;
            for (i, (n, ty)) in chunk.iter().enumerate() {
                let code = got[2 * i].as_deref().and_then(|c| c.trim().parse::<i32>().ok()).unwrap_or(-1);
                // 0 success, 32 the value is NULL; anything else (3: not in scope here) is left out.
                if code == 0 || code == 32 {
                    out.push(Variable {
                        name: n.clone(),
                        class: 'L',
                        line: 0,
                        type_name: ty.clone(),
                        value: if code == 0 { got[2 * i + 1].clone() } else { None },
                    });
                }
            }
        }
        Ok(out)
    }

    pub async fn set_breakpoint(&mut self, func: u32, line: i32, on: bool) -> DbResult<()> {
        if !on {
            if let Some(bp) = self.breakpoints.remove(&(func, line)) {
                self.control_call(
                    &format!("DECLARE r BINARY_INTEGER; BEGIN r := DBMS_DEBUG.DELETE_BREAKPOINT({bp}); END;"),
                    0,
                )
                .await?;
            }
            return Ok(());
        }
        let u = self.units.get(&func).ok_or_else(|| DbError::Invalid("unknown routine for this breakpoint".into()))?;
        let block = format!(
            "DECLARE pi DBMS_DEBUG.program_info; b BINARY_INTEGER; r BINARY_INTEGER; BEGIN \
             pi.namespace := {}; pi.name := {}; pi.owner := {}; r := DBMS_DEBUG.SET_BREAKPOINT(pi, {line}, b); :1 := r; :2 := b; END;",
            u.namespace(),
            lit(&u.name),
            lit(&u.owner)
        );
        let got = self.control_call(&block, 2).await?;
        let code: i32 = got[0].as_deref().and_then(|c| c.trim().parse().ok()).unwrap_or(-1);
        if code != 0 {
            return Err(DbError::Invalid(match code {
                12 => format!("line {line} holds no executable statement"),
                other => format!("DBMS_DEBUG could not set the breakpoint (error {other})"),
            }));
        }
        if let Some(b) = got[1].as_deref().and_then(|b| b.trim().parse().ok()) {
            self.breakpoints.insert((func, line), b);
        }
        Ok(())
    }

    /// Assigns `value` to variable `name` in the current frame and returns the refreshed stop.
    pub async fn set_value(&mut self, name: &str, _line: i32, value: &str) -> DbResult<Snapshot> {
        if !is_identifier(name) {
            return Err(DbError::Invalid(format!("{name:?} is not a variable name")));
        }
        let v = value.trim();
        let rhs = if v.eq_ignore_ascii_case("null") {
            "NULL".to_string()
        } else if v.parse::<f64>().is_ok() {
            v.to_string()
        } else {
            lit(v)
        };
        let assignment = format!("{name} := {rhs};");
        let got = self
            .control_call(
                &format!(
                    "DECLARE r BINARY_INTEGER; BEGIN r := DBMS_DEBUG.SET_VALUE(0, {}); :1 := r; END;",
                    lit(&assignment)
                ),
                1,
            )
            .await?;
        if got[0].as_deref().map(str::trim) != Some("0") {
            return Err(DbError::Invalid(format!("the server rejected the value for {name}")));
        }
        let &(func, line) = self.frames.last().ok_or_else(|| DbError::Invalid("the debugger is not paused".into()))?;
        let depth = i32::try_from(self.frames.len()).unwrap_or(1) + 1;
        self.snapshot(func, RunInfo { ret: 0, line, reason: 0, depth, terminated: false }).await
    }

    /// Aborts the routine and closes both sessions.
    pub async fn stop(mut self) -> DbResult<()> {
        if self.target.is_some() {
            // `abort_execution` makes the target leave its routine; it is racing the waiting statement.
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                self.control_call(
                    &format!("DECLARE ri DBMS_DEBUG.runtime_info; r BINARY_INTEGER; BEGIN r := DBMS_DEBUG.CONTINUE(ri, {ABORT_EXECUTION}, 0); END;"),
                    0,
                ),
            )
            .await;
            if let Some(run) = self.target.take() {
                match tokio::time::timeout(std::time::Duration::from_secs(5), run).await {
                    Ok(Ok((tgt, _))) => {
                        let _ = tgt.close().await;
                    }
                    // Still blocked: end its session.
                    _ => {
                        let _ = self.target_cancel.cancel().await;
                    }
                }
            }
        }
        let _ = self.control_call("BEGIN DBMS_DEBUG.DETACH_SESSION; END;", 0).await;
        self.ctl.close().await
    }
}

/// Setup hints for the privilege errors people hit first.
fn hint(e: DbError) -> DbError {
    match &e {
        DbError::Server { code: Some(c), .. } if c == "ORA-01031" || c == "ORA-06550" => DbError::Invalid(format!(
            "{e}\nDebugging needs the DEBUG CONNECT SESSION privilege (and DEBUG ANY PROCEDURE for other owners' routines)"
        )),
        _ => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> DebugTarget {
        DebugTarget {
            oid: 1,
            signature: "HR.ADD_UP(N NUMBER, LABEL VARCHAR2, TOTAL NUMBER)".into(),
            call_name: "\"HR\".\"ADD_UP\"".into(),
            is_procedure: true,
            params: vec![
                DebugParam { name: "N".into(), type_name: "NUMBER".into() },
                DebugParam { name: "LABEL".into(), type_name: "VARCHAR2".into() },
            ],
            args: vec![
                DebugArg { name: "N".into(), type_name: "NUMBER".into(), mode: 'I' },
                DebugArg { name: "LABEL".into(), type_name: "VARCHAR2".into(), mode: 'I' },
                DebugArg { name: "TOTAL".into(), type_name: "NUMBER".into(), mode: 'O' },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn call_block_declares_assigns_calls_and_prints_outputs() {
        let b = call_block(&target(), &[Some("3".into()), Some("it's".into())]).unwrap();
        assert!(b.contains("l_a0 NUMBER;") && b.contains("l_a1 VARCHAR2(32767);") && b.contains("l_a2 NUMBER;"), "{b}");
        assert!(b.contains("l_a0 := '3';") && b.contains("l_a1 := 'it''s';"), "{b}");
        assert!(!b.contains("l_a2 :="), "OUT parameters are not assigned: {b}");
        assert!(b.contains("DBMS_DEBUG.DEBUG_OFF;") && !b.contains("DEBUG_ON"), "{b}");
        assert!(b.find("\"HR\".\"ADD_UP\"(").unwrap() < b.find("DEBUG_OFF").unwrap(), "{b}");
        assert!(b.contains("\"HR\".\"ADD_UP\"(l_a0, l_a1, l_a2);"), "{b}");
        assert!(b.contains("'TOTAL = ' || TO_CHAR(l_a2)"), "{b}");
        let b = call_block(&target(), &[None, None]).unwrap();
        assert!(b.contains("l_a0 := NULL;"), "{b}");
        assert!(call_block(&target(), &[None]).unwrap_err().to_string().contains("expects 2"));
    }

    #[test]
    fn function_results_are_printed() {
        let mut t = target();
        t.is_procedure = false;
        t.returns = Some("NUMBER".into());
        t.args.pop();
        let b = call_block(&t, &[Some("1".into()), None]).unwrap();
        assert!(
            b.contains("l_ret := \"HR\".\"ADD_UP\"(l_a0, l_a1);") && b.contains("'result = ' || TO_CHAR(l_ret)"),
            "{b}"
        );
    }

    #[test]
    fn declarations_and_loop_indexes_are_found() {
        let src = "PROCEDURE p(a IN NUMBER) AS\n  v NUMBER := 1;\n  CONSTANT_X CONSTANT VARCHAR2(10) := 'x';\n  CURSOR c IS SELECT 1 FROM dual;\n  s VARCHAR2(20);\nBEGIN\n  FOR i IN 1..a LOOP v := v * 2; END LOOP;\n  DECLARE inner_v DATE; BEGIN NULL; END;\nEND;";
        let names: Vec<String> = declared_names(src).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["V", "CONSTANT_X", "S", "I", "INNER_V"]);
        let types: Vec<String> = declared_names(src).into_iter().map(|(_, t)| t).collect();
        assert_eq!(types[0], "NUMBER");
        assert_eq!(types[1], "VARCHAR2 ( 10 )");
    }

    #[test]
    fn assignments_are_checked_names_only() {
        assert!(is_identifier("v_total") && is_identifier("rec.field") && is_identifier("x$1"));
        assert!(!is_identifier("1x") && !is_identifier("a; drop") && !is_identifier("a b") && !is_identifier(""));
        assert_eq!(assignment_value("PL/SQL BOOLEAN", Some("Yes")), "TRUE");
        assert_eq!(assignment_value("RAW", Some("0xAB")), "HEXTORAW('AB')");
        assert_eq!(assignment_value("NUMBER", None), "NULL");
    }
}
