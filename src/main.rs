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

    // Initialize multi-thread Tokio runtime on the main thread
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime");

    // Enter Tokio runtime context so ambient reactor is always present
    let handle = rt.handle().clone();
    let _guard = handle.enter();

    // Spawn Axum server using runtime handle
    let server_state = state.clone();
    handle.spawn(async move {
        let app = create_router(server_state);
        let addr: SocketAddr = match LISTEN.parse() {
            Ok(a) => a,
            Err(e) => {
                logger::LOGGER.push("🔴", format!("Invalid listen address: {e}"));
                return;
            }
        };
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                logger::LOGGER.push("ℹ️", format!("Engine server live on http://{LISTEN} (Auth: Bearer public)"));
                let _ = axum::serve(listener, app).await;
            }
            Err(e) => {
                logger::LOGGER.push("🔴", format!("Failed to bind port 9090: {e}"));
            }
        }
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

        rt.block_on(async {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
            }
        });
        return Ok(());
    }

    // Windows Native GUI Options
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([880.0, 620.0])
            .with_min_inner_size([650.0, 450.0])
            .with_title("⚡ Cline Proxy Engine — Windows"),
        ..Default::default()
    };

    let app_handle = handle.clone();
    eframe::run_native(
        "Cline Proxy Engine",
        native_options,
        Box::new(move |_cc| Ok(Box::new(ProxyApp::new(key_manager, app_handle)))),
    )
}
