//! The print window driven headlessly: page setup changes the sheet count, printing goes through CUPS
//! commands (replaced here by scripts that record their arguments, so no real printer is ever used), the
//! queued job can be cancelled, and printer problems are shown.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use talus_app::print_fonts::PrintFonts;
use talus_app::ui::print_dialog::PrintDialog;
use talus_core::model::Engine;
use talus_core::model_design::{DesignModel, LType, MColumn, MFk, MTable, ModelKind};
use talus_core::print::{Fit, Paper};

fn model() -> DesignModel {
    let col = |n: &str, pk: bool| MColumn {
        name: n.into(),
        ty: LType::Integer,
        nullable: !pk,
        primary_key: pk,
        auto_increment: false,
        default: None,
    };
    let mut tables = Vec::new();
    for i in 0..12 {
        let mut t = MTable::new(&format!("table_{i:02}"), (i % 4) as f32 * 300.0, (i / 4) as f32 * 330.0);
        t.columns = vec![col("id", true), col("name", false), col("parent_id", false)];
        if i > 0 {
            t.foreign_keys.push(MFk {
                name: format!("fk_{i}"),
                columns: vec!["parent_id".into()],
                ref_table: format!("table_{:02}", i / 2),
                ref_columns: vec!["id".into()],
                on_update: "NO ACTION".into(),
                on_delete: "NO ACTION".into(),
            });
        }
        tables.push(t);
    }
    DesignModel { name: "Big model".into(), kind: ModelKind::Physical, tables }
}

/// The application's fonts and theme take effect one frame after they are set: true once they are in.
fn installed(ctx: &egui::Context) -> bool {
    if ctx.data(|d| d.get_temp::<bool>(egui::Id::new("talus-installed"))).is_none() {
        talus_app::theme::install_fonts(ctx);
        talus_app::theme::apply(ctx, talus_core::store::config::Theme::Light, 13.0);
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("talus-installed"), true));
        return false;
    }
    true
}

fn fake(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

fn step_until(h: &mut Harness<'_, PrintDialog>, what: &str, cond: impl Fn(&Harness<'_, PrintDialog>) -> bool) {
    for _ in 0..400 {
        h.step();
        if cond(h) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for: {what}; log: {:?}", h.state().log());
}

/// Opens the combo showing `current` and chooses `option`.
fn pick(h: &mut Harness<'_, PrintDialog>, current: &'static str, option: &'static str) {
    // Pointer clicks on combos inside a window are unreliable under kittest; the accessibility
    // action is the proven pattern (see `cloud_preset_fills_engine_port_and_tls`).
    h.get_by(|n: &egui_kittest::kittest::AccessKitNode<'_>| {
        n.role() == Role::ComboBox && n.value().as_deref() == Some(current)
    })
    .click_accesskit();
    let opt = move |n: &egui_kittest::kittest::AccessKitNode<'_>| {
        n.role() == Role::Button
            && n.label().as_deref().is_some_and(|l| l == option || l.starts_with(&format!("{option} (")))
    };
    step_until(h, option, |h| h.query_by(opt).is_some());
    h.get_by(opt).click_accesskit();
    h.run();
    step_until(h, option, |h| combo_shows(h, option));
}

fn has(h: &Harness<'_, PrintDialog>, text: &str) -> bool {
    h.query_all_by_label_contains(text).next().is_some()
}

/// A combo shows `name`: a combo's current choice is its accessible value, not a label.
fn combo_shows(h: &Harness<'_, PrintDialog>, name: &str) -> bool {
    h.query_by(|n: &egui_kittest::kittest::AccessKitNode<'_>| {
        n.role() == Role::ComboBox && n.value().as_deref() == Some(name)
    })
    .is_some()
}

/// Clicks the Print button (the window title and the section header also contain "Print").
fn click_print(h: &mut Harness<'_, PrintDialog>) {
    h.get_by(|n: &egui_kittest::kittest::AccessKitNode<'_>| {
        n.role() == Role::Button && n.label().is_some_and(|l| l.contains("Print"))
    })
    .click();
}

