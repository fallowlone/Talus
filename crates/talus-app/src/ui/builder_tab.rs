//! Visual query builder: pick tables, joins (suggested from foreign keys), columns, conditions
//! and ordering; the SELECT is generated live and can be opened in a query tab.

use egui::RichText;
use egui_phosphor::regular as icons;
use talus_core::model::{Engine, ObjectRef, SortDir, TableStructure};
use talus_core::sql::Dialect;
use talus_core::sql::builder::{ColRef, Condition, Join, JoinKind, Op, QuerySpec, TableRef, build};
use uuid::Uuid;

use super::diagram_tab::ref_name;
use crate::state::Loadable;
use crate::theme;

pub type Model = Vec<(ObjectRef, TableStructure)>;

/// A column of the table in `slot` (0 = base table, n = n-th join).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SlotCol {
    pub slot: usize,
    pub column: String,
}

#[derive(Debug, Clone)]
pub struct JoinRow {
    pub kind: JoinKind,
    pub table: usize,
    pub left: SlotCol,
    /// Column of the joined table.
    pub right: String,
}

pub struct CondRow {
    pub col: SlotCol,
    pub op: Op,
    pub value: String,
}

pub struct BuilderTab {
    pub conn: Uuid,
    pub engine: Engine,
    pub database: String,
    pub schema: Option<String>,
    pub model: Loadable<Model>,
    base: Option<usize>,
    joins: Vec<JoinRow>,
    columns: Vec<SlotCol>,
    conds: Vec<CondRow>,
    order: Vec<(SlotCol, SortDir)>,
    limit: String,
    distinct: bool,
}

pub enum BuilderAction {
    Reload,
    OpenInQuery(String),
    Copy(String),
}

/// Aliases are the table names, numbered when a table appears more than once.
fn aliases(model: &Model, base: usize, joins: &[JoinRow]) -> Vec<String> {
    let names: Vec<&str> =
        std::iter::once(base).chain(joins.iter().map(|j| j.table)).map(|i| model[i].0.name.as_str()).collect();
    names
        .iter()
        .enumerate()
        .map(
            |(i, n)| {
                if names.iter().filter(|x| *x == n).count() > 1 { format!("{n}_{}", i + 1) } else { (*n).to_string() }
            },
        )
        .collect()
}

/// A join that follows a foreign key between a table already in the query and a new one.
pub fn suggest_join(model: &Model, base: usize, joins: &[JoinRow]) -> Option<JoinRow> {
    let used: Vec<usize> = std::iter::once(base).chain(joins.iter().map(|j| j.table)).collect();
    for (slot, &ti) in used.iter().enumerate() {
        let (obj, st) = &model[ti];
        for fk in &st.foreign_keys {
            let target = ref_name(&fk.ref_table);
            if let Some(ni) = model.iter().position(|(o, _)| o.name == target)
                && !used.contains(&ni)
                && let (Some(l), Some(r)) = (fk.columns.first(), fk.ref_columns.first())
            {
                return Some(JoinRow {
                    kind: JoinKind::Inner,
                    table: ni,
                    left: SlotCol { slot, column: l.clone() },
                    right: r.clone(),
                });
            }
        }
        for (ci, (co, cst)) in model.iter().enumerate() {
            if used.contains(&ci) {
                continue;
            }
            for fk in cst.foreign_keys.iter().filter(|f| ref_name(&f.ref_table) == obj.name) {
                if let (Some(l), Some(r)) = (fk.ref_columns.first(), fk.columns.first()) {
                    let _ = co;
                    return Some(JoinRow {
                        kind: JoinKind::Inner,
                        table: ci,
                        left: SlotCol { slot, column: l.clone() },
                        right: r.clone(),
                    });
                }
            }
        }
    }
    None
}

