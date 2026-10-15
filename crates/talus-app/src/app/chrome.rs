//! Window chrome: full-height sidebar (workspace switcher, navigation, connections tree, tools,
//! settings), the top bar with the global search field, the tab strip and the footer status line.

use std::time::Duration;

use egui::{Align2, FontId, RichText, Sense, vec2};
use egui_phosphor::regular as icons;
use talus_core::store::config::Theme;
use talus_core::store::config::save_json;

use super::{App, TabKind};
use crate::actions::Action;
use crate::state::NavSel;
use crate::theme;
use crate::ui::{nav, widgets};

const SEARCH_ID: &str = "global-search";
pub(super) const TOP_BAR_H: f32 = 52.0;

impl App {
    /// Connection the chrome refers to: the selection's, else the active tab's.
    fn chrome_conn(&self) -> Option<uuid::Uuid> {
        self.selection.as_ref().map(NavSel::conn).or_else(|| match &self.tabs[self.active].kind {
            TabKind::Sql(s) => s.conn,
            TabKind::Data(d) => Some(d.conn),
            TabKind::Structure(s) => Some(s.conn),
            TabKind::Design(d) => Some(d.conn),
            _ => None,
        })
    }

    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        self.workspace_header(ui);
        ui.add_space(6.0);
        egui::Panel::bottom("sidebar-foot").frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(0, 8))).show(
            ui,
            |ui| {
                if widgets::nav_item(ui, icons::GEAR, "Settings", self.settings_open, false).clicked() {
                    self.actions.push(Action::OpenSettings);
                }
            },
        );
        egui::ScrollArea::vertical().id_salt("sidebar-scroll").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let kind = &self.tabs[self.active].kind;
            let (home, history, find) = (
                matches!(kind, TabKind::Objects),
                matches!(kind, TabKind::History(_)),
                matches!(kind, TabKind::Find(_)),
            );
            if widgets::nav_item(ui, icons::SQUARES_FOUR, "Home", home, false).clicked() {
                self.active = 0;
            }
            if widgets::nav_item(ui, icons::FILE_SQL, "New Query", false, false).on_hover_text("⌘Y").clicked() {
                self.actions.push(Action::NewQuery { conn: None, database: None, text: String::new() });
            }
            if widgets::nav_item(ui, icons::CLOCK_COUNTER_CLOCKWISE, "History", history, false)
                .on_hover_text("⇧⌘H")
                .clicked()
            {
                self.actions.push(Action::OpenHistory);
            }
            if widgets::nav_item(ui, icons::MAGNIFYING_GLASS, "Find in Database", find, false)
                .on_hover_text("⇧⌘F")
                .clicked()
            {
                self.actions.push(Action::OpenFind);
            }

            // Fixed-height sections first: the growing connections tree never moves them.
            widgets::section_header(ui, "Tools", &mut self.tools_open);
            if self.tools_open {
                self.tools_items(ui);
            }

            ui.horizontal(|ui| {
                widgets::section_header(ui, "Connections", &mut self.conns_open);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
                    if widgets::icon_button(ui, icons::PLUS, "New connection (⌘N)", false).clicked() {
                        self.actions.push(Action::NewConnection);
                    }
                });
            });
            if self.conns_open {
                let mut acts = Vec::new();
                nav::show(
                    ui,
                    nav::NavCtx {
                        connections: &self.connections,
                        states: &mut self.states,
                        selection: &self.selection,
                        filter: &self.nav_filter,
                    },
                    &mut acts,
                );
                self.actions.extend(acts);
            }
        });
    }

    fn tools_items(&mut self, ui: &mut egui::Ui) {
        let items: [(&str, &str, Option<Action>); 12] = [
            (icons::ARROWS_LEFT_RIGHT, "Structure Synchronization…", Some(Action::OpenSync)),
            (icons::TREE_STRUCTURE, "Data Modeling", Some(Action::OpenModel)),
            (icons::BLUEPRINT, "Query Builder", Some(Action::OpenBuilder)),
            (icons::USERS, "Users and Privileges", None),
            (icons::FUNCTION, "Routines and Triggers", Some(Action::OpenRoutines)),
            (icons::CLOCK_COUNTDOWN, "Automation (Jobs)…", Some(Action::OpenJobs)),
            (icons::USERS_THREE, "Shared Workspace…", Some(Action::OpenWorkspace)),
            (icons::DOWNLOAD_SIMPLE, "Dump Structure and Data…", Some(Action::DumpSql { include_data: true })),
            (icons::FILE_ARROW_DOWN, "Dump Structure Only…", Some(Action::DumpSql { include_data: false })),
            (icons::PLAY, "Execute SQL File…", Some(Action::RunSqlFile)),
            (icons::BOOK_OPEN, "Data Dictionary (HTML)…", Some(Action::Dictionary { html: true })),
            (icons::FILE_TEXT, "Data Dictionary (Markdown)…", Some(Action::Dictionary { html: false })),
        ];
        for (icon, label, action) in items {
            if !widgets::nav_item(ui, icon, label, false, false).clicked() {
                continue;
            }
            match action {
                Some(a) => self.actions.push(a),
                None => match &self.selection {
                    Some(sel) => self.actions.push(Action::OpenUsers(sel.conn())),
                    None => self.notify("Select a connection in the navigator first.", true),
                },
            }
        }
    }

    /// Workspace switcher: avatar, active connection, engine badge, chevron → connection menu.
    fn workspace_header(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        let cfg = self.chrome_conn().and_then(|c| self.cfg(c)).cloned();
        let name = cfg.as_ref().map_or("Talus".to_string(), |c| c.name.clone());
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Workspace"));
        if resp.hovered() {
            ui.painter().rect_filled(rect, 8.0, p.hover);
        }
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 0.0))));
        child.horizontal_centered(|ui| {
            let glyph = name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
            widgets::avatar(ui, &glyph, 24.0, p.accent, p.on_accent);
            ui.add_space(2.0);
            ui.add(egui::Label::new(RichText::new(&name).font(theme::semibold(14.0)).color(p.text)).truncate());
            if let Some(c) = &cfg {
                widgets::badge(ui, c.engine.label(), p.info, p.info.gamma_multiply(0.14));
            }
        });
        ui.painter().text(
            egui::pos2(rect.right() - 10.0, rect.center().y),
            Align2::RIGHT_CENTER,
            icons::CARET_DOWN,
            FontId::proportional(13.0),
            p.text_dim,
        );
        egui::Popup::menu(&resp).width(rect.width()).show(|ui| {
            for c in &self.connections {
                let open = self.states.get(&c.id).is_some_and(|s| s.status.is_open());
                let label = format!("{}  {}", theme::engine_icon(c.engine), c.name);
                let text = if open { RichText::new(label) } else { RichText::new(label).color(p.text_dim) };
                if ui.button(text).clicked() {
                    self.actions.push(Action::Select(NavSel::Conn(c.id)));
                    if !open {
                        self.actions.push(Action::Open(c.id));
                    }
                }
            }
            if !self.connections.is_empty() {
                ui.separator();
            }
            if ui.button(format!("{}  New Connection…", icons::PLUS)).clicked() {
                self.actions.push(Action::NewConnection);
            }
            if ui.button(format!("{}  Open SQL File…", icons::FOLDER_OPEN)).clicked() {
                self.actions.push(Action::OpenFile);
            }
            ui.separator();
            if ui.button(format!("{}  About Talus", icons::INFO)).clicked() {
                self.actions.push(Action::OpenAbout);
            }
        });
    }

    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        let full = ui.available_rect_before_wrap();
        ui.horizontal_centered(|ui| {
            if widgets::icon_button(ui, icons::SIDEBAR_SIMPLE, "Toggle sidebar (⌘1)", false).clicked() {
                self.show_nav = !self.show_nav;
            }
            // Search field, centred in the bar.
            let w = (full.width() - 220.0).clamp(160.0, 540.0);
            let search = egui::Rect::from_center_size(full.center(), vec2(w, 32.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(search));
            let hovered = child.rect_contains_pointer(search);
            let id = egui::Id::new(SEARCH_ID);
            let focused = child.memory(|m| m.has_focus(id));
            // A white, lifted command field (same elevation as cards); focus adds the black ring.
            for shadow in theme::card_shadows(p) {
                child.painter().add(shadow.as_shape(search, 9));
            }
            let ring = if focused { egui::Stroke::new(1.0, p.accent) } else { egui::Stroke::NONE };
            child.painter().rect(
                search,
                9.0,
                if hovered && !focused { p.hover } else { p.raised },
                ring,
                egui::StrokeKind::Inside,
            );
            child.painter().text(
                egui::pos2(search.left() + 12.0, search.center().y),
                Align2::LEFT_CENTER,
                icons::MAGNIFYING_GLASS,
                FontId::proportional(15.0),
                p.text_dim,
            );
            let mut caps_x = search.right() - 8.0;
            caps_x -= widgets::key_cap(child.painter(), p, egui::pos2(caps_x - 4.0, search.center().y), "⌘K") + 10.0;
            let edit_rect = egui::Rect::from_min_max(
                egui::pos2(search.left() + 34.0, search.top()),
                egui::pos2(caps_x, search.bottom()),
            );
            let r = child.put(
                edit_rect,
                egui::TextEdit::singleline(&mut self.nav_filter)
                    .id(id)
                    .frame(egui::Frame::NONE)
                    .hint_text(RichText::new("Search for anything…").color(p.text_faint))
                    .vertical_align(egui::Align::Center)
                    .desired_width(edit_rect.width()),
            );
            if std::mem::take(&mut self.focus_search) {
                r.request_focus();
            }
            if r.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.nav_filter.clear();
                r.surrender_focus();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button(ui, icons::SIDEBAR, "Information pane (⌘2)", self.show_info).clicked() {
                    self.show_info = !self.show_info;
                }
                let dark = self.settings.theme == Theme::Dark;
                let (icon, tip) =
                    if dark { (icons::SUN, "Switch to light theme") } else { (icons::MOON, "Switch to dark theme") };
                if widgets::icon_button(ui, icon, tip, false).clicked() {
                    self.settings.theme = if dark { Theme::Light } else { Theme::Dark };
                    self.settings_draft.theme = self.settings.theme;
                    if let Err(e) = save_json(&self.paths.settings_file(), &self.settings) {
                        self.notify(format!("Could not save settings: {e}"), true);
                    }
                }
                if widgets::icon_button(ui, icons::QUESTION, "About Talus", false).clicked() {
                    self.actions.push(Action::OpenAbout);
                }
            });
        });
    }

    pub(super) fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        egui::ScrollArea::horizontal().id_salt("tabbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let mut activate = None;
                let mut close = None;
                for (i, t) in self.tabs.iter().enumerate() {
                    let selected = i == self.active;
                    let closable = !matches!(t.kind, TabKind::Objects);
                    // Active tab: black semibold title; others dim. No chip, no underline.
                    egui::Frame::NONE.inner_margin(egui::Margin::symmetric(8, 2)).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let title = format!("{}  {}", t.icon(), t.title());
                        let text = if selected {
                            RichText::new(title).font(theme::semibold(13.5)).color(p.text)
                        } else {
                            RichText::new(title).color(p.text_dim)
                        };
                        let r = ui.add(egui::Button::new(text).frame_when_inactive(false));
                        if r.clicked() {
                            activate = Some(i);
                        }
                        if r.middle_clicked() && closable {
                            close = Some(t.id);
                        }
                        if closable
                            && ui
                                .add(
                                    egui::Button::new(RichText::new(icons::X).size(11.0).color(p.text_faint))
                                        .frame(false),
                                )
                                .on_hover_text("Close (⌘W)")
                                .clicked()
                        {
                            close = Some(t.id);
                        }
                    });
                }
                if let Some(i) = activate {
                    self.active = i;
                }
                if let Some(id) = close {
                    self.actions.push(Action::CloseTab(id));
                }
            });
        });
    }

    pub(super) fn status_bar(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        ui.horizontal(|ui| {
            ui.style_mut().override_text_style = Some(egui::TextStyle::Small);
            let (conn, db) = match &self.tabs[self.active].kind {
                TabKind::Sql(s) => (s.conn, s.database.clone()),
                TabKind::Data(d) => (Some(d.conn), Some(d.obj.database.clone())),
                TabKind::Structure(s) => (Some(s.conn), Some(s.obj.database.clone())),
                TabKind::Design(d) => (Some(d.conn), Some(d.database.clone())),
                TabKind::Monitor(m) => (Some(m.conn), None),
                TabKind::Diagram(d) => (Some(d.conn), Some(d.database.clone())),
                _ => (
                    self.selection.as_ref().map(NavSel::conn),
                    self.selection.as_ref().and_then(|s| s.database().map(String::from)),
                ),
            };
            if let Some(cfg) = conn.and_then(|c| self.cfg(c)) {
                let open = self.states.get(&cfg.id).is_some_and(|s| s.status.is_open());
                ui.label(RichText::new("●").color(if open { p.ok } else { p.text_faint }));
                ui.label(
                    RichText::new(format!("{}{}", cfg.name, db.map(|d| format!(" · {d}")).unwrap_or_default()))
                        .color(p.text_dim),
                );
            } else {
                ui.label(RichText::new("No connection").color(p.text_faint));
            }
            if let Some(n) = &self.notice
                && (n.at.elapsed() < Duration::from_secs(12) || n.error)
            {
                ui.label(RichText::new("·").color(p.text_faint));
                let r = ui.add(
                    egui::Label::new(RichText::new(&n.text).color(if n.error { p.danger } else { p.text }))
                        .truncate()
                        .sense(Sense::click()),
                );
                if r.on_hover_text("Click to dismiss").clicked() {
                    self.notice = None;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let running = self.pending.len();
                if running > 0 {
                    ui.label(RichText::new(format!("{running} operation(s) running")).color(p.warn));
                    ui.spinner();
                }
            });
        });
    }
}
