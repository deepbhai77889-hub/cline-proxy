use crate::key_manager::KeyManager;
use crate::logger::*;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{any, get, post},
    Router,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLINE_BASE: &str = "https://api.cline.bot/api/v1";
const PUBLIC_KEY: &str = "public";
const MAX_BODY_LIMIT: usize = 100 * 1024 * 1024; // 100MB

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .tcp_nodelay(true)
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Some(Duration::from_secs(120)))
        .timeout(Duration::from_secs(600))
        .build()
        .expect("Failed to build pooled reqwest Client")
});

static COLOR_ICONS: [&'static str; 6] = ["🟤", "🟣", "🟢", "🔵", "🟡", "🟠"];
static REQ_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
pub struct AppState {
    pub key_manager: KeyManager,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/dashboard", get(dashboard_html))
        .route("/", get(dashboard_html))
        .route("/api/keys", get(get_keys_api).post(add_key_api))
        .route("/api/keys/:id", axum::routing::delete(delete_key_api))
        .route("/api/test-key", post(test_key_api))
        .route("/api/logs", get(get_logs_api))
        .route("/v1/models", any(models_handler))
        .route("/v1/chat/completions", any(chat_completions_handler))
        .route("/v1/completions", any(chat_completions_handler))
        .route("/v1/messages", post(anthropic_handler))
        .fallback(any(chat_completions_handler))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

fn check_public_key(headers: &HeaderMap) -> bool {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let lower = auth.to_lowercase();
    if lower.starts_with("bearer ") {
        let key = auth[7..].trim();
        return key == PUBLIC_KEY;
    }
    if let Some(k) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if k.trim() == PUBLIC_KEY {
            return true;
        }
    }
    false
}

fn normalize_model_id(m: &str) -> &'static str {
    match m {
        "deepseek-v4-flash" | "deepseek_v4_flash" => "deepseek/deepseek-v4-flash",
        "deepseek-v4-pro" | "deepseek_v4_pro" => "deepseek/deepseek-v4-pro",
        "glm-5.3-flash" | "glm_5.3_flash" => "z-ai/glm-5.3-flash",
        "Solar Pro 4" | "solar-pro-4" | "solar-pro4" => "cline-free/solar-pro4",
        "Muse Spark 1.3 Contributor" | "muse-spark-1.3-contributor" => {
            "cline-free/muse-spark-1.3-contributor"
        }
        "LongCat-2.0" | "longcat-2.0" => "cline-free/longcat-2.0",
        _ => "",
    }
}

// GET /v1/models with round-robin key
async fn models_handler(State(state): State<AppState>, req: Request) -> Response {
    if !check_public_key(req.headers()) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {"message": "Unauthorized — use Authorization: Bearer public"}})),
        ).into_response();
    }

    let candidates = state.key_manager.get_round_robin_candidates().await;
    if candidates.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error": {"message": "No active API keys configured in Account settings"}})),
        ).into_response();
    }

    for candidate in &candidates {
        let auth_val = if candidate.key.starts_with("Bearer ") {
            candidate.key.clone()
        } else {
            format!("Bearer {}", candidate.key)
        };

        let resp = HTTP_CLIENT
            .get(format!("{CLINE_BASE}/models"))
            .header("authorization", auth_val)
            .header("user-agent", "Cline/3.0.61")
            .header("x-client-type", "cline-cli")
            .header("x-client-version", "3.0.61")
            .header("x-core-version", "0.0.82")
            .header("http-referer", "https://cline.bot")
            .header("x-title", "Cline")
            .send()
            .await;

        if let Ok(res) = resp {
            if res.status().is_success() {
                state.key_manager.record_success(&candidate.id).await;
                log_proxy_info("CLINE", "models", "Public", &candidate.name);
                let body = Body::from_stream(res.bytes_stream());
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(body)
                    .unwrap();
            }
        }
    }

    (StatusCode::BAD_GATEWAY, axum::Json(json!({"error": {"message": "Failed to fetch models from upstream"}}))).into_response()
}