/// The query for the current choices, or why there is none yet.
#[allow(clippy::too_many_arguments)] // mirrors the tab fields one to one; a parameter struct would only restate them
pub fn spec_sql(
    engine: Engine,
    model: &Model,
    base: usize,
    joins: &[JoinRow],
    columns: &[SlotCol],
    conds: &[CondRow],
    order: &[(SlotCol, SortDir)],
    limit: &str,
    distinct: bool,
) -> Result<String, String> {
    let al = aliases(model, base, joins);
    let cref = |c: &SlotCol| -> Result<ColRef, String> {
        al.get(c.slot)
            .map(|a| ColRef { alias: a.clone(), column: c.column.clone() })
            .ok_or_else(|| "a column refers to a table that is no longer in the query".to_string())
    };
    let tref = |i: usize, a: &str| TableRef { obj: model[i].0.clone(), alias: a.to_string() };
    let limit = match limit.trim() {
        "" => None,
        t => Some(t.parse::<u64>().map_err(|_| format!("limit must be a whole number, got {t:?}"))?),
    };
    let spec = QuerySpec {
        base: tref(base, &al[0]),
        joins: joins
            .iter()
            .enumerate()
            .map(|(i, j)| {
                Ok(Join {
                    kind: j.kind,
                    table: tref(j.table, &al[i + 1]),
                    left: cref(&j.left)?,
                    right: ColRef { alias: al[i + 1].clone(), column: j.right.clone() },
                })
            })
            .collect::<Result<_, String>>()?,
        columns: columns.iter().map(cref).collect::<Result<_, _>>()?,
        conditions: conds
            .iter()
            .map(|c| Ok(Condition { col: cref(&c.col)?, op: c.op, value: c.value.clone() }))
            .collect::<Result<_, String>>()?,
        order: order.iter().map(|(c, d)| Ok((cref(c)?, *d))).collect::<Result<_, String>>()?,
        limit,
        distinct,
    };
    build(Dialect(engine), &spec).map_err(|e| e.to_string())
}

impl BuilderTab {
    pub fn new(conn: Uuid, engine: Engine, database: String, schema: Option<String>) -> Self {
        Self {
            conn,
            engine,
            database,
            schema,
            model: Loadable::NotLoaded,
            base: None,
            joins: Vec::new(),
            columns: Vec::new(),
            conds: Vec::new(),
            order: Vec::new(),
            limit: "1000".into(),
            distinct: false,
        }
    }

    pub fn title(&self) -> String {
        format!("Query Builder — {}", self.schema.as_deref().unwrap_or(&self.database))
    }

