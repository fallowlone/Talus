//! Home tab: a dashboard (greeting, connections, query activity, recent queries, workspace,
//! automation) when nothing below a connection is selected, else the objects list of the
//! selected database / schema.

use std::time::{Duration, Instant};

use chrono::{Local, TimeZone, Timelike};
use egui::{Align2, Color32, FontId, RichText, Sense, Stroke, Ui, vec2};
use egui_phosphor::regular as icons;
use talus_core::store::history::HistoryEntry;

use super::{App, TabKind};
use crate::actions::Action;
use crate::state::{ConnStatus, NavSel};
use crate::theme::{self, Palette};
use crate::ui::{panels, widgets};

const GAP: f32 = 28.0;
const RECENT_LIMIT: usize = 500;
const RECENT_TTL: Duration = Duration::from_secs(3);
const CHART_DAYS: usize = 7;
const CHART_H: f32 = 140.0;

impl App {
    pub(super) fn home_ui(&mut self, ui: &mut Ui) {
        if !matches!(self.selection, None | Some(NavSel::Conn(_))) {
            let mut acts = Vec::new();
            panels::objects_view(
                ui,
                &self.connections,
                &self.states,
                &self.selection,
                &mut self.objects_filter,
                &mut acts,
            );
            self.actions.extend(acts);
            return;
        }
        if self.home_recent.as_ref().is_none_or(|(at, _)| at.elapsed() > RECENT_TTL) {
            self.home_recent = Some((Instant::now(), self.history.recent(RECENT_LIMIT).unwrap_or_default()));
        }
        egui::ScrollArea::vertical().id_salt("home-scroll").auto_shrink([false, false]).show(ui, |ui| {
            let p = theme::current(ui.ctx());
            ui.add_space(28.0);
            ui.label(RichText::new(greeting()).font(theme::semibold(30.0)).color(p.text));
            ui.add_space(26.0);
            let wide = ui.available_width() >= 760.0;
            self.card_row(ui, wide, 2, 300.0, |app, ui, i| match i {
                0 => app.connections_card(ui),
                _ => app.activity_card(ui),
            });
            ui.add_space(22.0);
            self.card_row(ui, wide, 3, 250.0, |app, ui, i| match i {
                0 => app.recent_card(ui),
                1 => app.workspace_card(ui),
                _ => app.automation_card(ui),
            });
            ui.add_space(28.0);
        });
    }