// POST /v1/chat/completions with Round-Robin & Auto-Failover
async fn chat_completions_handler(State(state): State<AppState>, req: Request) -> Response {
    let start_time = Instant::now();
    let color_icon = COLOR_ICONS[REQ_COUNTER.fetch_add(1, Ordering::Relaxed) % COLOR_ICONS.len()];

    if !check_public_key(req.headers()) {
        log_error("Unauthorized request rejected: missing or invalid Bearer public");
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({
                "error": {
                    "message": "Unauthorized — use Authorization: Bearer public",
                    "type": "invalid_request_error",
                    "code": "invalid_api_key"
                }
            })),
        ).into_response();
    }

    let candidates = state.key_manager.get_round_robin_candidates().await;
    if candidates.is_empty() {
        log_error("No active API keys available in Account pool");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error": {"message": "No active API keys configured. Please add one in the Accounts panel."}})),
        ).into_response();
    }

    let raw_path = req.uri().path().to_string();
    let path = if raw_path.starts_with("/v1/") {
        raw_path.replacen("/v1", "", 1)
    } else {
        raw_path.clone()
    };
    let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
    let upstream_url = format!("{}{}{}", CLINE_BASE, path, query);

    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY_LIMIT).await {
        Ok(b) => b,
        Err(e) => {
            log_error(&format!("Payload too large or read error: {e}"));
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": {"message": e.to_string()}})),
            ).into_response();
        }
    };

    let mut final_bytes = bytes.to_vec();
    let mut model_name = "unknown".to_string();
    let mut is_stream_req = false;
    let mut msg_count = 0;

    if !bytes.is_empty() {
        if let Ok(mut v) = serde_json::from_slice::<Value>(&bytes) {
            is_stream_req = v.get("stream").and_then(|x| x.as_bool()).unwrap_or(false);
            if let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) {
                msg_count = msgs.len();
            }
            if let Some(m) = v.get("model").and_then(|x| x.as_str()).map(|s| s.to_string()) {
                let norm = normalize_model_id(&m);
                let effective_model = if !norm.is_empty() { norm } else { m.as_str() };
                model_name = effective_model.to_string();
                if !norm.is_empty() && norm != m.as_str() {
                    v["model"] = json!(norm);
                    if let Ok(reencoded) = serde_json::to_vec(&v) {
                        final_bytes = reencoded;
                    }
                }
            }
        }
    }

    let ct = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    let accept = parts.headers.get("accept").and_then(|v| v.to_str().ok()).map(|s| s.to_string());

    let mut last_error_response: Option<Response> = None;

    if let Some(first) = candidates.first() {
        log_proxy_info("CLINE", &model_name, "Public", &first.name);
    }
    log_request_start(color_icon, "v1/chat/completions", &model_name, is_stream_req, msg_count, "Public");

    // Round-Robin execution loop with Auto-Failover
    for (i, candidate) in candidates.iter().enumerate() {
        let auth_val = if candidate.key.starts_with("Bearer ") {
            candidate.key.clone()
        } else {
            format!("Bearer {}", candidate.key)
        };

        let mut rb = HTTP_CLIENT.post(&upstream_url);
        rb = rb.header("content-type", &ct);
        rb = rb.header("authorization", auth_val);
        rb = rb.header("user-agent", "Cline/3.0.61");
        rb = rb.header("x-client-type", "cline-cli");
        rb = rb.header("x-client-version", "3.0.61");
        rb = rb.header("x-core-version", "0.0.82");
        rb = rb.header("http-referer", "https://cline.bot");
        rb = rb.header("x-title", "Cline");
        rb = rb.header("x-is-multiroot", "false");

        if let Some(a) = &accept {
            rb = rb.header("accept", a);
        }
        if !final_bytes.is_empty() {
            rb = rb.body(final_bytes.clone());
        }

        let upstream_res = rb.send().await;
        match upstream_res {
            Ok(upstream) => {
                let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

                // Failover on rate-limit (429), auth error (401), or server error (500/502/503)
                if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN || status.is_server_error() {
                    let err_text = upstream.text().await.unwrap_or_default();
                    state.key_manager.record_failure(&candidate.id, status.as_u16(), &err_text).await;

                    if i + 1 < candidates.len() {
                        let next_candidate = &candidates[i + 1];
                        log_failover(&candidate.name, &next_candidate.name, &format!("HTTP {status}"));
                        continue;
                    } else {
                        log_error(&format!("All {} keys failed. Last error: {err_text}", candidates.len()));
                        last_error_response = Some((status, err_text).into_response());
                        break;
                    }
                }

                // SUCCESS on this key!
                state.key_manager.record_success(&candidate.id).await;
                let duration_ms = start_time.elapsed().as_millis();
                log_request_done(color_icon, duration_ms, None, 0, 0);

                let headers = upstream.headers().clone();
                let stream = upstream.bytes_stream();
                let body = Body::from_stream(stream);

                let mut builder = Response::builder().status(status);
                if let Some(ct_val) = headers.get("content-type") {
                    builder = builder.header("content-type", ct_val);
                }
                if let Some(rid) = headers.get("x-request-id") {
                    builder = builder.header("x-request-id", rid);
                }
                if let Some(cc) = headers.get("cache-control") {
                    builder = builder.header("cache-control", cc);
                }

                return builder.body(body).unwrap_or_else(|_| {
                    (StatusCode::INTERNAL_SERVER_ERROR, "failed to build response").into_response()
                });
            }
            Err(e) => {
                state.key_manager.record_failure(&candidate.id, 502, "Network error").await;
                if i + 1 < candidates.len() {
                    let next_candidate = &candidates[i + 1];
                    log_failover(&candidate.name, &next_candidate.name, &format!("Network: {e}"));
                    continue;
                } else {
                    last_error_response = Some((StatusCode::BAD_GATEWAY, e.to_string()).into_response());
                    break;
                }
            }
        }
    }

    last_error_response.unwrap_or_else(|| {
        (StatusCode::BAD_GATEWAY, "All API keys exhausted or rate-limited").into_response()
    })
}

