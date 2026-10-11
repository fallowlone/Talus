//! Navigation pane: connections → databases → (schemas) → tables/views → columns.
//! Children load lazily on expand; the tree only emits `Action`s, the app does the I/O.

use std::collections::HashMap;

use egui::{Color32, RichText};
use egui_phosphor::regular as icons;
use talus_core::model::{ConnectionConfig, ObjectKind, ObjectRef};
use uuid::Uuid;

use crate::actions::Action;
use crate::state::{ConnState, ConnStatus, Loadable, NavSel, ObjNode, kind_icon};
use crate::theme;

const INDENT: f32 = 14.0;

struct Row<'a> {
    depth: usize,
    expanded: Option<bool>,
    icon: &'a str,
    icon_color: Color32,
    label: String,
    selected: bool,
    busy: bool,
    dim: bool,
}

/// Returns (caret clicked, row response). Full-width row; the caret is a hit zone at its left.
fn row(ui: &mut egui::Ui, r: Row<'_>) -> (bool, egui::Response) {
    let p = theme::current(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::click());
    let label = format!("{}  {}", r.icon, r.label);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), r.selected, &label));
    let cy = rect.center().y;
    let x0 = rect.left() + 4.0 + r.depth as f32 * INDENT;
    let caret = egui::Rect::from_center_size(egui::pos2(x0 + 8.0, cy), egui::vec2(18.0, 22.0));
    let toggled =
        resp.clicked() && r.expanded.is_some() && resp.interact_pointer_pos().is_some_and(|pos| caret.contains(pos));
    if !ui.is_rect_visible(rect) {
        return (toggled, resp);
    }
    if r.selected {
        ui.painter().rect_filled(rect, 7.0, p.selected);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 7.0, p.hover);
    }
    let painter = ui.painter_at(rect);
    if let Some(open) = r.expanded {
        let glyph = if open { icons::CARET_DOWN } else { icons::CARET_RIGHT };
        painter.text(
            caret.center(),
            egui::Align2::CENTER_CENTER,
            glyph,
            egui::FontId::proportional(11.0),
            p.text_faint,
        );
    }
    let text_color = if r.dim { p.text_dim } else { p.text };
    painter.text(
        egui::pos2(x0 + 20.0, cy),
        egui::Align2::LEFT_CENTER,
        r.icon,
        egui::FontId::proportional(15.0),
        r.icon_color,
    );
    let end = painter.text(
        egui::pos2(x0 + 42.0, cy),
        egui::Align2::LEFT_CENTER,
        &r.label,
        egui::FontId::proportional(13.5),
        text_color,
    );
    if r.busy {
        let spin = egui::Rect::from_center_size(egui::pos2(end.right() + 12.0, cy), egui::vec2(10.0, 10.0));
        ui.put(spin, egui::Spinner::new().size(10.0));
    }
    (toggled, resp)
}

fn matches(filter: &str, name: &str) -> bool {
    filter.is_empty() || name.to_lowercase().contains(filter)
}

pub struct NavCtx<'a> {
    pub connections: &'a [ConnectionConfig],
    pub states: &'a mut HashMap<Uuid, ConnState>,
    pub selection: &'a Option<NavSel>,
    pub filter: &'a str,
}

pub fn show(ui: &mut egui::Ui, ctx: NavCtx<'_>, actions: &mut Vec<Action>) {
    let p = theme::current(ui.ctx());
    let filter = ctx.filter.trim().to_lowercase();
    if ctx.connections.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.label(RichText::new("No connections yet").color(p.text_dim));
            if ui.button(format!("{} New Connection", icons::PLUGS)).clicked() {
                actions.push(Action::NewConnection);
            }
        });
        return;
    }
    let mut groups: Vec<Option<&str>> = ctx.connections.iter().map(|c| c.group.as_deref()).collect();
    groups.sort();
    groups.dedup();
    for group in groups {
        let depth0 = usize::from(group.is_some());
        if let Some(g) = group {
            ui.label(RichText::new(format!("{}  {g}", icons::FOLDER)).color(p.text_dim));
        }
        for cfg in ctx.connections.iter().filter(|c| c.group.as_deref() == group) {
            let state = ctx.states.entry(cfg.id).or_default();
            conn_node(ui, cfg, state, ctx.selection, &filter, depth0, actions);
        }
    }
}

