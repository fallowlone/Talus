//! Chart of a query result (bar / line / scatter via egui_plot, pie drawn by hand). The column
//! choices are remembered per result pane.

use egui::{Color32, RichText, Shape, Stroke, vec2};
use egui_plot::{Bar, BarChart, Legend, Line, Plot, PlotPoints, Points};
use talus_core::chart::{self, ChartData, ChartKind, ChartSpec};
use talus_core::model::QueryResult;

use crate::theme;

/// Muted categorical colours: the UI itself is monochrome, but series must stay distinguishable.
const PALETTE: [Color32; 8] = [
    Color32::from_rgb(0x6c, 0x9e, 0xe8),
    Color32::from_rgb(0xe8, 0x9b, 0x6c),
    Color32::from_rgb(0x7f, 0xc4, 0x8e),
    Color32::from_rgb(0xd9, 0x7c, 0x9c),
    Color32::from_rgb(0xb4, 0x9c, 0xe0),
    Color32::from_rgb(0xd9, 0xc2, 0x6c),
    Color32::from_rgb(0x6c, 0xc4, 0xc4),
    Color32::from_rgb(0x9a, 0x9a, 0x9a),
];

#[derive(Default)]
pub struct ChartState {
    pub open: bool,
    spec: Option<ChartSpec>,
    /// Suggestion computed once per result; `None` inside means nothing numeric to plot.
    initialized: bool,
}

fn color(i: usize) -> Color32 {
    PALETTE[i % PALETTE.len()]
}

impl ChartState {
    pub fn ui(&mut self, ui: &mut egui::Ui, id: egui::Id, result: &QueryResult) {
        let p = theme::current(ui.ctx());
        if !self.initialized {
            self.initialized = true;
            self.spec = chart::suggest(result);
        }
        let Some(spec) = self.spec.as_mut() else {
            ui.label(RichText::new("This result has no numeric column to chart.").color(p.text_dim));
            return;
        };
        let nums = chart::numeric_columns(result);
        ui.horizontal(|ui| {
            super::widgets::combo((id, "kind")).selected_text(spec.kind.label()).width(90.0).show_ui(ui, |ui| {
                for k in ChartKind::ALL {
                    ui.selectable_value(&mut spec.kind, k, k.label());
                }
            });
            ui.label("X");
            super::widgets::combo((id, "x"))
                .selected_text(result.columns.get(spec.x).map_or("?", |c| c.name.as_str()))
                .width(140.0)
                .show_ui(ui, |ui| {
                    for (i, c) in result.columns.iter().enumerate() {
                        ui.selectable_value(&mut spec.x, i, &c.name);
                    }
                });
            ui.label("Y");
            for &c in &nums {
                let mut on = spec.ys.contains(&c);
                if crate::ui::widgets::check(ui, &mut on, &result.columns[c].name).changed() {
                    if on {
                        spec.ys.push(c);
                    } else {
                        spec.ys.retain(|y| *y != c);
                    }
                }
            }
        });
        let data = chart::build(result, spec);
        if data.omitted_rows > 0 {
            ui.label(
                RichText::new(format!(
                    "Plotting the first {} rows ({} more omitted).",
                    data.labels.len(),
                    data.omitted_rows
                ))
                .color(p.warn),
            );
        }
        if data.series.is_empty() {
            ui.label(RichText::new("Tick at least one numeric column for Y.").color(p.text_dim));
            return;
        }
        match spec.kind {
            ChartKind::Pie => pie(ui, &data),
            kind => plot(ui, id, &data, kind),
        }
    }
}