// Anthropic /v1/messages Adapter with Round-Robin & Failover
async fn anthropic_handler(State(state): State<AppState>, req: Request) -> Response {
    let start_time = Instant::now();
    let color_icon = COLOR_ICONS[REQ_COUNTER.fetch_add(1, Ordering::Relaxed) % COLOR_ICONS.len()];

    if !check_public_key(req.headers()) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {"type": "authentication_error", "message": "Unauthorized — use x-api-key: public"}})),
        ).into_response();
    }

    let candidates = state.key_manager.get_round_robin_candidates().await;
    if candidates.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error": {"message": "No active API keys configured."}})),
        ).into_response();
    }

    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY_LIMIT).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": {"message": e.to_string(), "type": "invalid_request_error"}})),
            ).into_response();
        }
    };

    let anth_req: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": {"message": format!("Invalid JSON: {e}"), "type": "invalid_request_error"}})),
            ).into_response();
        }
    };

    let raw_model = anth_req.get("model").and_then(|x| x.as_str()).unwrap_or("deepseek-v4-flash");
    let norm = normalize_model_id(raw_model);
    let model = if !norm.is_empty() { norm } else { raw_model };

    let mut oai_messages: Vec<Value> = Vec::new();
    if let Some(sys) = anth_req.get("system") {
        if let Some(s) = sys.as_str() {
            oai_messages.push(json!({"role": "system", "content": s}));
        }
    }

    if let Some(msgs) = anth_req.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = m.get("content");

            if let Some(s) = content.and_then(|c| c.as_str()) {
                oai_messages.push(json!({"role": role, "content": s}));
            } else if let Some(blocks) = content.and_then(|c| c.as_array()) {
                let mut text_and_image_parts: Vec<Value> = Vec::new();
                let mut assistant_tool_calls: Vec<Value> = Vec::new();

                for b in blocks {
                    let b_type = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match b_type {
                        "text" => {
                            if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                                text_and_image_parts.push(json!({"type": "text", "text": t}));
                            }
                        }
                        "image" => {
                            if let Some(src) = b.get("source") {
                                let media_type = src.get("media_type").and_then(|m| m.as_str()).unwrap_or("image/png");
                                let data = src.get("data").and_then(|d| d.as_str()).unwrap_or("");
                                text_and_image_parts.push(json!({
                                    "type": "image_url",
                                    "image_url": {"url": format!("data:{media_type};base64,{data}")}
                                }));
                            }
                        }
                        "tool_use" => {
                            let id = b.get("id").and_then(|x| x.as_str()).unwrap_or("call_0");
                            let name = b.get("name").and_then(|x| x.as_str()).unwrap_or("");
                            let input = b.get("input").unwrap_or(&json!({})).to_string();
                            assistant_tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": input}
                            }));
                        }
                        "tool_result" => {
                            let tool_call_id = b.get("tool_use_id").and_then(|x| x.as_str()).unwrap_or("");
                            let res_content = b.get("content").map(|c| c.to_string()).unwrap_or_default();
                            oai_messages.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_call_id,
                                "content": res_content
                            }));
                        }
                        _ => {}
                    }
                }

                if role == "assistant" && !assistant_tool_calls.is_empty() {
                    let mut msg = json!({"role": "assistant", "tool_calls": assistant_tool_calls});
                    if !text_and_image_parts.is_empty() {
                        msg["content"] = Value::Array(text_and_image_parts);
                    }
                    oai_messages.push(msg);
                } else if !text_and_image_parts.is_empty() {
                    oai_messages.push(json!({"role": role, "content": text_and_image_parts}));
                }
            }
        }
    }

    let mut oai_tools: Vec<Value> = Vec::new();
    let default_schema = json!({"type": "object"});
    if let Some(tools) = anth_req.get("tools").and_then(|t| t.as_array()) {
        for t in tools {
            let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let desc = t.get("description").and_then(|d| d.as_str()).unwrap_or("");
            let schema = t.get("input_schema").unwrap_or(&default_schema);
            oai_tools.push(json!({
                "type": "function",
                "function": {"name": name, "description": desc, "parameters": schema}
            }));
        }
    }

    let mut oai_body = json!({
        "model": model,
        "messages": oai_messages,
        "parallel_tool_calls": true
    });
    if let Some(mt) = anth_req.get("max_tokens").and_then(|x| x.as_u64()) {
        oai_body["max_tokens"] = json!(mt);
    }
    if !oai_tools.is_empty() {
        oai_body["tools"] = Value::Array(oai_tools);
    }
    if let Some(first) = candidates.first() {
        log_proxy_info("CLINE", model, "Public", &first.name);
    }
    log_request_start(color_icon, "v1/messages", model, false, 1, "Public");

    for (i, candidate) in candidates.iter().enumerate() {
        let auth_val = if candidate.key.starts_with("Bearer ") {
            candidate.key.clone()
        } else {
            format!("Bearer {}", candidate.key)
        };

        let res = HTTP_CLIENT
            .post(format!("{CLINE_BASE}/chat/completions"))
            .header("authorization", auth_val)
            .header("content-type", "application/json")
            .header("user-agent", "Cline/3.0.61")
            .header("x-client-type", "cline-cli")
            .header("x-client-version", "3.0.61")
            .header("x-core-version", "0.0.82")
            .header("http-referer", "https://cline.bot")
            .header("x-title", "Cline")
            .header("x-is-multiroot", "false")
            .json(&oai_body)
            .send()
            .await;

        match res {
            Ok(resp) => {
                let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN || status.is_server_error() {
                    state.key_manager.record_failure(&candidate.id, status.as_u16(), &format!("{status}")).await;
                    if i + 1 < candidates.len() {
                        let next = &candidates[i + 1];
                        log_failover(&candidate.name, &next.name, &format!("{status}"));
                        continue;
                    }
                }

                state.key_manager.record_success(&candidate.id).await;
                let bytes = resp.bytes().await.unwrap_or_default();
                let oai_resp: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));

                let msg = oai_resp.pointer("/data/choices/0/message").or_else(|| oai_resp.pointer("/choices/0/message"));
                let mut anth_content: Vec<Value> = Vec::new();
                let mut stop_reason = "end_turn";

                if let Some(m) = msg {
                    if let Some(txt) = m.get("content").and_then(|c| c.as_str()) {
                        if !txt.is_empty() {
                            anth_content.push(json!({"type": "text", "text": txt}));
                        }
                    }
                    if let Some(tcs) = m.get("tool_calls").and_then(|t| t.as_array()) {
                        if !tcs.is_empty() {
                            stop_reason = "tool_use";
                            for tc in tcs {
                                let id = tc.get("id").and_then(|x| x.as_str()).unwrap_or("call_0");
                                let name = tc.pointer("/function/name").and_then(|x| x.as_str()).unwrap_or("");
                                let args_str = tc.pointer("/function/arguments").and_then(|x| x.as_str()).unwrap_or("{}");
                                let input_val: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
                                anth_content.push(json!({
                                    "type": "tool_use",
                                    "id": id,
                                    "name": name,
                                    "input": input_val
                                }));
                            }
                        }
                    }
                }

                let in_tok = oai_resp.pointer("/data/usage/prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                let out_tok = oai_resp.pointer("/data/usage/completion_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                let duration_ms = start_time.elapsed().as_millis();
                log_request_done(color_icon, duration_ms, None, in_tok, out_tok);

                let anth_out = json!({
                    "id": format!("msg_{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()),
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": anth_content,
                    "stop_reason": stop_reason,
                    "stop_sequence": null,
                    "usage": {"input_tokens": in_tok, "output_tokens": out_tok}
                });

                return (StatusCode::OK, [("content-type", "application/json")], axum::Json(anth_out)).into_response();
            }
            Err(e) => {
                state.key_manager.record_failure(&candidate.id, 502, "network error").await;
                if i + 1 < candidates.len() {
                    let next = &candidates[i + 1];
                    log_failover(&candidate.name, &next.name, &format!("Network: {e}"));
                    continue;
                }
            }
        }
    }

    (StatusCode::BAD_GATEWAY, axum::Json(json!({"error": {"message": "All keys failed"}}))).into_response()
}

