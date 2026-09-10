mod key_manager;
mod logger;
mod server;
#[cfg(target_os = "windows")]
mod ui;

use key_manager::KeyManager;
use server::{create_router, AppState};
use std::net::SocketAddr;
#[cfg(target_os = "windows")]
use ui::ProxyApp;

const LISTEN: &str = "127.0.0.1:9090";

#[cfg(target_os = "windows")]
type MainResult = eframe::Result<()>;
#[cfg(not(target_os = "windows"))]
type MainResult = Result<(), Box<dyn std::error::Error>>;

fn main() -> MainResult {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();


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
    #[cfg(not(target_os = "windows"))]
    {
        println!("============================================================");
        println!("⚡ Cline Proxy Engine — CLI Mode (Linux)");
        println!("  Listen: http://{LISTEN}");
        println!("  Auth: Authorization: Bearer public");
        println!("  Web Dashboard: http://{LISTEN}/dashboard");
        println!("  Press Ctrl+C to stop.");
        println!("============================================================");

        rt.block_on(async {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
            }
        });
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
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
}