    /// Lays out `n` equal-width columns (stacked when narrow); each gets `height` for header + card.
    fn card_row(
        &mut self,
        ui: &mut Ui,
        wide: bool,
        n: usize,
        height: f32,
        mut f: impl FnMut(&mut Self, &mut Ui, usize),
    ) {
        if !wide {
            for i in 0..n {
                ui.allocate_ui(vec2(ui.available_width(), height), |ui| f(self, ui, i));
                ui.add_space(18.0);
            }
            return;
        }
        let w = (ui.available_width() - GAP * (n - 1) as f32) / n as f32;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for i in 0..n {
                ui.allocate_ui_with_layout(vec2(w, height), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(w);
                    f(self, ui, i)
                });
            }
        });
    }

    fn connections_card(&mut self, ui: &mut Ui) {
        let p = theme::current(ui.ctx());
        if widgets::card_header(ui, icons::PLUGS, "Connections", true).on_hover_text("New connection").clicked() {
            self.actions.push(Action::NewConnection);
        }
        ui.add_space(6.0);
        let open = self.states.values().filter(|s| s.status.is_open()).count();
        card(ui, 220.0, |ui| {
            if self.connections.is_empty() {
                widgets::empty_state(ui, icons::PLUGS, "No connections yet", "Press ⌘N to add one.");
                return;
            }
            for c in self.connections.iter().take(4) {
                let status = self.states.get(&c.id).map(|s| &s.status);
                let sub = if c.engine.capabilities().network {
                    format!("{} · {}:{}", c.engine.label(), c.host, c.port)
                } else {
                    format!("{} · {}", c.engine.label(), c.database)
                };
                let r = list_row(ui, Some(theme::engine_icon(c.engine)), &c.name, &sub, |ui| match status {
                    Some(ConnStatus::Open { .. }) => {
                        ui.label(
                            RichText::new(format!("{} Connected", icons::CHECK))
                                .font(theme::semibold(13.5))
                                .color(p.ok),
                        );
                    }
                    Some(ConnStatus::Connecting(_)) => {
                        ui.spinner();
                    }
                    _ => {
                        if widgets::icon_button(ui, icons::PLUGS, "Open connection", false).clicked() {
                            self.actions.push(Action::Open(c.id));
                        }
                        ui.label(RichText::new("Offline").font(theme::semibold(13.5)).color(p.text_faint));
                    }
                });
                if r.clicked() {
                    self.actions.push(Action::Select(NavSel::Conn(c.id)));
                }
            }
        });
        let n = self.connections.len();
        let caption = format!("{n} connection{} · {open} open", if n == 1 { "" } else { "s" });
        if footer(ui, &caption, Some("New connection")) {
            self.actions.push(Action::NewConnection);
        }
    }

    fn activity_card(&mut self, ui: &mut Ui) {
        let p = theme::current(ui.ctx());
        if widgets::card_header(ui, icons::CHART_LINE_UP, "Query Activity", true).clicked() {
            self.actions.push(Action::OpenHistory);
        }
        ui.add_space(6.0);
        let recent = self.home_recent.as_ref().map_or(&[][..], |(_, r)| r.as_slice());
        let days = daily_counts(recent);
        let total: usize = days.iter().map(|(ok, bad)| ok + bad).sum();
        let avg =
            if recent.is_empty() { 0 } else { recent.iter().map(|e| e.elapsed_ms).sum::<u64>() / recent.len() as u64 };
        card(ui, 220.0, |ui| {
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(total.to_string()).font(theme::semibold(30.0)).color(p.text));
                    ui.label(RichText::new("Queries · last 7 days").color(p.text_dim));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    ui.add_space(16.0);
                    legend(ui, p.danger.gamma_multiply(0.85), "Failed");
                    legend(ui, p.info, "Succeeded");
                });
            });
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), CHART_H), Sense::hover());
            area_chart(ui, p, rect.shrink2(vec2(16.0, 10.0)), &days);
        });
        if footer(ui, &format!("Average {avg} ms per statement"), Some("Open history")) {
            self.actions.push(Action::OpenHistory);
        }
    }

    fn recent_card(&mut self, ui: &mut Ui) {
        let p = theme::current(ui.ctx());
        if widgets::card_header(ui, icons::CLOCK_COUNTER_CLOCKWISE, "Recent Queries", true).clicked() {
            self.actions.push(Action::OpenHistory);
        }
        ui.add_space(6.0);
        let recent: Vec<HistoryEntry> =
            self.home_recent.as_ref().map(|(_, r)| r.iter().take(3).cloned().collect()).unwrap_or_default();
        card(ui, 200.0, |ui| {
            if recent.is_empty() {
                widgets::empty_state(
                    ui,
                    icons::CLOCK_COUNTER_CLOCKWISE,
                    "No queries yet",
                    "Queries you run appear here.",
                );
                return;
            }
            for e in recent.iter() {
                let first = e.sql.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
                let when = Local.timestamp_millis_opt(e.ts_ms as i64).single().map(|t| t.format("%H:%M").to_string());
                let sub = format!("{} · {}", e.connection, when.unwrap_or_default());
                let r = list_row(ui, None, first, &sub, |ui| {
                    let (t, c) = if e.ok { ("OK", p.ok) } else { ("Failed", p.danger) };
                    ui.label(RichText::new(t).font(theme::semibold(13.0)).color(c));
                });
                if r.on_hover_text(&e.sql).clicked() {
                    self.actions.push(Action::NewQuery { conn: None, database: None, text: e.sql.clone() });
                }
            }
        });
    }

    fn workspace_card(&mut self, ui: &mut Ui) {
        let p = theme::current(ui.ctx());
        widgets::card_header(ui, icons::SQUARES_FOUR, "Workspace", false);
        ui.add_space(6.0);
        let count = |f: fn(&TabKind) -> bool| self.tabs.iter().filter(|t| f(&t.kind)).count();
        let queries = count(|k| matches!(k, TabKind::Sql(_)));
        let data = count(|k| matches!(k, TabKind::Data(_) | TabKind::Structure(_) | TabKind::Design(_)));
        let other = self.tabs.len() - 1 - queries - data;
        card(ui, 200.0, |ui| {
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new((self.tabs.len() - 1).to_string()).font(theme::semibold(30.0)).color(p.text),
                    );
                    ui.label(RichText::new("Open tabs").color(p.text_dim));
                });
            });
            ui.add_space(10.0);
            nested_list(
                ui,
                &[
                    (icons::FILE_SQL, "Queries", queries),
                    (icons::TABLE, "Data and design", data),
                    (icons::STACK, "Tools", other),
                ],
            );
        });
    }

    fn automation_card(&mut self, ui: &mut Ui) {
        let p = theme::current(ui.ctx());
        if widgets::card_header(ui, icons::CLOCK_COUNTDOWN, "Automation", true).clicked() {
            self.actions.push(Action::OpenJobs);
        }
        ui.add_space(6.0);
        card(ui, 200.0, |ui| {
            if self.jobs.is_empty() {
                widgets::empty_state(ui, icons::CLOCK_COUNTDOWN, "No scheduled jobs", "");
                return;
            }
            for j in self.jobs.iter().take(3) {
                let running = self.jobs_running.contains(&j.id);
                list_row(ui, None, &j.name, &j.schedule.label(), |ui| {
                    let (t, c) = match (running, j.enabled, j.last_run.as_ref().map(|r| r.ok)) {
                        (true, ..) => ("Running", p.info),
                        (_, false, _) => ("Paused", p.text_dim),
                        (_, _, Some(false)) => ("Failed", p.danger),
                        _ => ("Active", p.ok),
                    };
                    ui.label(RichText::new(t).font(theme::semibold(13.0)).color(c));
                });
            }
        });
    }
}