// REST API for managing keys & viewing logs from Web Dashboard or CLI
async fn get_keys_api(State(state): State<AppState>) -> impl IntoResponse {
    let keys = state.key_manager.keys.read().await;
    axum::Json(keys.clone())
}

#[derive(serde::Deserialize)]
struct AddKeyRequest {
    name: String,
    key: String,
}

async fn add_key_api(State(state): State<AppState>, axum::Json(payload): axum::Json<AddKeyRequest>) -> impl IntoResponse {
    match state.key_manager.add_key(payload.name, payload.key).await {
        Ok(entry) => (StatusCode::OK, axum::Json(json!({"success": true, "key": entry}))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, axum::Json(json!({"success": false, "error": e}))).into_response(),
    }
}

async fn delete_key_api(State(state): State<AppState>, axum::extract::Path(id): axum::extract::Path<String>) -> impl IntoResponse {
    state.key_manager.remove_key(&id).await;
    axum::Json(json!({"success": true}))
}

async fn get_logs_api() -> impl IntoResponse {
    let items = LOGGER.get_items();
    axum::Json(items)
}

#[derive(serde::Deserialize)]
struct TestKeyRequest {
    key: String,
}

async fn test_key_api(axum::Json(payload): axum::Json<TestKeyRequest>) -> impl IntoResponse {
    match crate::key_manager::test_key_connection(&payload.key).await {
        Ok(msg) => (StatusCode::OK, axum::Json(json!({"success": true, "message": msg}))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, axum::Json(json!({"success": false, "error": e}))).into_response(),
    }
}

