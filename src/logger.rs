use chrono::Local;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize)]
pub struct LogItem {
    pub id: usize,
    pub timestamp: String,
    pub icon: &'static str,
    pub text: String,
    pub raw: String,
}

pub struct GlobalLogBuffer {
    items: RwLock<VecDeque<LogItem>>,
    counter: AtomicUsize,
    max_size: usize,
}

impl GlobalLogBuffer {
    pub fn new(max_size: usize) -> Self {
        Self {
            items: RwLock::new(VecDeque::with_capacity(max_size)),
            counter: AtomicUsize::new(0),
            max_size,
        }
    }

    pub fn push(&self, icon: &'static str, text: String) {
        let timestamp = Local::now().format("%H:%M:%S").to_string();
        let raw = format!("[{timestamp}] {icon} {text}");

        // Print to standard terminal
        println!("{raw}");

        let item = LogItem {
            id: self.counter.fetch_add(1, Ordering::Relaxed),
            timestamp,
            icon,
            text,
            raw,
        };

        if let Ok(mut lock) = self.items.write() {
            if lock.len() >= self.max_size {
                lock.pop_front();
            }
            lock.push_back(item);
        }
    }

    pub fn get_items(&self) -> Vec<LogItem> {
        self.items.read().map(|l| l.iter().cloned().collect()).unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut lock) = self.items.write() {
            lock.clear();
        }
    }
}

// Global logger instance
pub static LOGGER: LazyLock<Arc<GlobalLogBuffer>> =
    LazyLock::new(|| Arc::new(GlobalLogBuffer::new(2000)));

// Structured logging helper functions
pub fn log_proxy_info(provider: &str, model: &str, conn: &str, key_name: &str) {
    let pool_id = format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() % 0xFFFFFFFF);
    let msg = format!("[PROXY] {provider} | {model} | conn={conn} | key={key_name} | pool={pool_id}");
    LOGGER.push("ℹ️", msg);
}

pub fn log_request_start(color_icon: &'static str, path: &str, target_model: &str, stream: bool, msg_count: usize, acc: &str) {
    let stream_tag = if stream { "STREAM" } else { "BATCH" };
    let msg = format!("▶ POST {path} → {target_model} · FMT: openai · {stream_tag} · {msg_count} MSG · ACC:{acc}");
    LOGGER.push(color_icon, msg);
}

pub fn log_request_done(color_icon: &'static str, duration_ms: u128, ttft_ms: Option<u128>, in_tok: u64, out_tok: u64) {
    let ttft_str = if let Some(t) = ttft_ms {
        format!(" · TTFT {t}ms")
    } else {
        String::new()
    };
    let msg = format!("📊 DONE {duration_ms}ms{ttft_str} · IN {in_tok} · OUT {out_tok}");
    LOGGER.push(color_icon, msg);
}

pub fn log_error(err: &str) {
    let msg = format!("⚠️ {err}");
    LOGGER.push("🔴", msg);
}

pub fn log_failover(from_key: &str, to_key: &str, reason: &str) {
    let msg = format!("🔄 FAILOVER from '{from_key}' → '{to_key}' (Reason: {reason})");
    LOGGER.push("🟣", msg);
}
