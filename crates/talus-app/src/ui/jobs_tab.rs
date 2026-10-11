//! Automation: saved jobs with schedules (list, editor, run now, launchd install).

use chrono::NaiveDateTime;
use egui::RichText;
use egui_phosphor::regular as icons;
use talus_core::jobs::{Job, JobAction, Schedule, TIME_FORMAT, parse_format};
use talus_core::model::{Engine, EngineKind};
use uuid::Uuid;

use crate::theme;

pub struct ConnChoice {
    pub id: Uuid,
    pub name: String,
}

#[derive(Default)]
pub struct JobsTab {
    form: Option<JobForm>,
    pub error: Option<String>,
}

pub enum JobsAction {
    Save(Box<Job>),
    Delete(Uuid),
    RunNow(Uuid),
    SetEnabled(Uuid, bool),
    InstallLaunchd(Uuid),
    RemoveLaunchd(Uuid),
}

#[derive(Clone)]
struct JobForm {
    id: Option<Uuid>,
    name: String,
    conn: Option<Uuid>,
    kind: usize,
    sql: String,
    database: String,
    schema: String,
    table: String,
    format: String,
    path: String,
    include_data: bool,
    schedule: usize,
    minutes: String,
    hour: String,
    minute: String,
    weekday: u8,
    enabled: bool,
    created: Option<String>,
    last_run: Option<talus_core::jobs::JobRun>,
}

const KINDS: [&str; 3] = ["Run SQL", "Export table", "Dump database"];
const SCHEDULES: [&str; 4] = ["Manual", "Every N minutes", "Daily", "Weekly"];
const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

impl JobForm {
    fn blank(conn: Option<Uuid>) -> Self {
        Self {
            id: None,
            name: String::new(),
            conn,
            kind: 0,
            sql: String::new(),
            database: String::new(),
            schema: String::new(),
            table: String::new(),
            format: "csv".into(),
            path: "~/Documents/export_{date}.csv".into(),
            include_data: true,
            schedule: 0,
            minutes: "60".into(),
            hour: "3".into(),
            minute: "0".into(),
            weekday: 0,
            enabled: true,
            created: None,
            last_run: None,
        }
    }

    fn from_job(j: &Job) -> Self {
        let mut f = Self::blank(Some(j.conn));
        f.id = Some(j.id);
        f.name = j.name.clone();
        f.enabled = j.enabled;
        f.created = Some(j.created.clone());
        f.last_run = j.last_run.clone();
        match &j.action {
            JobAction::RunSql { sql, database } => {
                f.kind = 0;
                f.sql = sql.clone();
                f.database = database.clone().unwrap_or_default();
            }
            JobAction::ExportTable { database, schema, table, format, path } => {
                f.kind = 1;
                f.database = database.clone();
                f.schema = schema.clone().unwrap_or_default();
                f.table = table.clone();
                f.format = format.clone();
                f.path = path.clone();
            }
            JobAction::DumpDatabase { database, schema, path, include_data } => {
                f.kind = 2;
                f.database = database.clone();
                f.schema = schema.clone().unwrap_or_default();
                f.path = path.clone();
                f.include_data = *include_data;
            }
        }
        match j.schedule {
            Schedule::Manual => f.schedule = 0,
            Schedule::EveryMinutes(n) => {
                f.schedule = 1;
                f.minutes = n.to_string();
            }
            Schedule::Daily { hour, minute } => {
                f.schedule = 2;
                f.hour = hour.to_string();
                f.minute = minute.to_string();
            }
            Schedule::Weekly { weekday, hour, minute } => {
                f.schedule = 3;
                f.weekday = weekday;
                f.hour = hour.to_string();
                f.minute = minute.to_string();
            }
        }
        f
    }

