mod key_manager;
mod logger;
mod server;
mod ui;

use key_manager::KeyManager;
use server::{create_router, AppState};
use std::net::SocketAddr;
use ui::ProxyApp;

const LISTEN: &str = "127.0.0.1:9090";

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args: Vec<String> = std::env::args().collect();
    let is_headless = args.iter().any(|a| a == "--headless" || a == "-h");

    // Initialize key manager
    let key_manager = KeyManager::new();
    let state = AppState {
        key_manager: key_manager.clone(),
    };

    // Spawn Tokio server on a background thread so GUI never blocks
    let server_state = state.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create Tokio runtime");

        rt.block_on(async move {
            let app = create_router(server_state);
            let addr: SocketAddr = LISTEN.parse().unwrap();
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            logger::LOGGER.push("ℹ️", format!("Engine server live on http://{LISTEN} (Auth: Bearer public)"));
            axum::serve(listener, app).await.unwrap();
        });
    });

    // If headless mode requested or no graphical display found on Linux, run CLI loop
    let has_display = cfg!(target_os = "windows")
        || std::env::var("DISPLAY").is_ok()
        || std::env::var("WAYLAND_DISPLAY").is_ok();

    if is_headless || !has_display {
        println!("============================================================");
        println!("⚡ Cline Proxy Engine running in Headless / Server Mode");
        println!("  Listen: http://{LISTEN}");
        println!("  Web Dashboard: http://{LISTEN}/dashboard");
        println!("  Auth: Authorization: Bearer public");
        println!("  Press Ctrl+C to stop.");
        println!("============================================================");

        // Keep main thread alive
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }

    // Windows Native GUI Options
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([880.0, 620.0])
            .with_min_inner_size([650.0, 450.0])
            .with_title("⚡ Cline Proxy Engine — Windows"),
        ..Default::default()
    };

    eframe::run_native(
        "Cline Proxy Engine",
        native_options,
        Box::new(move |_cc| Ok(Box::new(ProxyApp::new(key_manager)))),
    )
}
