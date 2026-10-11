//! The macOS menu bar. Items carry ids; clicks arrive as ids on a channel that the app turns
//! into `Action`s. There are no key equivalents here and no Edit menu: ⌘C/⌘V and the other
//! shortcuts stay with egui, because an NSMenu key equivalent would swallow the key event
//! before the text editors see it.

use std::sync::mpsc::{Receiver, channel};

use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

/// `(id, label)` of the items under each submenu, in display order; `None` is a separator.
pub const FILE: &[Option<(&str, &str)>] = &[
    Some(("new-connection", "New Connection…")),
    Some(("new-query", "New Query")),
    Some(("open-file", "Open SQL File…")),
    None,
    Some(("close-tab", "Close Tab")),
];

pub const VIEW: &[Option<(&str, &str)>] =
    &[Some(("toggle-nav", "Toggle Navigation Pane")), Some(("toggle-info", "Toggle Information Pane"))];

pub const TOOLS: &[Option<(&str, &str)>] = &[
    Some(("sync", "Structure Synchronization…")),
    Some(("model", "Data Modeling")),
    Some(("builder", "Query Builder")),
    Some(("users", "Users and Privileges")),
    Some(("routines", "Routines and Triggers")),
    None,
    Some(("dump-data", "Dump Structure and Data…")),
    Some(("dump-structure", "Dump Structure Only…")),
    Some(("run-file", "Execute SQL File…")),
    None,
    Some(("dict-html", "Data Dictionary (HTML)…")),
    Some(("dict-md", "Data Dictionary (Markdown)…")),
    Some(("jobs", "Automation (Jobs)…")),
    Some(("find", "Find in Database")),
    Some(("history", "History Log")),
];

fn submenu(title: &str, items: &[Option<(&str, &str)>]) -> Submenu {
    let sub = Submenu::new(title, true);
    for item in items {
        match item {
            Some((id, label)) => {
                let _ = sub.append(&MenuItem::with_id(*id, *label, true, None));
            }
            None => {
                let _ = sub.append(&PredefinedMenuItem::separator());
            }
        }
    }
    sub
}

/// Installs the menu bar; must run on the main thread (inside eframe's creation callback).
pub fn install(ctx: &egui::Context) -> Receiver<String> {
    let (tx, rx) = channel();
    let repaint = ctx.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = tx.send(e.id.0);
        repaint.request_repaint();
    }));
    let menu = Menu::new();
    let app = Submenu::new("Talus", true);
    let _ = app.append_items(&[
        &MenuItem::with_id("about", "About Talus", true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id("settings", "Settings…", true, None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::hide(None),
        &PredefinedMenuItem::hide_others(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::quit(None),
    ]);
    let window = Submenu::new("Window", true);
    let _ = window.append_items(&[&PredefinedMenuItem::minimize(None), &PredefinedMenuItem::maximize(None)]);
    let _ =
        menu.append_items(&[&app, &submenu("File", FILE), &submenu("View", VIEW), &submenu("Tools", TOOLS), &window]);
    menu.init_for_nsapp();
    window.set_as_windows_menu_for_nsapp();
    // The menu must outlive the app: AppKit only keeps a weak reference.
    std::mem::forget(menu);
    rx
}
