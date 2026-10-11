//! Redis key browser: SCAN-based key list, typed value viewer/editor, TTL, rename, delete.

use egui::RichText;
use egui_phosphor::regular as icons;
use talus_core::nosql::NoSqlOp;
use talus_core::nosql::NoSqlOut;
use talus_core::nosql::redis::{KeyInfo, RedisValue, ValueView};
use uuid::Uuid;

use super::nosql_common::{NoSqlCmd, ttl_label};
use crate::theme;

const SCAN_COUNT: usize = 500;
/// Keys beyond this stay unlisted until the pattern narrows them (the list stays responsive).
const MAX_LISTED: usize = 20_000;

const NEW_KEY_TYPES: [&str; 5] = ["string", "hash", "list", "set", "zset"];

pub struct KeysTab {
    pub conn: Uuid,
    pub conn_name: String,
    /// `db0` … as the navigator names it.
    pub database: String,
    pattern: String,
    keys: Vec<KeyInfo>,
    cursor: u64,
    scan_started: bool,
    selected: Option<String>,
    value: Option<ValueView>,
    string_edit: String,
    row_edits: Vec<String>,
    add_a: String,
    add_b: String,
    rename: Option<String>,
    ttl_edit: Option<String>,
    new_key: Option<NewKey>,
    pub busy: bool,
    pub error: Option<String>,
    dbsize: Option<u64>,
}

struct NewKey {
    name: String,
    ty: usize,
    first_a: String,
    first_b: String,
}

impl KeysTab {
    pub fn new(conn: Uuid, conn_name: String, database: String) -> Self {
        Self {
            conn,
            conn_name,
            database,
            pattern: String::new(),
            keys: Vec::new(),
            cursor: 0,
            scan_started: false,
            selected: None,
            value: None,
            string_edit: String::new(),
            row_edits: Vec::new(),
            add_a: String::new(),
            add_b: String::new(),
            rename: None,
            ttl_edit: None,
            new_key: None,
            busy: false,
            error: None,
            dbsize: None,
        }
    }

    pub fn title(&self) -> String {
        format!("{} · {}", self.conn_name, self.database)
    }

    fn scan(&mut self, fresh: bool) -> NoSqlCmd {
        self.busy = true;
        if fresh {
            self.keys.clear();
            self.cursor = 0;
        }
        NoSqlCmd::run(
            NoSqlOp::RedisScan { cursor: self.cursor, pattern: self.pattern.clone(), count: SCAN_COUNT },
            if fresh { "scan" } else { "scan-more" },
        )
    }

    fn select(&mut self, key: &str) -> NoSqlCmd {
        self.selected = Some(key.to_string());
        self.value = None;
        self.rename = None;
        self.ttl_edit = None;
        self.busy = true;
        NoSqlCmd::run(NoSqlOp::RedisGet(key.to_string()), "get")
    }

