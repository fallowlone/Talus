//! MongoDB collection browser: filtered / sorted document pages, a JSON editor per document,
//! insert, delete (by `_id`) and index management.

use egui::RichText;
use egui_phosphor::regular as icons;
use talus_core::nosql::mongo::{DocPage, FindSpec, IndexInfoDoc};
use talus_core::nosql::{NoSqlOp, NoSqlOut};
use uuid::Uuid;

use super::nosql_common::NoSqlCmd;
use crate::theme;

const PAGE_SIZES: [i64; 4] = [20, 50, 100, 500];

pub struct DocsTab {
    pub conn: Uuid,
    pub conn_name: String,
    pub database: String,
    pub collection: String,
    filter: String,
    sort: String,
    page_size: i64,
    skip: u64,
    page: Option<DocPage>,
    selected: Option<usize>,
    editor: String,
    /// The editor holds a new, not yet inserted document.
    inserting: bool,
    indexes: Option<Vec<IndexInfoDoc>>,
    show_indexes: bool,
    new_index_keys: String,
    new_index_unique: bool,
    started: bool,
    pub busy: bool,
    pub error: Option<String>,
    /// Set when the collection was dropped: the app closes the tab.
    pub dropped: bool,
}

impl DocsTab {
    pub fn new(conn: Uuid, conn_name: String, database: String, collection: String) -> Self {
        Self {
            conn,
            conn_name,
            database,
            collection,
            filter: String::new(),
            sort: String::new(),
            page_size: 50,
            skip: 0,
            page: None,
            selected: None,
            editor: String::new(),
            inserting: false,
            indexes: None,
            show_indexes: false,
            new_index_keys: String::new(),
            new_index_unique: false,
            started: false,
            busy: false,
            error: None,
            dropped: false,
        }
    }

    pub fn title(&self) -> String {
        format!("{}.{}", self.database, self.collection)
    }

    fn find(&mut self) -> NoSqlCmd {
        self.busy = true;
        NoSqlCmd::run(
            NoSqlOp::MongoFind {
                db: self.database.clone(),
                coll: self.collection.clone(),
                spec: FindSpec {
                    filter: self.filter.clone(),
                    sort: self.sort.clone(),
                    skip: self.skip,
                    limit: self.page_size,
                },
            },
            "find",
        )
    }

    fn select(&mut self, i: usize) {
        self.selected = Some(i);
        self.inserting = false;
        if let Some(d) = self.page.as_ref().and_then(|p| p.docs.get(i)) {
            self.editor = d.clone();
        }
    }

