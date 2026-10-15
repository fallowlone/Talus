//! Small modal windows: destructive-action confirmation, password prompt, settings, about.

use egui::RichText;
use talus_core::store::config::{Settings, Theme};
use talus_core::store::secrets::SecretKind;
use uuid::Uuid;

use crate::theme;

/// What to do once the user confirms. Carried by the confirmation dialog so nothing
/// destructive runs without an explicit click.
#[derive(Debug, Clone)]
pub enum Confirmed {
    DeleteConnection(Uuid),
    CloseTab(u64),
    DropDatabase {
        conn: Uuid,
        db: String,
        sql: String,
    },
    /// Cancel/terminate a server process from the monitor tab.
    Kill {
        tab: u64,
        sql: String,
    },
    /// Apply a structure-synchronization plan from this tab to its target.
    RunSync {
        tab: u64,
        statements: Vec<String>,
    },
    DeleteJob(Uuid),
    InstallLaunchd(Uuid),
    RemoveLaunchd(Uuid),
    /// Run a Redis / MongoDB operation the user confirmed.
    RunNoSql {
        tab: u64,
        op: talus_core::nosql::NoSqlOp,
        tag: String,
    },
    /// Run account SQL (may contain a password) from the users tab.
    RunUserSql {
        tab: u64,
        sql: String,
    },
    /// Drop a routine / trigger from the Routines tab, then reload its list.
    DropProgrammable {
        tab: u64,
        sql: String,
    },
    /// Run every statement of a SQL file on this database.
    RunSqlFile {
        conn: Uuid,
        database: String,
        path: std::path::PathBuf,
    },
    /// Apply the data-synchronization of these tables from this tab.
    RunDataSync {
        tab: u64,
        tables: Vec<String>,
    },
    /// Apply a table-designer plan from this tab.
    RunDesign {
        tab: u64,
        statements: Vec<String>,
    },
    /// Execute this SQL on the connection's metadata session, then refresh the object list.
    RunDestructive {
        conn: Uuid,
        database: String,
        schema: Option<String>,
        sql: String,
    },
}

pub struct Confirm {
    pub title: String,
    pub message: String,
    /// Exact SQL shown to the user, when the action runs SQL.
    pub sql: Option<String>,
    pub confirm_label: String,
    pub on_confirm: Confirmed,
}

pub enum ConfirmResult {
    Yes(Confirmed),
    No,
}

impl Confirm {
    pub fn ui(&self, ctx: &egui::Context) -> Option<ConfirmResult> {
        let p = theme::current(ctx);
        let mut out = None;
        egui::Modal::new(egui::Id::new("confirm-modal")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading(&self.title);
            ui.add_space(4.0);
            ui.label(&self.message);
            if let Some(sql) = &self.sql {
                ui.add_space(4.0);
                egui::Frame::group(ui.style()).fill(p.bg).show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(sql).monospace()).wrap());
                });
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new(&self.confirm_label).color(egui::Color32::WHITE))
                                .fill(p.danger)
                                .stroke(egui::Stroke::NONE),
                        )
                        .clicked()
                    {
                        out = Some(ConfirmResult::Yes(self.on_confirm.clone()));
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        out = Some(ConfirmResult::No);
                    }
                });
            });
        });
        out
    }
}

pub struct PasswordPrompt {
    pub conn: Uuid,
    pub conn_name: String,
    pub kind: SecretKind,
    pub password: String,
    pub save: bool,
    focus_requested: bool,
}

pub enum PromptResult {
    Ok { password: String, save: bool },
    Cancel,
}

