//! Talus desktop application (egui). Exposed as a library so UI tests can drive `App`.

pub mod actions;
pub mod app;
pub mod headless;
#[cfg(target_os = "macos")]
pub mod launchd;
#[cfg(target_os = "macos")]
pub mod native_menu;
pub mod state;
pub mod theme;
pub mod ui;
pub mod worker;

pub use app::{App, AppEnv};
