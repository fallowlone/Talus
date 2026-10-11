//! Table designer: edit fields of a new or existing table with a live SQL preview.
//! Saving goes through the app so destructive plans are confirmed first.

use egui::RichText;
use egui_phosphor::regular as icons;
use std::collections::HashMap;

use talus_core::model::{Engine, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectRef, TableStructure};
use talus_core::sql::Dialect;
use talus_core::sql::constraints::{self, FK_ACTIONS, FkDef, IndexDef};
use talus_core::sql::ddl::{self, AlterPlan, ColumnDef, DesignColumn};
use uuid::Uuid;

use crate::theme;
use crate::worker::JobId;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Fields,
    Indexes,
    ForeignKeys,
}

pub struct DesignTab {
    pub conn: Uuid,
    pub engine: Engine,
    pub database: String,
    pub schema: Option<String>,
    pub table_name: String,
    /// Existing table and its columns as loaded (None while creating a new table).
    pub existing: Option<(ObjectRef, Vec<ColumnDef>)>,
    pub rows: Vec<DesignColumn>,
    pub loading: Option<JobId>,
    pub saving: Option<JobId>,
    pub error: Option<String>,
    pub info: Option<String>,
    pub selected: Option<usize>,
    pub indexes: Vec<IndexDef>,
    pub fks: Vec<FkDef>,
    /// Indexes / foreign keys exactly as loaded (diff base).
    loaded: (Vec<IndexInfo>, Vec<ForeignKeyInfo>),
    section: Section,
}

pub enum DesignAction {
    Save(AlterPlan),
}

impl DesignTab {
    pub fn new_table(conn: Uuid, engine: Engine, database: String, schema: Option<String>) -> Self {
        let id_type = if engine == Engine::Sqlite { "INTEGER" } else { "int" };
        let rows = vec![DesignColumn {
            original: None,
            def: ColumnDef {
                name: "id".into(),
                data_type: id_type.into(),
                nullable: false,
                primary_key: true,
                ..Default::default()
            },
        }];
        Self {
            conn,
            engine,
            database,
            schema,
            table_name: String::new(),
            existing: None,
            rows,
            loading: None,
            saving: None,
            error: None,
            info: None,
            selected: None,
            indexes: Vec::new(),
            fks: Vec::new(),
            loaded: (Vec::new(), Vec::new()),
            section: Section::Fields,
        }
    }

    pub fn for_table(conn: Uuid, engine: Engine, obj: &ObjectRef) -> Self {
        Self {
            conn,
            engine,
            database: obj.database.clone(),
            schema: obj.schema.clone(),
            table_name: obj.name.clone(),
            existing: None,
            rows: Vec::new(),
            loading: None,
            saving: None,
            error: None,
            info: None,
            selected: None,
            indexes: Vec::new(),
            fks: Vec::new(),
            loaded: (Vec::new(), Vec::new()),
            section: Section::Fields,
        }
    }

    pub fn title(&self) -> String {
        if self.table_name.is_empty() { "New Table".into() } else { format!("{} (design)", self.table_name) }
    }

    pub fn object(&self) -> ObjectRef {
        ObjectRef {
            database: self.database.clone(),
            schema: self.schema.clone(),
            name: self.table_name.trim().to_string(),
            kind: ObjectKind::Table,
        }
    }

    pub fn set_structure(&mut self, obj: ObjectRef, st: &TableStructure) {
        let cols: Vec<ColumnDef> = st.columns.iter().map(ColumnDef::from).collect();
        self.rows = cols.iter().map(|c| DesignColumn { original: Some(c.name.clone()), def: c.clone() }).collect();
        self.table_name = obj.name.clone();
        self.existing = Some((obj, cols));
        self.indexes = IndexDef::from_info(&st.indexes);
        self.fks = FkDef::from_info(&st.foreign_keys);
        self.loaded = (st.indexes.clone(), st.foreign_keys.clone());
        self.selected = None;
    }

    pub fn is_dirty(&self) -> bool {
        match &self.existing {
            None => !self.table_name.trim().is_empty(),
            Some((_, cols)) => {
                self.indexes != IndexDef::from_info(&self.loaded.0)
                    || self.fks != FkDef::from_info(&self.loaded.1)
                    || self.rows.len() != cols.len()
                    || self
                        .rows
                        .iter()
                        .zip(cols)
                        .any(|(r, c)| r.original.as_deref() != Some(c.name.as_str()) || &r.def != c)
            }
        }
    }

