use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiKeyEntry {
    pub id: String,
    pub name: String,
    pub key: String,
    pub enabled: bool,
    pub total_calls: u64,
    pub failed_calls: u64,
    pub last_status: Option<String>,
}

#[derive(Clone)]
pub struct KeyManager {
    pub keys: Arc<RwLock<Vec<ApiKeyEntry>>>,
    index: Arc<AtomicUsize>,
    file_path: PathBuf,
}

impl KeyManager {
    pub fn new() -> Self {
        let path = get_keys_file_path();
        let mut initial_keys = Vec::new();

        // Load existing saved keys if file exists
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(loaded) = serde_json::from_str::<Vec<ApiKeyEntry>>(&content) {
                    initial_keys = loaded;
                }
            }
        }

        // If no keys saved yet, try to pre-seed from providers.json or the verified working key
        if initial_keys.is_empty() {
            // Seed verified working key
            initial_keys.push(ApiKeyEntry {
                id: "key-default".to_string(),
                name: "Primary Key".to_string(),
                key: "sk_0fd7d65fa600fa6972223b23750233dba01b3e420d809ffd940aef2359aff280".to_string(),
                enabled: true,
                total_calls: 0,
                failed_calls: 0,
                last_status: Some("Active".to_string()),
            });

            // Also check providers.json for OAuth token
            if let Some(oauth_key) = load_oauth_from_cline() {
                if !oauth_key.is_empty() {
                    initial_keys.push(ApiKeyEntry {
                        id: "key-oauth".to_string(),
                        name: "Cline OAuth Account".to_string(),
                        key: oauth_key,
                        enabled: true,
                        total_calls: 0,
                        failed_calls: 0,
                        last_status: Some("Active".to_string()),
                    });
                }
            }
        }

        let km = Self {
            keys: Arc::new(RwLock::new(initial_keys)),
            index: Arc::new(AtomicUsize::new(0)),
            file_path: path,
        };
        km.save_to_disk();
        km
    }

    pub fn save_to_disk(&self) {
        if let Ok(keys) = self.keys.try_read() {
            if let Ok(json) = serde_json::to_string_pretty(&*keys) {
                if let Some(parent) = self.file_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&self.file_path, json);
            }
        }
    }

    pub async fn add_key(&self, name: String, key: String) -> Result<ApiKeyEntry, String> {
        let test_res = test_key_connection(&key).await;
        if let Err(e) = test_res {
            return Err(format!("Connection test failed: {e}"));
        }

        let rand_suffix = format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() % 0xFFFFFFFF);
        let entry = ApiKeyEntry {
            id: format!("key-{rand_suffix}"),
            name,
            key,
            enabled: true,
            total_calls: 0,
            failed_calls: 0,
            last_status: Some("Verified OK".to_string()),
        };

        {
            let mut keys = self.keys.write().await;
            keys.push(entry.clone());
        }
        self.save_to_disk();
        Ok(entry)
    }

    pub async fn remove_key(&self, id: &str) {
        {
            let mut keys = self.keys.write().await;
            keys.retain(|k| k.id != id);
        }
        self.save_to_disk();
    }

    pub async fn toggle_key(&self, id: &str) {
        {
            let mut keys = self.keys.write().await;
            if let Some(k) = keys.iter_mut().find(|k| k.id == id) {
                k.enabled = !k.enabled;
            }
        }
        self.save_to_disk();
    }

    // Get active keys for round-robin rotation
    pub async fn get_round_robin_candidates(&self) -> Vec<ApiKeyEntry> {
        let keys = self.keys.read().await;
        let active: Vec<ApiKeyEntry> = keys.iter().filter(|k| k.enabled).cloned().collect();
        if active.is_empty() {
            return Vec::new();
        }
        let start = self.index.fetch_add(1, Ordering::Relaxed) % active.len();
        // Return rotated list starting from `start`
        let mut rotated = Vec::with_capacity(active.len());
        for i in 0..active.len() {
            rotated.push(active[(start + i) % active.len()].clone());
        }
        rotated
    }

    pub async fn record_success(&self, id: &str) {
        let mut keys = self.keys.write().await;
        if let Some(k) = keys.iter_mut().find(|k| k.id == id) {
            k.total_calls += 1;
            k.last_status = Some("Active".to_string());
        }
    }

    pub async fn record_failure(&self, id: &str, reason: &str) {
        let mut keys = self.keys.write().await;
        if let Some(k) = keys.iter_mut().find(|k| k.id == id) {
            k.total_calls += 1;
            k.failed_calls += 1;
            k.last_status = Some(format!("Error: {reason}"));
        }
    }
}

// Test connection against api.cline.bot
pub async fn test_key_connection(key: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let auth_header = if key.starts_with("Bearer ") {
        key.to_string()
    } else {
        format!("Bearer {key}")
    };

    let resp = client
        .post("https://api.cline.bot/api/v1/chat/completions")
        .header("Authorization", auth_header)
        .header("Content-Type", "application/json")
        .header("User-Agent", "Cline/3.0.61")
        .header("X-CLIENT-TYPE", "cline-cli")
        .header("X-CLIENT-VERSION", "3.0.61")
        .header("X-CORE-VERSION", "0.0.82")
        .header("HTTP-Referer", "https://cline.bot")
        .header("X-Title", "Cline")
        .json(&serde_json::json!({
            "model": "deepseek/deepseek-v4-flash",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    let status = resp.status();
    if status.is_success() {
        Ok("Connection verified! Key is valid and active.".to_string())
    } else {
        let err_text = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            Err(format!("Authentication failed (HTTP {status}): Invalid API key"))
        } else {
            Err(format!("HTTP {status}: {err_text}"))
        }
    }
}

fn get_keys_file_path() -> PathBuf {
    if let Ok(custom) = std::env::var("CLINE_PROXY_KEYS_PATH") {
        return PathBuf::from(custom);
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".cline-proxy")
        .join("keys.json")
}

fn load_oauth_from_cline() -> Option<String> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    let prov_path = PathBuf::from(home)
        .join(".cline")
        .join("data")
        .join("settings")
        .join("providers.json");
    if let Ok(data) = std::fs::read_to_string(prov_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
            return v.pointer("/providers/cline/settings/auth/accessToken")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
        }
    }
    None
}