    /// A finished operation. May ask for follow-up commands (reload the value, rescan the list).
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
            ("scan" | "scan-more", NoSqlOut::Scan { cursor, keys }) => {
                self.cursor = cursor;
                self.keys.extend(keys);
                if self.keys.len() >= MAX_LISTED {
                    self.cursor = 0;
                    self.error =
                        Some(format!("Showing the first {MAX_LISTED} keys; narrow the pattern to see others."));
                }
                self.keys.sort_by(|a, b| a.key.cmp(&b.key));
                self.keys.dedup_by(|a, b| a.key == b.key);
                self.busy = true;
                vec![NoSqlCmd::run(NoSqlOp::RedisDbSize, "dbsize")]
            }
            ("dbsize", NoSqlOut::Count(n)) => {
                self.dbsize = Some(n);
                self.busy = false;
                Vec::new()
            }
            ("get", NoSqlOut::Value(v)) => {
                self.string_edit = match &v.value {
                    RedisValue::Text { text, binary: false } => text.clone(),
                    _ => String::new(),
                };
                self.row_edits = rows_of(&v.value);
                self.value = Some(*v);
                Vec::new()
            }
            // Edits that keep the key in place: reload it and its TTL badge.
            ("mutate", _) => match self.selected.clone() {
                Some(k) => {
                    self.busy = true;
                    vec![
                        NoSqlCmd::run(NoSqlOp::RedisGet(k), "get"),
                        NoSqlCmd::run(
                            NoSqlOp::RedisScan {
                                cursor: 0,
                                pattern: escape_glob(&self.selected.clone().unwrap_or_default()),
                                count: 100,
                            },
                            "ttl-refresh",
                        ),
                    ]
                }
                None => Vec::new(),
            },
            ("ttl-refresh", NoSqlOut::Scan { keys, .. }) => {
                for k in keys {
                    if let Some(slot) = self.keys.iter_mut().find(|x| x.key == k.key) {
                        *slot = k;
                    }
                }
                Vec::new()
            }
            // Edits that change which keys exist: rescan, and drop the selection.
            ("reshape", _) => {
                self.selected = None;
                self.value = None;
                self.rename = None;
                vec![self.scan(true)]
            }
            _ => Vec::new(),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64) -> Vec<NoSqlCmd> {
        let p = theme::current(ui.ctx());
        let mut cmds: Vec<NoSqlCmd> = Vec::new();
        if !self.scan_started {
            self.scan_started = true;
            cmds.push(self.scan(true));
        }
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.pattern).desired_width(220.0).hint_text("pattern, e.g. user:*"),
            );
            let go = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add_enabled(!self.busy, egui::Button::new(format!("{} Scan", icons::MAGNIFYING_GLASS))).clicked()
                || go)
                && !self.busy
            {
                cmds.push(self.scan(true));
            }
            if ui
                .add_enabled(!self.busy && self.cursor != 0, egui::Button::new("More"))
                .on_hover_text("Continue the scan")
                .clicked()
            {
                cmds.push(self.scan(false));
            }
            if ui.button(format!("{} New Key", icons::PLUS)).clicked() {
                self.new_key =
                    Some(NewKey { name: String::new(), ty: 0, first_a: String::new(), first_b: String::new() });
            }
            ui.separator();
            if ui
                .add(egui::Button::new(RichText::new(format!("{} Flush DB", icons::TRASH)).color(p.danger)))
                .on_hover_text("Delete every key of this database")
                .clicked()
            {
                cmds.push(NoSqlCmd::confirmed(
                    NoSqlOp::RedisFlushDb,
                    "reshape",
                    "Flush Database",
                    format!("Delete ALL keys of {} on “{}”? This cannot be undone.", self.database, self.conn_name),
                    "Flush Database",
                    Some("FLUSHDB".into()),
                ));
            }
            if self.busy {
                ui.spinner();
            }
            let shown = self.keys.len();
            let total = self.dbsize.map_or(String::new(), |n| format!(" of {n}"));
            ui.label(
                RichText::new(format!(
                    "{shown}{total} keys{}",
                    if self.cursor != 0 { " (more available)" } else { "" }
                ))
                .color(p.text_dim),
            );
        });
        if let Some(e) = &self.error {
            ui.label(RichText::new(format!("{} {e}", icons::WARNING_CIRCLE)).color(p.danger));
        }
        if let Some(nk) = self.new_key.as_mut() {
            let mut close = false;
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut nk.name).desired_width(200.0).hint_text("key name"));
                    super::widgets::combo(("nk-type", tab_id)).selected_text(NEW_KEY_TYPES[nk.ty]).width(90.0).show_ui(
                        ui,
                        |ui| {
                            for (i, t) in NEW_KEY_TYPES.iter().enumerate() {
                                ui.selectable_value(&mut nk.ty, i, *t);
                            }
                        },
                    );
                    let (la, lb) = match nk.ty {
                        0 => ("value", ""),
                        1 => ("field", "value"),
                        2 => ("first element", ""),
                        3 => ("first member", ""),
                        _ => ("member", "score"),
                    };
                    ui.add(egui::TextEdit::singleline(&mut nk.first_a).desired_width(150.0).hint_text(la));
                    if !lb.is_empty() {
                        ui.add(egui::TextEdit::singleline(&mut nk.first_b).desired_width(100.0).hint_text(lb));
                    }
                    let score = nk.first_b.trim().parse::<f64>().ok();
                    let ready = !nk.name.is_empty() && (nk.ty != 4 || score.is_some());
                    if ui.add_enabled(ready && !self.busy, egui::Button::new("Create")).clicked() {
                        let key = nk.name.clone();
                        let op = match nk.ty {
                            0 => NoSqlOp::RedisSetString { key, value: nk.first_a.clone() },
                            1 => NoSqlOp::RedisHashSet { key, field: nk.first_a.clone(), value: nk.first_b.clone() },
                            2 => NoSqlOp::RedisListPush { key, value: nk.first_a.clone() },
                            3 => NoSqlOp::RedisSetAdd { key, member: nk.first_a.clone() },
                            _ => NoSqlOp::RedisZAdd { key, member: nk.first_a.clone(), score: score.unwrap_or(0.0) },
                        };
                        self.busy = true;
                        cmds.push(NoSqlCmd::run(op, "reshape"));
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.new_key = None;
            }
        }
        ui.separator();

        egui::Panel::left(egui::Id::new(("keys-list", tab_id))).resizable(true).default_size(340.0).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt(("keys-scroll", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
                let mut clicked = None;
                for k in &self.keys {
                    let sel = self.selected.as_deref() == Some(&k.key);
                    let label = format!("{}  {}", type_glyph(&k.ty), k.key);
                    if ui
                        .selectable_label(sel, label)
                        .on_hover_text(format!("{} · {}", k.ty, ttl_label(k.ttl_ms)))
                        .clicked()
                        && !sel
                    {
                        clicked = Some(k.key.clone());
                    }
                }
                if let Some(k) = clicked {
                    cmds.push(self.select(&k));
                }
                if self.keys.is_empty() && !self.busy {
                    ui.label(RichText::new("No keys match.").color(p.text_dim));
                }
            });
        });

        let Some(key) = self.selected.clone() else {
            ui.label(RichText::new("Select a key.").color(p.text_dim));
            return cmds;
        };
        let info = self.keys.iter().find(|k| k.key == key).cloned();
        ui.horizontal(|ui| {
            ui.label(RichText::new(&key).strong().monospace());
            if let Some(i) = &info {
                ui.label(RichText::new(format!("{} · {}", i.ty, ttl_label(i.ttl_ms))).color(p.text_dim));
            }
        });
        ui.horizontal(|ui| {
            match self.rename.as_mut() {
                Some(name) => {
                    ui.add(egui::TextEdit::singleline(name).desired_width(180.0));
                    if ui.button("Rename").clicked() && !name.is_empty() && *name != key {
                        cmds.push(NoSqlCmd::run(
                            NoSqlOp::RedisRename { key: key.clone(), new_key: name.clone() },
                            "reshape",
                        ));
                        self.busy = true;
                        self.rename = None;
                    } else if ui.button("Cancel").clicked() {
                        self.rename = None;
                    }
                }
                None => {
                    if ui.button(format!("{} Rename", icons::PENCIL_SIMPLE)).clicked() {
                        self.rename = Some(key.clone());
                    }
                }
            }
            ui.separator();
            match self.ttl_edit.as_mut() {
                Some(t) => {
                    ui.add(egui::TextEdit::singleline(t).desired_width(80.0).hint_text("seconds"));
                    let secs = t.trim().parse::<u64>().ok();
                    if ui.add_enabled(secs.is_some(), egui::Button::new("Set TTL")).clicked() {
                        cmds.push(NoSqlCmd::run(NoSqlOp::RedisSetTtl { key: key.clone(), seconds: secs }, "mutate"));
                        self.busy = true;
                        self.ttl_edit = None;
                    }
                    if ui.button("Persist").on_hover_text("Remove the expiry").clicked() {
                        cmds.push(NoSqlCmd::run(NoSqlOp::RedisSetTtl { key: key.clone(), seconds: None }, "mutate"));
                        self.busy = true;
                        self.ttl_edit = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.ttl_edit = None;
                    }
                }
                None => {
                    if ui.button(format!("{} TTL", icons::CLOCK)).clicked() {
                        self.ttl_edit = Some(String::new());
                    }
                }
            }
            ui.separator();
            if ui.add(egui::Button::new(RichText::new(format!("{} Delete", icons::TRASH)).color(p.danger))).clicked() {
                cmds.push(NoSqlCmd::confirmed(
                    NoSqlOp::RedisDelete(key.clone()),
                    "reshape",
                    "Delete Key",
                    format!("Delete the key “{key}”?"),
                    "Delete Key",
                    Some(format!("UNLINK {key}")),
                ));
            }
        });
        ui.separator();
        let Some(view) = self.value.clone() else {
            if self.busy {
                ui.spinner();
            }
            return cmds;
        };
        if view.truncated {
            ui.label(
                RichText::new(format!(
                    "Showing the first {} of {} elements.",
                    rows_of(&view.value).len().max(1),
                    view.size
                ))
                .color(p.warn),
            );
        }
        self.value_editor(ui, tab_id, &key, &view, &mut cmds);
        cmds
    }

    fn value_editor(&mut self, ui: &mut egui::Ui, tab_id: u64, key: &str, view: &ValueView, cmds: &mut Vec<NoSqlCmd>) {
        let p = theme::current(ui.ctx());
        let k = key.to_string();
        match &view.value {
            RedisValue::Text { binary: true, text } => {
                ui.label(RichText::new("Binary value (hex), read-only").color(p.text_dim));
                ui.add(egui::Label::new(RichText::new(text).monospace()).wrap());
            }
            RedisValue::Text { .. } => {
                egui::ScrollArea::vertical()
                    .id_salt(("val-str", tab_id))
                    .max_height(ui.available_height() - 40.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.string_edit)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .desired_rows(8),
                        );
                    });
                if ui.add_enabled(!self.busy, egui::Button::new(format!("{} Save", icons::FLOPPY_DISK))).clicked() {
                    self.busy = true;
                    cmds.push(NoSqlCmd::run(
                        NoSqlOp::RedisSetString { key: k, value: self.string_edit.clone() },
                        "mutate",
                    ));
                }
            }
            RedisValue::Hash(rows) => {
                let rows = rows.clone();
                egui::ScrollArea::vertical().id_salt(("val-hash", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new(("hash-grid", tab_id)).num_columns(3).striped(true).show(ui, |ui| {
                        for (i, (f, v)) in rows.iter().enumerate() {
                            ui.label(RichText::new(f).monospace());
                            if let Some(edit) = self.row_edits.get_mut(i) {
                                ui.add(
                                    egui::TextEdit::singleline(edit)
                                        .desired_width(280.0)
                                        .font(egui::TextStyle::Monospace),
                                );
                                ui.horizontal(|ui| {
                                    if *edit != *v
                                        && ui.small_button(icons::CHECK).on_hover_text("Save value").clicked()
                                    {
                                        self.busy = true;
                                        cmds.push(NoSqlCmd::run(
                                            NoSqlOp::RedisHashSet {
                                                key: k.clone(),
                                                field: f.clone(),
                                                value: edit.clone(),
                                            },
                                            "mutate",
                                        ));
                                    }
                                    if ui.small_button(icons::TRASH).on_hover_text("Delete field").clicked() {
                                        self.busy = true;
                                        cmds.push(NoSqlCmd::run(
                                            NoSqlOp::RedisHashDelete { key: k.clone(), field: f.clone() },
                                            "mutate",
                                        ));
                                    }
                                });
                            }
                            ui.end_row();
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.add_a).desired_width(140.0).hint_text("field"));
                        ui.add(egui::TextEdit::singleline(&mut self.add_b).desired_width(200.0).hint_text("value"));
                        if ui
                            .add_enabled(!self.add_a.is_empty(), egui::Button::new(format!("{} Add", icons::PLUS)))
                            .clicked()
                        {
                            self.busy = true;
                            cmds.push(NoSqlCmd::run(
                                NoSqlOp::RedisHashSet {
                                    key: k.clone(),
                                    field: std::mem::take(&mut self.add_a),
                                    value: std::mem::take(&mut self.add_b),
                                },
                                "mutate",
                            ));
                        }
                    });
                });
            }
            RedisValue::List(items) => {
                let items = items.clone();
                egui::ScrollArea::vertical().id_salt(("val-list", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new(("list-grid", tab_id)).num_columns(3).striped(true).show(ui, |ui| {
                        for (i, v) in items.iter().enumerate() {
                            ui.label(RichText::new(i.to_string()).color(p.text_dim));
                            if let Some(edit) = self.row_edits.get_mut(i) {
                                ui.add(
                                    egui::TextEdit::singleline(edit)
                                        .desired_width(320.0)
                                        .font(egui::TextStyle::Monospace),
                                );
                                ui.horizontal(|ui| {
                                    if *edit != *v
                                        && ui.small_button(icons::CHECK).on_hover_text("Save element").clicked()
                                    {
                                        self.busy = true;
                                        cmds.push(NoSqlCmd::run(
                                            NoSqlOp::RedisListSet {
                                                key: k.clone(),
                                                index: i as u64,
                                                value: edit.clone(),
                                            },
                                            "mutate",
                                        ));
                                    }
                                    if ui.small_button(icons::TRASH).on_hover_text("Remove one occurrence").clicked() {
                                        self.busy = true;
                                        cmds.push(NoSqlCmd::run(
                                            NoSqlOp::RedisListRemove { key: k.clone(), value: v.clone() },
                                            "mutate",
                                        ));
                                    }
                                });
                            }
                            ui.end_row();
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.add_a)
                                .desired_width(260.0)
                                .hint_text("new element (appended)"),
                        );
                        if ui
                            .add_enabled(!self.add_a.is_empty(), egui::Button::new(format!("{} Push", icons::PLUS)))
                            .clicked()
                        {
                            self.busy = true;
                            cmds.push(NoSqlCmd::run(
                                NoSqlOp::RedisListPush { key: k.clone(), value: std::mem::take(&mut self.add_a) },
                                "mutate",
                            ));
                        }
                    });
                });
            }
            RedisValue::Set(items) => {
                let items = items.clone();
                egui::ScrollArea::vertical().id_salt(("val-set", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
                    for m in &items {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(m).monospace());
                            if ui.small_button(icons::TRASH).on_hover_text("Remove member").clicked() {
                                self.busy = true;
                                cmds.push(NoSqlCmd::run(
                                    NoSqlOp::RedisSetRemove { key: k.clone(), member: m.clone() },
                                    "mutate",
                                ));
                            }
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.add_a).desired_width(260.0).hint_text("new member"),
                        );
                        if ui
                            .add_enabled(!self.add_a.is_empty(), egui::Button::new(format!("{} Add", icons::PLUS)))
                            .clicked()
                        {
                            self.busy = true;
                            cmds.push(NoSqlCmd::run(
                                NoSqlOp::RedisSetAdd { key: k.clone(), member: std::mem::take(&mut self.add_a) },
                                "mutate",
                            ));
                        }
                    });
                });
            }
            RedisValue::ZSet(items) => {
                let items = items.clone();
                egui::ScrollArea::vertical().id_salt(("val-zset", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new(("zset-grid", tab_id)).num_columns(3).striped(true).show(ui, |ui| {
                        for (m, s) in &items {
                            ui.label(RichText::new(m).monospace());
                            ui.label(RichText::new(s.to_string()).monospace());
                            if ui.small_button(icons::TRASH).on_hover_text("Remove member").clicked() {
                                self.busy = true;
                                cmds.push(NoSqlCmd::run(
                                    NoSqlOp::RedisZRemove { key: k.clone(), member: m.clone() },
                                    "mutate",
                                ));
                            }
                            ui.end_row();
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.add_a).desired_width(160.0).hint_text("member"));
                        ui.add(egui::TextEdit::singleline(&mut self.add_b).desired_width(80.0).hint_text("score"));
                        let score = self.add_b.trim().parse::<f64>().ok();
                        if ui
                            .add_enabled(
                                !self.add_a.is_empty() && score.is_some(),
                                egui::Button::new(format!("{} Add", icons::PLUS)),
                            )
                            .clicked()
                        {
                            self.busy = true;
                            cmds.push(NoSqlCmd::run(
                                NoSqlOp::RedisZAdd {
                                    key: k.clone(),
                                    member: std::mem::take(&mut self.add_a),
                                    score: score.unwrap_or(0.0),
                                },
                                "mutate",
                            ));
                            self.add_b.clear();
                        }
                    });
                });
            }
            RedisValue::Stream(entries) => {
                egui::ScrollArea::vertical().id_salt(("val-stream", tab_id)).auto_shrink([false, false]).show(
                    ui,
                    |ui| {
                        ui.label(
                            RichText::new("Streams are read-only here; use the console (XADD, XDEL).")
                                .color(p.text_dim),
                        );
                        for (id, fields) in entries {
                            let line = fields.iter().map(|(f, v)| format!("{f}={v}")).collect::<Vec<_>>().join("  ");
                            ui.label(RichText::new(format!("{id}   {line}")).monospace());
                        }
                    },
                );
            }
            RedisValue::Other(t) => {
                ui.label(RichText::new(format!("Type “{t}” is not shown here; use the console.")).color(p.text_dim));
            }
            RedisValue::Missing => {
                ui.label(RichText::new("The key no longer exists.").color(p.warn));
            }
        }
    }
}

