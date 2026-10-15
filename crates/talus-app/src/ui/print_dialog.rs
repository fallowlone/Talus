//! Print window of the diagrams: page setup, a preview of every sheet exactly as it will be cut, and the three
//! ways out — a PDF file, a printer (CUPS) or the system PDF viewer with its own print dialog.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, pos2, vec2};
use egui_phosphor::regular as icons;
use talus_core::print::{
    Align, Fit, FontStyle, Layout, Meta, PageSetup, Paper, Prim, Rgb, Scene, paginate, render_pdf,
};

use crate::print_fonts::PrintFonts;
use crate::print_job::{self, JobId, Printer};
use crate::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FitMode {
    Width,
    Pages,
    Percent,
}

impl FitMode {
    const ALL: [FitMode; 3] = [FitMode::Width, FitMode::Pages, FitMode::Percent];
    fn label(self) -> &'static str {
        match self {
            FitMode::Width => "Fit to page width",
            FitMode::Pages => "Fit to a number of pages",
            FitMode::Percent => "Custom scale",
        }
    }
}

/// What a background operation reports back.
enum Event {
    Printers(Result<Vec<Printer>, String>),
    Submitted(Result<JobId, String>),
    /// The queue no longer lists the job.
    Finished(JobId),
    Cancelled(Result<(), String>),
    Opened(Result<(), String>),
}

pub struct PrintDialog {
    scene: Scene,
    fonts: PrintFonts,
    setup: PageSetup,
    layout: Layout,
    fit: FitMode,
    pages_across: u32,
    pages_down: u32,
    percent: f32,
    /// Sheet shown in the preview (index into `layout.pages`).
    sheet: usize,
    printers: Option<Result<Vec<Printer>, String>>,
    printer: Option<String>,
    copies: u32,
    /// `(is_error, text)` lines under the buttons.
    log: Vec<(bool, String)>,
    job: Option<JobId>,
    /// Stops the thread that watches the queue.
    watching: Arc<AtomicBool>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    printed_at: String,
}

impl PrintDialog {
    pub fn new(scene: Scene) -> Self {
        let fonts = PrintFonts::default();
        let setup = PageSetup::default();
        let layout = paginate(&scene, &setup, &fonts);
        let (tx, rx) = mpsc::channel();
        let mut d = Self {
            scene,
            fonts,
            setup,
            layout,
            fit: FitMode::Width,
            pages_across: 1,
            pages_down: 1,
            percent: 100.0,
            sheet: 0,
            printers: None,
            printer: None,
            copies: 1,
            log: Vec::new(),
            job: None,
            watching: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
            printed_at: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
        };
        d.relayout();
        d
    }