    fn to_job(&self, now: NaiveDateTime) -> Result<Job, String> {
        if self.name.trim().is_empty() {
            return Err("Give the job a name.".into());
        }
        let conn = self.conn.ok_or("Choose a connection.")?;
        let opt = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        let action = match self.kind {
            0 => {
                if self.sql.trim().is_empty() {
                    return Err("Enter the SQL to run.".into());
                }
                JobAction::RunSql { sql: self.sql.clone(), database: opt(&self.database) }
            }
            1 => {
                if self.database.trim().is_empty() || self.table.trim().is_empty() {
                    return Err("Enter the database and table to export.".into());
                }
                if parse_format(&self.format).is_none() {
                    return Err("Unknown export format (csv, tsv, json, sql, html, xml, xlsx).".into());
                }
                if self.path.trim().is_empty() {
                    return Err("Enter the output file path.".into());
                }
                JobAction::ExportTable {
                    database: self.database.trim().into(),
                    schema: opt(&self.schema),
                    table: self.table.trim().into(),
                    format: self.format.trim().to_lowercase(),
                    path: self.path.trim().into(),
                }
            }
            _ => {
                if self.database.trim().is_empty() || self.path.trim().is_empty() {
                    return Err("Enter the database and the output file path.".into());
                }
                JobAction::DumpDatabase {
                    database: self.database.trim().into(),
                    schema: opt(&self.schema),
                    path: self.path.trim().into(),
                    include_data: self.include_data,
                }
            }
        };
        let num = |s: &str, what: &str| s.trim().parse::<u32>().map_err(|_| format!("{what} must be a whole number."));
        let schedule = match self.schedule {
            0 => Schedule::Manual,
            1 => Schedule::EveryMinutes(num(&self.minutes, "The interval")?),
            2 => Schedule::Daily {
                hour: num(&self.hour, "The hour")? as u8,
                minute: num(&self.minute, "The minute")? as u8,
            },
            _ => Schedule::Weekly {
                weekday: self.weekday,
                hour: num(&self.hour, "The hour")? as u8,
                minute: num(&self.minute, "The minute")? as u8,
            },
        };
        schedule.validate()?;
        let mut job = Job::new(self.name.trim().into(), conn, action, schedule, now);
        if let Some(id) = self.id {
            job.id = id;
        }
        if let Some(c) = &self.created {
            job.created.clone_from(c);
        }
        job.enabled = self.enabled;
        job.last_run = self.last_run.clone();
        Ok(job)
    }
}