fn conn_node(
    ui: &mut egui::Ui,
    cfg: &ConnectionConfig,
    st: &mut ConnState,
    sel: &Option<NavSel>,
    filter: &str,
    depth: usize,
    actions: &mut Vec<Action>,
) {
    let p = theme::current(ui.ctx());
    let any_child_match = !filter.is_empty() && st.databases.iter().any(|d| db_matches(d, filter));
    if !matches(filter, &cfg.name) && !any_child_match {
        return;
    }
    let (busy, dim) = match &st.status {
        ConnStatus::Connecting(_) => (true, false),
        ConnStatus::Open { .. } => (false, false),
        _ => (false, true),
    };
    let expanded = st.status.is_open() && (st.expanded || any_child_match);
    let (toggle, resp) = row(
        ui,
        Row {
            depth,
            expanded: Some(expanded),
            icon: theme::engine_icon(cfg.engine),
            icon_color: if dim {
                theme::engine_color(cfg.engine).gamma_multiply(0.55)
            } else {
                theme::engine_color(cfg.engine)
            },
            label: cfg.name.clone(),
            selected: *sel == Some(NavSel::Conn(cfg.id)),
            busy,
            dim,
        },
    );
    let resp = match &st.status {
        ConnStatus::Failed(e) => resp.on_hover_text(RichText::new(e).color(p.danger)),
        ConnStatus::Open { version } => resp.on_hover_text(version),
        _ => resp,
    };
    if resp.clicked() {
        actions.push(Action::Select(NavSel::Conn(cfg.id)));
    }
    if toggle || resp.double_clicked() {
        if st.status.is_open() {
            st.expanded = !st.expanded;
        } else if !busy {
            actions.push(Action::Open(cfg.id));
        }
    }
    let id = cfg.id;
    let open = st.status.is_open();
    resp.context_menu(|ui| {
        if open {
            if ui.button("Close Connection").clicked() {
                actions.push(Action::Close(id));
                ui.close();
            }
        } else if ui.button("Open Connection").clicked() {
            actions.push(Action::Open(id));
            ui.close();
        }
        let new_query = if cfg.engine.is_sql() { "New Query" } else { "Open Console" };
        if ui.button(new_query).clicked() {
            actions.push(Action::NewQuery { conn: Some(id), database: None, text: String::new() });
            ui.close();
        }
        if open && cfg.engine.is_sql() && cfg.engine.capabilities().network {
            if ui.button("New Database…").clicked() {
                actions.push(Action::NewDatabase(id));
                ui.close();
            }
            if ui.button("Server Monitor").clicked() {
                actions.push(Action::OpenMonitor(id));
                ui.close();
            }
            if ui.button("Users and Privileges").clicked() {
                actions.push(Action::OpenUsers(id));
                ui.close();
            }
        }
        if open && ui.button("Refresh").clicked() {
            actions.push(Action::Refresh(NavSel::Conn(id)));
            ui.close();
        }
        ui.separator();
        if ui.button("Edit Connection…").clicked() {
            actions.push(Action::EditConnection(id));
            ui.close();
        }
        if ui.button("Duplicate Connection").clicked() {
            actions.push(Action::DuplicateConnection(id));
            ui.close();
        }
        if ui.button(RichText::new("Delete Connection…").color(p.danger)).clicked() {
            actions.push(Action::AskDeleteConnection(id));
            ui.close();
        }
    });
    if let ConnStatus::Failed(e) = &st.status {
        ui.horizontal(|ui| {
            ui.add_space((depth + 2) as f32 * INDENT);
            ui.add(egui::Label::new(RichText::new(e).small().color(p.danger)).truncate());
        });
    }
    if !expanded {
        return;
    }
    let schemas = cfg.engine.capabilities().schemas;
    for db in &mut st.databases {
        if !filter.is_empty() && !db_matches(db, filter) {
            continue;
        }
        let db_open = db.expanded || (!filter.is_empty() && db_matches(db, filter) && !matches(filter, &db.name));
        let busy = db.objects.is_loading() || db.schemas.is_loading();
        let (toggle, resp) = row(
            ui,
            Row {
                depth: depth + 1,
                expanded: Some(db_open),
                icon: icons::DATABASE,
                icon_color: p.text_dim,
                label: db.name.clone(),
                selected: *sel == Some(NavSel::Db(cfg.id, db.name.clone())),
                busy,
                dim: false,
            },
        );
        if resp.clicked() {
            actions.push(Action::Select(NavSel::Db(cfg.id, db.name.clone())));
        }
        if toggle || resp.double_clicked() {
            db.expanded = !db.expanded;
            if db.expanded {
                if schemas && db.schemas.needs_load() {
                    actions.push(Action::LoadSchemas { conn: cfg.id, db: db.name.clone() });
                } else if !schemas && db.objects.needs_load() {
                    actions.push(Action::LoadObjects { conn: cfg.id, db: db.name.clone(), schema: None });
                }
            }
        }
        let dbname = db.name.clone();
        resp.context_menu(|ui| {
            if ui.button("New Query").clicked() {
                actions.push(Action::NewQuery { conn: Some(id), database: Some(dbname.clone()), text: String::new() });
                ui.close();
            }
            if !schemas && ui.button("ER Diagram").clicked() {
                actions.push(Action::OpenDiagram { conn: id, db: dbname.clone(), schema: None });
                ui.close();
            }
            if !schemas && ui.button("New Table…").clicked() {
                actions.push(Action::NewTable { conn: id, db: dbname.clone(), schema: None });
                ui.close();
            }
            if cfg.engine.capabilities().network && ui.button(RichText::new("Drop Database…").color(p.danger)).clicked()
            {
                actions.push(Action::AskDropDatabase { conn: id, db: dbname.clone() });
                ui.close();
            }
            if ui.button("Refresh").clicked() {
                actions.push(Action::Refresh(NavSel::Db(id, dbname.clone())));
                ui.close();
            }
        });
        if let Loadable::Failed(e) = db.schemas.clone() {
            error_line(ui, depth + 3, &e);
        }
        if let Loadable::Failed(e) = db.objects.clone() {
            error_line(ui, depth + 3, &e);
        }
        if !db_open {
            continue;
        }
        if schemas {
            if let Some(list) = db.schemas.loaded_mut() {
                for s in list {
                    let open = s.expanded || (!filter.is_empty() && objs_match(&s.objects, filter));
                    let (toggle, resp) = row(
                        ui,
                        Row {
                            depth: depth + 2,
                            expanded: Some(open),
                            icon: icons::TREE_STRUCTURE,
                            icon_color: p.text_dim,
                            label: s.name.clone(),
                            selected: *sel == Some(NavSel::Schema(cfg.id, dbname.clone(), s.name.clone())),
                            busy: s.objects.is_loading(),
                            dim: false,
                        },
                    );
                    if resp.clicked() {
                        actions.push(Action::Select(NavSel::Schema(cfg.id, dbname.clone(), s.name.clone())));
                    }
                    if toggle || resp.double_clicked() {
                        s.expanded = !s.expanded;
                        if s.expanded && s.objects.needs_load() {
                            actions.push(Action::LoadObjects {
                                conn: cfg.id,
                                db: dbname.clone(),
                                schema: Some(s.name.clone()),
                            });
                        }
                    }
                    let sname = s.name.clone();
                    resp.context_menu(|ui| {
                        if ui.button("New Query").clicked() {
                            actions.push(Action::NewQuery {
                                conn: Some(id),
                                database: Some(dbname.clone()),
                                text: format!(
                                    "SET search_path TO {};\n",
                                    talus_core::sql::Dialect(cfg.engine).quote_ident(&sname).unwrap_or_default()
                                ),
                            });
                            ui.close();
                        }
                        if ui.button("ER Diagram").clicked() {
                            actions.push(Action::OpenDiagram {
                                conn: id,
                                db: dbname.clone(),
                                schema: Some(sname.clone()),
                            });
                            ui.close();
                        }
                        if ui.button("New Table…").clicked() {
                            actions.push(Action::NewTable {
                                conn: id,
                                db: dbname.clone(),
                                schema: Some(sname.clone()),
                            });
                            ui.close();
                        }
                        if ui.button("Refresh").clicked() {
                            actions.push(Action::Refresh(NavSel::Schema(id, dbname.clone(), sname.clone())));
                            ui.close();
                        }
                    });
                    if let Loadable::Failed(e) = s.objects.clone() {
                        error_line(ui, depth + 4, &e);
                    }
                    if open {
                        objects(ui, cfg, &mut s.objects, sel, filter, depth + 3, actions);
                    }
                }
            }
        } else {
            objects(ui, cfg, &mut db.objects, sel, filter, depth + 2, actions);
        }
    }
}

