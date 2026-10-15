//! Model designer: draw tables and relations on a canvas, edit them in a side panel, save the model
//! as JSON, reverse-engineer it from a database, compare it with a live schema and forward-engineer
//! DDL for any SQL engine.

use std::path::PathBuf;

use egui::{FontId, Pos2, Rect, RichText, Sense, Stroke, pos2, vec2};
use egui_phosphor::regular as icons;
use talus_core::model::{Engine, ObjectRef, TableStructure};
use talus_core::model_design::{DesignModel, LType, MColumn, MFk, MIndex, MTable, ModelKind, TableRole};

use uuid::Uuid;

use crate::theme;

const BOX_W: f32 = 210.0;
const HEADER_H: f32 = 26.0;
const ROW_H: f32 = 18.0;
const ACTIONS: [&str; 5] = ["NO ACTION", "RESTRICT", "CASCADE", "SET NULL", "SET DEFAULT"];

pub struct ModelTab {
    pub model: DesignModel,
    pub path: Option<PathBuf>,
    dirty: bool,
    selected: Option<usize>,
    dragging: Option<usize>,
    pan: egui::Vec2,
    engine: Engine,
    database: String,
    schema: String,
    sql: Option<Result<Vec<String>, String>>,
    pub error: Option<String>,
    pub loading: bool,
    /// Connection and database of the last comparison; its script opens bound to them.
    pub target: Option<(Uuid, String)>,
    /// The print window while it is open.
    print: Option<super::print_dialog::PrintDialog>,
}

pub enum ModelAction {
    /// Replace the model with the tables of the navigator selection.
    Reverse,
    /// Diff the model against the tables of the navigator selection.
    Compare,
    /// Open the script in a query tab (engine decides the dialect shown).
    OpenSql(String),
    Copy(String),
}

fn box_height(t: &MTable) -> f32 {
    HEADER_H + ROW_H * t.columns.len().max(1) as f32 + 6.0
}

fn csv(list: &[String]) -> String {
    list.join(", ")
}

