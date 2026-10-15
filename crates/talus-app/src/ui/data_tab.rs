//! Table data editor: paging, server-side sort/filter, pending edits applied atomically.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use egui::RichText;
use talus_core::model::{
    Cell, ChangeSet, Engine, Filter, FilterOp, ObjectKind, ObjectRef, PageQuery, QueryResult, RowKey, RowUpdate,
    SortDir, TableStructure,
};
use talus_core::sql::Dialect;
use talus_core::sql::dml::{ROWID_ALIAS, key_columns};
use uuid::Uuid;

use super::grid::{self, GridEvent, GridState, GridView, RowMark};
use crate::state::Loadable;
use crate::theme;
use crate::worker::JobId;

pub struct DataTab {
    pub conn: Uuid,
    pub engine: Engine,
    pub obj: ObjectRef,
    pub structure: Loadable<TableStructure>,
    pub page: Option<QueryResult>,
    pub loading: Option<JobId>,
    pub applying: Option<JobId>,
    pub offset: u64,
    pub page_size: u64,
    pub sort: Vec<(String, SortDir)>,
    pub filters: Vec<Filter>,
    pub show_filters: bool,
    /// (row, column) → new value for existing rows.
    pub edits: BTreeMap<(usize, usize), Cell>,
    pub deleted: BTreeSet<usize>,
    /// New rows: column → value (unset columns use the server default).
    pub inserts: Vec<BTreeMap<usize, Cell>>,
    pub grid: GridState,
    pub error: Option<String>,
    pub info: Option<String>,
    pub elapsed: Option<Duration>,
    pub inspector_open: bool,
    /// Inspector edit buffer: cell, text, is-NULL.
    inspector: Option<((usize, usize), String, bool)>,
}

pub enum DataAction {
    Reload,
    Apply(ChangeSet),
}

impl DataTab {
    pub fn new(conn: Uuid, engine: Engine, obj: ObjectRef, page_size: u64) -> Self {
        Self {
            conn,
            engine,
            obj,
            structure: Loadable::NotLoaded,
            page: None,
            loading: None,
            applying: None,
            offset: 0,
            page_size,
            sort: Vec::new(),
            filters: Vec::new(),
            show_filters: false,
            edits: BTreeMap::new(),
            deleted: BTreeSet::new(),
            inserts: Vec::new(),
            grid: GridState::default(),
            error: None,
            info: None,
            elapsed: None,
            inspector_open: false,
            inspector: None,
        }
    }

    pub fn title(&self) -> String {
        let dirty = if self.is_dirty() { " •" } else { "" };
        format!("{}{dirty}", self.obj.display_name())
    }

    pub fn is_dirty(&self) -> bool {
        !self.edits.is_empty() || !self.deleted.is_empty() || !self.inserts.is_empty()
    }

    pub fn discard(&mut self) {
        self.edits.clear();
        self.deleted.clear();
        self.inserts.clear();
        self.grid.editing = None;
    }

    pub fn page_query(&self) -> PageQuery {
        PageQuery {
            table: self.obj.clone(),
            filters: self.filters.clone(),
            sort: self.sort.clone(),
            offset: self.offset,
            limit: self.page_size,
        }
    }