impl PasswordPrompt {
    pub fn new(conn: Uuid, conn_name: String, kind: SecretKind) -> Self {
        Self { conn, conn_name, kind, password: String::new(), save: false, focus_requested: false }
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> Option<PromptResult> {
        let mut out = None;
        egui::Modal::new(egui::Id::new("password-modal")).show(ctx, |ui| {
            ui.set_width(360.0);
            match self.kind {
                SecretKind::DbPassword => {
                    ui.heading("Password Required");
                    ui.label(format!("Enter the database password for “{}”.", self.conn_name));
                }
                SecretKind::TunnelSecret => {
                    ui.heading("Tunnel Password Required");
                    ui.label(format!(
                        "Enter the SSH password or key passphrase, the proxy password or the PHP tunnel key for “{}”.",
                        self.conn_name
                    ));
                }
            }
            let l = ui.label("Password");
            let r = ui
                .add(crate::ui::widgets::field(&mut self.password).password(true).desired_width(f32::INFINITY))
                .labelled_by(l.id);
            if !self.focus_requested {
                r.request_focus();
                self.focus_requested = true;
            }
            crate::ui::widgets::check(ui, &mut self.save, "Save in Keychain");
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(theme::primary_button(theme::current(ui.ctx()), "Connect")).clicked() || enter {
                        out = Some(PromptResult::Ok { password: self.password.clone(), save: self.save });
                    }
                    if ui.button("Cancel").clicked() {
                        out = Some(PromptResult::Cancel);
                    }
                });
            });
        });
        out
    }
}

/// Settings window. Returns `Some(new_settings)` when the user saves.
pub fn settings_ui(ctx: &egui::Context, draft: &mut Settings, open: &mut bool) -> Option<Settings> {
    let mut saved = None;
    let mut keep_open = *open;
    egui::Window::new("Settings").open(&mut keep_open).collapsible(false).resizable(false).default_width(440.0).show(
        ctx,
        |ui| {
            let p = theme::current(ui.ctx());
            let row = |ui: &mut egui::Ui, title: &str, hint: &str, control: &mut dyn FnMut(&mut egui::Ui)| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(title);
                        if !hint.is_empty() {
                            ui.label(egui::RichText::new(hint).size(11.5).color(p.text_faint));
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| control(ui));
                });
                ui.add_space(6.0);
            };
            row(ui, "Theme", "", &mut |ui| {
                super::widgets::segmented(ui, &mut draft.theme, &[(Theme::Light, "Light"), (Theme::Dark, "Dark")]);
            });
            row(ui, "Editor font size", "Query editor and grid", &mut |ui| {
                let mut v = f64::from(draft.editor_font_size);
                if super::widgets::stepper(ui, &mut v, 9.0..=24.0, 1.0, " pt").changed() {
                    draft.editor_font_size = v as f32;
                }
            });
            row(ui, "Records per page", "Rows loaded when browsing a table", &mut |ui| {
                let mut v = draft.page_size as f64;
                if super::widgets::stepper(ui, &mut v, 10.0..=100_000.0, 100.0, "").changed() {
                    draft.page_size = v as _;
                }
            });
            row(ui, "Max result rows", "Kept per result set in the query editor", &mut |ui| {
                let mut v = draft.max_result_rows as f64;
                if super::widgets::stepper(ui, &mut v, 100.0..=5_000_000.0, 1_000.0, "").changed() {
                    draft.max_result_rows = v as _;
                }
            });
            row(ui, "History entries shown", "", &mut |ui| {
                let mut v = draft.history_limit as f64;
                if super::widgets::stepper(ui, &mut v, 100.0..=1_000_000.0, 100.0, "").changed() {
                    draft.history_limit = v as _;
                }
            });
            row(ui, "Restore tabs on launch", "Reopen the previous session", &mut |ui| {
                super::widgets::toggle(ui, &mut draft.restore_session, "Restore tabs on launch");
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.add(theme::primary_button(p, "Save")).clicked() {
                    saved = Some(draft.clone().sanitized());
                }
                if ui.button("Reset to Defaults").clicked() {
                    *draft = Settings::default();
                }
            });
        },
    );
    *open = keep_open && saved.is_none();
    saved
}

pub fn about_ui(ctx: &egui::Context, open: &mut bool) {
    egui::Window::new("About Talus").open(open).collapsible(false).resizable(false).show(ctx, |ui| {
        ui.heading("Talus");
        ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
        ui.label("A native database client written in Rust.");
        ui.label("Engines: SQLite, PostgreSQL, MySQL / MariaDB.");
        ui.label("All features are available — no trial, no licence key.");
    });
}
