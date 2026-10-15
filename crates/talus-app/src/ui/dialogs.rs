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
                .add(egui::TextEdit::singleline(&mut self.password).password(true).desired_width(f32::INFINITY))
                .labelled_by(l.id);
            if !self.focus_requested {
                r.request_focus();
                self.focus_requested = true;
            }
            ui.checkbox(&mut self.save, "Save in Keychain");
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
    egui::Window::new("Settings").open(&mut keep_open).collapsible(false).resizable(false).default_width(420.0).show(
        ctx,
        |ui| {
            egui::Grid::new("settings-grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label("Theme");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut draft.theme, Theme::Dark, "Dark");
                    ui.selectable_value(&mut draft.theme, Theme::Light, "Light");
                });
                ui.end_row();
                ui.label("Editor font size");
                ui.add(egui::Slider::new(&mut draft.editor_font_size, 9.0..=24.0).step_by(1.0));
                ui.end_row();
                ui.label("Records per page");
                ui.add(egui::DragValue::new(&mut draft.page_size).range(10..=100_000));
                ui.end_row();
                ui.label("Max result rows");
                ui.add(egui::DragValue::new(&mut draft.max_result_rows).range(100..=5_000_000))
                    .on_hover_text("Rows kept per result set in the query editor");
                ui.end_row();
                ui.label("History entries shown");
                ui.add(egui::DragValue::new(&mut draft.history_limit).range(100..=1_000_000));
                ui.end_row();
                ui.label("Restore tabs on launch");
                ui.checkbox(&mut draft.restore_session, "");
                ui.end_row();
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.add(theme::primary_button(theme::current(ui.ctx()), "Save")).clicked() {
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