fn error_line(ui: &mut egui::Ui, depth: usize, e: &str) {
    let p = theme::current(ui.ctx());
    ui.horizontal(|ui| {
        ui.add_space(depth as f32 * INDENT);
        ui.add(egui::Label::new(RichText::new(e).small().color(p.danger)).truncate());
    });
}

fn objs_match(objs: &Loadable<Vec<ObjNode>>, filter: &str) -> bool {
    objs.loaded().is_some_and(|l| l.iter().any(|o| matches(filter, &o.obj.name)))
}

fn db_matches(db: &crate::state::DbNode, filter: &str) -> bool {
    matches(filter, &db.name)
        || objs_match(&db.objects, filter)
        || db
            .schemas
            .loaded()
            .is_some_and(|s| s.iter().any(|x| matches(filter, &x.name) || objs_match(&x.objects, filter)))
}

fn objects(
    ui: &mut egui::Ui,
    cfg: &ConnectionConfig,
    objs: &mut Loadable<Vec<ObjNode>>,
    sel: &Option<NavSel>,
    filter: &str,
    depth: usize,
    actions: &mut Vec<Action>,
) {
    let p = theme::current(ui.ctx());
    let Some(list) = objs.loaded_mut() else { return };
    for kind in [ObjectKind::Table, ObjectKind::View] {
        let count = list.iter().filter(|o| o.obj.kind == kind && matches(filter, &o.obj.name)).count();
        if count == 0 {
            continue;
        }
        // Redis lists a single pseudo-object; a heading above it would only repeat its name.
        if cfg.engine.kind() != talus_core::model::EngineKind::KeyValue {
            ui.horizontal(|ui| {
                ui.add_space(depth as f32 * INDENT + 16.0);
                let heading = match (cfg.engine.kind(), kind) {
                    (talus_core::model::EngineKind::KeyValue, _) => "Keys".to_string(),
                    (talus_core::model::EngineKind::Document, ObjectKind::Table) => "Collections".to_string(),
                    _ => format!("{}s", kind.label()),
                };
                let shown = if cfg.engine.kind() == talus_core::model::EngineKind::KeyValue {
                    String::new()
                } else {
                    format!(" ({count})")
                };
                ui.label(RichText::new(format!("{heading}{shown}")).small().color(p.text_dim));
            });
        }
        for node in list.iter_mut().filter(|o| o.obj.kind == kind && matches(filter, &o.obj.name)) {
            let (toggle, resp) = row(
                ui,
                Row {
                    depth,
                    expanded: cfg.engine.is_sql().then_some(node.expanded),
                    icon: kind_icon(kind),
                    icon_color: if kind == ObjectKind::Table { p.accent } else { p.syntax_ident },
                    label: node.obj.name.clone(),
                    selected: *sel == Some(NavSel::Obj(cfg.id, node.obj.clone())),
                    busy: node.structure.is_loading(),
                    dim: false,
                },
            );
            if resp.clicked() {
                actions.push(Action::Select(NavSel::Obj(cfg.id, node.obj.clone())));
            }
            if resp.double_clicked() {
                actions.push(Action::OpenData { conn: cfg.id, obj: node.obj.clone() });
            }
            if toggle {
                node.expanded = !node.expanded;
                if node.expanded && node.structure.needs_load() {
                    actions.push(Action::LoadStructure { conn: cfg.id, obj: node.obj.clone() });
                }
            }
            object_menu(&resp, cfg, &node.obj, actions);
            if node.expanded {
                match &node.structure {
                    Loadable::Loaded(s) => {
                        for c in &s.columns {
                            let icon = if c.primary_key { icons::KEY } else { icons::COLUMNS };
                            ui.horizontal(|ui| {
                                ui.add_space((depth + 1) as f32 * INDENT + 16.0);
                                ui.label(RichText::new(icon).color(if c.primary_key { p.warn } else { p.text_dim }));
                                ui.label(&c.name);
                                ui.label(RichText::new(&c.data_type).small().color(p.text_dim));
                            });
                        }
                    }
                    Loadable::Failed(e) => error_line(ui, depth + 2, e),
                    _ => {}
                }
            }
        }
    }
}

