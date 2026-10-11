//! Server accounts: list, grants of the selected account, create and drop (PostgreSQL, MySQL).

use egui::RichText;
use egui_phosphor::regular as icons;
use talus_core::model::Engine;
use talus_core::model::{ObjectKind, ObjectRef};
use talus_core::sql::users::{GrantTarget, UserInfo, grant_sql, privileges};
use uuid::Uuid;

use crate::state::Loadable;
use crate::theme;

#[derive(Default)]
pub struct NewUser {
    pub name: String,
    pub host: String,
    pub password: String,
}

#[derive(Default)]
struct GrantForm {
    open: bool,
    /// 0 = database, 1 = schema, 2 = table
    kind: usize,
    database: String,
    schema: String,
    table: String,
    chosen: Vec<String>,
}

impl GrantForm {
    fn target(&self, engine: Engine) -> Result<GrantTarget, String> {
        let db = self.database.trim();
        let schema = Some(self.schema.trim()).filter(|s| !s.is_empty()).or(engine.default_schema());
        match self.kind {
            0 if db.is_empty() => Err("Enter the database.".into()),
            0 => Ok(GrantTarget::Database(db.into())),
            1 if self.schema.trim().is_empty() && engine != Engine::MySql => Err("Enter the schema.".into()),
            1 => Ok(GrantTarget::Schema(if engine == Engine::MySql {
                db.to_string()
            } else {
                self.schema.trim().to_string()
            })),
            _ if self.table.trim().is_empty() => Err("Enter the table.".into()),
            _ => Ok(GrantTarget::Table(ObjectRef {
                database: db.into(),
                schema: schema.map(String::from),
                name: self.table.trim().into(),
                kind: ObjectKind::Table,
            })),
        }
    }
}

pub struct UsersTab {
    pub conn: Uuid,
    pub conn_name: String,
    pub engine: Engine,
    pub users: Loadable<Vec<UserInfo>>,
    pub selected: Option<usize>,
    pub grants: Loadable<Vec<String>>,
    pub form: Option<NewUser>,
    grant: GrantForm,
    pub error: Option<String>,
}

pub enum UsersAction {
    Reload,
    LoadGrants(UserInfo),
    Create {
        name: String,
        host: String,
        password: String,
    },
    Drop(UserInfo),
    /// GRANT / REVOKE template for the selected account, opened in a query tab.
    OpenTemplate(String),
    /// Run this GRANT / REVOKE after confirmation.
    RunGrant {
        title: String,
        sql: String,
    },
}

impl UsersTab {
    pub fn new(conn: Uuid, conn_name: String, engine: Engine) -> Self {
        Self {
            conn,
            conn_name,
            engine,
            users: Loadable::NotLoaded,
            selected: None,
            grants: Loadable::NotLoaded,
            form: None,
            grant: GrantForm::default(),
            error: None,
        }
    }

    pub fn selected_user(&self) -> Option<&UserInfo> {
        self.users.loaded()?.get(self.selected?)
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64) -> Vec<UsersAction> {
        let p = theme::current(ui.ctx());
        let mut out = Vec::new();
        if matches!(self.users, Loadable::NotLoaded) {
            out.push(UsersAction::Reload);
        }
        let is_mysql = self.engine == Engine::MySql;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.users.is_loading(), egui::Button::new(format!("{} Refresh", icons::ARROW_CLOCKWISE)))
                .clicked()
            {
                out.push(UsersAction::Reload);
            }
            if ui.button(format!("{} New User…", icons::USER_PLUS)).clicked() {
                self.form = Some(NewUser::default());
            }
            let sel = self.selected_user().cloned();
            if ui.add_enabled(sel.is_some(), egui::Button::new(format!("{} Drop", icons::TRASH))).clicked()
                && let Some(u) = sel.clone()
            {
                out.push(UsersAction::Drop(u));
            }
            if ui
                .add_enabled(sel.is_some(), egui::Button::new(format!("{} GRANT Template", icons::FILE_SQL)))
                .on_hover_text("Open a GRANT statement for this account in a query tab")
                .clicked()
                && let Some(u) = sel
            {
                out.push(UsersAction::OpenTemplate(grant_template(self.engine, &u)));
            }
            if self.users.is_loading() {
                ui.spinner();
            }
        });
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(p.danger));
        }
        if let Some(f) = self.form.as_mut() {
            let mut close = false;
            ui.group(|ui| {
                egui::Grid::new(("new-user", tab_id)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    let l = ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut f.name).desired_width(220.0)).labelled_by(l.id);
                    ui.end_row();
                    if is_mysql {
                        let l = ui.label("Host");
                        ui.add(egui::TextEdit::singleline(&mut f.host).hint_text("%").desired_width(220.0))
                            .labelled_by(l.id);
                        ui.end_row();
                    }
                    let l = ui.label("Password");
                    ui.add(egui::TextEdit::singleline(&mut f.password).password(true).desired_width(220.0))
                        .labelled_by(l.id);
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    if ui.button("Review SQL…").clicked() {
                        out.push(UsersAction::Create {
                            name: f.name.clone(),
                            host: f.host.clone(),
                            password: f.password.clone(),
                        });
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.form = None;
            }
        }
        ui.separator();
        let users = match &self.users {
            Loadable::Loaded(v) => v.clone(),
            Loadable::Failed(e) => {
                ui.label(RichText::new(e).color(p.danger));
                return out;
            }
            _ => return out,
        };
        egui::Panel::left(egui::Id::new(("users-list", tab_id))).resizable(true).default_size(340.0).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for (i, u) in users.iter().enumerate() {
                    let label = if u.attrs.is_empty() {
                        format!("{} {}", icons::USER, u.display())
                    } else {
                        format!("{} {}  ·  {}", icons::USER, u.display(), u.attrs.join(", "))
                    };
                    if ui.selectable_label(self.selected == Some(i), label).clicked() && self.selected != Some(i) {
                        self.selected = Some(i);
                        self.grants = Loadable::NotLoaded;
                        out.push(UsersAction::LoadGrants(u.clone()));
                    }
                }
            });
        });
        if let Some(user) = self.selected_user().cloned() {
            self.grant_editor(ui, tab_id, &user, &mut out);
        }
        match &self.grants {
            Loadable::Loaded(g) if g.is_empty() => {
                ui.label(RichText::new("No grants.").color(p.text_dim));
            }
            Loadable::Loaded(g) => {
                egui::ScrollArea::both().id_salt(("grants", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
                    for line in g {
                        ui.label(RichText::new(line).monospace());
                    }
                });
            }
            Loadable::Failed(e) => {
                ui.label(RichText::new(e).color(p.danger));
            }
            Loadable::Loading(_) => {
                ui.spinner();
            }
            Loadable::NotLoaded => {
                ui.label(RichText::new("Select an account to see its grants.").color(p.text_dim));
            }
        }
        out
    }
}

