//! Headless job runs (what launchd starts): outcomes are recorded, errors give a non-zero exit.

mod common;

use common::*;
use talus_app::headless::run_job_with;
use talus_core::jobs::{Job, JobAction, JobsFile, Schedule};
use talus_core::model::{ConnectionConfig, Engine};
use talus_core::store::config::{AppPaths, ConnectionsFile, load_json, save_json};
use talus_core::store::secrets::MemoryStore;

fn setup(fx: &Fixture, jobs: Vec<(&str, JobAction)>) -> (AppPaths, Vec<uuid::Uuid>) {
    let paths = AppPaths::at(fx.dir.join("jobs-support")).unwrap();
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "Shop");
    cfg.database = fx.db.to_string_lossy().into_owned();
    save_json(&paths.connections_file(), &ConnectionsFile { connections: vec![cfg.clone()] }).unwrap();
    let now = chrono::Local::now().naive_local();
    let jobs: Vec<Job> = jobs.into_iter().map(|(n, a)| Job::new(n.into(), cfg.id, a, Schedule::Manual, now)).collect();
    let ids = jobs.iter().map(|j| j.id).collect();
    save_json(&paths.jobs_file(), &JobsFile { jobs }).unwrap();
    (paths, ids)
}

#[test]
fn successful_failed_and_unknown_jobs_are_recorded_with_exit_codes() {
    let fx = fixture();
    let out = fx.dir.join("customers_{date}.csv");
    let (paths, ids) = setup(
        &fx,
        vec![
            ("count", JobAction::RunSql { sql: "SELECT count(*) FROM customers".into(), database: None }),
            ("broken", JobAction::RunSql { sql: "SELECT * FROM missing_table".into(), database: None }),
            (
                "export",
                JobAction::ExportTable {
                    database: "main".into(),
                    schema: None,
                    table: "customers".into(),
                    format: "csv".into(),
                    path: out.to_string_lossy().into_owned(),
                },
            ),
        ],
    );
    let secrets = MemoryStore::default();
    assert_eq!(run_job_with(&paths, &secrets, "count"), 0);
    assert_eq!(run_job_with(&paths, &secrets, &ids[1].to_string()), 1, "selectable by id; SQL errors fail the run");
    assert_eq!(run_job_with(&paths, &secrets, "EXPORT"), 0, "names are case-insensitive");
    assert_eq!(run_job_with(&paths, &secrets, "nope"), 2);

    let file: JobsFile = load_json(&paths.jobs_file()).unwrap();
    let run = |i: usize| file.jobs[i].last_run.clone().unwrap();
    assert!(run(0).ok && run(0).message.contains("1 statement result(s), 1 row(s)"), "{:?}", run(0));
    assert!(!run(1).ok && run(1).message.contains("statement 1 failed"), "{:?}", run(1));
    assert!(run(2).ok && run(2).message.contains("exported 250 row(s)"), "{:?}", run(2));
    let date = chrono::Local::now().format("%Y%m%d").to_string();
    let csv = std::fs::read_to_string(fx.dir.join(format!("customers_{date}.csv"))).unwrap();
    assert_eq!(csv.lines().count(), 251);
    let log = std::fs::read_to_string(paths.jobs_log_file()).unwrap();
    assert_eq!(log.lines().count(), 3, "{log}");
    assert!(log.contains("[FAILED] broken") && log.contains("[ok] count"), "{log}");
}

#[test]
fn jobs_that_cannot_run_unattended_say_why() {
    let fx = fixture();
    let (paths, _) = setup(&fx, vec![("j", JobAction::RunSql { sql: "SELECT 1".into(), database: None })]);
    // Same job, but the connection now goes through SSH.
    let mut conns: ConnectionsFile = load_json(&paths.connections_file()).unwrap();
    conns.connections[0].ssh = Some(talus_core::model::SshConfig {
        host: "h".into(),
        port: 22,
        user: "u".into(),
        auth: talus_core::model::SshAuth::Password,
        save_secret: false,
        trust_new_host_key: false,
    });
    save_json(&paths.connections_file(), &conns).unwrap();
    assert_eq!(run_job_with(&paths, &MemoryStore::default(), "j"), 1);
    let file: JobsFile = load_json(&paths.jobs_file()).unwrap();
    assert!(
        file.jobs[0].last_run.as_ref().unwrap().message.contains("SSH or HTTP tunnel run only while Talus is open")
    );
}