// Web Dashboard HTML with Accounts section, Test Connection, Save, and Live Logs
async fn dashboard_html() -> Html<&'static str> {
    Html(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Cline Proxy Management</title>
<style>
  :root { --bg: #0d1117; --panel: #161b22; --border: #30363d; --text: #c9d1d9; --accent: #58a6ff; --green: #3fb950; --red: #f85149; }
  body { margin: 0; padding: 20px; background: var(--bg); color: var(--text); font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; }
  .header { display: flex; align-items: center; justify-content: space-between; border-bottom: 1px solid var(--border); padding-bottom: 15px; margin-bottom: 20px; }
  .tabs { display: flex; gap: 10px; margin-bottom: 20px; }
  .tab-btn { background: var(--panel); border: 1px solid var(--border); color: var(--text); padding: 8px 16px; border-radius: 6px; cursor: pointer; font-size: 14px; }
  .tab-btn.active { background: var(--accent); color: #fff; font-weight: bold; border-color: var(--accent); }
  .panel { background: var(--panel); border: 1px solid var(--border); border-radius: 8px; padding: 20px; }
  .btn { background: var(--accent); color: white; border: none; padding: 8px 16px; border-radius: 6px; cursor: pointer; font-size: 14px; font-weight: bold; }
  .btn-outline { background: transparent; border: 1px solid var(--border); color: var(--text); }
  .btn-danger { background: var(--red); }
  input { background: #0d1117; border: 1px solid var(--border); color: #fff; padding: 8px 12px; border-radius: 6px; width: 100%; box-sizing: border-box; margin-bottom: 10px; font-family: monospace; }
  table { width: 100%; border-collapse: collapse; margin-top: 15px; }
  th, td { text-align: left; padding: 10px; border-bottom: 1px solid var(--border); font-size: 13px; }
  th { color: #8b949e; font-weight: 600; }
  .badge { padding: 3px 8px; border-radius: 12px; font-size: 11px; font-weight: bold; background: #238636; color: white; }
  .badge-err { background: #da3633; }
  #terminal { background: #090d13; border: 1px solid var(--border); border-radius: 6px; padding: 15px; font-family: 'JetBrains Mono', Consolas, monospace; font-size: 13px; height: 500px; overflow-y: auto; white-space: pre-wrap; line-height: 1.6; }
  .modal { display: none; position: fixed; top: 0; left: 0; width: 100%; height: 100%; background: rgba(0,0,0,0.7); align-items: center; justify-content: center; }
  .modal-content { background: var(--panel); border: 1px solid var(--border); padding: 25px; border-radius: 8px; width: 450px; }
</style>
</head>
<body>
<div class="header">
  <h2>⚡ Cline Proxy Engine <span style="font-size: 14px; color: #8b949e;">(localhost:9090)</span></h2>
  <span class="badge">Active & Ready</span>
</div>

<div class="tabs">
  <button class="tab-btn active" onclick="showTab('accounts')">🔑 Accounts (API Keys)</button>
  <button class="tab-btn" onclick="showTab('logs')">📜 Console Logs</button>
</div>

<div id="accounts-panel" class="panel">
  <div style="display: flex; justify-content: space-between; align-items: center;">
    <h3>Configured API Keys (Round-Robin & Auto-Failover)</h3>
    <button class="btn" onclick="openAddKeyModal()">+ Add API Key</button>
  </div>
  <table id="keys-table">
    <thead>
      <tr><th>Name</th><th>API Key</th><th>Status</th><th>Calls</th><th>Failed</th><th>Action</th></tr>
    </thead>
    <tbody id="keys-body"><tr><td colspan="6">Loading keys...</td></tr></tbody>
  </table>
</div>

<div id="logs-panel" class="panel" style="display: none;">
  <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 10px;">
    <h3>Real-time Console Logs</h3>
    <div>
      <button class="btn btn-outline" onclick="clearLogs()">Clear Logs</button>
    </div>
  </div>
  <div id="terminal">Listening for live proxy traffic...</div>
</div>

<div id="add-modal" class="modal">
  <div class="modal-content">
    <h3>Add New Cline API Key</h3>
    <label>Account / Key Name</label>
    <input id="key-name" placeholder="e.g. Account 2" />
    <label>API Key (sk_... or Bearer workos:...)</label>
    <input id="key-val" placeholder="sk_0fd7..." />
    <div id="test-result" style="font-size: 13px; margin-bottom: 12px; min-height: 18px;"></div>
    <div style="display: flex; gap: 10px; justify-content: flex-end;">
      <button class="btn btn-outline" onclick="closeAddKeyModal()">Cancel</button>
      <button class="btn btn-outline" onclick="testConnection()">Test Connection</button>
      <button class="btn" onclick="saveKey()">Save & Add</button>
    </div>
  </div>
</div>

<script>
let currentTab = 'accounts';
function showTab(t) {
  document.querySelectorAll('.tab-btn').forEach(b => b.classList.remove('active'));
  event.target.classList.add('active');
  document.getElementById('accounts-panel').style.display = t === 'accounts' ? 'block' : 'none';
  document.getElementById('logs-panel').style.display = t === 'logs' ? 'block' : 'none';
}

function openAddKeyModal() {
  document.getElementById('add-modal').style.display = 'flex';
  document.getElementById('test-result').innerText = '';
}
function closeAddKeyModal() {
  document.getElementById('add-modal').style.display = 'none';
}

async function loadKeys() {
  try {
    const res = await fetch('/api/keys');
    const keys = await res.json();
    const tbody = document.getElementById('keys-body');
    if (keys.length === 0) {
      tbody.innerHTML = '<tr><td colspan="6">No keys added yet. Click "+ Add API Key" above.</td></tr>';
      return;
    }
    tbody.innerHTML = keys.map(k => `
      <tr>
        <td><strong>${k.name}</strong></td>
        <td><code>${k.key.slice(0, 10)}...${k.key.slice(-6)}</code></td>
        <td><span class="badge ${k.failed_calls > 0 ? 'badge-err' : ''}">${k.last_status || 'Active'}</span></td>
        <td>${k.total_calls}</td>
        <td>${k.failed_calls}</td>
        <td><button class="btn btn-danger" style="padding: 4px 8px; font-size: 12px;" onclick="deleteKey('${k.id}')">Delete</button></td>
      </tr>
    `).join('');
  } catch(e) {}
}

async function testConnection() {
  const key = document.getElementById('key-val').value.trim();
  const resEl = document.getElementById('test-result');
  if (!key) { resEl.innerText = '⚠️ Please enter an API key first'; resEl.style.color = 'var(--red)'; return false; }
  resEl.innerText = 'Testing connection with Cline backend...'; resEl.style.color = 'var(--accent)';
  try {
    const resp = await fetch('/api/test-key', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({key})
    });
    const data = await resp.json();
    if (data.success) {
      resEl.innerText = '✅ ' + data.message;
      resEl.style.color = 'var(--green)';
      return true;
    } else {
      resEl.innerText = '❌ ' + (data.error || 'Connection failed');
      resEl.style.color = 'var(--red)';
      return false;
    }
  } catch(e) {
    resEl.innerText = '❌ Network error: ' + e.message;
    resEl.style.color = 'var(--red)';
    return false;
  }
}

async function saveKey() {
  const name = document.getElementById('key-name').value.trim() || 'New Key';
  const key = document.getElementById('key-val').value.trim();
  const valid = await testConnection();
  if (!valid) return;

  const resp = await fetch('/api/keys', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({name, key})
  });
  if (resp.ok) {
    closeAddKeyModal();
    loadKeys();
  }
}

async function deleteKey(id) {
  if (confirm('Delete this key?')) {
    await fetch('/api/keys/' + id, {method: 'DELETE'});
    loadKeys();
  }
}

async function pollLogs() {
  try {
    const res = await fetch('/api/logs');
    const logs = await res.json();
    const term = document.getElementById('terminal');
    if (logs.length > 0) {
      term.innerHTML = logs.map(l => {
        let color = '#c9d1d9';
        if (l.icon.includes('🔴')) color = '#f85149';
        else if (l.icon.includes('🟢') || l.icon.includes('📊')) color = '#3fb950';
        else if (l.icon.includes('🟣')) color = '#d2a8ff';
        else if (l.icon.includes('ℹ️')) color = '#58a6ff';
        else if (l.icon.includes('🟤')) color = '#d29922';
        return `<span style="color: ${color}">[${l.timestamp}] ${l.icon} ${escapeHtml(l.text)}</span>`;
      }).join('\n');
      term.scrollTop = term.scrollHeight;
    }
  } catch(e) {}
}

function escapeHtml(s) {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

function clearLogs() {
  document.getElementById('terminal').innerHTML = '';
}

loadKeys();
setInterval(loadKeys, 5000);
setInterval(pollLogs, 1000);
</script>
</body>
</html>"#)
}