fn greeting() -> String {
    let part = match Local::now().hour() {
        5..=11 => "morning",
        12..=17 => "afternoon",
        _ => "evening",
    };
    let user = std::env::var("USER").ok().filter(|u| !u.is_empty()).map(|u| {
        let mut c = u.chars();
        c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
    });
    match user {
        Some(u) => format!("Good {part}, {u}"),
        None => format!("Good {part}"),
    }
}

/// White lifted card with a minimum height; rows bring their own padding.
fn card(ui: &mut Ui, min_h: f32, add: impl FnOnce(&mut Ui)) {
    widgets::card(ui, egui::Margin::same(4), |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(min_h);
        ui.spacing_mut().item_spacing.y = 0.0;
        add(ui);
    });
}

/// Caption line under a card: text on the left, an optional action link on the right.
/// Returns whether the link was clicked.
fn footer(ui: &mut Ui, left: &str, link: Option<&str>) -> bool {
    let p = theme::current(ui.ctx());
    let mut clicked = false;
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.label(RichText::new(left).color(p.text_dim).size(12.5));
        if let Some(text) = link {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(16.0);
                let r = ui.add(egui::Button::new(RichText::new(text).size(12.5)).frame(false));
                clicked = r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
            });
        }
    });
    clicked
}

/// Clickable list row with card padding: optional avatar glyph, title over subtitle (truncated),
/// and `trailing` right-aligned.
fn list_row(
    ui: &mut Ui,
    avatar: Option<&str>,
    title: &str,
    sub: &str,
    trailing: impl FnOnce(&mut Ui),
) -> egui::Response {
    let p = theme::current(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 58.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 9.0, p.hover);
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(vec2(16.0, 0.0)))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    child.spacing_mut().item_spacing = vec2(8.0, 2.0);
    trailing(&mut child);
    child.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
        if let Some(glyph) = avatar {
            widgets::avatar(ui, glyph, 32.0, p.selected, p.text);
            ui.add_space(4.0);
        }
        // Fixed-height block so the centred layout can centre it (its size is known up front).
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 36.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(egui::Label::new(RichText::new(title).size(14.0).color(p.text)).truncate());
            ui.add(egui::Label::new(RichText::new(sub).size(12.5).color(p.text_dim)).truncate());
        });
    });
    resp
}

fn legend(ui: &mut Ui, color: Color32, label: &str) {
    let p = theme::current(ui.ctx());
    ui.label(RichText::new(label).color(p.text_dim).size(12.5));
    let (r, _) = ui.allocate_exact_size(vec2(9.0, 9.0), Sense::hover());
    ui.painter().rect_filled(r, 2.0, color);
}

/// Breakdown rows: icon, label and a right-aligned number; grouped by spacing only.
fn nested_list(ui: &mut Ui, rows: &[(&str, &str, usize)]) {
    let p = theme::current(ui.ctx());
    egui::Frame::NONE.inner_margin(egui::Margin::symmetric(16, 0)).show(ui, |ui| {
        for (icon, label, n) in rows {
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::hover());
            let (y, painter) = (rect.center().y, ui.painter());
            let font = FontId::proportional(13.5);
            let icon_font = FontId::proportional(15.0);
            painter.text(egui::pos2(rect.left() + 8.0, y), Align2::CENTER_CENTER, *icon, icon_font, p.text_dim);
            painter.text(egui::pos2(rect.left() + 26.0, y), Align2::LEFT_CENTER, *label, font.clone(), p.text);
            painter.text(egui::pos2(rect.right(), y), Align2::RIGHT_CENTER, n.to_string(), font, p.text);
        }
    });
}