pub fn object_menu(resp: &egui::Response, cfg: &ConnectionConfig, obj: &ObjectRef, actions: &mut Vec<Action>) {
    let p = theme::current(&resp.ctx);
    let (id, obj) = (cfg.id, obj.clone());
    resp.context_menu(|ui| {
        let open_label = match cfg.engine.kind() {
            talus_core::model::EngineKind::KeyValue => "Open Keys".to_string(),
            talus_core::model::EngineKind::Document => "Open Collection".to_string(),
            talus_core::model::EngineKind::Sql => format!("Open {}", obj.kind.label()),
        };
        if ui.button(format!("{} {open_label}", icons::TABLE)).clicked() {
            actions.push(Action::OpenData { conn: id, obj: obj.clone() });
            ui.close();
        }
        if !cfg.engine.is_sql() {
            if ui.button(format!("{} Copy Name", icons::COPY)).clicked() {
                actions.push(Action::CopyText(obj.name.clone()));
                ui.close();
            }
            return;
        }
        if obj.kind == ObjectKind::Table && ui.button(format!("{} Design Table…", icons::PENCIL_RULER)).clicked() {
            actions.push(Action::DesignTable { conn: id, obj: obj.clone() });
            ui.close();
        }
        if ui.button(format!("{} Structure", icons::COLUMNS)).clicked() {
            actions.push(Action::OpenStructure { conn: id, obj: obj.clone() });
            ui.close();
        }
        if ui.button(format!("{} New Query", icons::FILE_SQL)).clicked() {
            let d = talus_core::sql::Dialect(cfg.engine);
            let text = d.qualified(&obj).map(|q| format!("SELECT * FROM {q} LIMIT 100;")).unwrap_or_default();
            actions.push(Action::NewQuery { conn: Some(id), database: Some(obj.database.clone()), text });
            ui.close();
        }
        if obj.kind == ObjectKind::Table
            && ui.button(format!("{} Profile Columns", icons::CHART_BAR_HORIZONTAL)).clicked()
        {
            actions.push(Action::Profile { conn: id, obj: obj.clone() });
            ui.close();
        }
        if obj.kind == ObjectKind::Table && ui.button(format!("{} Generate Test Data…", icons::MAGIC_WAND)).clicked()
        {
            actions.push(Action::GenerateData { conn: id, obj: obj.clone() });
            ui.close();
        }
        if ui.button(format!("{} Export Data…", icons::EXPORT)).clicked() {
            actions.push(Action::Export { conn: id, obj: obj.clone() });
            ui.close();
        }
        if obj.kind == ObjectKind::Table
            && ui.button(format!("{} Import CSV / JSON / XML…", icons::DOWNLOAD_SIMPLE)).clicked()
        {
            actions.push(Action::Import { conn: id, obj: obj.clone() });
            ui.close();
        }
        if ui.button(format!("{} Copy Name", icons::COPY)).clicked() {
            actions.push(Action::CopyText(obj.name.clone()));
            ui.close();
        }
        ui.separator();
        if obj.kind == ObjectKind::Table && ui.button(RichText::new("Empty Table…").color(p.danger)).clicked() {
            actions.push(Action::AskTruncate { conn: id, obj: obj.clone() });
            ui.close();
        }
        if ui.button(RichText::new(format!("Drop {}…", obj.kind.label())).color(p.danger)).clicked() {
            actions.push(Action::AskDrop { conn: id, obj: obj.clone() });
            ui.close();
        }
    });
}