    pub fn on_result(&mut self, tag: &str, r: Result<NoSqlOut, String>) -> Vec<NoSqlCmd> {
        self.busy = false;
        let out = match r {
            Ok(o) => {
                self.error = None;
                o
            }
            Err(e) => {
                self.error = Some(e);
                return Vec::new();
            }
        };
        match (tag, out) {
            ("find", NoSqlOut::Docs(page)) => {
                let keep = self.selected.filter(|i| *i < page.docs.len());
                self.page = Some(page);
                match keep {
                    Some(i) if !self.inserting => self.select(i),
                    _ if self.inserting => {}
                    _ => {
                        self.selected = None;
                        self.editor.clear();
                    }
                }
                Vec::new()
            }
            // Writes: show the result of the same query again.
            ("write", _) => {
                self.inserting = false;
                vec![self.find()]
            }
            ("dropped", _) => {
                self.dropped = true;
                Vec::new()
            }
            ("indexes", NoSqlOut::Indexes(list)) => {
                self.indexes = Some(list);
                Vec::new()
            }
            ("index-write", _) => {
                vec![NoSqlCmd::run(
                    NoSqlOp::MongoIndexes { db: self.database.clone(), coll: self.collection.clone() },
                    "indexes",
                )]
            }
            _ => Vec::new(),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64) -> Vec<NoSqlCmd> {
        let p = theme::current(ui.ctx());
        let mut cmds = Vec::new();
        if !self.started {
            self.started = true;
            cmds.push(self.find());
        }
        ui.horizontal(|ui| {
            ui.label("Filter");
            let f = ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .desired_width(280.0)
                    .hint_text(r#"{ "age": { "$gt": 30 } }"#)
                    .font(egui::TextStyle::Monospace),
            );
            ui.label("Sort");
            let s = ui.add(
                egui::TextEdit::singleline(&mut self.sort)
                    .desired_width(160.0)
                    .hint_text(r#"{ "name": 1 }"#)
                    .font(egui::TextStyle::Monospace),
            );
            let enter = (f.lost_focus() || s.lost_focus()) && ui.input(|i| i.key_pressed(egui::Key::Enter));
            super::widgets::combo(("docs-size", tab_id))
                .selected_text(format!("{} / page", self.page_size))
                .width(100.0)
                .show_ui(ui, |ui| {
                    for n in PAGE_SIZES {
                        ui.selectable_value(&mut self.page_size, n, format!("{n} / page"));
                    }
                });
            if (ui.add_enabled(!self.busy, egui::Button::new(format!("{} Find", icons::MAGNIFYING_GLASS))).clicked()
                || enter)
                && !self.busy
            {
                self.skip = 0;
                cmds.push(self.find());
            }
            if self.busy {
                ui.spinner();
            }
        });
        ui.horizontal(|ui| {
            let total = self.page.as_ref().map_or(0, |p| p.total);
            let shown = self.page.as_ref().map_or(0, |p| p.docs.len() as u64);
            if ui.add_enabled(self.skip > 0 && !self.busy, egui::Button::new(icons::CARET_LEFT)).clicked() {
                self.skip = self.skip.saturating_sub(self.page_size as u64);
                cmds.push(self.find());
            }
            ui.label(format!("{}–{} of {}", if shown == 0 { 0 } else { self.skip + 1 }, self.skip + shown, total));
            if ui.add_enabled(self.skip + shown < total && !self.busy, egui::Button::new(icons::CARET_RIGHT)).clicked()
            {
                self.skip += self.page_size as u64;
                cmds.push(self.find());
            }
            ui.separator();
            if ui.button(format!("{} Insert Document", icons::PLUS)).clicked() {
                self.inserting = true;
                self.selected = None;
                self.editor = "{\n  \n}".into();
            }
            if ui.button(format!("{} Indexes", icons::LIST_MAGNIFYING_GLASS)).clicked() {
                self.show_indexes = !self.show_indexes;
                if self.show_indexes {
                    cmds.push(NoSqlCmd::run(
                        NoSqlOp::MongoIndexes { db: self.database.clone(), coll: self.collection.clone() },
                        "indexes",
                    ));
                }
            }
            if ui
                .add(egui::Button::new(RichText::new(format!("{} Drop Collection", icons::TRASH)).color(p.danger)))
                .clicked()
            {
                cmds.push(NoSqlCmd::confirmed(
                    NoSqlOp::MongoDropCollection { db: self.database.clone(), name: self.collection.clone() },
                    "dropped",
                    "Drop Collection",
                    format!("Permanently delete the collection “{}” with all its documents?", self.title()),
                    "Drop Collection",
                    Some(format!("db.{}.drop()", self.collection)),
                ));
            }
        });
        if let Some(e) = &self.error {
            ui.label(RichText::new(format!("{} {e}", icons::WARNING_CIRCLE)).color(p.danger));
        }
        if self.show_indexes {
            ui.group(|ui| {
                match &self.indexes {
                    None => {
                        ui.spinner();
                    }
                    Some(list) => {
                        for ix in list.clone() {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{}  {}{}",
                                        ix.name,
                                        ix.keys,
                                        if ix.unique { "  unique" } else { "" }
                                    ))
                                    .monospace(),
                                );
                                if ix.name != "_id_"
                                    && ui.small_button(icons::TRASH).on_hover_text("Drop index").clicked()
                                {
                                    cmds.push(NoSqlCmd::confirmed(
                                        NoSqlOp::MongoDropIndex {
                                            db: self.database.clone(),
                                            coll: self.collection.clone(),
                                            name: ix.name.clone(),
                                        },
                                        "index-write",
                                        "Drop Index",
                                        format!("Drop the index “{}”?", ix.name),
                                        "Drop Index",
                                        None,
                                    ));
                                }
                            });
                        }
                    }
                }
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_index_keys)
                            .desired_width(220.0)
                            .hint_text(r#"{ "field": 1 }"#)
                            .font(egui::TextStyle::Monospace),
                    );
                    ui.checkbox(&mut self.new_index_unique, "unique");
                    if ui
                        .add_enabled(!self.new_index_keys.trim().is_empty(), egui::Button::new("Create Index"))
                        .clicked()
                    {
                        cmds.push(NoSqlCmd::run(
                            NoSqlOp::MongoCreateIndex {
                                db: self.database.clone(),
                                coll: self.collection.clone(),
                                keys: self.new_index_keys.clone(),
                                unique: self.new_index_unique,
                            },
                            "index-write",
                        ));
                    }
                });
            });
        }
        ui.separator();

        egui::Panel::left(egui::Id::new(("docs-list", tab_id))).resizable(true).default_size(380.0).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt(("docs-scroll", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
                let mut clicked = None;
                if let Some(page) = &self.page {
                    for (i, d) in page.docs.iter().enumerate() {
                        let compact: String = d.split_whitespace().collect::<Vec<_>>().join(" ");
                        let preview: String = compact.chars().take(90).collect();
                        let label = format!("{}  {}", page.ids.get(i).map_or("", String::as_str), preview);
                        if ui
                            .selectable_label(self.selected == Some(i), RichText::new(label).monospace().size(11.5))
                            .clicked()
                        {
                            clicked = Some(i);
                        }
                    }
                    if page.docs.is_empty() && !self.busy {
                        ui.label(RichText::new("No documents match.").color(p.text_dim));
                    }
                }
                if let Some(i) = clicked {
                    self.select(i);
                }
            });
        });

        if self.selected.is_none() && !self.inserting {
            ui.label(RichText::new("Select a document, or insert a new one.").color(p.text_dim));
            return cmds;
        }
        ui.horizontal(|ui| {
            if self.inserting {
                ui.label(RichText::new("New document").strong());
                if ui.add_enabled(!self.busy, egui::Button::new(format!("{} Insert", icons::CHECK))).clicked() {
                    self.busy = true;
                    cmds.push(NoSqlCmd::run(
                        NoSqlOp::MongoInsert {
                            db: self.database.clone(),
                            coll: self.collection.clone(),
                            json: self.editor.clone(),
                        },
                        "write",
                    ));
                }
                if ui.button("Cancel").clicked() {
                    self.inserting = false;
                    self.editor.clear();
                }
            } else if let Some(i) = self.selected {
                let id = self.page.as_ref().and_then(|p| p.ids.get(i)).cloned().unwrap_or_default();
                ui.label(RichText::new(format!("_id: {id}")).monospace().color(p.text_dim));
                let changed = self.page.as_ref().and_then(|p| p.docs.get(i)).is_some_and(|orig| *orig != self.editor);
                if ui
                    .add_enabled(changed && !self.busy, egui::Button::new(format!("{} Save", icons::FLOPPY_DISK)))
                    .clicked()
                {
                    self.busy = true;
                    cmds.push(NoSqlCmd::run(
                        NoSqlOp::MongoReplace {
                            db: self.database.clone(),
                            coll: self.collection.clone(),
                            id: id.clone(),
                            json: self.editor.clone(),
                        },
                        "write",
                    ));
                }
                if ui.add_enabled(changed, egui::Button::new("Revert")).clicked()
                    && let Some(orig) = self.page.as_ref().and_then(|p| p.docs.get(i))
                {
                    self.editor = orig.clone();
                }
                if ui
                    .button(format!("{} Duplicate", icons::COPY))
                    .on_hover_text("Insert a copy (without _id)")
                    .clicked()
                {
                    self.inserting = true;
                    self.editor = without_id(&self.editor);
                }
                if ui
                    .add(egui::Button::new(RichText::new(format!("{} Delete", icons::TRASH)).color(p.danger)))
                    .clicked()
                {
                    cmds.push(NoSqlCmd::confirmed(
                        NoSqlOp::MongoDelete {
                            db: self.database.clone(),
                            coll: self.collection.clone(),
                            id: id.clone(),
                        },
                        "write",
                        "Delete Document",
                        format!("Delete the document with _id {id}?"),
                        "Delete Document",
                        Some(format!("db.{}.deleteOne({{ _id: {id} }})", self.collection)),
                    ));
                }
            }
        });
        egui::ScrollArea::both().id_salt(("docs-editor", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut self.editor)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .desired_rows(20)
                    .code_editor(),
            );
        });
        cmds
    }
}

/// The JSON text without its top-level `_id` line, for "Duplicate".
fn without_id(json: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::Object(mut m)) => {
            m.remove("_id");
            serde_json::to_string_pretty(&m).unwrap_or_else(|_| json.to_string())
        }
        _ => json.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_drops_the_id() {
        assert_eq!(without_id(r#"{"_id": 1, "a": 2}"#), "{\n  \"a\": 2\n}");
        assert_eq!(without_id("not json"), "not json");
    }

    #[test]
    fn a_write_triggers_a_fresh_find_and_errors_stop_follow_ups() {
        let mut t = DocsTab::new(Uuid::nil(), "c".into(), "db".into(), "coll".into());
        let follow = t.on_result("write", Ok(NoSqlOut::Done));
        assert!(matches!(follow[0].op, NoSqlOp::MongoFind { .. }));
        assert!(t.on_result("write", Err("dup key".into())).is_empty());
        assert_eq!(t.error.as_deref(), Some("dup key"));
        t.on_result("dropped", Ok(NoSqlOut::Done));
        assert!(t.dropped);
    }
}