fn plot(ui: &mut egui::Ui, id: egui::Id, data: &ChartData, kind: ChartKind) {
    let labels = data.labels.clone();
    let numeric_x = data.xs.clone();
    let x_of = move |i: usize| numeric_x.as_ref().map_or(i as f64, |xs| xs[i]);
    let mut plot = Plot::new((id, "plot"))
        .legend(Legend::default())
        .allow_scroll(false)
        .show_grid([false, true])
        .height(ui.available_height().max(160.0) - 8.0);
    if data.xs.is_none() {
        let l = labels.clone();
        plot = plot.x_axis_formatter(move |mark, _| {
            let i = mark.value.round();
            if (mark.value - i).abs() < 1e-6 && i >= 0.0 {
                l.get(i as usize).cloned().unwrap_or_default()
            } else {
                String::new()
            }
        });
    }
    plot.show(ui, |plot_ui| {
        let n = data.series.len().max(1) as f64;
        for (si, s) in data.series.iter().enumerate() {
            let pts: Vec<[f64; 2]> = s.values.iter().enumerate().filter_map(|(i, v)| v.map(|v| [x_of(i), v])).collect();
            match kind {
                ChartKind::Bar => {
                    let width = 0.8 / n;
                    let offset = (si as f64 - (n - 1.0) / 2.0) * width;
                    let bars = s
                        .values
                        .iter()
                        .enumerate()
                        .filter_map(|(i, v)| {
                            v.map(|v| {
                                Bar::new(x_of(i) + offset, v).width(width).fill(color(si)).name(labels[i].clone())
                            })
                        })
                        .collect();
                    plot_ui.bar_chart(BarChart::new(s.name.clone(), bars).color(color(si)));
                }
                ChartKind::Line => {
                    plot_ui.line(Line::new(s.name.clone(), PlotPoints::from(pts)).color(color(si)).width(2.0));
                }
                _ => {
                    plot_ui.points(Points::new(s.name.clone(), PlotPoints::from(pts)).color(color(si)).radius(3.5));
                }
            }
        }
    });
}

fn pie(ui: &mut egui::Ui, data: &ChartData) {
    let p = theme::current(ui.ctx());
    let Some(series) = data.series.first() else { return };
    let slices: Vec<(&str, f64)> = data
        .labels
        .iter()
        .zip(&series.values)
        .filter_map(|(l, v)| v.filter(|v| *v > 0.0).map(|v| (l.as_str(), v)))
        .collect();
    let total: f64 = slices.iter().map(|(_, v)| v).sum();
    if total <= 0.0 {
        ui.label(RichText::new("A pie chart needs positive values.").color(p.text_dim));
        return;
    }
    ui.horizontal_top(|ui| {
        let side = (ui.available_height().min(ui.available_width() * 0.6) - 8.0).max(120.0);
        let (rect, _) = ui.allocate_exact_size(vec2(side, side), egui::Sense::hover());
        let center = rect.center();
        let radius = side / 2.0 - 4.0;
        let mut start = -std::f64::consts::FRAC_PI_2;
        for (i, (_, v)) in slices.iter().enumerate() {
            let sweep = v / total * std::f64::consts::TAU;
            let steps = ((sweep / 0.05).ceil() as usize).max(2);
            let mut pts = vec![center];
            for s in 0..=steps {
                let a = start + sweep * s as f64 / steps as f64;
                pts.push(center + vec2(a.cos() as f32, a.sin() as f32) * radius);
            }
            // A convex polygon needs convex input: slices above half a circle are split in two.
            if sweep > std::f64::consts::PI {
                let mid = pts.len() / 2;
                let mut first = vec![center];
                first.extend_from_slice(&pts[1..=mid]);
                let mut second = vec![center];
                second.extend_from_slice(&pts[mid..]);
                ui.painter().add(Shape::convex_polygon(first, color(i), Stroke::NONE));
                ui.painter().add(Shape::convex_polygon(second, color(i), Stroke::NONE));
            } else {
                ui.painter().add(Shape::convex_polygon(pts, color(i), Stroke::NONE));
            }
            start += sweep;
        }
        ui.vertical(|ui| {
            for (i, (l, v)) in slices.iter().enumerate() {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().rect_filled(r, 2.0, color(i));
                    ui.label(format!("{l}  {:.1}%  ({v})", v / total * 100.0));
                });
            }
        });
    });
}