    pub fn setup(&self) -> &PageSetup {
        &self.setup
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn log(&self) -> &[(bool, String)] {
        &self.log
    }

    pub fn job(&self) -> Option<&JobId> {
        self.job.as_ref()
    }

    fn relayout(&mut self) {
        self.setup.fit = match self.fit {
            FitMode::Width => Fit::Width,
            FitMode::Pages => Fit::Pages { cols: self.pages_across, rows: self.pages_down },
            FitMode::Percent => Fit::Percent(self.percent),
        };
        self.layout = paginate(&self.scene, &self.setup, &self.fonts);
        self.sheet = self.sheet.min(self.layout.pages.len().saturating_sub(1));
    }

    pub fn set_setup(&mut self, setup: PageSetup) {
        self.setup = setup;
        match setup.fit {
            Fit::Width => self.fit = FitMode::Width,
            Fit::Pages { cols, rows } => (self.fit, self.pages_across, self.pages_down) = (FitMode::Pages, cols, rows),
            Fit::Percent(p) => (self.fit, self.percent) = (FitMode::Percent, p),
        }
        self.relayout();
    }

    /// The PDF of the current setup.
    pub fn pdf(&self) -> Vec<u8> {
        let meta = Meta {
            title: self.scene.title.clone(),
            subtitle: self.scene.subtitle.clone(),
            printed_at: self.printed_at.clone(),
        };
        render_pdf(&self.scene, &self.layout, &self.setup, &self.fonts, &meta)
    }

    pub fn save_to(&mut self, path: &Path) {
        match std::fs::write(path, self.pdf()) {
            Ok(()) => {
                self.log.push((false, format!("Saved {} sheet(s) to {}", self.layout.pages.len(), path.display())))
            }
            Err(e) => self.log.push((true, format!("Cannot write {}: {e}", path.display()))),
        }
    }

    /// A temporary file holding the PDF, for the printer or the viewer.
    fn temp_pdf(&mut self) -> Option<PathBuf> {
        let path = std::env::temp_dir().join(format!("talus-print-{}.pdf", uuid::Uuid::new_v4().simple()));
        match std::fs::write(&path, self.pdf()) {
            Ok(()) => Some(path),
            Err(e) => {
                self.log.push((true, format!("Cannot prepare the print file: {e}")));
                None
            }
        }
    }

    pub fn refresh_printers(&mut self, ctx: &egui::Context) {
        self.printers = None;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Event::Printers(print_job::printers()));
            ctx.request_repaint();
        });
    }

    pub fn print(&mut self, ctx: &egui::Context) {
        let Some(printer) = self.printer.clone() else {
            self.log.push((true, "Choose a printer first".into()));
            return;
        };
        let Some(path) = self.temp_pdf() else { return };
        self.log.push((false, format!("Sending {} sheet(s) to {printer}…", self.layout.pages.len())));
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        let (copies, paper, title) = (self.copies, self.setup.paper, self.scene.title.clone());
        std::thread::spawn(move || {
            let r = print_job::submit(&path, &printer, copies, paper, &title);
            // CUPS has its own copy once `lp` returns.
            let _ = std::fs::remove_file(&path);
            let _ = tx.send(Event::Submitted(r));
            ctx.request_repaint();
        });
    }

    pub fn cancel_job(&mut self, ctx: &egui::Context) {
        let Some(job) = self.job.clone() else { return };
        self.watching.store(false, Ordering::SeqCst);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Event::Cancelled(print_job::cancel(&job)));
            ctx.request_repaint();
        });
    }

    fn open_in_viewer(&mut self, ctx: &egui::Context) {
        let Some(path) = self.temp_pdf() else { return };
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Event::Opened(print_job::open_in_viewer(&path)));
            ctx.request_repaint();
        });
    }

    /// Watches the queue until the job is gone.
    fn watch(&mut self, job: JobId, ctx: &egui::Context) {
        self.watching.store(false, Ordering::SeqCst);
        let stop = Arc::new(AtomicBool::new(true));
        self.watching = stop.clone();
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(2));
                if !stop.load(Ordering::SeqCst) {
                    return;
                }
                if !print_job::is_pending(&job) {
                    let _ = tx.send(Event::Finished(job));
                    ctx.request_repaint();
                    return;
                }
            }
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Printers(r) => {
                    if let Ok(list) = &r
                        && self.printer.is_none()
                    {
                        self.printer = list.iter().find(|p| p.is_default).or(list.first()).map(|p| p.name.clone());
                    }
                    self.printers = Some(r);
                }
                Event::Submitted(Ok(job)) => {
                    self.log.push((false, format!("Queued as {}", job.0)));
                    self.job = Some(job.clone());
                    self.watch(job, ctx);
                }
                Event::Submitted(Err(e)) => self.log.push((true, format!("Printing failed: {e}"))),
                Event::Finished(job) => {
                    if self.job.as_ref() == Some(&job) {
                        self.log.push((false, format!("{} left the queue", job.0)));
                        self.job = None;
                    }
                }
                Event::Cancelled(Ok(())) => {
                    self.log.push((false, "Print job cancelled".into()));
                    self.job = None;
                }
                Event::Cancelled(Err(e)) => self.log.push((true, format!("Cannot cancel the job: {e}"))),
                Event::Opened(Ok(())) => self.log.push((false, "Opened in the PDF viewer".into())),
                Event::Opened(Err(e)) => self.log.push((true, format!("Cannot open the PDF viewer: {e}"))),
            }
        }
    }

    /// The window. Returns `false` once it is closed.
    pub fn ui(&mut self, ctx: &egui::Context) -> bool {
        if self.printers.is_none() {
            self.refresh_printers(ctx);
            self.printers = Some(Ok(Vec::new()));
            self.printer = None;
        }
        self.poll(ctx);
        let mut open = true;
        egui::Window::new(format!("Print — {}", self.scene.title))
            .id(egui::Id::new("print-dialog"))
            .open(&mut open)
            .default_size([1000.0, 680.0])
            .collapsible(false)
            .show(ctx, |ui| self.contents(ui));
        if !open {
            self.watching.store(false, Ordering::SeqCst);
        }
        open
    }

    pub fn contents(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(300.0);
                self.settings(ui);
                ui.add_space(10.0);
                self.output(ui, &ctx);
            });
            ui.separator();
            ui.vertical(|ui| self.preview(ui));
        });
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        let before = (self.setup, self.fit, self.pages_across, self.pages_down, self.percent);
        ui.label(RichText::new("Page").font(theme::semibold(13.0)));
        egui::Grid::new("print-page").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
            let l = ui.label("Paper");
            super::widgets::combo("print-paper")
                .selected_text(self.setup.paper.label())
                .show_ui(ui, |ui| {
                    for p in Paper::ALL {
                        ui.selectable_value(&mut self.setup.paper, p, p.label());
                    }
                })
                .response
                .labelled_by(l.id);
            ui.end_row();
            ui.label("Orientation");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.setup.landscape, false, "Portrait");
                ui.radio_value(&mut self.setup.landscape, true, "Landscape");
            });
            ui.end_row();
            let l = ui.label("Margins (mm)");
            ui.add(egui::DragValue::new(&mut self.setup.margin_mm).range(0.0..=40.0).speed(0.5)).labelled_by(l.id);
            ui.end_row();
        });
        ui.add_space(6.0);
        ui.label(RichText::new("Scale").font(theme::semibold(13.0)));
        super::widgets::combo("print-fit").selected_text(self.fit.label()).width(260.0).show_ui(ui, |ui| {
            for m in FitMode::ALL {
                ui.selectable_value(&mut self.fit, m, m.label());
            }
        });
        match self.fit {
            FitMode::Width => {}
            FitMode::Pages => {
                ui.horizontal(|ui| {
                    let l = ui.label("Across");
                    ui.add(egui::DragValue::new(&mut self.pages_across).range(1..=12)).labelled_by(l.id);
                    let l = ui.label("Down");
                    ui.add(egui::DragValue::new(&mut self.pages_down).range(1..=12)).labelled_by(l.id);
                });
            }
            FitMode::Percent => {
                ui.horizontal(|ui| {
                    let l = ui.label("Percent");
                    ui.add(egui::DragValue::new(&mut self.percent).range(5.0..=400.0).suffix(" %").speed(1.0))
                        .labelled_by(l.id);
                });
            }
        }
        ui.add_space(4.0);
        ui.checkbox(&mut self.setup.keep_whole, "Keep tables whole when splitting")
            .on_hover_text("Moves the cut between sheets into the gaps between tables where there is room");
        ui.checkbox(&mut self.setup.header, "Title at the top of each sheet");
        ui.checkbox(&mut self.setup.footer, "Page numbers and date at the bottom");
        if before != (self.setup, self.fit, self.pages_across, self.pages_down, self.percent) {
            self.relayout();
        }
        ui.add_space(6.0);
        let l = &self.layout;
        ui.label(format!(
            "{} sheet(s): {} across × {} down, at {:.0}%",
            l.pages.len(),
            l.cols.len(),
            l.rows.len(),
            l.percent()
        ));
        if l.split_cards > 0 {
            ui.label(
                RichText::new(format!("{} table(s) are cut by a sheet boundary", l.split_cards))
                    .color(theme::current(ui.ctx()).warn),
            );
        }
        if let Some(w) = &l.warning {
            ui.label(RichText::new(w).color(theme::current(ui.ctx()).warn));
        }
    }

    fn output(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label(RichText::new("Print").font(theme::semibold(13.0)));
        let list = match &self.printers {
            Some(Ok(l)) => l.clone(),
            _ => Vec::new(),
        };
        ui.horizontal(|ui| {
            let l = ui.label("Printer");
            let shown = self.printer.clone().unwrap_or_else(|| "none found".into());
            super::widgets::combo("print-printer")
                .selected_text(shown)
                .width(190.0)
                .show_ui(ui, |ui| {
                    for p in &list {
                        let label = if p.is_default { format!("{} (default)", p.name) } else { p.name.clone() };
                        ui.selectable_value(&mut self.printer, Some(p.name.clone()), label);
                    }
                })
                .response
                .labelled_by(l.id);
            if ui.button(icons::ARROW_CLOCKWISE).on_hover_text("Look for printers again").clicked() {
                self.printer = None;
                self.refresh_printers(ctx);
            }
        });
        if let Some(Err(e)) = &self.printers {
            ui.label(RichText::new(format!("Cannot list printers: {e}")).color(theme::current(ui.ctx()).danger));
        }
        ui.horizontal(|ui| {
            let l = ui.label("Copies");
            ui.add(egui::DragValue::new(&mut self.copies).range(1..=99)).labelled_by(l.id);
        });
        let p = theme::current(ui.ctx());
        ui.horizontal_wrapped(|ui| {
            let can_print = self.printer.is_some() && self.job.is_none();
            if ui.add_enabled(can_print, theme::primary_button(p, format!("{} Print", icons::PRINTER))).clicked() {
                self.print(ctx);
            }
            if ui.button(format!("{} Save as PDF…", icons::FILE_PDF)).clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("PDF", &["pdf"])
                    .set_file_name(format!("{}.pdf", self.scene.title.replace(['/', '\\', ':'], "-")))
                    .save_file()
            {
                self.save_to(&path);
            }
            if ui.button(format!("{} Open in Preview", icons::ARROW_SQUARE_OUT)).clicked() {
                self.open_in_viewer(ctx);
            }
        });
        if let Some(job) = self.job.clone() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Job {} is waiting or printing", job.0));
                if ui.button("Cancel job").clicked() {
                    self.cancel_job(ctx);
                }
            });
        }
        for (error, line) in self.log.iter().rev().take(4).rev() {
            let color = if *error { p.danger } else { p.text_dim };
            ui.label(RichText::new(line).small().color(color));
        }
    }

    fn preview(&mut self, ui: &mut egui::Ui) {
        let total = self.layout.pages.len();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.sheet > 0, egui::Button::new(icons::CARET_LEFT))
                .on_hover_text("Previous sheet")
                .clicked()
            {
                self.sheet -= 1;
            }
            ui.label(format!("Sheet {} of {total}", self.sheet + 1));
            if ui
                .add_enabled(self.sheet + 1 < total, egui::Button::new(icons::CARET_RIGHT))
                .on_hover_text("Next sheet")
                .clicked()
            {
                self.sheet += 1;
            }
            if let Some(slot) = self.layout.pages.get(self.sheet) {
                ui.label(
                    RichText::new(format!(
                        "column {} of {}, row {} of {}",
                        slot.col + 1,
                        self.layout.cols.len(),
                        slot.row + 1,
                        self.layout.rows.len()
                    ))
                    .color(theme::current(ui.ctx()).text_dim),
                );
            }
        });
        let avail = ui.available_size();
        let k = ((avail.x - 8.0) / self.layout.page_w).min((avail.y - 8.0).max(200.0) / self.layout.page_h).max(0.2);
        let (rect, _) = ui
            .allocate_exact_size(vec2(self.layout.page_w * k, self.layout.page_h * k) + vec2(8.0, 8.0), Sense::hover());
        let sheet =
            Rect::from_min_size(rect.min + vec2(4.0, 4.0), vec2(self.layout.page_w * k, self.layout.page_h * k));
        self.draw_sheet(&ui.painter_at(rect), sheet, k);
    }

    /// One sheet, drawn from the same scene and layout the PDF is written from.
    fn draw_sheet(&self, painter: &egui::Painter, sheet: Rect, k: f32) {
        let l = &self.layout;
        painter.rect_filled(sheet.translate(vec2(3.0, 3.0)), 2.0, Color32::from_black_alpha(40));
        painter.rect_filled(sheet, 1.0, Color32::WHITE);
        painter.rect_stroke(sheet, 1.0, Stroke::new(1.0, Color32::from_gray(190)), StrokeKind::Inside);
        let Some(page) = l.pages.get(self.sheet) else { return };
        let ink = Color32::from_gray(30);
        let dim = Color32::from_gray(120);
        let at = |x: f32, y: f32| sheet.min + vec2(x * k, y * k);
        let (cx, cy, cw, ch) = l.content;
        if self.setup.header {
            painter.text(at(cx, cy - 6.0), Align2::LEFT_BOTTOM, &self.scene.title, theme::semibold(11.0 * k), ink);
            painter.text(
                at(cx + cw, cy - 6.0),
                Align2::RIGHT_BOTTOM,
                &self.scene.subtitle,
                FontId::proportional(8.5 * k),
                dim,
            );
            painter.hline(
                at(cx, cy - 4.0).x..=at(cx + cw, cy - 4.0).x,
                at(cx, cy - 4.0).y,
                Stroke::new(1.0, Color32::from_gray(200)),
            );
        }
        if self.setup.footer {
            let y = cy + ch + l.footer_h - 5.0;
            painter.text(
                at(cx, y),
                Align2::LEFT_BOTTOM,
                format!("Page {} of {}", page.number, l.pages.len()),
                FontId::proportional(8.0 * k),
                dim,
            );
            painter.text(
                at(cx + cw, y),
                Align2::RIGHT_BOTTOM,
                format!("{} · Talus", self.printed_at),
                FontId::proportional(8.0 * k),
                dim,
            );
            painter.hline(
                at(cx, cy + ch + 4.0).x..=at(cx + cw, cy + ch + 4.0).x,
                at(cx, cy + ch + 4.0).y,
                Stroke::new(1.0, Color32::from_gray(200)),
            );
        }
        let area = Rect::from_min_max(at(cx, cy), at(cx + cw, cy + ch));
        let clip = painter.with_clip_rect(area);
        let (x0, y0, x1, y1) = page.src;
        let m = |x: f32, y: f32| -> Pos2 { at(cx + (x - x0) * l.scale, cy + (y - y0) * l.scale) };
        let s = l.scale * k;
        let color = |c: Rgb| Color32::from_rgb(c.0, c.1, c.2);
        for p in &self.scene.prims {
            let b = p.bounds(&self.fonts);
            if !(b.2 > x0 && b.0 < x1 && b.3 > y0 && b.1 < y1) {
                continue;
            }
            match p {
                Prim::Rect { x, y, w, h, radius, fill, stroke } => {
                    let r = Rect::from_min_size(m(*x, *y), vec2(w * s, h * s));
                    let cr = CornerRadius::same((radius * s).round().clamp(0.0, 255.0) as u8);
                    if let Some(f) = fill {
                        clip.rect_filled(r, cr, color(*f));
                    }
                    if let Some((c, sw)) = stroke {
                        clip.rect_stroke(r, cr, Stroke::new((sw * s).max(0.5), color(*c)), StrokeKind::Middle);
                    }
                }
                Prim::Header { x, y, w, h, radius, fill } => {
                    let r = Rect::from_min_size(m(*x, *y), vec2(w * s, h * s));
                    let rad = (radius * s).round().clamp(0.0, 255.0) as u8;
                    clip.rect_filled(r, CornerRadius { nw: rad, ne: rad, sw: 0, se: 0 }, color(*fill));
                }
                Prim::Line { points, color: c, width } => {
                    let pts: Vec<Pos2> = points.iter().map(|(x, y)| m(*x, *y)).collect();
                    clip.line(pts, Stroke::new((width * s).max(0.5), color(*c)));
                }
                Prim::Dot { x, y, r, fill } => {
                    clip.circle_filled(m(*x, *y), r * s, color(*fill));
                }
                Prim::Text { x, y, text, size, style, color: c, align } => {
                    let anchor = match align {
                        Align::Left => Align2::LEFT_BOTTOM,
                        Align::Right => Align2::RIGHT_BOTTOM,
                        Align::Center => Align2::CENTER_BOTTOM,
                    };
                    let font = match style {
                        FontStyle::Mono => FontId::monospace(size * s),
                        FontStyle::Semibold => theme::semibold(size * s),
                        FontStyle::Regular => FontId::proportional(size * s),
                    };
                    clip.text(m(*x, *y), anchor, text, font, color(*c));
                }
            }
        }
        clip.rect_stroke(area, 0.0, Stroke::new(0.5, Color32::from_gray(235)), StrokeKind::Inside);
        let _ = pos2(0.0, 0.0);
    }
}