fn parse_csv(text: &str) -> Vec<String> {
    text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

impl ModelTab {
    pub fn new() -> Self {
        Self {
            model: DesignModel { name: "Untitled model".into(), kind: ModelKind::Physical, tables: Vec::new() },
            path: None,
            dirty: false,
            selected: None,
            dragging: None,
            pan: vec2(20.0, 20.0),
            engine: Engine::Postgres,
            database: String::new(),
            schema: "public".into(),
            sql: None,
            error: None,
            loading: false,
            target: None,
            print: None,
        }
    }

    pub fn title(&self) -> String {
        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map_or_else(|| self.model.name.clone(), |s| s.to_string_lossy().into_owned());
        format!("Model — {name}{}", if self.dirty { " •" } else { "" })
    }

    pub fn fail(&mut self, msg: String) {
        self.loading = false;
        self.error = Some(msg);
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn set_model(&mut self, model: DesignModel) {
        self.model = model;
        self.selected = None;
        self.sql = None;
        self.dirty = true;
        self.loading = false;
    }

    /// Shows the statements that make the live schema `live` (on `conn`) match the model.
    pub fn compare_with(
        &mut self,
        conn: Uuid,
        engine: Engine,
        database: &str,
        schema: Option<&str>,
        live: &[(ObjectRef, TableStructure)],
    ) {
        self.loading = false;
        self.engine = engine;
        self.database = database.to_string();
        self.schema = schema.unwrap_or_default().to_string();
        self.target = Some((conn, database.to_string()));
        match self.model.diff(engine, database, schema, live, false) {
            Ok(plan) if plan.statements.is_empty() => {
                self.sql = None;
                self.error = Some("The model is consistent with the database.".into());
            }
            Ok(plan) => {
                self.error = None;
                self.sql = Some(Ok(plan.statements));
            }
            Err(e) => {
                self.sql = None;
                self.error = Some(e.to_string());
            }
        }
    }

    pub fn save(&mut self, path: PathBuf) {
        match serde_json::to_string_pretty(&self.model)
            .map_err(|e| e.to_string())
            .and_then(|j| std::fs::write(&path, j).map_err(|e| e.to_string()))
        {
            Ok(()) => {
                self.path = Some(path);
                self.dirty = false;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("Could not save the model: {e}")),
        }
    }

    pub fn load(&mut self, path: PathBuf) {
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<DesignModel>(&t).map_err(|e| e.to_string()))
        {
            Ok(m) => {
                self.model = m;
                self.path = Some(path);
                self.dirty = false;
                self.selected = None;
                self.sql = None;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("Could not open the model: {e}")),
        }
    }

    fn new_table(&mut self) {
        let name = self.unique_name("table");
        let (x, y) = self.next_slot();
        self.add(MTable::new(&name, x, y));
    }

    /// `prefix1`, `prefix2`, … — the first name no table uses.
    fn unique_name(&self, prefix: &str) -> String {
        let mut n = self.model.tables.len() + 1;
        while self.model.tables.iter().any(|t| t.name.eq_ignore_ascii_case(&format!("{prefix}{n}"))) {
            n += 1;
        }
        format!("{prefix}{n}")
    }

    fn next_slot(&self) -> (f32, f32) {
        ((self.model.tables.len() % 4) as f32 * 260.0, (self.model.tables.len() / 4) as f32 * 260.0)
    }

    fn add(&mut self, t: MTable) {
        self.model.tables.push(t);
        self.selected = Some(self.model.tables.len() - 1);
        self.dirty = true;
        self.sql = None;
    }

    /// Dimensional and Data Vault templates. A link joins the selected hub with another hub; a
    /// satellite hangs off the selected hub or link.
    fn templates_menu(&mut self, ui: &mut egui::Ui) {
        let selected = self.selected.and_then(|i| self.model.tables.get(i)).cloned();
        let hubs: Vec<MTable> = self.model.tables.iter().filter(|t| t.role == TableRole::Hub).cloned().collect();
        let (x, y) = self.next_slot();
        for role in [TableRole::Fact, TableRole::Dimension] {
            if ui.button(format!("New {}", role.label())).clicked() {
                let mut t = MTable::new(&self.unique_name(if role == TableRole::Fact { "fact" } else { "dim" }), x, y);
                t.role = role;
                self.add(t);
                ui.close();
            }
        }
        ui.separator();
        if ui.button("New Hub").clicked() {
            let name = self.unique_name("hub");
            self.add(MTable::hub(&name, x, y));
            ui.close();
        }
        let first = selected.clone().filter(|t| t.role == TableRole::Hub).or_else(|| hubs.first().cloned());
        let second = first.as_ref().and_then(|f| hubs.iter().find(|h| h.name != f.name).cloned());
        let link =
            ui.add_enabled(second.is_some(), egui::Button::new("New Link")).on_disabled_hover_text("Needs two hubs");
        if link.clicked()
            && let (Some(a), Some(b)) = (first, second)
        {
            let name = self.unique_name("link");
            self.add(MTable::link(&name, &[&a, &b], x, y));
            ui.close();
        }
        let parent = selected.filter(|t| matches!(t.role, TableRole::Hub | TableRole::Link));
        let sat = ui
            .add_enabled(parent.is_some(), egui::Button::new("New Satellite"))
            .on_disabled_hover_text("Select a hub or link first");
        if sat.clicked()
            && let Some(p) = parent
        {
            let name = self.unique_name(&format!("sat_{}_", p.name));
            self.add(MTable::satellite(&name, &p, x, y));
            ui.close();
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64) -> Vec<ModelAction> {
        let p = theme::current(ui.ctx());
        let mut out = Vec::new();
        let kind = self.model.kind;
        let noun = kind.object_noun();
        // Wraps on narrow windows so no action ends up clipped off the tab.
        ui.horizontal_wrapped(|ui| {
            if ui.button(format!("{} New {noun}", icons::PLUS)).clicked() {
                self.new_table();
            }
            ui.menu_button(format!("{} Templates", icons::STACK), |ui| self.templates_menu(ui))
                .response
                .on_hover_text("Star schema (fact, dimension) and Data Vault (hub, link, satellite) tables");
            if ui
                .add_enabled(self.selected.is_some(), egui::Button::new(format!("{} Delete {noun}", icons::TRASH)))
                .clicked()
                && let Some(i) = self.selected.take()
            {
                let gone = self.model.tables.remove(i).name;
                // Relations that pointed at it would dangle.
                for t in &mut self.model.tables {
                    t.foreign_keys.retain(|f| !f.ref_table.eq_ignore_ascii_case(&gone));
                }
                self.dirty = true;
            }
            ui.separator();
            if ui.button(format!("{} Open…", icons::FOLDER_OPEN)).clicked()
                && let Some(f) = rfd::FileDialog::new().add_filter("Talus model", &["json", "talus-model"]).pick_file()
            {
                self.load(f);
            }
            if ui.button(format!("{} Save", icons::FLOPPY_DISK)).clicked() {
                let target = self.path.clone().or_else(|| {
                    rfd::FileDialog::new().add_filter("Talus model", &["json"]).set_file_name("model.json").save_file()
                });
                if let Some(path) = target {
                    self.save(path);
                }
            }
            if ui
                .add_enabled(!self.model.tables.is_empty(), egui::Button::new(format!("{} Print…", icons::PRINTER)))
                .on_hover_text("Page setup, preview, PDF and printing of the whole model")
                .clicked()
            {
                let scene = crate::print_scene::model_scene(
                    &self.model,
                    self.engine,
                    &crate::print_fonts::PrintFonts::default(),
                );
                self.print = Some(super::print_dialog::PrintDialog::new(scene));
            }
            ui.separator();
            // Converting between levels is lossless: the level only decides what is shown and allowed.
            let l = ui.label("Model type");
            super::widgets::combo(("model-kind", tab_id))
                .selected_text(kind.label())
                .width(110.0)
                .show_ui(ui, |ui| {
                    for k in ModelKind::ALL {
                        if ui.selectable_value(&mut self.model.kind, k, k.label()).changed() {
                            self.dirty = true;
                            self.sql = None;
                            self.error = None;
                        }
                    }
                })
                .response
                .labelled_by(l.id);
            ui.separator();
            if ui
                .add_enabled(
                    !self.loading,
                    egui::Button::new(format!("{} Reverse from Selection", icons::ARROW_FAT_LINES_DOWN)),
                )
                .on_hover_text("Replace the model with the tables of the schema selected in the navigator")
                .clicked()
            {
                self.loading = true;
                out.push(ModelAction::Reverse);
            }
            if ui
                .add_enabled(
                    !self.loading && kind == ModelKind::Physical,
                    egui::Button::new(format!("{} Compare with Selection", icons::GIT_DIFF)),
                )
                .on_hover_text("Statements that make the schema selected in the navigator match the model")
                .clicked()
            {
                self.loading = true;
                out.push(ModelAction::Compare);
            }
            if self.loading {
                ui.spinner();
            }
        });
        if kind != ModelKind::Physical {
            ui.label(
                RichText::new(format!(
                    "{} model: switch Model type to Physical to generate SQL or compare with a database.",
                    kind.label()
                ))
                .color(p.text_dim),
            );
        }
        ui.horizontal(|ui| {
            if kind != ModelKind::Physical {
                return;
            }
            ui.label("Forward engineer for");
            super::widgets::combo(("model-engine", tab_id)).selected_text(self.engine.label()).width(130.0).show_ui(
                ui,
                |ui| {
                    for e in Engine::ALL.into_iter().filter(|e| e.is_sql()) {
                        if ui.selectable_value(&mut self.engine, e, e.label()).clicked() {
                            self.schema = e.default_schema().unwrap_or("").into();
                            self.sql = None;
                        }
                    }
                },
            );
            ui.add(egui::TextEdit::singleline(&mut self.database).desired_width(110.0).hint_text("database"));
            if self.engine.default_schema().is_some() {
                ui.add(egui::TextEdit::singleline(&mut self.schema).desired_width(90.0).hint_text("schema"));
            }
            if ui.button(format!("{} Generate SQL", icons::CODE)).clicked() {
                let schema =
                    Some(self.schema.trim()).filter(|s| !s.is_empty() && self.engine.default_schema().is_some());
                self.sql =
                    Some(self.model.forward(self.engine, self.database.trim(), schema).map_err(|e| e.to_string()));
            }
            if ui.button(format!("{} Check", icons::CHECK_CIRCLE)).clicked() {
                let problems = self.model.validate();
                self.error =
                    Some(if problems.is_empty() { "The model is consistent.".into() } else { problems.join("\n") });
            }
        });
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(if e.starts_with("The model is consistent") { p.ok } else { p.danger }));
        }
        if let Some(sql) = &self.sql {
            egui::Panel::bottom(egui::Id::new(("model-sql", tab_id))).resizable(true).default_size(170.0).show(
                ui,
                |ui| match sql {
                    Ok(list) => {
                        let text = list.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n\n");
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{} statement(s)", list.len())).strong());
                            if ui.button(format!("{} Open in Query", icons::FILE_SQL)).clicked() {
                                out.push(ModelAction::OpenSql(text.clone()));
                            }
                            if ui.button(format!("{} Copy", icons::COPY)).clicked() {
                                out.push(ModelAction::Copy(text.clone()));
                            }
                        });
                        egui::ScrollArea::both()
                            .id_salt(("model-sql-scroll", tab_id))
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let job = super::editor::highlight(
                                    ui.ctx(),
                                    self.engine,
                                    &egui::TextStyle::Monospace.resolve(ui.style()),
                                    &text,
                                );
                                ui.add(egui::Label::new(job).selectable(true));
                            });
                    }
                    Err(e) => {
                        ui.label(RichText::new(e).color(p.danger));
                    }
                },
            );
        }
        egui::Panel::right(egui::Id::new(("model-editor", tab_id)))
            .resizable(true)
            .default_size(480.0)
            .size_range(460.0..=900.0)
            .show(ui, |ui| {
                self.editor(ui, tab_id);
            });
        self.canvas(ui, tab_id);
        if let Some(d) = &mut self.print
            && !d.ui(ui.ctx())
        {
            self.print = None;
        }
        out
    }

    fn canvas(&mut self, ui: &mut egui::Ui, tab_id: u64) {
        let p = theme::current(ui.ctx());
        let (canvas, bg) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let painter = ui.painter_at(canvas);
        painter.rect_filled(canvas, 0.0, p.bg);
        if bg.dragged() && self.dragging.is_none() {
            self.pan += bg.drag_delta();
        }
        if bg.clicked() {
            self.selected = None;
        }
        if self.model.tables.is_empty() {
            painter.text(
                canvas.center(),
                egui::Align2::CENTER_CENTER,
                "Add a table, or reverse-engineer one from a database.",
                FontId::proportional(14.0),
                p.text_dim,
            );
        }
        let to_screen = |x: f32, y: f32| canvas.min + self.pan + vec2(x, y);
        let rects: Vec<Rect> = self
            .model
            .tables
            .iter()
            .map(|t| Rect::from_min_size(to_screen(t.x, t.y), vec2(BOX_W, box_height(t))))
            .collect();
        let edge = Stroke::new(1.5, p.text_dim);
        for (i, t) in self.model.tables.iter().enumerate() {
            for f in &t.foreign_keys {
                let Some(pi) = self.model.tables.iter().position(|x| x.name.eq_ignore_ascii_case(&f.ref_table)) else {
                    continue;
                };
                let row = f
                    .columns
                    .first()
                    .and_then(|c| t.columns.iter().position(|x| x.name.eq_ignore_ascii_case(c)))
                    .unwrap_or(0);
                let (src, dst) = (rects[i], rects[pi]);
                let y = src.top() + HEADER_H + ROW_H * (row as f32 + 0.5);
                let (from, to): (Pos2, Pos2) = if dst.center().x < src.center().x {
                    (pos2(src.left(), y), pos2(dst.right(), dst.top() + HEADER_H * 0.5))
                } else {
                    (pos2(src.right(), y), pos2(dst.left(), dst.top() + HEADER_H * 0.5))
                };
                let mid = (from.x + to.x) / 2.0;
                painter.line(vec![from, pos2(mid, from.y), pos2(mid, to.y), to], edge);
                painter.circle_filled(to, 3.5, p.accent);
            }
        }
        let mut grabbed = None;
        for (i, t) in self.model.tables.iter().enumerate() {
            let r = rects[i];
            let resp = ui.interact(r, egui::Id::new(("model-box", tab_id, i)), Sense::click_and_drag());
            resp.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("model table {}", t.name))
            });
            if resp.clicked() || resp.drag_started() {
                self.selected = Some(i);
            }
            if resp.drag_started() {
                grabbed = Some(i);
            }
            let sel = self.selected == Some(i);
            super::widgets::table_card(&painter, r, sel);
            let header = Rect::from_min_size(r.min, vec2(r.width(), HEADER_H));
            painter.text(
                header.left_center() + vec2(8.0, 0.0),
                egui::Align2::LEFT_CENTER,
                &t.name,
                theme::semibold(13.0),
                p.text,
            );
            if t.role != TableRole::None {
                painter.text(
                    header.right_center() - vec2(8.0, 0.0),
                    egui::Align2::RIGHT_CENTER,
                    t.role.label().to_uppercase(),
                    theme::semibold(10.0),
                    p.text_dim,
                );
            }
            let fk_cols: Vec<&str> = t.foreign_keys.iter().flat_map(|f| f.columns.iter().map(String::as_str)).collect();
            for (ci, c) in t.columns.iter().enumerate() {
                let y = r.top() + HEADER_H + ROW_H * (ci as f32 + 0.5);
                let marker = if c.primary_key {
                    icons::KEY
                } else if fk_cols.contains(&c.name.as_str()) {
                    icons::LINK
                } else {
                    ""
                };
                if !marker.is_empty() {
                    painter.text(
                        pos2(r.left() + 8.0, y),
                        egui::Align2::LEFT_CENTER,
                        marker,
                        FontId::monospace(11.5),
                        p.warn,
                    );
                }
                painter.text(
                    pos2(r.left() + 24.0, y),
                    egui::Align2::LEFT_CENTER,
                    &c.name,
                    FontId::monospace(11.5),
                    p.text,
                );
                // Conceptual: names only; logical: portable types; physical: the target engine's types.
                let ty = match self.model.kind {
                    ModelKind::Conceptual => String::new(),
                    ModelKind::Logical => c.ty.label(),
                    ModelKind::Physical => c.ty.sql(self.engine),
                };
                painter.text(
                    pos2(r.right() - 8.0, y),
                    egui::Align2::RIGHT_CENTER,
                    ty,
                    FontId::monospace(11.0),
                    p.text_dim,
                );
            }
        }
        if grabbed.is_some() {
            self.dragging = grabbed;
        }
        if let Some(i) = self.dragging {
            let delta = ui.input(|inp| inp.pointer.delta());
            if ui.input(|inp| inp.pointer.primary_down()) {
                if let Some(t) = self.model.tables.get_mut(i) {
                    t.x += delta.x;
                    t.y += delta.y;
                    if delta != egui::Vec2::ZERO {
                        self.dirty = true;
                    }
                }
            } else {
                self.dragging = None;
            }
        }
    }

    fn editor(&mut self, ui: &mut egui::Ui, tab_id: u64) {
        let p = theme::current(ui.ctx());
        let Some(ti) = self.selected.filter(|i| *i < self.model.tables.len()) else {
            let what = if self.model.kind == ModelKind::Physical { "a table" } else { "an entity" };
            ui.label(RichText::new(format!("Select {what} to edit it.")).color(p.text_dim));
            return;
        };
        let kind = self.model.kind;
        let (logical, physical) = (kind != ModelKind::Conceptual, kind == ModelKind::Physical);
        let names: Vec<String> = self.model.tables.iter().map(|t| t.name.clone()).collect();
        let before = self.model.tables[ti].clone();
        let t = &mut self.model.tables[ti];
        egui::ScrollArea::vertical().id_salt(("model-editor-scroll", tab_id)).auto_shrink([false, false]).show(
            ui,
            |ui| {
                ui.horizontal(|ui| {
                    let l = ui.label(kind.object_noun());
                    ui.add(egui::TextEdit::singleline(&mut t.name).desired_width(240.0)).labelled_by(l.id);
                });
                ui.horizontal(|ui| {
                    let l = ui.label("Role");
                    super::widgets::combo(("model-role", tab_id))
                        .selected_text(t.role.label())
                        .width(140.0)
                        .show_ui(ui, |ui| {
                            for r in TableRole::ALL {
                                ui.selectable_value(&mut t.role, r, r.label());
                            }
                        })
                        .response
                        .labelled_by(l.id);
                });
                ui.add_space(4.0);
                ui.label(RichText::new(if physical { "Columns" } else { "Attributes" }).strong());
                let mut remove = None;
                let headers: &[&str] = match kind {
                    ModelKind::Conceptual => &["Name", "PK", ""],
                    ModelKind::Logical => &["Name", "Type", "Null", "PK", ""],
                    ModelKind::Physical => &["Name", "Type", "Null", "PK", "Auto", "Default", ""],
                };
                egui::Grid::new(("model-cols", tab_id)).num_columns(headers.len()).spacing([6.0, 4.0]).show(ui, |ui| {
                    for h in headers {
                        ui.label(RichText::new(*h).small().color(p.text_dim));
                    }
                    ui.end_row();
                    for (i, c) in t.columns.iter_mut().enumerate() {
                        // Exact size: a grid cell otherwise keeps last frame's (too narrow) column width.
                        ui.add_sized([140.0, 22.0], egui::TextEdit::singleline(&mut c.name));
                        if logical {
                            type_picker(ui, egui::Id::new(("model-type", tab_id, i)), &mut c.ty);
                            ui.checkbox(&mut c.nullable, "");
                        }
                        if ui.checkbox(&mut c.primary_key, "").changed() && c.primary_key {
                            c.nullable = false;
                        }
                        if physical {
                            ui.checkbox(&mut c.auto_increment, "");
                            let mut d = c.default.clone().unwrap_or_default();
                            if ui.add(egui::TextEdit::singleline(&mut d).desired_width(70.0)).changed() {
                                c.default = Some(d).filter(|d| !d.trim().is_empty());
                            }
                        }
                        if ui.small_button(icons::TRASH).clicked() {
                            remove = Some(i);
                        }
                        ui.end_row();
                    }
                });
                if let Some(i) = remove {
                    t.columns.remove(i);
                }
                if ui.button(format!("{} {}", icons::PLUS, if physical { "Column" } else { "Attribute" })).clicked() {
                    let n = t.columns.len() + 1;
                    t.columns.push(MColumn {
                        name: format!("column{n}"),
                        ty: LType::Varchar(255),
                        nullable: true,
                        primary_key: false,
                        auto_increment: false,
                        default: None,
                    });
                }
                ui.add_space(6.0);
                ui.label(RichText::new(if physical { "Foreign keys" } else { "Relationships" }).strong());
                let mut remove = None;
                for (i, f) in t.foreign_keys.iter_mut().enumerate() {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut f.name).desired_width(150.0));
                            if ui.small_button(icons::TRASH).clicked() {
                                remove = Some(i);
                            }
                        });
                        let mut cols = csv(&f.columns);
                        let mut rcols = csv(&f.ref_columns);
                        ui.horizontal(|ui| {
                            if ui
                                .add(egui::TextEdit::singleline(&mut cols).desired_width(110.0).hint_text("columns"))
                                .changed()
                            {
                                f.columns = parse_csv(&cols);
                            }
                            ui.label("→");
                            super::widgets::combo(("model-fk-ref", tab_id, i))
                                .selected_text(f.ref_table.clone())
                                .width(110.0)
                                .show_ui(ui, |ui| {
                                    for n in &names {
                                        ui.selectable_value(&mut f.ref_table, n.clone(), n);
                                    }
                                });
                            if ui
                                .add(egui::TextEdit::singleline(&mut rcols).desired_width(90.0).hint_text("columns"))
                                .changed()
                            {
                                f.ref_columns = parse_csv(&rcols);
                            }
                        });
                        if !physical {
                            return;
                        }
                        ui.horizontal(|ui| {
                            ui.label("on delete");
                            super::widgets::combo(("model-fk-del", tab_id, i))
                                .selected_text(f.on_delete.clone())
                                .width(110.0)
                                .show_ui(ui, |ui| {
                                    for a in ACTIONS {
                                        ui.selectable_value(&mut f.on_delete, a.to_string(), a);
                                    }
                                });
                        });
                    });
                }
                if let Some(i) = remove {
                    t.foreign_keys.remove(i);
                }
                if ui
                    .button(format!("{} {}", icons::PLUS, if physical { "Foreign Key" } else { "Relationship" }))
                    .clicked()
                {
                    let n = t.foreign_keys.len() + 1;
                    t.foreign_keys.push(MFk {
                        name: format!("fk_{}_{n}", t.name),
                        columns: t
                            .columns
                            .iter()
                            .find(|c| !c.primary_key)
                            .map(|c| vec![c.name.clone()])
                            .unwrap_or_default(),
                        ref_table: names.iter().find(|n| **n != t.name).cloned().unwrap_or_default(),
                        ref_columns: vec!["id".into()],
                        on_update: "NO ACTION".into(),
                        on_delete: "NO ACTION".into(),
                    });
                }
                if !logical {
                    return;
                }
                ui.add_space(6.0);
                ui.label(RichText::new("Indexes").strong());
                let mut remove = None;
                for (i, ix) in t.indexes.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut ix.name).desired_width(130.0));
                        let mut cols = csv(&ix.columns);
                        if ui
                            .add(egui::TextEdit::singleline(&mut cols).desired_width(110.0).hint_text("columns"))
                            .changed()
                        {
                            ix.columns = parse_csv(&cols);
                        }
                        ui.checkbox(&mut ix.unique, "unique");
                        if ui.small_button(icons::TRASH).clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    t.indexes.remove(i);
                }
                if ui.button(format!("{} Index", icons::PLUS)).clicked() {
                    let n = t.indexes.len() + 1;
                    t.indexes.push(MIndex { name: format!("ix_{}_{n}", t.name), columns: Vec::new(), unique: false });
                }
            },
        );
        if self.model.tables[ti] != before {
            // A renamed table keeps the relations that pointed at it.
            let (old, new) = (before.name.clone(), self.model.tables[ti].name.clone());
            if old != new {
                for t in &mut self.model.tables {
                    for f in &mut t.foreign_keys {
                        if f.ref_table.eq_ignore_ascii_case(&old) {
                            f.ref_table = new.clone();
                        }
                    }
                }
            }
            self.dirty = true;
            self.sql = None;
        }
    }
}