impl UsersTab {
    /// Privilege picker for the selected account: target, privileges, Grant / Revoke.
    fn grant_editor(&mut self, ui: &mut egui::Ui, tab_id: u64, user: &UserInfo, out: &mut Vec<UsersAction>) {
        let p = theme::current(ui.ctx());
        let engine = self.engine;
        let g = &mut self.grant;
        if ui.selectable_label(g.open, format!("{} Privileges…", icons::SHIELD_CHECK)).clicked() {
            g.open = !g.open;
        }
        if !g.open {
            return;
        }
        ui.group(|ui| {
            ui.horizontal(|ui| {
                for (i, label) in ["Database", "Schema", "Table"].iter().enumerate() {
                    if engine == Engine::MySql && i == 1 {
                        continue;
                    }
                    if ui.selectable_value(&mut g.kind, i, *label).clicked() {
                        g.chosen.clear();
                    }
                }
            });
            egui::Grid::new(("grant-target", tab_id)).num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                let l = ui.label("Database");
                ui.add(egui::TextEdit::singleline(&mut g.database).desired_width(180.0)).labelled_by(l.id);
                ui.end_row();
                if g.kind >= 1 && engine != Engine::MySql {
                    let l = ui.label("Schema");
                    ui.add(
                        egui::TextEdit::singleline(&mut g.schema)
                            .hint_text(engine.default_schema().unwrap_or(""))
                            .desired_width(180.0),
                    )
                    .labelled_by(l.id);
                    ui.end_row();
                }
                if g.kind == 2 {
                    let l = ui.label("Table");
                    ui.add(egui::TextEdit::singleline(&mut g.table).desired_width(180.0)).labelled_by(l.id);
                    ui.end_row();
                }
            });
            let target = g.target(engine);
            let offered: &[&str] = match &target {
                Ok(t) => privileges(engine, t),
                Err(_) => privileges(
                    engine,
                    &match g.kind {
                        0 => GrantTarget::Database(String::new()),
                        1 => GrantTarget::Schema(String::new()),
                        _ => GrantTarget::Table(ObjectRef {
                            database: String::new(),
                            schema: None,
                            name: String::new(),
                            kind: ObjectKind::Table,
                        }),
                    },
                ),
            };
            ui.horizontal_wrapped(|ui| {
                for pr in offered {
                    let mut on = g.chosen.iter().any(|c| c == pr);
                    if ui.checkbox(&mut on, *pr).changed() {
                        if on {
                            g.chosen.push((*pr).to_string());
                        } else {
                            g.chosen.retain(|c| c != pr);
                        }
                    }
                }
            });
            match &target {
                Err(e) => {
                    ui.label(RichText::new(e).color(p.text_dim));
                }
                Ok(t) => {
                    ui.horizontal(|ui| {
                        for (label, grant) in [("Grant", true), ("Revoke", false)] {
                            let sql = grant_sql(engine, user, &g.chosen, t, grant);
                            if ui.add_enabled(sql.is_ok(), egui::Button::new(label)).clicked()
                                && let Ok(sql) = sql
                            {
                                out.push(UsersAction::RunGrant { title: format!("{label} Privileges"), sql });
                            }
                        }
                    });
                }
            }
        });
    }
}

fn grant_template(engine: Engine, u: &UserInfo) -> String {
    match engine {
        Engine::Postgres => format!(
            "-- Adjust the object and privileges, then run.\nGRANT SELECT ON TABLE schema_name.table_name TO \"{}\";\n-- REVOKE SELECT ON TABLE schema_name.table_name FROM \"{}\";",
            u.name.replace('"', "\"\""),
            u.name.replace('"', "\"\"")
        ),
        _ => {
            let who = format!("'{}'@'{}'", u.name.replace('\'', "''"), u.host.as_deref().unwrap_or("%"));
            format!(
                "-- Adjust the object and privileges, then run.\nGRANT SELECT ON database_name.* TO {who};\n-- REVOKE SELECT ON database_name.* FROM {who};"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_quotes_names() {
        let u = UserInfo { name: "a\"b".into(), host: None, attrs: vec![] };
        assert!(grant_template(Engine::Postgres, &u).contains("TO \"a\"\"b\";"));
        let m = UserInfo { name: "o'x".into(), host: Some("h".into()), attrs: vec![] };
        assert!(grant_template(Engine::MySql, &m).contains("'o''x'@'h'"));
    }
}