    /// Why the grid is read-only, if it is.
    pub fn read_only_reason(&self) -> Option<&'static str> {
        if self.obj.kind == ObjectKind::View {
            return Some("Views are read-only");
        }
        match self.structure.loaded() {
            None => Some("Loading structure…"),
            Some(s) if key_columns(self.engine, s).is_none() => Some("Read-only: table has no primary key"),
            Some(_) => None,
        }
    }

    fn key_indices(&self) -> Option<Vec<usize>> {
        let s = self.structure.loaded()?;
        let page = self.page.as_ref()?;
        key_columns(self.engine, s)?.iter().map(|k| page.columns.iter().position(|c| c.name == *k)).collect()
    }

    /// Builds the change set from pending edits. Pure; unit-tested.
    pub fn change_set(&self) -> Result<ChangeSet, String> {
        let page = self.page.as_ref().ok_or("no data loaded")?;
        let keys = self.key_indices().ok_or("rows cannot be identified (no primary key)")?;
        let key_of = |r: usize| -> Result<RowKey, String> {
            let row = page.rows.get(r).ok_or("row out of range")?;
            Ok(RowKey(keys.iter().map(|&k| (page.columns[k].name.clone(), row[k].clone())).collect()))
        };
        let mut cs = ChangeSet::default();
        for &r in &self.deleted {
            cs.deletes.push(key_of(r)?);
        }
        let mut by_row: BTreeMap<usize, Vec<(String, Cell)>> = BTreeMap::new();
        for (&(r, c), v) in &self.edits {
            if self.deleted.contains(&r) {
                continue;
            }
            // Unchanged value: skip so we never issue no-op updates.
            if page.rows.get(r).and_then(|row| row.get(c)) == Some(v) {
                continue;
            }
            by_row.entry(r).or_default().push((page.columns[c].name.clone(), v.clone()));
        }
        for (r, values) in by_row {
            cs.updates.push(RowUpdate { key: key_of(r)?, values });
        }
        for row in &self.inserts {
            cs.inserts.push(row.iter().map(|(&c, v)| (page.columns[c].name.clone(), v.clone())).collect());
        }
        Ok(cs)
    }

    fn existing_rows(&self) -> usize {
        self.page.as_ref().map_or(0, |p| p.rows.len())
    }

    fn cell_value(&self, r: usize, c: usize) -> (Option<&str>, RowMark) {
        let n = self.existing_rows();
        if r >= n {
            let v = self.inserts.get(r - n).and_then(|m| m.get(&c));
            return (v.and_then(|x| x.as_deref()), RowMark::Inserted);
        }
        let original = self.page.as_ref().and_then(|p| p.rows[r][c].as_deref());
        if self.deleted.contains(&r) {
            return (original, RowMark::Deleted);
        }
        match self.edits.get(&(r, c)) {
            Some(v) => (v.as_deref(), RowMark::Edited),
            None => (original, RowMark::Normal),
        }
    }

    fn row_mark(&self, r: usize) -> RowMark {
        if r >= self.existing_rows() {
            RowMark::Inserted
        } else if self.deleted.contains(&r) {
            RowMark::Deleted
        } else if self.edits.range((r, 0)..(r + 1, 0)).next().is_some() {
            RowMark::Edited
        } else {
            RowMark::Normal
        }
    }

    fn set_value(&mut self, r: usize, c: usize, v: Cell) {
        let n = self.existing_rows();
        if r >= n {
            if let Some(m) = self.inserts.get_mut(r - n) {
                m.insert(c, v);
            }
        } else if !self.deleted.contains(&r) {
            self.edits.insert((r, c), v);
        }
    }

    fn column_editable(&self, c: usize) -> bool {
        self.page.as_ref().is_some_and(|p| !p.columns[c].binary && p.columns[c].name != ROWID_ALIAS)
    }

    pub fn add_row(&mut self) {
        if self.read_only_reason().is_none() && self.page.is_some() {
            self.inserts.push(BTreeMap::new());
            let r = self.existing_rows() + self.inserts.len() - 1;
            let first =
                self.page.as_ref().and_then(|p| p.columns.iter().position(|c| c.name != ROWID_ALIAS)).unwrap_or(0);
            self.grid.selected = Some((r, first));
        }
    }

    pub fn delete_selected(&mut self) {
        if self.read_only_reason().is_some() {
            return;
        }
        if let Some((r, _)) = self.grid.selected {
            self.delete_row(r);
        }
    }

    fn delete_row(&mut self, r: usize) {
        let n = self.existing_rows();
        if r >= n {
            if r - n < self.inserts.len() {
                self.inserts.remove(r - n);
            }
        } else if !self.deleted.remove(&r) {
            self.deleted.insert(r);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64) -> Vec<DataAction> {
        let mut actions = Vec::new();
        let p = theme::current(ui.ctx());
        let busy = self.loading.is_some() || self.applying.is_some();
        let ro = self.read_only_reason();
        let dirty = self.is_dirty();
        if dirty {
            // A previous "Saved" message no longer describes the grid.
            self.info = None;
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy, egui::Button::new(format!("{} Refresh", egui_phosphor::regular::ARROW_CLOCKWISE)))
                .clicked()
            {
                if dirty {
                    self.error = Some("Apply or discard pending changes before refreshing.".into());
                } else {
                    actions.push(DataAction::Reload);
                }
            }
            ui.separator();
            if ui
                .add_enabled(ro.is_none() && !busy, egui::Button::new(format!("{} Add", egui_phosphor::regular::PLUS)))
                .on_hover_text("Add record (⌘+)")
                .clicked()
            {
                self.add_row();
            }
            if ui
                .add_enabled(
                    ro.is_none() && !busy && self.grid.selected.is_some(),
                    egui::Button::new(format!("{} Delete", egui_phosphor::regular::MINUS)),
                )
                .on_hover_text("Mark selected record for deletion (⌘−)")
                .clicked()
            {
                self.delete_selected();
            }
            // Pending changes make Apply the screen's primary action (filled black).
            let label = format!("{} Apply", egui_phosphor::regular::CHECK);
            let apply = if dirty { theme::primary_button(p, label) } else { egui::Button::new(label) };
            if ui.add_enabled(dirty && !busy, apply).on_hover_text("Apply changes in one transaction (⌘↩)").clicked()
            {
                match self.change_set() {
                    Ok(cs) => actions.push(DataAction::Apply(cs)),
                    Err(e) => self.error = Some(e),
                }
            }
            if ui
                .add_enabled(dirty && !busy, egui::Button::new(format!("{} Discard", egui_phosphor::regular::X)))
                .on_hover_text("Discard changes (⌘⎋)")
                .clicked()
            {
                self.discard();
            }
            ui.separator();
            let ftxt = if self.filters.iter().any(|f| f.enabled) {
                RichText::new(format!("{} Filter ●", egui_phosphor::regular::FUNNEL)).color(p.accent)
            } else {
                RichText::new(format!("{} Filter", egui_phosphor::regular::FUNNEL))
            };
            if ui.selectable_label(self.show_filters, ftxt).clicked() {
                self.show_filters = !self.show_filters;
            }
            if ui
                .selectable_label(self.inspector_open, format!("{} Inspector", egui_phosphor::regular::SIDEBAR_SIMPLE))
                .on_hover_text("Show the full value of the selected cell")
                .clicked()
            {
                self.inspector_open = !self.inspector_open;
            }
            if !self.sort.is_empty() && ui.button("Clear Sort").clicked() && !dirty {
                self.sort.clear();
                self.offset = 0;
                actions.push(DataAction::Reload);
            }
            if busy {
                ui.spinner();
            }
            if let Some(r) = ro {
                ui.label(RichText::new(r).color(p.warn));
            }
        });
        if self.show_filters {
            self.filter_ui(ui, tab_id, &mut actions, dirty);
        }
        if let Some(e) = self.error.clone() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} {e}", egui_phosphor::regular::WARNING_CIRCLE)).color(p.danger));
                if ui.small_button("Dismiss").clicked() {
                    self.error = None;
                }
            });
        }

        egui::Panel::bottom(egui::Id::new(("data-footer", tab_id))).show(ui, |ui| {
            ui.horizontal(|ui| {
                let rows = self.existing_rows();
                let page_no = self.offset / self.page_size.max(1) + 1;
                let can_prev = self.offset > 0 && !busy;
                let can_next = rows as u64 >= self.page_size && !busy;
                let nav = |ui: &mut egui::Ui, enabled: bool, icon: &str, tip: &str| {
                    ui.add_enabled(enabled, egui::Button::new(icon).small()).on_hover_text(tip).clicked()
                };
                if nav(ui, can_prev, egui_phosphor::regular::CARET_DOUBLE_LEFT, "First page") {
                    self.goto(0, dirty, &mut actions);
                }
                if nav(ui, can_prev, egui_phosphor::regular::CARET_LEFT, "Previous page") {
                    self.goto(self.offset.saturating_sub(self.page_size), dirty, &mut actions);
                }
                ui.label(format!("Page {page_no}"));
                if nav(ui, can_next, egui_phosphor::regular::CARET_RIGHT, "Next page") {
                    self.goto(self.offset + self.page_size, dirty, &mut actions);
                }
                ui.separator();
                ui.label(
                    RichText::new(format!(
                        "Records {}–{}",
                        if rows == 0 { 0 } else { self.offset + 1 },
                        self.offset + rows as u64
                    ))
                    .color(p.text_dim),
                );
                if let Some(e) = self.elapsed {
                    ui.label(RichText::new(format!("· {:.1} ms", e.as_secs_f64() * 1000.0)).color(p.text_dim));
                }
                if let Some(i) = &self.info {
                    ui.label(RichText::new(i).color(p.ok));
                }
                let pending = self.edits.keys().map(|k| k.0).collect::<BTreeSet<_>>().len()
                    + self.deleted.len()
                    + self.inserts.len();
                if pending > 0 {
                    ui.label(RichText::new(format!("{pending} pending change(s)")).color(p.warn));
                }
            });
        });

        let Some(page) = self.page.as_ref() else {
            ui.centered_and_justified(|ui| {
                if self.loading.is_some() || self.structure.is_loading() {
                    ui.spinner();
                } else {
                    ui.label(RichText::new("No data loaded").color(p.text_dim));
                }
            });
            return actions;
        };
        let visible: Vec<usize> =
            page.columns.iter().enumerate().filter(|(_, c)| c.name != ROWID_ALIAS).map(|(i, _)| i).collect();
        if self.inspector_open {
            self.inspector_ui(ui, tab_id, ro.is_none() && !busy);
        }
        let Some(page) = self.page.as_ref() else { return actions };
        let columns = page.columns.clone();
        let rows = self.existing_rows() + self.inserts.len();
        let editable = ro.is_none() && !busy;
        let sort = self.sort.clone();
        let view = GridView {
            id: egui::Id::new(("data-grid", tab_id)),
            columns: &columns,
            visible: &visible,
            rows,
            first_row_number: self.offset + 1,
            sort: &sort,
            editable,
            can_filter: true,
            row_mark: None,
        };
        let mut grid_state = std::mem::take(&mut self.grid);
        let events = {
            let this = &*self;
            let cell = |r: usize, c: usize| this.cell_value(r, c);
            let row_mark = |r: usize| this.row_mark(r);
            let view = GridView { row_mark: Some(&row_mark), ..view };
            grid::show(ui, &view, &mut grid_state, &cell)
        };
        self.grid = grid_state;
        for ev in events {
            match ev {
                GridEvent::HeaderClicked(c) => {
                    if dirty {
                        self.error = Some("Apply or discard pending changes before sorting.".into());
                        continue;
                    }
                    let name = columns[c].name.clone();
                    let next = match self.sort.iter().find(|(n, _)| *n == name).map(|(_, d)| *d) {
                        None => Some(SortDir::Asc),
                        Some(SortDir::Asc) => Some(SortDir::Desc),
                        Some(SortDir::Desc) => None,
                    };
                    self.sort.clear();
                    if let Some(d) = next {
                        self.sort.push((name, d));
                    }
                    self.offset = 0;
                    actions.push(DataAction::Reload);
                }
                GridEvent::Activate(r, c) if editable && self.column_editable(c) && !self.deleted.contains(&r) => {
                    let current = self.cell_value(r, c).0.map(str::to_string);
                    self.grid.begin_edit(r, c, current.as_deref());
                }
                GridEvent::Activate(..) => {}
                GridEvent::Commit { row, col, value } if editable && self.column_editable(col) => {
                    let unchanged = row < self.existing_rows()
                        && self.page.as_ref().and_then(|p| p.rows[row].get(col)) == Some(&value)
                        && !self.edits.contains_key(&(row, col));
                    if !unchanged {
                        self.set_value(row, col, value);
                    }
                }
                GridEvent::SetNull(r, c) if editable && self.column_editable(c) => self.set_value(r, c, None),
                GridEvent::DeleteRow(r) if editable => self.delete_row(r),
                GridEvent::CopyAsInsert(r) => {
                    if let Some(sql) = self.row_insert_sql(r) {
                        ui.ctx().copy_text(sql);
                        self.info = Some("INSERT statement copied".into());
                    }
                }
                GridEvent::FilterByValue(r, c) => {
                    if dirty {
                        self.error = Some("Apply or discard pending changes before filtering.".into());
                        continue;
                    }
                    let (v, _) = self.cell_value(r, c);
                    let (op, value) = match v {
                        None => (FilterOp::IsNull, String::new()),
                        Some(s) => (FilterOp::Eq, s.to_string()),
                    };
                    self.filters.push(Filter { enabled: true, column: columns[c].name.clone(), op, value });
                    self.show_filters = true;
                    self.offset = 0;
                    actions.push(DataAction::Reload);
                }
                _ => {}
            }
        }
        actions
    }

    /// `INSERT` for one displayed row (pending edits included), with literal values.
    pub fn row_insert_sql(&self, r: usize) -> Option<String> {
        let page = self.page.as_ref()?;
        let d = Dialect(self.engine);
        let target = d.qualified(&self.obj).ok()?;
        let mut cols = Vec::new();
        let mut vals = Vec::new();
        for (c, meta) in page.columns.iter().enumerate().filter(|(_, m)| m.name != ROWID_ALIAS) {
            cols.push(d.quote_ident(&meta.name).ok()?);
            let (v, _) = self.cell_value(r, c);
            vals.push(match v {
                None => "NULL".to_string(),
                Some(hex) if meta.binary && hex.starts_with("0x") => match self.engine {
                    Engine::Postgres => format!("'\\x{}'::bytea", &hex[2..]),
                    Engine::SqlServer => hex.to_string(),
                    Engine::Oracle => format!("HEXTORAW('{}')", &hex[2..]),
                    Engine::Snowflake => format!("TO_BINARY('{}', 'HEX')", &hex[2..]),
                    _ => format!("X'{}'", &hex[2..]),
                },
                Some(v) => d.string_literal(v),
            });
        }
        Some(format!("INSERT INTO {target} ({}) VALUES ({});", cols.join(", "), vals.join(", ")))
    }

    fn inspector_ui(&mut self, ui: &mut egui::Ui, tab_id: u64, editable: bool) {
        let Some((r, c)) = self.grid.selected else {
            egui::Panel::right(egui::Id::new(("data-inspector", tab_id))).resizable(true).default_size(260.0).show(
                ui,
                |ui| {
                    ui.label(RichText::new("Select a cell to inspect it.").color(theme::current(ui.ctx()).text_dim));
                },
            );
            return;
        };
        let Some(meta) = self.page.as_ref().and_then(|p| p.columns.get(c)).cloned() else { return };
        let current = self.cell_value(r, c).0.map(str::to_string);
        if self.inspector.as_ref().map(|(pos, _, _)| *pos) != Some((r, c)) {
            let text = current
                .as_deref()
                .map(|v| super::grid::pretty_json(v).unwrap_or_else(|| v.to_string()))
                .unwrap_or_default();
            self.inspector = Some(((r, c), text, current.is_none()));
        }
        let can_edit = editable && self.column_editable(c) && !self.deleted.contains(&r);
        let mut apply = None;
        egui::Panel::right(egui::Id::new(("data-inspector", tab_id))).resizable(true).default_size(280.0).show(
            ui,
            |ui| {
                let p = theme::current(ui.ctx());
                ui.label(RichText::new(&meta.name).strong());
                if !meta.type_name.is_empty() {
                    ui.label(RichText::new(&meta.type_name).small().color(p.text_dim));
                }
                ui.separator();
                let Some((_, text, is_null)) = self.inspector.as_mut() else { return };
                ui.add_enabled(can_edit, egui::Checkbox::new(is_null, "NULL"));
                if !*is_null {
                    ui.label(RichText::new(format!("{} characters", text.chars().count())).small().color(p.text_dim));
                }
                egui::ScrollArea::both().id_salt(("inspector", tab_id)).max_height(ui.available_height() - 36.0).show(
                    ui,
                    |ui| {
                        ui.add_enabled(
                            can_edit && !*is_null,
                            egui::TextEdit::multiline(text)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .desired_rows(12),
                        );
                    },
                );
                if can_edit && ui.button(format!("{} Apply to Cell", egui_phosphor::regular::CHECK)).clicked() {
                    apply = Some(if *is_null { None } else { Some(text.clone()) });
                }
            },
        );
        if let Some(v) = apply {
            if current != v {
                self.set_value(r, c, v);
            }
            self.inspector = None;
        }
    }

    fn goto(&mut self, offset: u64, dirty: bool, actions: &mut Vec<DataAction>) {
        if dirty {
            self.error = Some("Apply or discard pending changes before changing page.".into());
            return;
        }
        self.offset = offset;
        actions.push(DataAction::Reload);
    }

    fn filter_ui(&mut self, ui: &mut egui::Ui, tab_id: u64, actions: &mut Vec<DataAction>, dirty: bool) {
        let cols: Vec<String> =
            self.structure.loaded().map(|s| s.columns.iter().map(|c| c.name.clone()).collect()).unwrap_or_default();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            let mut remove = None;
            for (i, f) in self.filters.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut f.enabled, "");
                    super::widgets::combo(("fcol", tab_id, i)).selected_text(&f.column).width(140.0).show_ui(
                        ui,
                        |ui| {
                            for c in &cols {
                                ui.selectable_value(&mut f.column, c.clone(), c);
                            }
                        },
                    );
                    super::widgets::combo(("fop", tab_id, i)).selected_text(f.op.label()).width(120.0).show_ui(
                        ui,
                        |ui| {
                            for op in FilterOp::ALL {
                                ui.selectable_value(&mut f.op, op, op.label());
                            }
                        },
                    );
                    if f.op.takes_value() {
                        ui.add(egui::TextEdit::singleline(&mut f.value).desired_width(200.0).hint_text("value"));
                    }
                    if ui.small_button(egui_phosphor::regular::X).on_hover_text("Remove condition").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                self.filters.remove(i);
            }
            ui.horizontal(|ui| {
                if ui.button(format!("{} Add Condition", egui_phosphor::regular::PLUS)).clicked()
                    && let Some(c) = cols.first()
                {
                    self.filters.push(Filter {
                        enabled: true,
                        column: c.clone(),
                        op: FilterOp::Eq,
                        value: String::new(),
                    });
                }
                if ui.button("Apply Filter").clicked() {
                    if dirty {
                        self.error = Some("Apply or discard pending changes before filtering.".into());
                    } else {
                        self.offset = 0;
                        actions.push(DataAction::Reload);
                    }
                }
                if !self.filters.is_empty() && ui.button("Clear").clicked() && !dirty {
                    self.filters.clear();
                    self.offset = 0;
                    actions.push(DataAction::Reload);
                }
                ui.label(RichText::new("All conditions are combined with AND; values are sent as parameters.").small());
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use talus_core::model::{ColumnInfo, ColumnMeta};

    fn tab() -> DataTab {
        let obj = ObjectRef { database: "main".into(), schema: None, name: "t".into(), kind: ObjectKind::Table };
        let mut t = DataTab::new(Uuid::new_v4(), Engine::Sqlite, obj, 100);
        let col = |n: &str, pk: bool| ColumnInfo {
            name: n.into(),
            data_type: "TEXT".into(),
            nullable: !pk,
            default: None,
            primary_key: pk,
            comment: None,
            extra: None,
        };
        t.structure = Loadable::Loaded(TableStructure {
            columns: vec![col("id", true), col("name", false)],
            ..Default::default()
        });
        let meta = |n: &str| ColumnMeta { name: n.into(), type_name: String::new(), binary: false };
        t.page = Some(QueryResult {
            columns: vec![meta("id"), meta("name")],
            rows: vec![vec![Some("1".into()), Some("a".into())], vec![Some("2".into()), Some("b".into())]],
            truncated: false,
        });
        t
    }

    #[test]
    fn builds_change_set_and_skips_noops() {
        let mut t = tab();
        t.edits.insert((0, 1), Some("A".into()));
        t.edits.insert((1, 1), Some("b".into())); // unchanged → skipped
        t.deleted.insert(1);
        t.inserts.push(BTreeMap::from([(1, Some("new".into()))]));
        let cs = t.change_set().unwrap();
        assert_eq!(cs.updates.len(), 1);
        assert_eq!(cs.updates[0].key, RowKey(vec![("id".into(), Some("1".into()))]));
        assert_eq!(cs.updates[0].values, vec![("name".into(), Some("A".into()))]);
        assert_eq!(cs.deletes, vec![RowKey(vec![("id".into(), Some("2".into()))])]);
        assert_eq!(cs.inserts, vec![vec![("name".into(), Some("new".into()))]]);
    }

    #[test]
    fn edits_on_deleted_rows_are_ignored_and_delete_toggles() {
        let mut t = tab();
        t.edits.insert((0, 1), Some("X".into()));
        t.delete_row(0);
        assert!(t.change_set().unwrap().updates.is_empty());
        t.delete_row(0);
        assert!(t.deleted.is_empty());
    }

    #[test]
    fn copy_row_as_insert_uses_pending_values() {
        let mut t = tab();
        t.edits.insert((1, 1), None);
        assert_eq!(t.row_insert_sql(1).unwrap(), "INSERT INTO \"main\".\"t\" (\"id\", \"name\") VALUES ('2', NULL);");
        assert_eq!(t.row_insert_sql(0).unwrap(), "INSERT INTO \"main\".\"t\" (\"id\", \"name\") VALUES ('1', 'a');");
    }

    #[test]
    fn views_and_keyless_tables_are_read_only() {
        let mut t = tab();
        assert!(t.read_only_reason().is_none());
        t.obj.kind = ObjectKind::View;
        assert!(t.read_only_reason().is_some());
        let mut t = tab();
        t.engine = Engine::Postgres;
        if let Loadable::Loaded(s) = &mut t.structure {
            s.columns[0].primary_key = false;
        }
        assert_eq!(t.read_only_reason(), Some("Read-only: table has no primary key"));
    }
}
