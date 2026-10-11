//! Automation: saved jobs (run SQL, export a table, dump a database) with a schedule. This module
//! is pure data and time arithmetic; running a job is the app's business (in the UI loop or via
//! `talus --run-job` for launchd).

use chrono::{Datelike, Duration, NaiveDateTime, NaiveTime, Timelike, Weekday};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::export::ExportFormat;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobAction {
    /// Runs the script on `database` (default database when `None`).
    RunSql {
        sql: String,
        database: Option<String>,
    },
    ExportTable {
        database: String,
        schema: Option<String>,
        table: String,
        format: String,
        path: String,
    },
    DumpDatabase {
        database: String,
        schema: Option<String>,
        path: String,
        include_data: bool,
    },
}

impl JobAction {
    pub fn kind_label(&self) -> &'static str {
        match self {
            JobAction::RunSql { .. } => "Run SQL",
            JobAction::ExportTable { .. } => "Export table",
            JobAction::DumpDatabase { .. } => "Dump database",
        }
    }
}

/// `csv`, `tsv`, `json`, `sql`, `html`, `xml`, `xlsx` → format.
pub fn parse_format(name: &str) -> Option<ExportFormat> {
    ExportFormat::ALL.into_iter().find(|f| f.extension() == name.trim().to_ascii_lowercase())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Schedule {
    /// Only runs when started by hand.
    Manual,
    EveryMinutes(u32),
    Daily {
        hour: u8,
        minute: u8,
    },
    /// `weekday` 0 = Monday … 6 = Sunday.
    Weekly {
        weekday: u8,
        hour: u8,
        minute: u8,
    },
}

impl Schedule {
    pub fn label(&self) -> String {
        match self {
            Schedule::Manual => "manual".into(),
            Schedule::EveryMinutes(n) => format!("every {n} min"),
            Schedule::Daily { hour, minute } => format!("daily at {hour:02}:{minute:02}"),
            Schedule::Weekly { weekday, hour, minute } => {
                let d = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][usize::from(*weekday).min(6)];
                format!("{d} at {hour:02}:{minute:02}")
            }
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match *self {
            Schedule::EveryMinutes(0) => Err("the interval must be at least one minute".into()),
            Schedule::Daily { hour, minute } | Schedule::Weekly { hour, minute, .. } if hour > 23 || minute > 59 => {
                Err("the time of day is out of range".into())
            }
            Schedule::Weekly { weekday, .. } if weekday > 6 => Err("the weekday is out of range".into()),
            _ => Ok(()),
        }
    }

    /// First run time strictly after `after`; `None` for manual jobs.
    pub fn next_after(&self, after: NaiveDateTime) -> Option<NaiveDateTime> {
        match *self {
            Schedule::Manual => None,
            Schedule::EveryMinutes(n) => Some(after + Duration::minutes(i64::from(n.max(1)))),
            Schedule::Daily { hour, minute } => {
                let at = NaiveTime::from_hms_opt(hour.into(), minute.into(), 0)?;
                let today = after.date().and_time(at);
                Some(if today > after { today } else { today + Duration::days(1) })
            }
            Schedule::Weekly { weekday, hour, minute } => {
                let at = NaiveTime::from_hms_opt(hour.into(), minute.into(), 0)?;
                let target = Weekday::try_from(weekday).ok()?;
                let mut day = after.date();
                for _ in 0..8 {
                    let candidate = day.and_time(at);
                    if day.weekday() == target && candidate > after {
                        return Some(candidate);
                    }
                    day = day.succ_opt()?;
                }
                None
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRun {
    /// Local time the run started, `YYYY-MM-DD HH:MM:SS`.
    pub at: String,
    pub ok: bool,
    pub message: String,
    pub duration_ms: u64,
}

pub const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub name: String,
    pub conn: Uuid,
    pub action: JobAction,
    pub schedule: Schedule,
    pub enabled: bool,
    /// Local creation time; the first scheduled run is counted from here.
    pub created: String,
    pub last_run: Option<JobRun>,
}

impl Job {
    pub fn new(name: String, conn: Uuid, action: JobAction, schedule: Schedule, now: NaiveDateTime) -> Self {
        Self {
            id: Uuid::new_v4(),
            name,
            conn,
            action,
            schedule,
            enabled: true,
            created: now.format(TIME_FORMAT).to_string(),
            last_run: None,
        }
    }

    fn baseline(&self) -> Option<NaiveDateTime> {
        let s = self.last_run.as_ref().map_or(self.created.as_str(), |r| r.at.as_str());
        NaiveDateTime::parse_from_str(s, TIME_FORMAT).ok()
    }

    pub fn next_run(&self) -> Option<NaiveDateTime> {
        if !self.enabled {
            return None;
        }
        self.schedule.next_after(self.baseline()?)
    }

    /// Due now, including a run missed while the app was closed (it then runs once, not per miss).
    pub fn is_due(&self, now: NaiveDateTime) -> bool {
        self.next_run().is_some_and(|t| t <= now)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobsFile {
    #[serde(default)]
    pub jobs: Vec<Job>,
}

impl JobsFile {
    /// Job whose id or (case-insensitive) name equals `selector`.
    pub fn find(&self, selector: &str) -> Option<&Job> {
        let sel = selector.trim();
        self.jobs
            .iter()
            .find(|j| j.id.to_string() == sel)
            .or_else(|| self.jobs.iter().find(|j| j.name.eq_ignore_ascii_case(sel)))
    }
}

/// `~/` and `{date}` / `{datetime}` placeholders in an output path, so scheduled exports keep history.
pub fn expand_path(path: &str, now: NaiveDateTime) -> std::path::PathBuf {
    let p = path
        .replace("{datetime}", &now.format("%Y%m%d-%H%M%S").to_string())
        .replace("{date}", &now.format("%Y%m%d").to_string());
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => std::path::Path::new(&home).join(rest),
        _ => std::path::PathBuf::from(p),
    }
}

/// Runs one job action on an open SQL session; returns a one-line summary.
pub async fn execute_action(
    s: &mut crate::engine::Session,
    action: &JobAction,
    now: NaiveDateTime,
) -> crate::error::DbResult<String> {
    use crate::engine::ScriptOptions;
    use crate::error::DbError;
    use crate::model::{ObjectKind, ObjectRef, Outcome};
    match action {
        JobAction::RunSql { sql, .. } => {
            let out = s.run_script(sql, &ScriptOptions::default()).await;
            if let Some(e) = out.errors.first() {
                return Err(DbError::Invalid(format!("statement {} failed: {}", e.index + 1, e.error)));
            }
            let rows: usize = out
                .results
                .iter()
                .map(|r| match &r.outcome {
                    Outcome::Rows(q) => q.rows.len(),
                    Outcome::Affected(n) => *n as usize,
                })
                .sum();
            Ok(format!("{} statement result(s), {rows} row(s)", out.results.len()))
        }
        JobAction::ExportTable { database, schema, table, format, path } => {
            let f =
                parse_format(format).ok_or_else(|| DbError::Invalid(format!("unknown export format {format:?}")))?;
            let out = expand_path(path, now);
            let obj = ObjectRef {
                database: database.clone(),
                schema: schema.clone(),
                name: table.clone(),
                kind: ObjectKind::Table,
            };
            let rows = crate::export::export_table(s, &obj, f, &out).await?;
            Ok(format!("exported {rows} row(s) to {}", out.display()))
        }
        JobAction::DumpDatabase { database, schema, path, include_data } => {
            let out = expand_path(path, now);
            let r = crate::dump::dump_schema(s, database, schema.as_deref(), &out, *include_data).await?;
            Ok(format!("dumped {} table(s), {} view(s), {} row(s) to {}", r.tables, r.views, r.rows, out.display()))
        }
    }
}

/// launchd agent that starts `exe --run-job <id>` on the job's schedule (`None` for manual jobs).
pub fn launchd_plist(job: &Job, exe: &str) -> Option<String> {
    let schedule = match job.schedule {
        Schedule::Manual => return None,
        Schedule::EveryMinutes(n) => format!("  <key>StartInterval</key><integer>{}</integer>\n", u64::from(n) * 60),
        Schedule::Daily { hour, minute } => {
            format!(
                "  <key>StartCalendarInterval</key>\n  <dict><key>Hour</key><integer>{hour}</integer><key>Minute</key><integer>{minute}</integer></dict>\n"
            )
        }
        Schedule::Weekly { weekday, hour, minute } => format!(
            "  <key>StartCalendarInterval</key>\n  <dict><key>Weekday</key><integer>{}</integer><key>Hour</key><integer>{hour}</integer><key>Minute</key><integer>{minute}</integer></dict>\n",
            // launchd: 1 = Monday … 6 = Saturday, 0/7 = Sunday.
            u32::from(weekday) + 1
        ),
    };
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    Some(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{}</string>\n  <key>ProgramArguments</key>\n  <array><string>{}</string><string>--run-job</string><string>{}</string></array>\n{schedule}  <key>RunAtLoad</key><false/>\n</dict>\n</plist>\n",
        launchd_label(job),
        esc(exe),
        job.id
    ))
}

pub fn launchd_label(job: &Job) -> String {
    format!("io.github.fallowlone.talus.job.{}", job.id)
}

/// Whole minutes of `now.time()`, used by tests and the UI clock.
pub fn minutes_of_day(t: NaiveDateTime) -> u32 {
    t.hour() * 60 + t.minute()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, TIME_FORMAT).unwrap()
    }

    fn job(schedule: Schedule, created: &str) -> Job {
        Job::new(
            "j".into(),
            Uuid::nil(),
            JobAction::RunSql { sql: "select 1".into(), database: None },
            schedule,
            at(created),
        )
    }

    #[test]
    fn interval_daily_and_weekly_next_runs() {
        let now = at("2025-03-05 10:30:00"); // a Wednesday
        assert_eq!(Schedule::EveryMinutes(15).next_after(now), Some(at("2025-03-05 10:45:00")));
        assert_eq!(Schedule::Daily { hour: 11, minute: 0 }.next_after(now), Some(at("2025-03-05 11:00:00")));
        assert_eq!(
            Schedule::Daily { hour: 10, minute: 30 }.next_after(now),
            Some(at("2025-03-06 10:30:00")),
            "strictly after"
        );
        assert_eq!(
            Schedule::Weekly { weekday: 2, hour: 12, minute: 0 }.next_after(now),
            Some(at("2025-03-05 12:00:00"))
        );
        assert_eq!(
            Schedule::Weekly { weekday: 2, hour: 9, minute: 0 }.next_after(now),
            Some(at("2025-03-12 09:00:00"))
        );
        assert_eq!(
            Schedule::Weekly { weekday: 0, hour: 9, minute: 0 }.next_after(now),
            Some(at("2025-03-10 09:00:00"))
        );
        assert_eq!(Schedule::Manual.next_after(now), None);
    }

    #[test]
    fn jobs_become_due_and_missed_runs_collapse_into_one() {
        let mut j = job(Schedule::Daily { hour: 3, minute: 0 }, "2025-03-01 12:00:00");
        assert!(!j.is_due(at("2025-03-02 02:59:59")));
        assert!(j.is_due(at("2025-03-02 03:00:00")));
        // The app was closed for a week: it is due once; after that run the next one is tomorrow.
        assert!(j.is_due(at("2025-03-09 09:00:00")));
        j.last_run =
            Some(JobRun { at: "2025-03-09 09:00:00".into(), ok: true, message: String::new(), duration_ms: 1 });
        assert!(!j.is_due(at("2025-03-09 09:00:01")));
        assert_eq!(j.next_run(), Some(at("2025-03-10 03:00:00")));
        j.enabled = false;
        assert!(!j.is_due(at("2030-01-01 00:00:00")) && j.next_run().is_none());
        assert!(!job(Schedule::Manual, "2025-03-01 00:00:00").is_due(at("2030-01-01 00:00:00")));
    }

    #[test]
    fn validation_formats_and_selectors() {
        assert!(Schedule::EveryMinutes(0).validate().is_err());
        assert!(Schedule::Daily { hour: 24, minute: 0 }.validate().is_err());
        assert!(Schedule::Weekly { weekday: 7, hour: 1, minute: 1 }.validate().is_err());
        assert!(Schedule::Weekly { weekday: 6, hour: 23, minute: 59 }.validate().is_ok());
        assert_eq!(parse_format("XLSX"), Some(ExportFormat::Xlsx));
        assert_eq!(parse_format("nope"), None);
        let f = JobsFile { jobs: vec![job(Schedule::Manual, "2025-03-01 00:00:00")] };
        assert!(f.find("J").is_some() && f.find(&f.jobs[0].id.to_string()).is_some() && f.find("x").is_none());
        let back: JobsFile = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn output_paths_expand_placeholders_and_home() {
        let now = at("2025-03-05 10:30:07");
        assert_eq!(expand_path("/tmp/a_{date}.csv", now).to_str(), Some("/tmp/a_20250305.csv"));
        assert_eq!(expand_path("/tmp/{datetime}.sql", now).to_str(), Some("/tmp/20250305-103007.sql"));
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_path("~/x.csv", now).to_str(), Some(format!("{home}/x.csv").as_str()));
        assert_eq!(expand_path("rel/x.csv", now).to_str(), Some("rel/x.csv"));
    }

    #[test]
    fn launchd_plist_matches_the_schedule() {
        let j = job(Schedule::Weekly { weekday: 6, hour: 4, minute: 5 }, "2025-03-01 00:00:00");
        let p = launchd_plist(&j, "/Applications/Talus & Co/talus").unwrap();
        assert!(
            p.contains("<key>Weekday</key><integer>7</integer>") && p.contains("<key>Hour</key><integer>4</integer>")
        );
        assert!(p.contains("/Applications/Talus &amp; Co/talus") && p.contains(&j.id.to_string()));
        assert!(
            launchd_plist(&job(Schedule::EveryMinutes(10), "2025-03-01 00:00:00"), "/x")
                .unwrap()
                .contains("<integer>600</integer>")
        );
        assert!(launchd_plist(&job(Schedule::Manual, "2025-03-01 00:00:00"), "/x").is_none());
    }
}