/// (succeeded, failed) per local day, oldest first, for the last `CHART_DAYS` days.
fn daily_counts(entries: &[HistoryEntry]) -> Vec<(usize, usize)> {
    let today = Local::now().date_naive();
    let mut days = vec![(0, 0); CHART_DAYS];
    for e in entries {
        let Some(t) = Local.timestamp_millis_opt(e.ts_ms as i64).single() else { continue };
        let age = (today - t.date_naive()).num_days();
        if (0..CHART_DAYS as i64).contains(&age) {
            let d = &mut days[CHART_DAYS - 1 - age as usize];
            if e.ok { d.0 += 1 } else { d.1 += 1 }
        }
    }
    days
}

/// Two stacked series as filled areas with dashed grid lines and value labels on the right.
fn area_chart(ui: &Ui, p: &Palette, rect: egui::Rect, days: &[(usize, usize)]) {
    let painter = ui.painter_at(rect.expand(2.0));
    let plot = egui::Rect::from_min_max(rect.min, egui::pos2(rect.right() - 34.0, rect.bottom()));
    let max = days.iter().map(|(a, b)| a + b).max().unwrap_or(0).max(4) as f32;
    let step = (max / 4.0).ceil();
    for k in 1..=4 {
        let v = step * k as f32;
        let y = plot.bottom() - plot.height() * (v / (step * 4.0));
        let mut x = plot.left();
        while x < plot.right() {
            painter.hline(x..=(x + 3.0).min(plot.right()), y, Stroke::new(1.0, p.border));
            x += 6.0;
        }
        painter.text(
            egui::pos2(rect.right(), y),
            Align2::RIGHT_CENTER,
            format!("{v:.0}"),
            FontId::proportional(11.0),
            p.text_faint,
        );
    }
    let top = step * 4.0;
    let pts = |f: &dyn Fn(&(usize, usize)) -> usize| -> Vec<egui::Pos2> {
        let n = days.len().max(2) - 1;
        days.iter()
            .enumerate()
            .map(|(i, d)| {
                egui::pos2(
                    plot.left() + plot.width() * i as f32 / n as f32,
                    plot.bottom() - plot.height() * f(d) as f32 / top,
                )
            })
            .collect()
    };
    for (series, color) in [(pts(&|d| d.0 + d.1), p.info), (pts(&|d| d.1), p.danger.gamma_multiply(0.85))] {
        let mut mesh = egui::Mesh::default();
        let (fill_top, fill_bottom) = (color.gamma_multiply(0.22), color.gamma_multiply(0.02));
        for w in series.windows(2) {
            let base = mesh.vertices.len() as u32;
            mesh.colored_vertex(w[0], fill_top);
            mesh.colored_vertex(w[1], fill_top);
            mesh.colored_vertex(egui::pos2(w[1].x, plot.bottom()), fill_bottom);
            mesh.colored_vertex(egui::pos2(w[0].x, plot.bottom()), fill_bottom);
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base, base + 2, base + 3);
        }
        painter.add(mesh);
        painter.add(egui::Shape::line(series, Stroke::new(2.0, color)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ts_ms: u64, ok: bool) -> HistoryEntry {
        HistoryEntry {
            ts_ms,
            connection: "c".into(),
            database: String::new(),
            sql: "select 1".into(),
            elapsed_ms: 1,
            ok,
            message: String::new(),
        }
    }

    #[test]
    fn daily_counts_bucket_by_local_day() {
        let now = Local::now().timestamp_millis() as u64;
        let day = 86_400_000;
        let counts =
            daily_counts(&[entry(now, true), entry(now, false), entry(now - day, true), entry(now - 30 * day, true)]);
        assert_eq!(counts.len(), CHART_DAYS);
        assert_eq!(counts[CHART_DAYS - 1], (1, 1));
        assert_eq!(counts[CHART_DAYS - 2], (1, 0));
        assert_eq!(counts.iter().map(|(a, b)| a + b).sum::<usize>(), 3);
    }
}