    /// The statements that saving would run (or why the design is invalid).
    pub fn plan(&self) -> Result<AlterPlan, String> {
        let d = Dialect(self.engine);
        match &self.existing {
            None => {
                if self.table_name.trim().is_empty() {
                    return Err("Enter a table name.".into());
                }
                let cols: Vec<ColumnDef> = self.rows.iter().map(|r| r.def.clone()).collect();
                constraints::validate_new(d, &self.column_names(), &self.indexes, &self.fks)
                    .map_err(|e| e.to_string())?;
                let obj = self.object();
                let mut statements =
                    vec![ddl::create_table_with(d, &obj, &cols, &self.fks).map_err(|e| e.to_string())?];
                for i in &self.indexes {
                    statements.push(constraints::create_index(d, &obj, i).map_err(|e| e.to_string())?);
                }
                Ok(AlterPlan { statements, destructive: Vec::new() })
            }
            Some((obj, original)) => match self.alter_plan(d, obj, original) {
                // SQLite cannot express these edits as ALTERs: rebuild the table instead.
                Err(_) if self.engine == Engine::Sqlite => {
                    ddl::rebuild_sqlite(d, obj, &self.rows, &self.fks, &self.indexes).map_err(|e| e.to_string())
                }
                other => other,
            },
        }
    }

    /// Column ALTERs plus index / foreign-key drops before and creates after them.
    fn alter_plan(&self, d: Dialect, obj: &ObjectRef, original: &[ColumnDef]) -> Result<AlterPlan, String> {
        let mut plan = ddl::alter_table(d, obj, original, &self.rows).map_err(|e| e.to_string())?;
        let renames: HashMap<String, String> = self
            .rows
            .iter()
            .filter_map(|r| r.original.as_ref().filter(|o| **o != r.def.name).map(|o| (o.clone(), r.def.name.clone())))
            .collect();
        let cp = constraints::diff(
            d,
            obj,
            &self.column_names(),
            &renames,
            (&self.loaded.0, &self.loaded.1),
            (&self.indexes, &self.fks),
        )
        .map_err(|e| e.to_string())?;
        plan.statements.splice(0..0, cp.drops);
        plan.statements.extend(cp.creates);
        Ok(plan)
    }

    fn column_names(&self) -> Vec<String> {
        self.rows.iter().map(|r| r.def.name.clone()).collect()
    }

    /// Keeps index / foreign-key column lists pointing at columns renamed in the grid.
    fn propagate_renames(&mut self, before: &[String]) {
        for (old, row) in before.iter().zip(&self.rows) {
            let new = &row.def.name;
            if old == new {
                continue;
            }
            let swap = |cols: &mut Vec<String>| cols.iter_mut().filter(|c| *c == old).for_each(|c| *c = new.clone());
            self.indexes.iter_mut().for_each(|i| swap(&mut i.columns));
            self.fks.iter_mut().for_each(|f| swap(&mut f.columns));
        }
    }