impl Default for ModelTab {
    fn default() -> Self {
        Self::new()
    }
}

fn type_picker(ui: &mut egui::Ui, id: egui::Id, ty: &mut LType) {
    super::widgets::combo(id).selected_text(ty.label()).width(110.0).show_ui(ui, |ui| {
        for c in LType::choices() {
            let label = c.label();
            if ui.selectable_label(*ty == c, label).clicked() {
                *ty = c;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_helpers_trim_and_skip_empties() {
        assert_eq!(parse_csv(" a, b ,, c "), ["a", "b", "c"]);
        assert_eq!(csv(&["a".into(), "b".into()]), "a, b");
    }

    #[test]
    fn tabs_save_and_load_models_and_report_bad_files() {
        let dir = std::env::temp_dir().join(format!("talus-model-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut t = ModelTab::new();
        t.new_table();
        assert!(t.is_dirty() && t.model.tables[0].name == "table1");
        let path = dir.join("m.json");
        t.save(path.clone());
        assert!(!t.is_dirty() && t.error.is_none());
        let mut other = ModelTab::new();
        other.load(path.clone());
        assert_eq!(other.model, t.model);
        std::fs::write(&path, "not json").unwrap();
        other.load(path);
        assert!(other.error.as_deref().is_some_and(|e| e.contains("Could not open")));
    }
}