    pub fn set_model(&mut self, model: Model) {
        self.base = (!model.is_empty()).then_some(0);
        self.joins.clear();
        self.columns.clear();
        self.conds.clear();
        self.order.clear();
        self.model = Loadable::Loaded(model);
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64) -> Vec<BuilderAction> {
        let p = theme::current(ui.ctx());
        let mut out = Vec::new();
        if matches!(self.model, Loadable::NotLoaded) {
            out.push(BuilderAction::Reload);
        }
        let model = match &self.model {
            Loadable::Loaded(m) if !m.is_empty() => m.clone(),
            Loadable::Loaded(_) => {
                ui.label(RichText::new("No tables in this schema.").color(p.text_dim));
                return out;
            }
            Loadable::Failed(e) => {
                ui.label(RichText::new(e).color(p.danger));
                return out;
            }
            _ => {
                ui.spinner();
                return out;
            }
        };
        let Some(base) = self.base.filter(|b| *b < model.len()) else { return out };
        let engine = self.engine;
        let sql = spec_sql(
            engine,
            &model,
            base,
            &self.joins,
            &self.columns,
            &self.conds,
            &self.order,
            &self.limit,
            self.distinct,
        );
        egui::Panel::bottom(egui::Id::new(("qb-sql", tab_id))).resizable(true).default_size(170.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("SQL").strong());
                if let Ok(s) = &sql {
                    if ui.button(format!("{} Copy", icons::COPY)).clicked() {
                        out.push(BuilderAction::Copy(s.clone()));
                    }
                    if ui.button(format!("{} Open in Query", icons::FILE_SQL)).clicked() {
                        out.push(BuilderAction::OpenInQuery(s.clone()));
                    }
                }
            });
            egui::ScrollArea::both().id_salt(("qb-sql-scroll", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
                match &sql {
                    Ok(s) => {
                        let job = super::editor::highlight(
                            ui.ctx(),
                            engine,
                            &egui::TextStyle::Monospace.resolve(ui.style()),
                            s,
                        );
                        ui.add(egui::Label::new(job).selectable(true));
                    }
                    Err(e) => {
                        ui.label(RichText::new(e).color(p.danger));
                    }
                }
            });
        });
        let al = aliases(&model, base, &self.joins);
        // (alias, table index) of every slot, for the column pickers.
        let slots: Vec<(String, usize)> =
            al.iter().cloned().zip(std::iter::once(base).chain(self.joins.iter().map(|j| j.table))).collect();
        egui::ScrollArea::vertical().id_salt(("qb-main", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
            ui.label(RichText::new("From").strong());
            let mut new_base = base;
            table_combo(ui, egui::Id::new(("qb-base", tab_id)), &model, &mut new_base);
            if new_base != base {
                self.base = Some(new_base);
                self.joins.clear();
                self.columns.clear();
                self.conds.clear();
                self.order.clear();
            }
            ui.add_space(6.0);
            ui.label(RichText::new("Joins").strong());
            let mut remove_join = None;
            for (i, j) in self.joins.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    super::widgets::combo(("qb-jk", tab_id, i))
                        .selected_text(if j.kind == JoinKind::Inner { "INNER" } else { "LEFT" })
                        .width(70.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut j.kind, JoinKind::Inner, "INNER");
                            ui.selectable_value(&mut j.kind, JoinKind::Left, "LEFT");
                        });
                    let before = j.table;
                    table_combo(ui, egui::Id::new(("qb-jt", tab_id, i)), &model, &mut j.table);
                    if before != j.table {
                        j.right = model[j.table].1.columns.first().map(|c| c.name.clone()).unwrap_or_default();
                    }
                    ui.label("ON");
                    slot_col_combo(ui, egui::Id::new(("qb-jl", tab_id, i)), &model, &slots[..=i], &mut j.left);
                    ui.label("=");
                    let own = &model[j.table].1;
                    super::widgets::combo(("qb-jr", tab_id, i))
                        .selected_text(format!("{}.{}", slots[i + 1].0, j.right))
                        .show_ui(ui, |ui| {
                            for c in &own.columns {
                                ui.selectable_value(&mut j.right, c.name.clone(), &c.name);
                            }
                        });
                    if ui.button(icons::TRASH).clicked() {
                        remove_join = Some(i);
                    }
                });
            }
            if let Some(i) = remove_join {
                self.joins.remove(i);
                // Slots after the removed one shift; picks that used it are dropped.
                let fix = |c: &SlotCol| {
                    (c.slot != i + 1)
                        .then(|| SlotCol { slot: if c.slot > i + 1 { c.slot - 1 } else { c.slot }, ..c.clone() })
                };
                self.columns = self.columns.iter().filter_map(fix).collect();
                self.conds.retain(|c| c.col.slot != i + 1);
                for c in &mut self.conds {
                    if c.col.slot > i + 1 {
                        c.col.slot -= 1;
                    }
                }
                self.order = self.order.iter().filter_map(|(c, d)| fix(c).map(|c| (c, *d))).collect();
                for j in &mut self.joins[i..] {
                    if j.left.slot > i + 1 {
                        j.left.slot -= 1;
                    }
                }
            }
            ui.horizontal(|ui| {
                let suggestion = suggest_join(&model, base, &self.joins);
                if ui
                    .add_enabled(
                        suggestion.is_some(),
                        egui::Button::new(format!("{} Join (from foreign key)", icons::PLUS)),
                    )
                    .clicked()
                    && let Some(s) = suggestion
                {
                    self.joins.push(s);
                }
                if ui.button(format!("{} Join (manual)", icons::PLUS)).clicked() {
                    let ti = (base + 1) % model.len();
                    self.joins.push(JoinRow {
                        kind: JoinKind::Inner,
                        table: ti,
                        left: SlotCol {
                            slot: 0,
                            column: model[base].1.columns.first().map(|c| c.name.clone()).unwrap_or_default(),
                        },
                        right: model[ti].1.columns.first().map(|c| c.name.clone()).unwrap_or_default(),
                    });
                }
            });
            ui.add_space(6.0);
            ui.label(RichText::new("Columns (none selected = all)").strong());
            for (slot, (alias, ti)) in slots.iter().enumerate() {
                egui::CollapsingHeader::new(alias).id_salt(("qb-cols", tab_id, slot)).default_open(slot == 0).show(
                    ui,
                    |ui| {
                        for c in &model[*ti].1.columns {
                            let pick = SlotCol { slot, column: c.name.clone() };
                            let mut on = self.columns.contains(&pick);
                            if crate::ui::widgets::check(ui, &mut on, &c.name).changed() {
                                if on {
                                    self.columns.push(pick);
                                } else {
                                    self.columns.retain(|x| *x != pick);
                                }
                            }
                        }
                    },
                );
            }
            ui.add_space(6.0);
            ui.label(RichText::new("Conditions (all must match)").strong());
            let mut remove_cond = None;
            for (i, c) in self.conds.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    slot_col_combo(ui, egui::Id::new(("qb-cc", tab_id, i)), &model, &slots, &mut c.col);
                    super::widgets::combo(("qb-co", tab_id, i)).selected_text(c.op.label()).width(90.0).show_ui(
                        ui,
                        |ui| {
                            for op in Op::ALL {
                                ui.selectable_value(&mut c.op, op, op.label());
                            }
                        },
                    );
                    if c.op.needs_value() {
                        let hint = if c.op == Op::In { "a, b, c" } else { "value" };
                        ui.add(crate::ui::widgets::field(&mut c.value).desired_width(180.0).hint_text(hint));
                    }
                    if ui.button(icons::TRASH).clicked() {
                        remove_cond = Some(i);
                    }
                });
            }
            if let Some(i) = remove_cond {
                self.conds.remove(i);
            }
            if ui.button(format!("{} Condition", icons::PLUS)).clicked() {
                self.conds.push(CondRow {
                    col: SlotCol {
                        slot: 0,
                        column: model[base].1.columns.first().map(|c| c.name.clone()).unwrap_or_default(),
                    },
                    op: Op::Eq,
                    value: String::new(),
                });
            }
            ui.add_space(6.0);
            ui.label(RichText::new("Order by").strong());
            let mut remove_order = None;
            for (i, (c, dir)) in self.order.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    slot_col_combo(ui, egui::Id::new(("qb-oc", tab_id, i)), &model, &slots, c);
                    ui.selectable_value(dir, SortDir::Asc, "ASC");
                    ui.selectable_value(dir, SortDir::Desc, "DESC");
                    if ui.button(icons::TRASH).clicked() {
                        remove_order = Some(i);
                    }
                });
            }
            if let Some(i) = remove_order {
                self.order.remove(i);
            }
            if ui.button(format!("{} Order", icons::PLUS)).clicked() {
                self.order.push((
                    SlotCol {
                        slot: 0,
                        column: model[base].1.columns.first().map(|c| c.name.clone()).unwrap_or_default(),
                    },
                    SortDir::Asc,
                ));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let l = ui.label("Limit");
                ui.add(crate::ui::widgets::field(&mut self.limit).desired_width(80.0)).labelled_by(l.id);
                crate::ui::widgets::check(ui, &mut self.distinct, "DISTINCT");
            });
        });
        out
    }
}