fn rows_of(v: &RedisValue) -> Vec<String> {
    match v {
        RedisValue::List(l) | RedisValue::Set(l) => l.clone(),
        RedisValue::Hash(h) => h.iter().map(|(_, v)| v.clone()).collect(),
        _ => Vec::new(),
    }
}

fn type_glyph(ty: &str) -> &'static str {
    match ty {
        "string" => icons::TEXT_T,
        "hash" => icons::TABLE,
        "list" => icons::LIST_NUMBERS,
        "set" => icons::SELECTION,
        "zset" => icons::RANKING,
        "stream" => icons::WAVEFORM,
        _ => icons::QUESTION,
    }
}

/// Escapes glob metacharacters so SCAN MATCH finds exactly this key.
fn escape_glob(key: &str) -> String {
    key.chars().flat_map(|c| if "*?[]\\".contains(c) { vec!['\\', c] } else { vec![c] }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_escape_matches_literally() {
        assert_eq!(escape_glob("a*b?[c]\\"), "a\\*b\\?\\[c\\]\\\\");
        assert_eq!(escape_glob("plain:key"), "plain:key");
    }

    #[test]
    fn scan_results_merge_sorted_and_deduplicated() {
        let mut t = KeysTab::new(Uuid::nil(), "c".into(), "db0".into());
        let k = |n: &str| KeyInfo { key: n.into(), ty: "string".into(), ttl_ms: None };
        t.on_result("scan", Ok(NoSqlOut::Scan { cursor: 7, keys: vec![k("b"), k("a")] }));
        let follow = t.on_result("scan-more", Ok(NoSqlOut::Scan { cursor: 0, keys: vec![k("b"), k("c")] }));
        assert_eq!(t.keys.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(t.cursor, 0);
        assert!(matches!(follow[0].op, NoSqlOp::RedisDbSize));
    }

    #[test]
    fn errors_are_kept_and_stop_follow_ups() {
        let mut t = KeysTab::new(Uuid::nil(), "c".into(), "db0".into());
        t.busy = true;
        assert!(t.on_result("scan", Err("boom".into())).is_empty());
        assert_eq!(t.error.as_deref(), Some("boom"));
        assert!(!t.busy);
    }
}
