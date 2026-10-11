//! Command console for Redis (`SET k v`) and MongoDB (`{ "ping": 1 }`).

use egui::RichText;
use egui_phosphor::regular as icons;
use talus_core::model::Engine;
use talus_core::nosql::{NoSqlOp, NoSqlOut};
use uuid::Uuid;

use super::nosql_common::NoSqlCmd;
use crate::theme;

pub struct ConsoleTab {
    pub conn: Uuid,
    pub conn_name: String,
    pub engine: Engine,
    pub database: String,
    input: String,
    history: Vec<(String, Result<String, String>)>,
    pub busy: bool,
    pending_line: Option<String>,
}

impl ConsoleTab {
    pub fn new(conn: Uuid, conn_name: String, engine: Engine, database: String) -> Self {
        Self {
            conn,
            conn_name,
            engine,
            database,
            input: String::new(),
            history: Vec::new(),
            busy: false,
            pending_line: None,
        }
    }

    pub fn title(&self) -> String {
        format!("Console — {}", self.conn_name)
    }

    fn run(&mut self) -> Option<NoSqlCmd> {
        let line = self.input.trim().to_string();
        if line.is_empty() || self.busy {
            return None;
        }
        self.busy = true;
        self.pending_line = Some(line.clone());
        let op = match self.engine {
            Engine::MongoDb => NoSqlOp::MongoCommand { db: self.database.clone(), json: line },
            _ => NoSqlOp::RedisCommand(line),
        };
        Some(NoSqlCmd::run(op, "run"))
    }

    pub fn on_result(&mut self, r: Result<NoSqlOut, String>) {
        self.busy = false;
        let line = self.pending_line.take().unwrap_or_default();
        let shown = match r {
            Ok(NoSqlOut::Text(t)) => Ok(t),
            Ok(other) => Ok(format!("{other:?}")),
            Err(e) => Err(e),
        };
        if shown.is_ok() {
            self.input.clear();
        }
        self.history.push((line, shown));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64) -> Vec<NoSqlCmd> {
        let p = theme::current(ui.ctx());
        let mut cmds = Vec::new();
        let mongo = self.engine == Engine::MongoDb;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(match self.engine {
                    Engine::MongoDb => format!("db: {}", self.database),
                    _ => self.database.clone(),
                })
                .color(p.text_dim),
            );
            if ui.button(format!("{} Clear", icons::BROOM)).clicked() {
                self.history.clear();
            }
            if self.busy {
                ui.spinner();
            }
        });
        egui::Panel::bottom(egui::Id::new(("console-input", tab_id))).show(ui, |ui| {
            ui.horizontal(|ui| {
                let hint = if mongo { r#"{ "listCollections": 1 }"# } else { "SET key value" };
                let rows = if mongo { 4 } else { 1 };
                let r = ui.add(
                    egui::TextEdit::multiline(&mut self.input)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(ui.available_width() - 90.0)
                        .desired_rows(rows)
                        .hint_text(hint),
                );
                // Enter runs a Redis line; Mongo JSON is multi-line, so it needs ⌘↩.
                let submit = ui.input(|i| {
                    i.key_pressed(egui::Key::Enter) && (i.modifiers.command || (!mongo && !i.modifiers.shift))
                });
                if r.has_focus() && submit && !mongo {
                    // The newline the key press inserted is not part of the command.
                    self.input = self.input.trim_end_matches('\n').to_string();
                }
                if (ui.add_enabled(!self.busy, egui::Button::new(format!("{} Run", icons::PLAY))).clicked()
                    || (r.has_focus() && submit))
                    && let Some(c) = self.run()
                {
                    cmds.push(c);
                }
            });
        });
        egui::ScrollArea::vertical()
            .id_salt(("console-out", tab_id))
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (line, res) in &self.history {
                    ui.label(RichText::new(format!("> {line}")).monospace().color(p.accent));
                    match res {
                        Ok(t) => ui.add(egui::Label::new(RichText::new(t).monospace()).selectable(true)),
                        Err(e) => ui.label(RichText::new(format!("(error) {e}")).monospace().color(p.danger)),
                    };
                    ui.add_space(4.0);
                }
            });
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_builds_the_engine_specific_operation_once() {
        let mut c = ConsoleTab::new(Uuid::nil(), "c".into(), Engine::Redis, "db0".into());
        assert!(c.run().is_none(), "empty input does nothing");
        c.input = "  GET k ".into();
        let cmd = c.run().unwrap();
        assert!(matches!(&cmd.op, NoSqlOp::RedisCommand(l) if l == "GET k"));
        assert!(c.run().is_none(), "busy until the reply arrives");
        c.on_result(Ok(NoSqlOut::Text("\"v\"\n".into())));
        assert!(c.input.is_empty() && c.history.len() == 1);
        let mut m = ConsoleTab::new(Uuid::nil(), "m".into(), Engine::MongoDb, "admin".into());
        m.input = r#"{"ping":1}"#.into();
        assert!(matches!(m.run().unwrap().op, NoSqlOp::MongoCommand { .. }));
        m.on_result(Err("boom".into()));
        assert_eq!(m.input, r#"{"ping":1}"#, "a failed command stays editable");
    }
}
