use std::sync::Arc;

use talus_app::{App, AppEnv};
use talus_core::store::config::AppPaths;

fn main() -> eframe::Result {
    // `talus --run-job <id|name>`: run a saved job headless (used by launchd), then exit.
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--run-job") {
        match args.get(i + 1) {
            Some(sel) => std::process::exit(talus_app::headless::run_job_cli(sel)),
            None => {
                eprintln!("usage: talus --run-job <job id or name>");
                std::process::exit(2);
            }
        }
    }
    let started = std::time::Instant::now();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("talus-io")
        // Driver futures (SQL Server in particular) are large; do not rely on the 2 MiB default.
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("failed to start async runtime");
    let paths = match AppPaths::system() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Cannot create the application support directory: {e}");
            std::process::exit(1);
        }
    };
    #[cfg(target_os = "macos")]
    let secrets: Arc<dyn talus_core::store::secrets::SecretStore> = Arc::new(talus_core::store::secrets::KeychainStore);
    #[cfg(not(target_os = "macos"))]
    let secrets: Arc<dyn talus_core::store::secrets::SecretStore> =
        Arc::new(talus_core::store::secrets::MemoryStore::default());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Talus")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    let handle = rt.handle().clone();
    eframe::run_native(
        "Talus",
        options,
        Box::new(move |cc| {
            let mut app = App::new(&cc.egui_ctx, AppEnv { paths, secrets, rt: handle });
            #[cfg(target_os = "macos")]
            app.set_native_menu(talus_app::native_menu::install(&cc.egui_ctx));
            // `TALUS_STARTUP_PROBE=1`: print time to first frame and quit (startup measurement).
            if std::env::var_os("TALUS_STARTUP_PROBE").is_some() {
                app.startup_probe = Some(started);
            }
            Ok(Box::new(app))
        }),
    )
}