impl JobsTab {
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        jobs: &[Job],
        conns: &[ConnChoice],
        installed: &dyn Fn(&Job) -> bool,
        running: &dyn Fn(Uuid) -> bool,
        now: NaiveDateTime,
    ) -> Vec<JobsAction> {
        let p = theme::current(ui.ctx());
        let mut out = Vec::new();
        ui.horizontal(|ui| {
            if ui.button(format!("{} New Job", icons::PLUS)).clicked() {
                self.form = Some(JobForm::blank(conns.first().map(|c| c.id)));
            }
            ui.label(
                RichText::new(
                    "Jobs run while Talus is open; “Install” also schedules them with launchd while it is closed.",
                )
                .small()
                .color(p.text_dim),
            );
        });
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(p.danger));
        }
        if let Some(f) = self.form.as_mut() {
            let mut close = false;
            ui.group(|ui| {
                egui::Grid::new("job-form").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    let l = ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut f.name).desired_width(280.0)).labelled_by(l.id);
                    ui.end_row();
                    ui.label("Connection");
                    let cname =
                        conns.iter().find(|c| Some(c.id) == f.conn).map_or("— choose —".into(), |c| c.name.clone());
                    super::widgets::combo("job-conn").selected_text(cname).width(280.0).show_ui(ui, |ui| {
                        for c in conns {
                            ui.selectable_value(&mut f.conn, Some(c.id), &c.name);
                        }
                    });
                    ui.end_row();
                    ui.label("Action");
                    ui.horizontal(|ui| {
                        for (i, k) in KINDS.iter().enumerate() {
                            ui.selectable_value(&mut f.kind, i, *k);
                        }
                    });
                    ui.end_row();
                    if f.kind == 0 {
                        let l = ui.label("SQL");
                        ui.add(
                            egui::TextEdit::multiline(&mut f.sql)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(420.0)
                                .desired_rows(4),
                        )
                        .labelled_by(l.id);
                        ui.end_row();
                    }
                    let l = ui.label("Database");
                    ui.add(
                        egui::TextEdit::singleline(&mut f.database).desired_width(200.0).hint_text(if f.kind == 0 {
                            "(default)"
                        } else {
                            ""
                        }),
                    )
                    .labelled_by(l.id);
                    ui.end_row();
                    if f.kind != 0 {
                        let l = ui.label("Schema");
                        ui.add(
                            egui::TextEdit::singleline(&mut f.schema)
                                .desired_width(200.0)
                                .hint_text("(PostgreSQL / SQL Server)"),
                        )
                        .labelled_by(l.id);
                        ui.end_row();
                    }
                    if f.kind == 1 {
                        let l = ui.label("Table");
                        ui.add(egui::TextEdit::singleline(&mut f.table).desired_width(200.0)).labelled_by(l.id);
                        ui.end_row();
                        let l = ui.label("Format");
                        ui.add(egui::TextEdit::singleline(&mut f.format).desired_width(80.0).hint_text("csv"))
                            .labelled_by(l.id);
                        ui.end_row();
                    }
                    if f.kind != 0 {
                        let l = ui.label("Output file");
                        ui.add(
                            egui::TextEdit::singleline(&mut f.path)
                                .desired_width(420.0)
                                .hint_text("~/Documents/dump_{datetime}.sql"),
                        )
                        .labelled_by(l.id);
                        ui.end_row();
                    }
                    if f.kind == 2 {
                        ui.label("Data");
                        ui.checkbox(&mut f.include_data, "Include table data");
                        ui.end_row();
                    }
                    ui.label("Schedule");
                    ui.horizontal(|ui| {
                        super::widgets::combo("job-schedule")
                            .selected_text(SCHEDULES[f.schedule])
                            .width(160.0)
                            .show_ui(ui, |ui| {
                                for (i, s) in SCHEDULES.iter().enumerate() {
                                    ui.selectable_value(&mut f.schedule, i, *s);
                                }
                            });
                        match f.schedule {
                            1 => {
                                ui.add(egui::TextEdit::singleline(&mut f.minutes).desired_width(50.0));
                                ui.label("minutes");
                            }
                            2 | 3 => {
                                if f.schedule == 3 {
                                    super::widgets::combo("job-day")
                                        .selected_text(DAYS[usize::from(f.weekday).min(6)])
                                        .width(110.0)
                                        .show_ui(ui, |ui| {
                                            for (i, d) in DAYS.iter().enumerate() {
                                                ui.selectable_value(&mut f.weekday, i as u8, *d);
                                            }
                                        });
                                }
                                ui.add(egui::TextEdit::singleline(&mut f.hour).desired_width(36.0));
                                ui.label(":");
                                ui.add(egui::TextEdit::singleline(&mut f.minute).desired_width(36.0));
                            }
                            _ => {}
                        }
                    });
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        match f.to_job(now) {
                            Ok(job) => {
                                out.push(JobsAction::Save(Box::new(job)));
                                close = true;
                                self.error = None;
                            }
                            Err(e) => self.error = Some(e),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                        self.error = None;
                    }
                });
            });
            if close {
                self.form = None;
            }
        }
        ui.separator();
        if jobs.is_empty() {
            ui.label(
                RichText::new("No jobs yet. Create one to run SQL, export a table or dump a database on a schedule.")
                    .color(p.text_dim),
            );
            return out;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("jobs-grid").num_columns(7).striped(true).spacing([14.0, 8.0]).show(ui, |ui| {
                for h in ["", "Name", "Action", "Schedule", "Next run", "Last run", ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for j in jobs {
                    let mut enabled = j.enabled;
                    if ui.checkbox(&mut enabled, "").on_hover_text("Enabled").changed() {
                        out.push(JobsAction::SetEnabled(j.id, enabled));
                    }
                    ui.label(RichText::new(&j.name).strong());
                    let engine_note = conns
                        .iter()
                        .find(|c| c.id == j.conn)
                        .map_or("(connection missing)".to_string(), |c| c.name.clone());
                    ui.label(format!("{} — {}", j.action.kind_label(), engine_note));
                    ui.label(j.schedule.label());
                    ui.label(j.next_run().map_or("—".into(), |t| t.format(TIME_FORMAT).to_string()));
                    match &j.last_run {
                        Some(r) => {
                            let color = if r.ok { p.ok } else { p.danger };
                            ui.label(
                                RichText::new(format!(
                                    "{} {}",
                                    if r.ok { icons::CHECK_CIRCLE } else { icons::X_CIRCLE },
                                    r.at
                                ))
                                .color(color),
                            )
                            .on_hover_text(format!("{} ({} ms)", r.message, r.duration_ms));
                        }
                        None => {
                            ui.label(RichText::new("never").color(p.text_dim));
                        }
                    }
                    ui.horizontal(|ui| {
                        if running(j.id) {
                            ui.spinner();
                        } else if ui.small_button(icons::PLAY).on_hover_text("Run now").clicked() {
                            out.push(JobsAction::RunNow(j.id));
                        }
                        if ui.small_button(icons::PENCIL_SIMPLE).on_hover_text("Edit").clicked() {
                            self.form = Some(JobForm::from_job(j));
                        }
                        if j.schedule != Schedule::Manual {
                            if installed(j) {
                                if ui
                                    .small_button(icons::CLOCK_COUNTDOWN)
                                    .on_hover_text("Scheduled with launchd — click to remove")
                                    .clicked()
                                {
                                    out.push(JobsAction::RemoveLaunchd(j.id));
                                }
                            } else if ui
                                .small_button(icons::CLOCK)
                                .on_hover_text("Install a launchd agent so it runs while Talus is closed")
                                .clicked()
                            {
                                out.push(JobsAction::InstallLaunchd(j.id));
                            }
                        }
                        if ui.small_button(icons::TRASH).on_hover_text("Delete").clicked() {
                            out.push(JobsAction::Delete(j.id));
                        }
                    });
                    ui.end_row();
                }
            });
        });
        out
    }
}

