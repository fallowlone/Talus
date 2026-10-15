//! Import window: CSV (format options, preview, column mapping) or a JSON array of objects.

use std::path::PathBuf;

use egui::RichText;
use talus_core::import::{CsvPreview, ImportSpec, auto_mapping, preview_csv};
use talus_core::model::{ObjectRef, TableStructure};
use uuid::Uuid;

use crate::state::Loadable;
use crate::theme;
use crate::worker::JobId;

pub struct ImportDialog {
    pub conn: Uuid,
    pub obj: ObjectRef,
    pub structure: Loadable<TableStructure>,
    pub path: String,
    pub delimiter: u8,
    pub has_header: bool,
    pub empty_as_null: bool,
    preview: Option<Result<CsvPreview, String>>,
    previewed: Option<(String, u8, bool)>,
    /// Table column → CSV column index.
    pub mapping: Vec<(String, Option<usize>)>,
    pub running: Option<JobId>,
    pub error: Option<String>,
}

pub enum ImportAction {
    Start(ImportSpec),
    StartJson(PathBuf),
    StartXml(PathBuf),
    Close,
}

impl ImportDialog {
    pub fn new(conn: Uuid, obj: ObjectRef) -> Self {
        Self {
            conn,
            obj,
            structure: Loadable::NotLoaded,
            path: String::new(),
            delimiter: b',',
            has_header: true,
            empty_as_null: true,
            preview: None,
            previewed: None,
            mapping: Vec::new(),
            running: None,
            error: None,
        }
    }

    pub fn is_json(&self) -> bool {
        self.path.trim().to_ascii_lowercase().ends_with(".json")
    }

    pub fn is_xml(&self) -> bool {
        self.path.trim().to_ascii_lowercase().ends_with(".xml")
    }

    /// JSON and XML files have no delimiter / mapping step: keys and names match columns.
    fn is_structured(&self) -> bool {
        self.is_json() || self.is_xml()
    }

    fn refresh_preview(&mut self) {
        if self.is_structured() {
            return;
        }
        let key = (self.path.clone(), self.delimiter, self.has_header);
        if self.previewed.as_ref() == Some(&key) || self.path.trim().is_empty() {
            return;
        }
        self.previewed = Some(key);
        self.preview = Some(
            preview_csv(std::path::Path::new(self.path.trim()), self.delimiter, self.has_header, 10)
                .map_err(|e| e.to_string()),
        );
        self.remap();
    }

    fn remap(&mut self) {
        if let (Some(Ok(p)), Some(st)) = (&self.preview, self.structure.loaded()) {
            let cols: Vec<String> = st.columns.iter().map(|c| c.name.clone()).collect();
            self.mapping = auto_mapping(&p.headers, &cols);
        }
    }