#[test]
fn page_setup_preview_printing_cancelling_and_errors() {
    let dir = std::env::temp_dir().join(format!("talus-print-ui-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("calls.log");
    // A fake CUPS: two printers (Home is the default), `lp` records its arguments and the size of the file it
    // was given, the queue keeps the job until `cancel` runs.
    let queue = dir.join("queue");
    let lp = fake(
        &dir,
        "lp",
        &format!(
            "case \"$*\" in *Broken*) echo 'lp: The printer or class does not exist.' >&2; exit 1;; esac\n\
             for last; do :; done\n\
             echo \"lp $* size=$(wc -c < \"$last\" | tr -d ' ') head=$(head -c 8 \"$last\")\" >> {log}\n\
             touch {queue}; echo 'request id is Home-42 (1 file(s))'",
            log = log.display(),
            queue = queue.display()
        ),
    );
    let lpstat = fake(
        &dir,
        "lpstat",
        &format!(
            "case \"$1\" in -e) printf 'Office\\nHome\\nBroken\\n';; -d) echo 'system default destination: Home';; -W) [ -e {q} ] && echo 'Home-42 me 1024 Mon';; esac",
            q = queue.display()
        ),
    );
    let cancel = fake(&dir, "cancel", &format!("echo \"cancel $@\" >> {}; rm -f {}", log.display(), queue.display()));
    // The commands are read from the environment at each call; this file's single test owns the variables.
    unsafe {
        std::env::set_var("TALUS_LP", &lp);
        std::env::set_var("TALUS_LPSTAT", &lpstat);
        std::env::set_var("TALUS_CANCEL", &cancel);
    }

    let fonts = PrintFonts::default();
    let scene = talus_app::print_scene::model_scene(&model(), Engine::Postgres, &fonts);
    let dialog = PrintDialog::new(scene);
    let mut h = Harness::builder().with_size(egui::vec2(1100.0, 760.0)).build_ui_state(
        |ui, d: &mut PrintDialog| {
            if installed(ui.ctx()) {
                d.ui(ui.ctx());
            }
        },
        dialog,
    );
    h.step();
    assert_eq!(h.state().setup().paper, Paper::A4);
    assert!(matches!(h.state().setup().fit, Fit::Width));
    let sheets_fit = h.state().layout().pages.len();
    assert!(has(&h, &format!("{sheets_fit} sheet(s)")), "the summary line shows the sheet count");
    assert!(has(&h, "Sheet 1 of"), "the preview is on");

    // Fixed scale: the model needs more sheets, and the preview follows.
    pick(&mut h, "Fit to page width", "Custom scale");
    let actual = h.state().layout().pages.len();
    assert!(actual > sheets_fit, "{actual} sheets at 100% vs {sheets_fit} fitted");
    assert!((h.state().layout().percent() - 100.0).abs() < 0.01);
    // Landscape and another paper size change the sheets too.
    h.get_by_role_and_label(Role::RadioButton, "Landscape").click();
    h.run();
    assert!(h.state().setup().landscape);
    let before = h.state().layout().pages.len();
    assert_ne!(before, actual);
    shot(&mut h, "print-dialog");

    // The printers were listed; the default is chosen.
    step_until(&mut h, "printers", |h| combo_shows(h, "Home"));
    // Print: the PDF reaches `lp` as a real PDF with the chosen media and copies.
    click_print(&mut h);
    step_until(&mut h, "job queued", |h| h.state().job().is_some());
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("-d Home -n 1") && calls.contains("-o media=A4"), "{calls}");
    assert!(calls.contains("head=%PDF-1.4"), "{calls}");
    let size: usize = calls.split("size=").nth(1).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    assert!(size > 5000, "a real document was sent: {size} bytes");
    assert!(has(&h, "Queued as Home-42") && has(&h, "Cancel job"));
    // Cancel the job: `cancel` runs, the window forgets it.
    h.get_by_label("Cancel job").click();
    step_until(&mut h, "job cancelled", |h| h.state().job().is_none());
    assert!(std::fs::read_to_string(&log).unwrap().contains("cancel -- Home-42"));
    assert!(has(&h, "Print job cancelled"));

    // A printer that refuses: the reason is shown and nothing is pending.
    drop(h);
    let dialog = PrintDialog::new(talus_app::print_scene::model_scene(&model(), Engine::Postgres, &fonts));
    let mut h = Harness::builder().with_size(egui::vec2(1100.0, 760.0)).build_ui_state(
        |ui, d: &mut PrintDialog| {
            if installed(ui.ctx()) {
                d.ui(ui.ctx());
            }
        },
        dialog,
    );
    step_until(&mut h, "printers", |h| combo_shows(h, "Home"));
    pick(&mut h, "Home", "Broken");
    click_print(&mut h);
    step_until(&mut h, "failure shown", |h| has(h, "Printing failed"));
    assert!(has(&h, "does not exist") && h.state().job().is_none());

    // Saving writes the same document.
    let mut d = PrintDialog::new(talus_app::print_scene::model_scene(&model(), Engine::Postgres, &fonts));
    let out = dir.join("model.pdf");
    d.save_to(&out);
    let bytes = std::fs::read(&out).unwrap();
    assert!(bytes.starts_with(b"%PDF-1.4") && bytes.ends_with(b"%%EOF\n"));
    d.save_to(&dir.join("missing-dir/x.pdf"));
    assert!(d.log().last().is_some_and(|(err, text)| *err && text.contains("Cannot write")), "{:?}", d.log());
    unsafe {
        std::env::remove_var("TALUS_LP");
        std::env::remove_var("TALUS_LPSTAT");
        std::env::remove_var("TALUS_CANCEL");
    }
}

fn shot(h: &mut Harness<'_, PrintDialog>, name: &str) {
    let Ok(dir) = std::env::var("TALUS_SCREENSHOT_DIR") else { return };
    h.step();
    h.render().expect("render").save(Path::new(&dir).join(format!("{name}.png"))).expect("save png");
}