    pub fn add_field(&mut self) {
        let n = self.rows.len() + 1;
        self.rows.push(DesignColumn {
            original: None,
            def: ColumnDef {
                name: format!("field{n}"),
                data_type: ddl::common_types(self.engine)[1].into(),
                nullable: true,
                ..Default::default()
            },
        });
        self.selected = Some(self.rows.len() - 1);
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64) -> Option<DesignAction> {
        let p = theme::current(ui.ctx());
        let mut action = None;
        let busy = self.loading.is_some() || self.saving.is_some();
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, egui::Button::new(format!("{} Add Field", icons::PLUS))).clicked() {
                self.add_field();
            }
            let can_delete = self.selected.is_some() && !busy;
            if ui.add_enabled(can_delete, egui::Button::new(format!("{} Delete Field", icons::MINUS))).clicked()
                && let Some(i) = self.selected.take()
                && i < self.rows.len()
            {
                self.rows.remove(i);
            }
            ui.separator();
            let plan = self.plan();
            let can_save = !busy && self.is_dirty() && plan.as_ref().is_ok_and(|p| !p.statements.is_empty());
            if ui
                .add_enabled(
                    can_save,
                    egui::Button::new(RichText::new(format!("{} Save", icons::FLOPPY_DISK)).color(if can_save {
                        p.ok
                    } else {
                        p.text_dim
                    })),
                )
                .on_hover_text("Apply the SQL shown below (⌘S)")
                .clicked()
                && let Ok(plan) = plan
            {
                action = Some(DesignAction::Save(plan));
            }
            if busy {
                ui.spinner();
            }
            if let Some(i) = &self.info {
                ui.label(RichText::new(i).color(p.ok));
            }
        });
        if let Some(e) = &self.error {
            ui.label(RichText::new(format!("{} {e}", icons::WARNING_CIRCLE)).color(p.danger));
        }
        ui.separator();
        ui.horizontal(|ui| {
            let l = ui.label("Table name");
            let editable = self.existing.is_none();
            ui.add_enabled(
                editable,
                egui::TextEdit::singleline(&mut self.table_name).desired_width(260.0).hint_text("new_table"),
            )
            .labelled_by(l.id);
            if let Some(s) = &self.schema {
                ui.label(RichText::new(format!("in schema {s}")).color(p.text_dim));
            } else {
                ui.label(RichText::new(format!("in {}", self.database)).color(p.text_dim));
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for (sec, label) in
                [(Section::Fields, "Fields"), (Section::Indexes, "Indexes"), (Section::ForeignKeys, "Foreign Keys")]
            {
                ui.selectable_value(&mut self.section, sec, label);
            }
        });

        egui::Panel::bottom(egui::Id::new(("design-sql", tab_id)))
            .resizable(true)
            .default_size(170.0)
            .min_size(60.0)
            .show(ui, |ui| {
                ui.label(RichText::new("SQL Preview").strong());
                egui::ScrollArea::both().id_salt(("design-sql-scroll", tab_id)).auto_shrink([false, false]).show(
                    ui,
                    |ui| match self.plan() {
                        Ok(plan) if plan.statements.is_empty() => {
                            ui.label(RichText::new("No changes.").color(p.text_dim));
                        }
                        Ok(plan) => {
                            let text = plan.statements.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n");
                            let job = super::editor::highlight(
                                ui.ctx(),
                                self.engine,
                                &egui::TextStyle::Monospace.resolve(ui.style()),
                                &text,
                            );
                            ui.add(egui::Label::new(job).selectable(true));
                            for d in &plan.destructive {
                                ui.label(RichText::new(format!("{} {d}", icons::WARNING)).color(p.warn));
                            }
                        }
                        Err(e) => {
                            ui.label(RichText::new(e).color(p.danger));
                        }
                    },
                );
            });

        if self.loading.is_some() {
            ui.spinner();
            return action;
        }
        match self.section {
            Section::Fields => self.fields_ui(ui, tab_id),
            Section::Indexes => self.indexes_ui(ui, tab_id),
            Section::ForeignKeys => self.fks_ui(ui, tab_id),
        }
        action
    }

    fn fields_ui(&mut self, ui: &mut egui::Ui, tab_id: u64) {
        let p = theme::current(ui.ctx());
        let types = ddl::common_types(self.engine);
        let before = self.column_names();
        egui::ScrollArea::vertical().id_salt(("design-fields", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new(("design-grid", tab_id)).num_columns(6).striped(true).spacing([10.0, 6.0]).show(ui, |ui| {
                for h in ["", "Key", "Name", "Type", "Not Null", "Default"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for (i, row) in self.rows.iter_mut().enumerate() {
                    let sel = self.selected == Some(i);
                    if ui
                        .selectable_label(sel, if row.original.is_none() { icons::PLUS } else { icons::DOT_OUTLINE })
                        .on_hover_text("Select field")
                        .clicked()
                    {
                        self.selected = Some(i);
                    }
                    ui.checkbox(&mut row.def.primary_key, "").on_hover_text("Primary key");
                    let name = ui.add(egui::TextEdit::singleline(&mut row.def.name).desired_width(170.0));
                    if name.gained_focus() {
                        self.selected = Some(i);
                    }
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut row.def.data_type)
                                .desired_width(150.0)
                                .font(egui::TextStyle::Monospace),
                        );
                        ui.menu_button(icons::CARET_DOWN, |ui| {
                            for t in types {
                                if ui.button(*t).clicked() {
                                    row.def.data_type = (*t).to_string();
                                    ui.close();
                                }
                            }
                        });
                    });
                    let mut not_null = !row.def.nullable;
                    if ui.checkbox(&mut not_null, "").changed() {
                        row.def.nullable = !not_null;
                    }
                    let mut default = row.def.default.clone().unwrap_or_default();
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut default)
                                .desired_width(160.0)
                                .font(egui::TextStyle::Monospace)
                                .hint_text("SQL expression"),
                        )
                        .changed()
                    {
                        row.def.default = Some(default).filter(|d| !d.trim().is_empty());
                    }
                    ui.end_row();
                }
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Defaults are SQL expressions: strings need quotes ('text'), functions as written (now()).",
                )
                .small()
                .color(p.text_dim),
            );
        });
        self.propagate_renames(&before);
    }

    fn indexes_ui(&mut self, ui: &mut egui::Ui, tab_id: u64) {
        let names = self.column_names();
        if ui.button(format!("{} Add Index", icons::PLUS)).clicked() {
            let n = self.indexes.len() + 1;
            self.indexes.push(IndexDef {
                name: format!("ix_{}_{n}", self.table_name.trim()),
                columns: names.first().cloned().into_iter().collect(),
                ..Default::default()
            });
        }
        let mut remove = None;
        egui::ScrollArea::vertical().id_salt(("design-indexes", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new(("design-ix-grid", tab_id)).num_columns(4).striped(true).spacing([10.0, 6.0]).show(
                ui,
                |ui| {
                    for h in ["Name", "Columns", "Unique", ""] {
                        ui.label(RichText::new(h).strong());
                    }
                    ui.end_row();
                    for (i, ix) in self.indexes.iter_mut().enumerate() {
                        ui.add(egui::TextEdit::singleline(&mut ix.name).desired_width(170.0));
                        columns_menu(ui, egui::Id::new(("ix-cols", tab_id, i)), &mut ix.columns, &names);
                        ui.checkbox(&mut ix.unique, "");
                        if ui.button(icons::TRASH).on_hover_text("Delete index").clicked() {
                            remove = Some(i);
                        }
                        ui.end_row();
                    }
                },
            );
        });
        if let Some(i) = remove {
            self.indexes.remove(i);
        }
    }

    fn fks_ui(&mut self, ui: &mut egui::Ui, tab_id: u64) {
        let names = self.column_names();
        let p = theme::current(ui.ctx());
        if ui.button(format!("{} Add Foreign Key", icons::PLUS)).clicked() {
            let n = self.fks.len() + 1;
            self.fks.push(FkDef {
                name: format!("fk_{}_{n}", self.table_name.trim()),
                columns: names.first().cloned().into_iter().collect(),
                on_update: "NO ACTION".into(),
                on_delete: "NO ACTION".into(),
                ..Default::default()
            });
        }
        if self.engine == Engine::Sqlite && self.existing.is_some() {
            ui.label(RichText::new("SQLite cannot add or drop foreign keys on an existing table.").color(p.text_dim));
        }
        let mut remove = None;
        egui::ScrollArea::both().id_salt(("design-fks", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new(("design-fk-grid", tab_id)).num_columns(7).striped(true).spacing([10.0, 6.0]).show(
                ui,
                |ui| {
                    for h in ["Name", "Columns", "Referenced Table", "Referenced Columns", "On Update", "On Delete", ""]
                    {
                        ui.label(RichText::new(h).strong());
                    }
                    ui.end_row();
                    for (i, fk) in self.fks.iter_mut().enumerate() {
                        ui.add(egui::TextEdit::singleline(&mut fk.name).desired_width(150.0));
                        columns_menu(ui, egui::Id::new(("fk-cols", tab_id, i)), &mut fk.columns, &names);
                        ui.add(
                            egui::TextEdit::singleline(&mut fk.ref_table)
                                .desired_width(150.0)
                                .hint_text("table or schema.table"),
                        );
                        let mut refs = fk.ref_columns.join(", ");
                        if ui
                            .add(egui::TextEdit::singleline(&mut refs).desired_width(150.0).hint_text("col1, col2"))
                            .changed()
                        {
                            fk.ref_columns =
                                refs.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
                        }
                        action_combo(ui, egui::Id::new(("fk-upd", tab_id, i)), &mut fk.on_update);
                        action_combo(ui, egui::Id::new(("fk-del", tab_id, i)), &mut fk.on_delete);
                        if ui.button(icons::TRASH).on_hover_text("Delete foreign key").clicked() {
                            remove = Some(i);
                        }
                        ui.end_row();
                    }
                },
            );
        });
        if let Some(i) = remove {
            self.fks.remove(i);
        }
    }
}