    pub fn set_structure(&mut self, st: TableStructure) {
        self.structure = Loadable::Loaded(st);
        self.remap();
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> Option<ImportAction> {
        let p = theme::current(ctx);
        let mut action = None;
        let mut open = true;
        let is_json = self.is_structured();
        let is_xml = self.is_xml();
        egui::Window::new(format!(
            "Import {} into {}",
            if is_xml {
                "XML"
            } else if is_json {
                "JSON"
            } else {
                "CSV"
            },
            self.obj.display_name()
        ))
        .id(egui::Id::new("import-dialog"))
        .open(&mut open)
        .collapsible(false)
        .default_width(640.0)
        .show(ctx, |ui| {
            egui::Grid::new("import-opts").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                let l = ui.label("File");
                ui.horizontal(|ui| {
                    ui.add(
                        crate::ui::widgets::field(&mut self.path).desired_width(380.0).hint_text("/path/to/data.csv"),
                    )
                    .labelled_by(l.id);
                    if ui.button("Browse…").clicked()
                        && let Some(f) = rfd::FileDialog::new()
                            .add_filter("CSV / JSON / XML / text", &["csv", "tsv", "txt", "json", "xml"])
                            .pick_file()
                    {
                        self.path = f.to_string_lossy().into_owned();
                    }
                });
                ui.end_row();
                if is_json {
                    return;
                }
                ui.label("Delimiter");
                ui.horizontal(|ui| {
                    for (d, label) in [(b',', "Comma"), (b';', "Semicolon"), (b'\t', "Tab"), (b'|', "Pipe")] {
                        ui.selectable_value(&mut self.delimiter, d, label);
                    }
                });
                ui.end_row();
                ui.label("Options");
                ui.horizontal(|ui| {
                    crate::ui::widgets::check(ui, &mut self.has_header, "First row is a header");
                    crate::ui::widgets::check(ui, &mut self.empty_as_null, "Empty values as NULL");
                });
                ui.end_row();
            });
            self.refresh_preview();
            ui.separator();
            if is_json {
                ui.label(
                    RichText::new(
                        "A JSON array of objects or an XML file in the export formats. Keys / names are matched to \
                             column names case-insensitively; missing ones use the column default.",
                    )
                    .color(p.text_dim),
                );
            }
            match self.preview.as_ref().filter(|_| !is_json) {
                None if is_json => {}
                None => {
                    ui.label(RichText::new("Choose a file to preview it.").color(p.text_dim));
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e).color(p.danger));
                }
                Some(Ok(pv)) => {
                    ui.label(RichText::new("Preview").strong());
                    egui::ScrollArea::both().id_salt("import-preview").max_height(160.0).show(ui, |ui| {
                        egui::Grid::new("import-preview-grid").striped(true).show(ui, |ui| {
                            for h in &pv.headers {
                                ui.label(RichText::new(h).strong());
                            }
                            ui.end_row();
                            for r in &pv.rows {
                                for v in r {
                                    ui.label(RichText::new(v).monospace());
                                }
                                ui.end_row();
                            }
                        });
                    });
                    ui.add_space(6.0);
                    ui.label(RichText::new("Column mapping").strong());
                    match self.structure.loaded() {
                        None => {
                            ui.spinner();
                        }
                        Some(_) => {
                            let headers = pv.headers.clone();
                            egui::Grid::new("import-mapping").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                                for (i, (col, src)) in self.mapping.iter_mut().enumerate() {
                                    ui.label(col.as_str());
                                    let shown = src
                                        .and_then(|s| headers.get(s))
                                        .map_or("(skip — use default)".to_string(), |h| h.clone());
                                    super::widgets::combo(("map", i)).selected_text(shown).width(240.0).show_ui(
                                        ui,
                                        |ui| {
                                            ui.selectable_value(src, None, "(skip — use default)");
                                            for (hi, h) in headers.iter().enumerate() {
                                                ui.selectable_value(src, Some(hi), h);
                                            }
                                        },
                                    );
                                    ui.end_row();
                                }
                            });
                        }
                    }
                }
            }
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(p.danger));
            }
            ui.separator();
            ui.horizontal(|ui| {
                let mapped: Vec<(String, usize)> =
                    self.mapping.iter().filter_map(|(c, s)| s.map(|s| (c.clone(), s))).collect();
                let ready = if is_json {
                    !self.path.trim().is_empty() && self.running.is_none()
                } else {
                    matches!(self.preview, Some(Ok(_))) && !mapped.is_empty() && self.running.is_none()
                };
                if ui
                    .add_enabled(ready, theme::primary_button(p, "Import"))
                    .on_hover_text("Runs in one transaction; any error imports nothing")
                    .clicked()
                {
                    action = Some(if is_xml {
                        ImportAction::StartXml(PathBuf::from(self.path.trim()))
                    } else if is_json {
                        ImportAction::StartJson(PathBuf::from(self.path.trim()))
                    } else {
                        ImportAction::Start(ImportSpec {
                            path: PathBuf::from(self.path.trim()),
                            delimiter: self.delimiter,
                            has_header: self.has_header,
                            mapping: mapped,
                            empty_as_null: self.empty_as_null,
                        })
                    });
                }
                if self.running.is_some() {
                    ui.spinner();
                    ui.label("Importing…");
                }
            });
        });
        if !open && self.running.is_none() {
            action = Some(ImportAction::Close);
        }
        action
    }
}
