#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod system;
#[cfg(any(windows, target_os = "macos"))]
mod tray;
mod worker;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,sideward_core=info,isideload=info".into()),
        )
        .init();

    // One copy at a time: a second launch just brings the running window forward
    let instance_listener = match system::single_instance() {
        system::Instance::First(listener) => Some(listener),
        system::Instance::AlreadyRunning => return Ok(()),
    };
    if let Err(e) = sideward_core::init() {
        eprintln!("Startup failed: {e:#}");
    }
    let background = std::env::args().any(|a| a == system::BACKGROUND_ARG);

    let icon = eframe::egui::IconData {
        rgba: system::icon_rgba(64),
        width: 64,
        height: 64,
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Sideward")
            .with_icon(icon)
            .with_inner_size([820.0, 680.0])
            .with_min_inner_size([620.0, 520.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "Sideward",
        options,
        Box::new(move |cc| {
            let launch = app::Launch {
                background,
                instance_listener,
            };
            Ok(Box::new(app::SidewardApp::new(cc, launch)?))
        }),
    )
}