/// Drop-down with a checkbox per table column; keeps the selection in table-column order.
fn columns_menu(ui: &mut egui::Ui, id: egui::Id, selected: &mut Vec<String>, all: &[String]) {
    let label = if selected.is_empty() { "choose…".to_string() } else { selected.join(", ") };
    ui.push_id(id, |ui| {
        ui.menu_button(label, |ui| {
            for c in all {
                let mut on = selected.contains(c);
                if ui.checkbox(&mut on, c).changed() {
                    let mut next: Vec<String> = selected.iter().filter(|x| *x != c).cloned().collect();
                    if on {
                        next.push(c.clone());
                    }
                    next.sort_by_key(|x| all.iter().position(|a| a == x));
                    *selected = next;
                }
            }
        });
    });
}

fn action_combo(ui: &mut egui::Ui, id: egui::Id, value: &mut String) {
    super::widgets::combo(id).selected_text(value.clone()).width(110.0).show_ui(ui, |ui| {
        for a in FK_ACTIONS {
            ui.selectable_value(value, a.to_string(), a);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_table_plan_and_dirty_tracking() {
        let mut t = DesignTab::new_table(Uuid::new_v4(), Engine::Sqlite, "main".into(), None);
        assert!(t.plan().is_err(), "name required");
        t.table_name = "things".into();
        t.add_field();
        let plan = t.plan().unwrap();
        assert!(plan.statements[0].starts_with("CREATE TABLE \"main\".\"things\""));
        assert!(t.is_dirty());
        let obj = t.object();
        let st = TableStructure {
            columns: vec![talus_core::model::ColumnInfo {
                name: "id".into(),
                data_type: "INTEGER".into(),
                nullable: false,
                default: None,
                primary_key: true,
                comment: None,
                extra: None,
            }],
            ..Default::default()
        };
        t.set_structure(obj, &st);
        assert!(!t.is_dirty());
        assert!(t.plan().unwrap().statements.is_empty());
        t.rows[0].def.name = "pk".into();
        assert!(t.is_dirty());
        assert_eq!(
            t.plan().unwrap().statements,
            vec!["ALTER TABLE \"main\".\"things\" RENAME COLUMN \"id\" TO \"pk\""]
        );
    }

    #[test]
    fn indexes_and_foreign_keys_flow_into_the_plan() {
        let mut t = DesignTab::new_table(Uuid::new_v4(), Engine::Sqlite, "main".into(), None);
        t.table_name = "things".into();
        t.add_field();
        t.indexes.push(IndexDef { name: "ix_f".into(), columns: vec!["field2".into()], ..Default::default() });
        let plan = t.plan().unwrap();
        assert_eq!(plan.statements.len(), 2);
        assert!(plan.statements[1].starts_with("CREATE INDEX \"main\".\"ix_f\" ON \"things\""));
        // Renaming a column in the grid carries the index with it.
        let before = t.column_names();
        t.rows[1].def.name = "renamed".into();
        t.propagate_renames(&before);
        assert_eq!(t.indexes[0].columns, ["renamed"]);
        // An index on a missing column is reported, not silently emitted.
        t.indexes[0].columns = vec!["nope".into()];
        assert!(t.plan().unwrap_err().contains("unknown column"));
    }

    #[test]
    fn sqlite_type_change_falls_back_to_table_rebuild() {
        let mut t = DesignTab::new_table(Uuid::new_v4(), Engine::Sqlite, "main".into(), None);
        t.table_name = "things".into();
        let obj = t.object();
        let col = |name: &str, ty: &str, pk: bool| talus_core::model::ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            nullable: !pk,
            default: None,
            primary_key: pk,
            comment: None,
            extra: None,
        };
        let st =
            TableStructure { columns: vec![col("id", "INTEGER", true), col("n", "TEXT", false)], ..Default::default() };
        t.set_structure(obj, &st);
        t.rows[1].def.data_type = "VARCHAR(5)".into();
        let plan = t.plan().unwrap();
        assert_eq!(plan.statements[0], ddl::SQLITE_FK_OFF);
        assert!(plan.statements.iter().any(|s| s.starts_with("DROP TABLE")));
        assert!(!plan.destructive.is_empty());
    }
}