/// Whether jobs can be created for this engine (SQL engines only).
pub fn supports_jobs(e: Engine) -> bool {
    e.kind() == EngineKind::Sql
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2025-03-05 10:00:00", TIME_FORMAT).unwrap()
    }

    #[test]
    fn forms_validate_and_round_trip() {
        let conn = Uuid::new_v4();
        let mut f = JobForm::blank(Some(conn));
        assert!(f.to_job(now()).unwrap_err().contains("name"));
        f.name = "nightly".into();
        assert!(f.to_job(now()).unwrap_err().contains("SQL"));
        f.sql = "VACUUM".into();
        f.schedule = 2;
        f.hour = "25".into();
        assert!(f.to_job(now()).unwrap_err().contains("out of range"));
        f.hour = "2".into();
        f.minute = "30".into();
        let job = f.to_job(now()).unwrap();
        assert_eq!(job.schedule, Schedule::Daily { hour: 2, minute: 30 });
        let again = JobForm::from_job(&job).to_job(now()).unwrap();
        assert_eq!((again.id, again.action, again.schedule), (job.id, job.action, job.schedule));
        f.kind = 1;
        f.database = "main".into();
        f.table = "t".into();
        f.format = "docx".into();
        assert!(f.to_job(now()).unwrap_err().contains("format"));
        assert!(supports_jobs(Engine::Postgres) && !supports_jobs(Engine::Redis));
    }
}
