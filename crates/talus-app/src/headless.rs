//! `talus --run-job <id|name>`: runs one saved job without opening a window, so launchd can
//! schedule it while the app is closed. Passwords come from the Keychain (jobs through SSH
//! tunnels or against connections without a saved password cannot run unattended).

use std::sync::Arc;

use talus_core::engine::Session;
use talus_core::jobs::{Job, JobRun, JobsFile, TIME_FORMAT, execute_action};
use talus_core::model::ConnectionConfig;
use talus_core::store::config::{AppPaths, ConnectionsFile, load_json, save_json};
use talus_core::store::secrets::{SecretKind, SecretStore};
use talus_core::{DbError, Secret};

/// Result line appended to the jobs log and printed to stderr.
pub fn log_line(job: &Job, run: &JobRun) -> String {
    format!("{} [{}] {} — {}", run.at, if run.ok { "ok" } else { "FAILED" }, job.name, run.message)
}

/// Stores the outcome of a run in `jobs.json` and the log (re-reading the file first so edits
/// made in the app meanwhile are not lost).
pub fn record_run(paths: &AppPaths, id: uuid::Uuid, run: JobRun) {
    let mut file: JobsFile = load_json(&paths.jobs_file()).unwrap_or_default();
    let Some(job) = file.jobs.iter_mut().find(|j| j.id == id) else { return };
    job.last_run = Some(run.clone());
    let line = log_line(job, &run);
    if let Err(e) = save_json(&paths.jobs_file(), &file) {
        eprintln!("cannot save jobs.json: {e}");
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(paths.jobs_log_file()) {
        let _ = writeln!(f, "{line}");
    }
}

async fn run(secrets: &dyn SecretStore, job: &Job, cfg: &ConnectionConfig) -> Result<String, DbError> {
    if cfg.has_tunnel() {
        return Err(DbError::Invalid("jobs through an SSH or HTTP tunnel run only while Talus is open".into()));
    }
    if !cfg.engine.is_sql() {
        return Err(DbError::Invalid(format!("jobs are not available for {} connections", cfg.engine.label())));
    }
    let password: Option<Secret> = if cfg.needs_password() {
        match secrets.get(&cfg.id, SecretKind::DbPassword).map_err(DbError::Io)? {
            Some(p) => Some(p),
            None if cfg.save_password => Some(Secret::new("")),
            None => {
                return Err(DbError::Invalid(
                    "the connection's password is not saved in the Keychain; save it to run jobs unattended".into(),
                ));
            }
        }
    } else {
        None
    };
    let database = match &job.action {
        talus_core::jobs::JobAction::RunSql { database, .. } => database.as_deref(),
        talus_core::jobs::JobAction::ExportTable { database, .. }
        | talus_core::jobs::JobAction::DumpDatabase { database, .. } => Some(database.as_str()),
    };
    let mut s = Session::connect(cfg, password.as_ref(), database).await?;
    let now = chrono::Local::now().naive_local();
    let r = execute_action(&mut s, &job.action, now).await;
    let _ = s.close().await;
    r
}

/// Entry point for the command line; returns the process exit code.
pub fn run_job_cli(selector: &str) -> i32 {
    let paths = match AppPaths::system() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot open the application support directory: {e}");
            return 2;
        }
    };
    #[cfg(target_os = "macos")]
    let secrets: Arc<dyn SecretStore> = Arc::new(talus_core::store::secrets::KeychainStore);
    #[cfg(not(target_os = "macos"))]
    let secrets: Arc<dyn SecretStore> = Arc::new(talus_core::store::secrets::MemoryStore::default());
    run_job_with(&paths, secrets.as_ref(), selector)
}

/// Runs the job named or numbered `selector` against the stores under `paths`.
pub fn run_job_with(paths: &AppPaths, secrets: &dyn SecretStore, selector: &str) -> i32 {
    let jobs: JobsFile = match load_json(&paths.jobs_file()) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("jobs.json is unreadable: {e}");
            return 2;
        }
    };
    let Some(job) = jobs.find(selector).cloned() else {
        eprintln!("no job named or numbered {selector:?}");
        return 2;
    };
    let conns: ConnectionsFile = load_json(&paths.connections_file()).unwrap_or_default();
    let Some(cfg) = conns.connections.iter().find(|c| c.id == job.conn).cloned() else {
        eprintln!("the connection of job {:?} no longer exists", job.name);
        return 2;
    };
    let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("cannot start the async runtime: {e}");
            return 2;
        }
    };
    let started = std::time::Instant::now();
    let at = chrono::Local::now().naive_local().format(TIME_FORMAT).to_string();
    let result = rt.block_on(run(secrets, &job, &cfg));
    let (ok, message) = match result {
        Ok(m) => (true, m),
        Err(e) => (false, e.to_string()),
    };
    let run = JobRun { at, ok, message, duration_ms: started.elapsed().as_millis() as u64 };
    eprintln!("{}", log_line(&job, &run));
    record_run(paths, job.id, run);
    if ok { 0 } else { 1 }
}