fn table_combo(ui: &mut egui::Ui, id: egui::Id, model: &Model, value: &mut usize) {
    super::widgets::combo(id).selected_text(model[*value].0.name.clone()).width(180.0).show_ui(ui, |ui| {
        for (i, (o, _)) in model.iter().enumerate() {
            ui.selectable_value(value, i, &o.name);
        }
    });
}

fn slot_col_combo(ui: &mut egui::Ui, id: egui::Id, model: &Model, slots: &[(String, usize)], value: &mut SlotCol) {
    let shown = slots.get(value.slot).map_or("?".to_string(), |(a, _)| format!("{a}.{}", value.column));
    super::widgets::combo(id).selected_text(shown).width(200.0).show_ui(ui, |ui| {
        for (slot, (alias, ti)) in slots.iter().enumerate() {
            for c in &model[*ti].1.columns {
                let pick = SlotCol { slot, column: c.name.clone() };
                if ui.selectable_label(*value == pick, format!("{alias}.{}", c.name)).clicked() {
                    *value = pick;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use talus_core::model::{ColumnInfo, ForeignKeyInfo, ObjectKind};

    fn table(name: &str, cols: &[&str], fk: Option<(&str, &str, &str)>) -> (ObjectRef, TableStructure) {
        let columns = cols
            .iter()
            .map(|n| ColumnInfo {
                name: (*n).into(),
                data_type: "int".into(),
                nullable: true,
                default: None,
                primary_key: *n == "id",
                comment: None,
                extra: None,
            })
            .collect();
        let foreign_keys = fk
            .map(|(col, target, rc)| ForeignKeyInfo {
                name: "fk".into(),
                columns: vec![col.into()],
                ref_table: target.into(),
                ref_columns: vec![rc.into()],
                on_update: "NO ACTION".into(),
                on_delete: "NO ACTION".into(),
            })
            .into_iter()
            .collect();
        (
            ObjectRef { database: "main".into(), schema: None, name: name.into(), kind: ObjectKind::Table },
            TableStructure { columns, foreign_keys, ..Default::default() },
        )
    }

    fn model() -> Model {
        vec![
            table("orders", &["id", "cid"], Some(("cid", "customers", "id"))),
            table("customers", &["id", "name"], None),
            table("items", &["id", "oid"], Some(("oid", "orders", "id"))),
        ]
    }

    #[test]
    fn suggests_joins_in_both_directions_until_exhausted() {
        let m = model();
        let j1 = suggest_join(&m, 0, &[]).unwrap();
        assert_eq!((j1.table, j1.left.column.as_str(), j1.right.as_str()), (1, "cid", "id"));
        let j2 = suggest_join(&m, 0, std::slice::from_ref(&j1)).unwrap();
        assert_eq!((j2.table, j2.left.column.as_str(), j2.right.as_str()), (2, "id", "oid"));
        assert!(suggest_join(&m, 0, &[j1, j2]).is_none());
    }

    #[test]
    fn generates_sql_with_unique_aliases_for_self_joins() {
        let m = model();
        let join = JoinRow {
            kind: JoinKind::Left,
            table: 0,
            left: SlotCol { slot: 0, column: "id".into() },
            right: "cid".into(),
        };
        let sql =
            spec_sql(Engine::Sqlite, &m, 0, &[join], &[SlotCol { slot: 1, column: "id".into() }], &[], &[], "5", false)
                .unwrap();
        assert!(sql.contains("AS \"orders_1\"") && sql.contains("AS \"orders_2\""), "{sql}");
        assert!(sql.ends_with("LIMIT 5"));
        let err = spec_sql(Engine::Sqlite, &m, 0, &[], &[], &[], &[], "five", false).unwrap_err();
        assert!(err.contains("whole number"), "{err}");
    }
}
