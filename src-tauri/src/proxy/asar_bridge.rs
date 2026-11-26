//! Dedicated `127.0.0.1:{asar_port}` ASAR Zero-Reload Bridge Server
//!
//! Intercepts Google Cloud Code PA `/v1internal:*` traffic from patched Antigravity IDE / Classic
//! `app.asar` binaries, providing:
//! - Dynamic model catalog injection on `/v1internal:fetchAvailableModels`
//! - Zero-Reload per-request token rotation via `TokenManager`
//! - Dual-Track Slot Pinning (`👑 Claude Lead` vs `⚡ Gemini Worker`)
//! - Adaptive TTFB Fast-Failover Watchdog
//! - Project ID rewriting + `403 SERVICE_DISABLED` auto-heal
//! - Instant `429` / `503` failover across pool accounts
//! - `400` thoughtSignature / thinkingLevel sanitization & retry
//! - `Empty Thinking Completion` SSE auto-heal via `wrap_stream_with_empty_thinking_auto_heal`
//! - Zero-copy SSE & non-stream token metering into `ProxyMonitor` & `token_stats`

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use bytes::Bytes;
use dashmap::DashMap;
use futures::{Stream, StreamExt, TryStreamExt};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::proxy::asar_model_catalog;
use crate::proxy::monitor::ProxyRequestLog;
use crate::proxy::pipeline::auto_heal::{
    wrap_stream_with_empty_thinking_auto_heal, ThinkingAutoHealContext,
};
use crate::proxy::pipeline::inbound::InboundThinkingPipeline;
use crate::proxy::pipeline::policy::UpstreamClassification;
use crate::proxy::pipeline::CanonicalUsage;
use crate::proxy::server::{bind_tcp_listener, AppState};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsarBridgeStatus {
    pub running: bool,
    pub port: u16,
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsarTelemetrySnapshot {
    pub in_flight: u64,
    pub asar_in_flight_account_ids: Vec<String>,
    pub total_requests: u64,
    pub failover_count: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cached_tokens: u64,
    pub last_active_account_id: Option<String>,
    pub last_active_email: Option<String>,
    pub last_active_model: Option<String>,
    pub last_active_timestamp: Option<i64>,
    pub dual_track_enabled: bool,
    pub claude_slot_account_id: Option<String>,
    pub claude_slot_email: Option<String>,
    pub gemini_slot_account_id: Option<String>,
    pub gemini_slot_email: Option<String>,
    pub proxy_last_account_id: Option<String>,
    pub proxy_last_email: Option<String>,
    pub proxy_in_flight_account_ids: Vec<String>,
    pub native_current_account_id: Option<String>,
}

#[derive(Debug, Clone)]
struct LastActiveRecord {
    account_id: String,
    email: String,
    model: String,
    timestamp: i64,
}

struct AsarTelemetryHub {
    in_flight: AtomicU64,
    in_flight_by_account: DashMap<String, AtomicUsize>,
    total_requests: AtomicU64,
    failover_count: AtomicU64,
    total_input_tokens: AtomicU64,
    total_output_tokens: AtomicU64,
    total_cached_tokens: AtomicU64,
    last_active: RwLock<Option<LastActiveRecord>>,
    dual_track_enabled: AtomicBool,
    claude_slot: RwLock<Option<(String, String)>>,
    gemini_slot: RwLock<Option<(String, String)>>,
}

impl AsarTelemetryHub {
    fn new() -> Self {
        Self {
            in_flight: AtomicU64::new(0),
            in_flight_by_account: DashMap::new(),
            total_requests: AtomicU64::new(0),
            failover_count: AtomicU64::new(0),
            total_input_tokens: AtomicU64::new(0),
            total_output_tokens: AtomicU64::new(0),
            total_cached_tokens: AtomicU64::new(0),
            last_active: RwLock::new(None),
            dual_track_enabled: AtomicBool::new(true),
            claude_slot: RwLock::new(None),
            gemini_slot: RwLock::new(None),
        }
    }

    fn is_dual_track_enabled(&self) -> bool {
        self.dual_track_enabled.load(Ordering::Relaxed)
    }

    fn set_dual_track(&self, enabled: bool) {
        self.dual_track_enabled.store(enabled, Ordering::Relaxed);
        if !enabled {
            self.reset_slots();
        }
    }

    fn reset_slots(&self) {
        if let Ok(mut c) = self.claude_slot.write() {
            *c = None;
        }
        if let Ok(mut g) = self.gemini_slot.write() {
            *g = None;
        }
    }

    fn get_slot(&self, is_claude_track: bool) -> Option<(String, String)> {
        let lock = if is_claude_track {
            &self.claude_slot
        } else {
            &self.gemini_slot
        };
        lock.read().ok().and_then(|g| g.clone())
    }

    fn set_slot(&self, is_claude_track: bool, account_id: String, email: String) {
        let lock = if is_claude_track {
            &self.claude_slot
        } else {
            &self.gemini_slot
        };
        if let Ok(mut g) = lock.write() {
            *g = Some((account_id, email));
        }
    }

    fn clear_slot_if_matches(&self, is_claude_track: bool, account_id: &str) {
        let lock = if is_claude_track {
            &self.claude_slot
        } else {
            &self.gemini_slot
        };
        if let Ok(mut g) = lock.write() {
            if g.as_ref().is_some_and(|(id, _)| id == account_id) {
                *g = None;
            }
        }
    }

    fn record_failover(&self) {
        self.failover_count.fetch_add(1, Ordering::Relaxed);
    }

    fn record_active(&self, account_id: &str, email: &str, model: &str) {
        if let Ok(mut g) = self.last_active.write() {
            *g = Some(LastActiveRecord {
                account_id: account_id.to_string(),
                email: email.to_string(),
                model: model.to_string(),
                timestamp: chrono::Utc::now().timestamp(),
            });
        }
    }

    fn record_tokens(&self, input: u32, output: u32, cached: u32) {
        self.total_input_tokens
            .fetch_add(u64::from(input), Ordering::Relaxed);
        self.total_output_tokens
            .fetch_add(u64::from(output), Ordering::Relaxed);
        self.total_cached_tokens
            .fetch_add(u64::from(cached), Ordering::Relaxed);
    }

    fn active_account_ids(&self) -> Vec<String> {
        self.in_flight_by_account
            .iter()
            .filter(|entry| entry.value().load(Ordering::Relaxed) > 0)
            .map(|entry| entry.key().clone())
            .collect()
    }
}

static ASAR_TELEMETRY: Lazy<AsarTelemetryHub> = Lazy::new(AsarTelemetryHub::new);

struct AsarInFlightGuard {
    account_id: Option<String>,
}

impl AsarInFlightGuard {
    fn acquire(account_id: Option<String>) -> Self {
        ASAR_TELEMETRY.in_flight.fetch_add(1, Ordering::SeqCst);
        if let Some(ref id) = account_id {
            ASAR_TELEMETRY
                .in_flight_by_account
                .entry(id.clone())
                .or_insert_with(|| AtomicUsize::new(0))
                .fetch_add(1, Ordering::SeqCst);
        }
        Self { account_id }
    }
}

impl Drop for AsarInFlightGuard {
    fn drop(&mut self) {
        ASAR_TELEMETRY.in_flight.fetch_sub(1, Ordering::SeqCst);
        if let Some(ref id) = self.account_id {
            if let Some(counter) = ASAR_TELEMETRY.in_flight_by_account.get(id) {
                counter.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
}

pub fn set_dual_track_enabled(enabled: bool) {
    ASAR_TELEMETRY.set_dual_track(enabled);
}

pub fn reset_dual_track_slots() {
    ASAR_TELEMETRY.reset_slots();
}

pub async fn snapshot_asar_telemetry(state: Option<&AppState>) -> AsarTelemetrySnapshot {
    let last_active = ASAR_TELEMETRY
        .last_active
        .read()
        .ok()
        .and_then(|g| g.clone());
    let claude_slot = ASAR_TELEMETRY.get_slot(true);
    let gemini_slot = ASAR_TELEMETRY.get_slot(false);

    let native_current_account_id = tokio::task::spawn_blocking(|| {
        crate::modules::account::get_current_account_id()
            .ok()
            .flatten()
    })
    .await
    .ok()
    .flatten();

    let mut proxy_in_flight_account_ids = Vec::new();
    let mut proxy_last_account_id = None;
    let mut proxy_last_email = None;

    if let Some(app_state) = state {
        for acc_id in app_state.token_manager.enabled_account_ids() {
            if app_state.token_manager.get_in_flight(&acc_id) > 0 {
                proxy_in_flight_account_ids.push(acc_id);
            }
        }

        if let Some(first_active_id) = proxy_in_flight_account_ids.first() {
            proxy_last_account_id = Some(first_active_id.clone());
            proxy_last_email = app_state
                .token_manager
                .get_token_by_id(first_active_id)
                .map(|t| t.email);
        } else {
            let recent_logs = app_state.monitor.get_logs(30).await;
            if let Some(proxy_log) = recent_logs
                .into_iter()
                .find(|l| l.protocol.as_deref() != Some("v1internal") && l.account_email.is_some())
            {
                if let Some(email) = proxy_log.account_email {
                    proxy_last_account_id = app_state.token_manager.get_account_id_by_email(&email);
                    proxy_last_email = Some(email);
                }
            }
        }
    }

    AsarTelemetrySnapshot {
        in_flight: ASAR_TELEMETRY.in_flight.load(Ordering::Relaxed),
        asar_in_flight_account_ids: ASAR_TELEMETRY.active_account_ids(),
        total_requests: ASAR_TELEMETRY.total_requests.load(Ordering::Relaxed),
        failover_count: ASAR_TELEMETRY.failover_count.load(Ordering::Relaxed),
        total_input_tokens: ASAR_TELEMETRY.total_input_tokens.load(Ordering::Relaxed),
        total_output_tokens: ASAR_TELEMETRY.total_output_tokens.load(Ordering::Relaxed),
        total_cached_tokens: ASAR_TELEMETRY.total_cached_tokens.load(Ordering::Relaxed),
        last_active_account_id: last_active.as_ref().map(|r| r.account_id.clone()),
        last_active_email: last_active.as_ref().map(|r| r.email.clone()),
        last_active_model: last_active.as_ref().map(|r| r.model.clone()),
        last_active_timestamp: last_active.as_ref().map(|r| r.timestamp),
        dual_track_enabled: ASAR_TELEMETRY.is_dual_track_enabled(),
        claude_slot_account_id: claude_slot.as_ref().map(|(id, _)| id.clone()),
        claude_slot_email: claude_slot.as_ref().map(|(_, email)| email.clone()),
        gemini_slot_account_id: gemini_slot.as_ref().map(|(id, _)| id.clone()),
        gemini_slot_email: gemini_slot.as_ref().map(|(_, email)| email.clone()),
        proxy_last_account_id,
        proxy_last_email,
        proxy_in_flight_account_ids,
        native_current_account_id,
    }
}

pub struct AsarBridgeHandle {
    pub port: u16,
    cancel_token: CancellationToken,
    server_handle: tokio::task::JoinHandle<()>,
}

impl AsarBridgeHandle {
    /// Gracefully stop the ASAR Bridge HTTP server and release its TCP port.
    pub async fn stop(mut self) {
        self.cancel_token.cancel();
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(1000),
            &mut self.server_handle,
        )
        .await;
        if !self.server_handle.is_finished() {
            self.server_handle.abort();
        }
    }
}

/// Start the dedicated ASAR Zero-Reload Bridge server on `127.0.0.1:{port}`.
pub async fn start_asar_bridge(port: u16, state: AppState) -> Result<AsarBridgeHandle, String> {
    tokio::task::spawn_blocking(|| {
        if let Err(e) = asar_model_catalog::sync_models_from_accounts() {
            tracing::warn!("[ASAR-Bridge] Failed to sync models from accounts: {}", e);
        }
    });

    // Ensure ProxyMonitor logging reflects AppConfig even when :8045 proxy was not manually started
    if let Ok(cfg) = crate::modules::config::load_app_config() {
        state.monitor.set_enabled(cfg.proxy.enable_logging);
    }

    let listener = bind_tcp_listener("127.0.0.1", port)?;
    tracing::info!(
        "[ASAR-Bridge] Listening on http://127.0.0.1:{} (Zero-Reload v1internal bridge)",
        port
    );

    let max_body_size: usize = std::env::var("ABV_MAX_BODY_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100 * 1024 * 1024);

    let app = Router::new()
        .fallback(handle_asar_request)
        .layer(crate::proxy::middleware::cors_layer())
        .layer(DefaultBodyLimit::max(max_body_size))
        .with_state(state);

    let cancel_token = CancellationToken::new();
    let server_cancel = cancel_token.clone();

    let server_handle = tokio::spawn(async move {
        use hyper::body::Incoming;
        use hyper::server::conn::http1;
        use hyper_util::rt::TokioIo;
        use hyper_util::service::TowerToHyperService;
        use tower::ServiceExt;

        loop {
            tokio::select! {
                _ = server_cancel.cancelled() => {
                    tracing::info!("[ASAR-Bridge] Shutdown signal received, stopping listener on port {}", port);
                    break;
                }
                res = listener.accept() => {
                    match res {
                        Ok((stream, remote_addr)) => {
                            let io = TokioIo::new(stream);
                            let app_with_info = app.clone().map_request(move |mut req: axum::http::Request<Incoming>| {
                                req.extensions_mut().insert(axum::extract::ConnectInfo(remote_addr));
                                req
                            });
                            let service = TowerToHyperService::new(app_with_info);
                            let conn_cancel = server_cancel.clone();

                            tokio::task::spawn(async move {
                                let conn = http1::Builder::new().serve_connection(io, service);
                                tokio::pin!(conn);
                                tokio::select! {
                                    res = conn.as_mut() => {
                                        if let Err(err) = res {
                                            tracing::debug!("[ASAR-Bridge] Connection closed: {}", err);
                                        }
                                    }
                                    _ = conn_cancel.cancelled() => {
                                        conn.as_mut().graceful_shutdown();
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("[ASAR-Bridge] Accept error: {}", e);
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    }
                }
            }
        }
    });

    Ok(AsarBridgeHandle {
        port,
        cancel_token,
        server_handle,
    })
}

async fn handle_asar_request(State(state): State<AppState>, req: Request) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| q.to_string());
    let headers = req.headers().clone();

    if method == axum::http::Method::OPTIONS {
        return StatusCode::OK.into_response();
    }

    if path == "/health" || path == "/healthz" {
        return (
            StatusCode::OK,
            axum::Json(json!({
                "status": "ok",
                "service": "asar-bridge",
                "port": crate::modules::asar_patch::get_asar_bridge_port(),
            })),
        )
            .into_response();
    }

    if path.starts_with("/v1/api/event_logging") {
        return (StatusCode::OK, axum::Json(json!({}))).into_response();
    }

    let body_bytes = match axum::body::to_bytes(req.into_body(), 100 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                &format!("Failed to read request body: {}", e),
            );
        }
    };

    if path.contains("fetchAvailableModels") {
        return handle_fetch_available_models(state, headers, body_bytes).await;
    }

    if let Some(rpc_method) = extract_v1internal_method(&path) {
        if is_pool_ai_rpc(rpc_method) {
            return handle_v1internal_pool(state, rpc_method, query, headers, body_bytes).await;
        }
        return handle_v1internal_passthrough(state, rpc_method, query, headers, body_bytes).await;
    }

    json_error(
        StatusCode::NOT_FOUND,
        &format!("Unsupported ASAR bridge path: {}", path),
    )
}

pub(crate) fn fetch_models_tier_rank(tier: Option<&str>) -> u8 {
    match tier.unwrap_or("").trim().to_ascii_uppercase().as_str() {
        "ULTRA" => 0,
        "PRO" => 1,
        _ => 2,
    }
}

fn ordered_pool_account_ids_by_tier(state: &AppState) -> Vec<String> {
    let mut candidates: Vec<(u8, u8, String)> = state
        .token_manager
        .enabled_account_ids()
        .into_iter()
        .filter_map(|id| {
            let tok = state.token_manager.get_token_by_id(&id)?;
            if tok.validation_blocked {
                return None;
            }
            let tier = state
                .token_manager
                .get_account_tier(&id)
                .or(tok.subscription_tier.clone());
            Some((fetch_models_tier_rank(tier.as_deref()), tok.priority, id))
        })
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    candidates.into_iter().map(|(_, _, id)| id).collect()
}

/// Intercept `/v1internal:fetchAvailableModels`:
/// Call upstream Google prioritizing `ULTRA > PRO > FREE` pool accounts (preserving raw body/headers),
/// auto-ingest newly discovered Google models + union pool models across all accounts,
/// then inject active `models_catalog.json` models.
/// Falls back to synthetic baseline response if upstream is unreachable.
async fn handle_fetch_available_models(
    state: AppState,
    headers: HeaderMap,
    body_bytes: Bytes,
) -> Response {
    let raw_req_body = if body_bytes.is_empty() {
        Bytes::from_static(b"{}")
    } else {
        body_bytes
    };
    let incoming_ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // Union any models known in TokenManager across all pool accounts into the catalog
    let pool_model_ids: Vec<String> = state
        .token_manager
        .get_all_available_models()
        .into_iter()
        .collect();
    if !pool_model_ids.is_empty() {
        let _ = asar_model_catalog::ingest_pool_model_ids(pool_model_ids);
    }

    let extra_headers = collect_extra_headers(&headers);
    let ordered_ids = ordered_pool_account_ids_by_tier(&state);
    let all_enabled: HashSet<String> = state
        .token_manager
        .enabled_account_ids()
        .into_iter()
        .collect();
    let mut tried = HashSet::new();

    // Try up to 5 healthy accounts in strict ULTRA > PRO > FREE tier order
    for target_acc_id in ordered_ids.into_iter().take(5) {
        if tried.contains(&target_acc_id) {
            continue;
        }
        let mut exclude_others: HashSet<String> = all_enabled
            .iter()
            .filter(|id| *id != &target_acc_id)
            .cloned()
            .collect();
        exclude_others.extend(tried.iter().cloned());

        let mut tok_res = state
            .token_manager
            .get_token_filtered("gemini", false, None, "", &exclude_others)
            .await;
        if tok_res.is_err() {
            tok_res = state
                .token_manager
                .get_token_filtered("gemini", false, None, "gemini-3.8-flash", &exclude_others)
                .await;
        }

        match tok_res {
            Ok((access_token, _project_id, email, account_id, _wait_ms)) => {
                match state
                    .upstream
                    .call_v1_internal_raw_bytes(
                        "fetchAvailableModels",
                        &access_token,
                        raw_req_body.clone(),
                        incoming_ct.as_deref(),
                        None,
                        extra_headers.clone(),
                        Some(&account_id),
                    )
                    .await
                {
                    Ok(call_res) if call_res.response.status().is_success() => {
                        if let Ok(raw_text) = call_res.response.text().await {
                            let _ =
                                asar_model_catalog::ingest_live_fetch_available_models(&raw_text);
                            let injected =
                                asar_model_catalog::inject_models_into_fetch_response(&raw_text);
                            return json_raw_ok(injected);
                        }
                    }
                    Ok(call_res) => {
                        let status = call_res.response.status().as_u16();
                        tracing::warn!(
                            "[ASAR-Bridge] fetchAvailableModels upstream {} for {}, trying next account",
                            status,
                            email
                        );
                        tried.insert(account_id);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[ASAR-Bridge] fetchAvailableModels error for {}: {}",
                            email,
                            e
                        );
                        tried.insert(account_id);
                    }
                }
            }
            Err(_) => {
                tried.insert(target_acc_id);
            }
        }
    }

    // Fallback: try client's own Bearer token if pool had no usable token
    if let Some(client_token) = extract_bearer_token(&headers) {
        if let Ok(call_res) = state
            .upstream
            .call_v1_internal_raw_bytes(
                "fetchAvailableModels",
                &client_token,
                raw_req_body,
                incoming_ct.as_deref(),
                None,
                extra_headers,
                None,
            )
            .await
        {
            if call_res.response.status().is_success() {
                if let Ok(raw_text) = call_res.response.text().await {
                    let _ = asar_model_catalog::ingest_live_fetch_available_models(&raw_text);
                    let injected = asar_model_catalog::inject_models_into_fetch_response(&raw_text);
                    return json_raw_ok(injected);
                }
            }
        }
    }

    tracing::warn!(
        "[ASAR-Bridge] All upstream attempts failed for fetchAvailableModels; serving synthetic baseline catalog"
    );
    json_raw_ok(asar_model_catalog::create_synthetic_available_models_response())
}

/// Handle `/v1internal:streamGenerateContent`, `generateContent`, `countTokens`
/// with Zero-Reload per-request token rotation, Dual-Track slot pinning, Adaptive TTFB watchdog,
/// project rewrite, 403/429/503/400 auto-heal, Empty Thinking Completion SSE auto-heal,
/// and full SSE/non-stream token metering.
async fn handle_v1internal_pool(
    state: AppState,
    rpc_method: &str,
    query: Option<String>,
    headers: HeaderMap,
    body_bytes: Bytes,
) -> Response {
    let start_time = Instant::now();
    let body_len = body_bytes.len();
    let is_ai_request = matches!(rpc_method, "streamGenerateContent" | "generateContent");
    let is_stream = rpc_method == "streamGenerateContent";

    if is_ai_request {
        ASAR_TELEMETRY
            .total_requests
            .fetch_add(1, Ordering::Relaxed);
    }

    let mut body_json: Value = if body_bytes.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid JSON body: {}", e),
                );
            }
        }
    };

    let requested_model = extract_model_from_v1internal_body(&body_json);
    let mut target_model = if requested_model.is_empty() {
        "gemini-2.5-flash".to_string()
    } else {
        asar_model_catalog::resolve_target_model(&requested_model)
    };

    if is_ai_request && !requested_model.is_empty() && target_model != requested_model {
        rewrite_model_in_v1internal_body(&mut body_json, &target_model);
    }

    if is_ai_request {
        InboundThinkingPipeline::sanitize_inbound_signatures(&mut body_json, &target_model);
    }

    let quota_group = classify_quota_group(&target_model);
    let is_claude_track = quota_group == "claude";
    let dual_track = is_ai_request && ASAR_TELEMETRY.is_dual_track_enabled();

    let client_session_id = headers
        .get("x-session-id")
        .or_else(|| headers.get("x-vscode-sessionid"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let effective_session_id = if dual_track {
        Some(if is_claude_track {
            "asar_slot_claude".to_string()
        } else {
            "asar_slot_gemini".to_string()
        })
    } else {
        client_session_id.clone()
    };

    let extra_headers = collect_extra_headers(&headers);
    let audit_req_body = if is_ai_request && state.monitor.is_enabled() {
        let storage_mode = crate::proxy::config::get_payload_storage_mode();
        Some(format_audit_request_body(&body_json, &storage_mode))
    } else {
        None
    };

    let pool_len = state.token_manager.len();
    let max_account_attempts = pool_len.clamp(1, 8);
    let mut tried_accounts = HashSet::new();
    let mut force_rotate = false;
    let mut last_error_status = StatusCode::SERVICE_UNAVAILABLE;
    let mut last_error_body = String::from("No available accounts in Google Account Pool");

    for _attempt in 0..max_account_attempts {
        let token_res = select_account_for_request(
            &state,
            quota_group,
            is_claude_track,
            dual_track,
            force_rotate,
            effective_session_id.as_deref(),
            &target_model,
            pool_len,
            &tried_accounts,
        )
        .await;

        let (access_token, project_id, email, account_id, _wait_ms) = match token_res {
            Ok(tuple) => tuple,
            Err(e) => {
                last_error_body = e;
                break;
            }
        };

        let mut retried_without_project = false;
        let mut retried_sanitized = false;

        // Per-account inner retry loop for same-account auto-heal (403 SERVICE_DISABLED, 400 signature/thinkingLevel)
        loop {
            let mut req_body = body_json.clone();
            if !retried_without_project && !project_id.is_empty() {
                rewrite_project_in_body(&mut req_body, &project_id);
            } else if retried_without_project {
                remove_project_from_body(&mut req_body);
            }

            let ttfb_secs = adaptive_ttfb_timeout_secs(&target_model, &req_body, body_len);
            let in_flight_guard = if is_ai_request {
                Some(AsarInFlightGuard::acquire(Some(account_id.clone())))
            } else {
                None
            };

            let upstream_fut = state.upstream.call_v1_internal_with_headers(
                rpc_method,
                &access_token,
                req_body.clone(),
                query.as_deref(),
                extra_headers.clone(),
                Some(&account_id),
            );

            let timed_res =
                tokio::time::timeout(Duration::from_secs(ttfb_secs), upstream_fut).await;

            let call_res = match timed_res {
                Err(_elapsed) => {
                    drop(in_flight_guard);
                    tracing::warn!(
                        "[ASAR-Bridge] Fast-Failover TTFB timeout (>{}s) on {}",
                        ttfb_secs,
                        email
                    );
                    ASAR_TELEMETRY.record_failover();
                    if dual_track {
                        ASAR_TELEMETRY.clear_slot_if_matches(is_claude_track, &account_id);
                    }
                    last_error_status = StatusCode::GATEWAY_TIMEOUT;
                    last_error_body =
                        format!("Upstream TTFB timeout (>{}s) on {}", ttfb_secs, email);
                    tried_accounts.insert(account_id.clone());
                    force_rotate = true;
                    break;
                }
                Ok(Err(e)) => {
                    drop(in_flight_guard);
                    tracing::warn!(
                        "[ASAR-Bridge] Upstream network error for {} ({}): {}",
                        email,
                        rpc_method,
                        e
                    );
                    ASAR_TELEMETRY.record_failover();
                    if dual_track {
                        ASAR_TELEMETRY.clear_slot_if_matches(is_claude_track, &account_id);
                    }
                    last_error_status = StatusCode::BAD_GATEWAY;
                    last_error_body = e;
                    tried_accounts.insert(account_id.clone());
                    force_rotate = true;
                    break;
                }
                Ok(Ok(r)) => r,
            };

            let status = call_res.response.status();

            if status.is_success() {
                state.token_manager.mark_account_success(&account_id);
                if is_ai_request {
                    ASAR_TELEMETRY.record_active(&account_id, &email, &target_model);
                    if dual_track {
                        ASAR_TELEMETRY.set_slot(is_claude_track, account_id.clone(), email.clone());
                    }
                }

                if is_stream {
                    let resp_headers = call_res.response.headers().clone();
                    let raw_stream = call_res
                        .response
                        .bytes_stream()
                        .map_err(std::io::Error::other);

                    let heal_ctx = ThinkingAutoHealContext {
                        upstream: state.upstream.clone(),
                        method: "streamGenerateContent",
                        access_token: access_token.clone(),
                        original_body: req_body,
                        query_string: Some("alt=sse"),
                        extra_headers: extra_headers.clone(),
                        account_id: Some(account_id.clone()),
                        trace_id: format!("asar_{}", chrono::Utc::now().timestamp_subsec_millis()),
                        budget: None,
                    };

                    let healed_stream =
                        wrap_stream_with_empty_thinking_auto_heal(Box::pin(raw_stream), heal_ctx);

                    let metered_stream = wrap_sse_stream_with_metering(
                        healed_stream,
                        state.clone(),
                        rpc_method.to_string(),
                        status.as_u16(),
                        start_time,
                        requested_model.clone(),
                        target_model.clone(),
                        email.clone(),
                        effective_session_id.clone(),
                        audit_req_body,
                        in_flight_guard,
                    );

                    let mut builder = Response::builder().status(status);
                    if let Some(ct) = resp_headers.get(axum::http::header::CONTENT_TYPE) {
                        builder = builder.header(axum::http::header::CONTENT_TYPE, ct);
                    } else {
                        builder =
                            builder.header(axum::http::header::CONTENT_TYPE, "text/event-stream");
                    }
                    builder = builder.header("cache-control", "no-cache");
                    builder = builder.header("x-accel-buffering", "no");

                    return builder
                        .body(Body::from_stream(metered_stream))
                        .unwrap_or_else(|e| {
                            json_error(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                &format!("Stream response error: {}", e),
                            )
                        });
                } else {
                    let resp_ct = call_res
                        .response
                        .headers()
                        .get(axum::http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("application/json")
                        .to_string();
                    let resp_bytes = call_res.response.bytes().await.unwrap_or_default();
                    let duration_ms = start_time.elapsed().as_millis() as u64;
                    drop(in_flight_guard);

                    let usage = if is_ai_request {
                        extract_usage_from_bytes(&resp_bytes)
                    } else {
                        None
                    };
                    if let Some(ref u) = usage {
                        ASAR_TELEMETRY.record_tokens(
                            u.total_input_tokens,
                            u.output_tokens,
                            u.cached_tokens,
                        );
                    }

                    if is_ai_request {
                        record_bridge_log(
                            &state,
                            rpc_method,
                            status.as_u16(),
                            duration_ms,
                            Some(&requested_model),
                            Some(&target_model),
                            Some(&email),
                            None,
                            usage.as_ref(),
                            Some(duration_ms as f64),
                            effective_session_id.as_deref(),
                            audit_req_body,
                        );
                    }

                    return Response::builder()
                        .status(status)
                        .header(axum::http::header::CONTENT_TYPE, resp_ct)
                        .body(Body::from(resp_bytes))
                        .unwrap_or_else(|e| {
                            json_error(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                &format!("Response build error: {}", e),
                            )
                        });
                }
            }

            drop(in_flight_guard);

            let retry_after = call_res
                .response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let err_body = call_res.response.text().await.unwrap_or_default();
            last_error_status = status;
            last_error_body = err_body.clone();

            // 429 or 503 -> instant failover to next account
            if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE
            {
                tracing::warn!(
                    "[ASAR-Bridge] {} returned {} for {} ({}), failing over to next account",
                    email,
                    status.as_u16(),
                    target_model,
                    rpc_method
                );
                ASAR_TELEMETRY.record_failover();
                if dual_track {
                    ASAR_TELEMETRY.clear_slot_if_matches(is_claude_track, &account_id);
                }
                state
                    .token_manager
                    .mark_rate_limited_fast(
                        &email,
                        status.as_u16(),
                        retry_after.as_deref(),
                        &err_body,
                        Some(&target_model),
                    )
                    .await;
                tried_accounts.insert(account_id.clone());
                force_rotate = true;
                break;
            }

            // 403 Forbidden -> SERVICE_DISABLED auto-heal, VALIDATION_REQUIRED block, or ToS/forbidden mark
            if status == StatusCode::FORBIDDEN {
                match classify_403_error(&err_body) {
                    Forbidden403Kind::ServiceDisabled if !retried_without_project => {
                        tracing::warn!(
                            "[ASAR-Bridge] 403 SERVICE_DISABLED for {} (project={}); retrying without project",
                            email,
                            project_id
                        );
                        retried_without_project = true;
                        continue;
                    }
                    Forbidden403Kind::LocationUnsupported => {
                        tracing::warn!(
                            "[ASAR-Bridge] 403 location unsupported for {}; rotating without marking account forbidden",
                            email
                        );
                    }
                    Forbidden403Kind::ValidationRequired => {
                        tracing::warn!(
                            "[ASAR-Bridge] 403 VALIDATION_REQUIRED for {}; marking sticky validation_blocked",
                            email
                        );
                        let extracted_url =
                            crate::models::AccountBanInfo::extract_url_from_text(&err_body);
                        let reason_str = crate::modules::account::format_validation_reason(
                            &err_body,
                            extracted_url.as_deref(),
                        );
                        if let Err(e) = state
                            .token_manager
                            .set_validation_block_public(&account_id, 0, &reason_str)
                            .await
                        {
                            tracing::error!("[ASAR-Bridge] Failed to set validation block: {}", e);
                        }
                    }
                    Forbidden403Kind::ServiceDisabled | Forbidden403Kind::TosOrForbidden => {
                        tracing::warn!(
                            "[ASAR-Bridge] 403 Forbidden/ToS for {} on {}; marking forbidden and rotating",
                            email,
                            target_model
                        );
                        let req_opt =
                            (!requested_model.is_empty()).then_some(requested_model.as_str());
                        if let Err(e) = state
                            .token_manager
                            .set_forbidden_with_models(
                                &account_id,
                                &err_body,
                                req_opt,
                                Some(&target_model),
                            )
                            .await
                        {
                            tracing::error!("[ASAR-Bridge] Failed to set forbidden status: {}", e);
                        }
                    }
                }
                ASAR_TELEMETRY.record_failover();
                if dual_track {
                    ASAR_TELEMETRY.clear_slot_if_matches(is_claude_track, &account_id);
                }
                tried_accounts.insert(account_id.clone());
                force_rotate = true;
                break;
            }

            // 401 Unauthorized -> rotate account
            if status == StatusCode::UNAUTHORIZED {
                tracing::warn!(
                    "[ASAR-Bridge] 401 Unauthorized for {}; rotating account",
                    email
                );
                ASAR_TELEMETRY.record_failover();
                if dual_track {
                    ASAR_TELEMETRY.clear_slot_if_matches(is_claude_track, &account_id);
                }
                tried_accounts.insert(account_id.clone());
                force_rotate = true;
                break;
            }

            // 400 Bad Request -> sanitize thoughtSignature / thinkingLevel and retry once
            if status == StatusCode::BAD_REQUEST && is_ai_request && !retried_sanitized {
                let is_thinking_level_err =
                    UpstreamClassification::is_unsupported_thinking_level(400, &err_body);
                let is_sig_err = is_thought_signature_400(&err_body);

                if is_thinking_level_err || is_sig_err {
                    retried_sanitized = true;
                    if is_thinking_level_err {
                        tracing::warn!(
                            "[ASAR-Bridge] 400 unsupported thinkingLevel on {}; clamping to HIGH and retrying",
                            target_model
                        );
                        InboundThinkingPipeline::try_clamp_thinking_level(&mut body_json);
                        target_model = extract_model_from_v1internal_body(&body_json);
                    }
                    if is_sig_err {
                        tracing::warn!(
                            "[ASAR-Bridge] 400 thought signature error on {}; stripping signatures and retrying",
                            target_model
                        );
                        InboundThinkingPipeline::strip_signatures_for_retry(
                            &mut body_json,
                            client_session_id.as_deref(),
                            &target_model,
                        );
                    }
                    continue;
                }
            }

            // 404 Model Not Found -> mark unsupported on this account and try next
            if status == StatusCode::NOT_FOUND {
                ASAR_TELEMETRY.record_failover();
                if dual_track {
                    ASAR_TELEMETRY.clear_slot_if_matches(is_claude_track, &account_id);
                }
                state
                    .token_manager
                    .mark_model_unsupported(&account_id, &target_model, Some(900));
                tried_accounts.insert(account_id.clone());
                force_rotate = true;
                break;
            }

            // Other non-retryable status -> return directly to client
            record_bridge_log(
                &state,
                rpc_method,
                status.as_u16(),
                start_time.elapsed().as_millis() as u64,
                Some(&requested_model),
                Some(&target_model),
                Some(&email),
                Some(&err_body),
                None,
                None,
                effective_session_id.as_deref(),
                audit_req_body,
            );
            return Response::builder()
                .status(status)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(err_body))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
        }
    }

    record_bridge_log(
        &state,
        rpc_method,
        last_error_status.as_u16(),
        start_time.elapsed().as_millis() as u64,
        Some(&requested_model),
        Some(&target_model),
        None,
        Some(&last_error_body),
        None,
        None,
        effective_session_id.as_deref(),
        audit_req_body,
    );

    Response::builder()
        .status(last_error_status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(last_error_body))
        .unwrap_or_else(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())
}

#[allow(clippy::too_many_arguments)]
async fn select_account_for_request(
    state: &AppState,
    quota_group: &str,
    is_claude_track: bool,
    dual_track: bool,
    force_rotate: bool,
    session_id: Option<&str>,
    target_model: &str,
    pool_len: usize,
    tried_accounts: &HashSet<String>,
) -> Result<(String, String, String, String, u64), String> {
    // 1. If Dual-Track is active and this track already has a pinned healthy slot, prefer it first
    if dual_track && !force_rotate {
        if let Some((slot_acc_id, _slot_email)) = ASAR_TELEMETRY.get_slot(is_claude_track) {
            if !tried_accounts.contains(&slot_acc_id) {
                let mut exclude_others: HashSet<String> = state
                    .token_manager
                    .enabled_account_ids()
                    .into_iter()
                    .filter(|id| id != &slot_acc_id)
                    .collect();
                exclude_others.extend(tried_accounts.iter().cloned());

                if let Ok(tuple) = get_token_with_25_fallback(
                    state,
                    quota_group,
                    false,
                    session_id,
                    target_model,
                    &exclude_others,
                )
                .await
                {
                    return Ok(tuple);
                } else {
                    ASAR_TELEMETRY.clear_slot_if_matches(is_claude_track, &slot_acc_id);
                }
            }
        }
    }

    // 2. On Gemini worker track, avoid burning the pinned Claude Lead account when >1 accounts exist
    let claude_slot_id = ASAR_TELEMETRY.get_slot(true).map(|(id, _)| id);
    if let Some(exclusions_with_claude_lead) = build_dual_track_initial_exclusions(
        dual_track,
        quota_group,
        pool_len,
        claude_slot_id.as_deref(),
        tried_accounts,
    ) {
        if let Ok(tuple) = get_token_with_25_fallback(
            state,
            quota_group,
            force_rotate,
            session_id,
            target_model,
            &exclusions_with_claude_lead,
        )
        .await
        {
            return Ok(tuple);
        }
    }

    // 3. Standard pool selection (including fallback to Claude Lead if no other Gemini account is available)
    get_token_with_25_fallback(
        state,
        quota_group,
        force_rotate,
        session_id,
        target_model,
        tried_accounts,
    )
    .await
}

async fn get_token_with_25_fallback(
    state: &AppState,
    quota_group: &str,
    force_rotate: bool,
    session_id: Option<&str>,
    target_model: &str,
    excluded: &HashSet<String>,
) -> Result<(String, String, String, String, u64), String> {
    let mut res = state
        .token_manager
        .get_token_filtered(
            quota_group,
            force_rotate,
            session_id,
            target_model,
            excluded,
        )
        .await;

    if let Err(ref e) = res {
        if target_model == "gemini-2.5-flash"
            && e.contains("No accounts available with quota for model:")
        {
            for fallback_model in ["", "gemini-3.8-flash"] {
                let retry = state
                    .token_manager
                    .get_token_filtered(
                        quota_group,
                        force_rotate,
                        session_id,
                        fallback_model,
                        excluded,
                    )
                    .await;
                if retry.is_ok() {
                    res = retry;
                    break;
                }
            }
        }
    }
    res
}

pub(crate) fn build_dual_track_initial_exclusions(
    dual_track_enabled: bool,
    quota_group: &str,
    pool_len: usize,
    claude_slot_id: Option<&str>,
    tried_accounts: &HashSet<String>,
) -> Option<HashSet<String>> {
    if !dual_track_enabled || quota_group == "claude" || pool_len <= 1 {
        return None;
    }
    let cid = claude_slot_id?;
    if tried_accounts.contains(cid) {
        return None;
    }
    let mut excl = tried_accounts.clone();
    excl.insert(cid.to_string());
    Some(excl)
}

/// Adaptive TTFB Fast-Failover timeout:
/// - `60s` for Claude/Opus or heavy body (`>15_000` bytes) with thinking
/// - `35s` for Gemini with tools/images/search/thinking
/// - `22s` for light text Gemini calls
pub(crate) fn adaptive_ttfb_timeout_secs(
    target_model: &str,
    req_body: &Value,
    body_len: usize,
) -> u64 {
    let model_lower = target_model.to_ascii_lowercase();
    let body_str = serde_json::to_string(req_body).unwrap_or_default();
    let body_lower = body_str.to_ascii_lowercase();

    let has_thinking = model_lower.contains("thinking")
        || model_lower.contains("high")
        || body_lower.contains("\"thinkingconfig\"")
        || body_lower.contains("\"thinking_config\"")
        || body_lower.contains("\"thinkingbudget\"")
        || body_lower.contains("\"thinkinglevel\"");

    if model_lower.contains("claude")
        || model_lower.contains("opus")
        || (body_len > 15_000 && has_thinking)
    {
        return 60;
    }

    let has_rich_features = has_thinking
        || body_lower.contains("inlinedata")
        || body_lower.contains("inline_data")
        || body_lower.contains("\"tools\"")
        || body_lower.contains("functiondeclarations")
        || body_lower.contains("function_declarations")
        || body_lower.contains("googlesearch")
        || body_lower.contains("google_search");

    if has_rich_features {
        35
    } else {
        22
    }
}

struct SseMeterFinalizer {
    state: AppState,
    rpc_method: String,
    status: u16,
    start_time: Instant,
    requested_model: String,
    target_model: String,
    email: String,
    session_id: Option<String>,
    request_body: Option<String>,
    latest_usage: Option<CanonicalUsage>,
    ttft_ms: Option<f64>,
    output_bytes: usize,
    _in_flight_guard: Option<AsarInFlightGuard>,
}

impl Drop for SseMeterFinalizer {
    fn drop(&mut self) {
        let total_duration_ms = self.start_time.elapsed().as_millis() as u64;
        let resolved_usage = self.latest_usage.clone().or_else(|| {
            if self.output_bytes > 0 {
                Some(CanonicalUsage::new(
                    0,
                    0,
                    (self.output_bytes / 4).max(1) as u32,
                    0,
                ))
            } else {
                None
            }
        });

        if let Some(ref u) = resolved_usage {
            ASAR_TELEMETRY.record_tokens(u.total_input_tokens, u.output_tokens, u.cached_tokens);
        }

        record_bridge_log(
            &self.state,
            &self.rpc_method,
            self.status,
            total_duration_ms,
            Some(&self.requested_model),
            Some(&self.target_model),
            Some(&self.email),
            None,
            resolved_usage.as_ref(),
            self.ttft_ms,
            self.session_id.as_deref(),
            self.request_body.take(),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn wrap_sse_stream_with_metering<E>(
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, E>> + Send>>,
    state: AppState,
    rpc_method: String,
    status: u16,
    start_time: Instant,
    requested_model: String,
    target_model: String,
    email: String,
    session_id: Option<String>,
    request_body: Option<String>,
    in_flight_guard: Option<AsarInFlightGuard>,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, E>> + Send>>
where
    E: Send + 'static,
{
    Box::pin(async_stream::stream! {
        let mut finalizer = SseMeterFinalizer {
            state,
            rpc_method,
            status,
            start_time,
            requested_model,
            target_model,
            email,
            session_id,
            request_body,
            latest_usage: None,
            ttft_ms: None,
            output_bytes: 0,
            _in_flight_guard: in_flight_guard,
        };

        let mut tail_buf: Vec<u8> = Vec::with_capacity(4096);
        let mut stream = inner;

        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    if finalizer.ttft_ms.is_none() && !chunk.is_empty() {
                        finalizer.ttft_ms = Some(finalizer.start_time.elapsed().as_secs_f64() * 1000.0);
                    }
                    finalizer.output_bytes = finalizer.output_bytes.saturating_add(chunk.len());

                    // Lightweight line parser for SSE `usageMetadata` without buffering full response
                    tail_buf.extend_from_slice(&chunk);
                    while let Some(nl_pos) = tail_buf.iter().position(|&b| b == b'\n') {
                        if let Ok(line) = std::str::from_utf8(&tail_buf[..nl_pos]) {
                            if let Some(usage) = extract_usage_from_sse_line(line) {
                                finalizer.latest_usage = Some(usage);
                            }
                        }
                        tail_buf.drain(..=nl_pos);
                    }
                    // Prevent unbounded line growth on malformed non-newline chunks
                    if tail_buf.len() > 262_144 {
                        tail_buf.clear();
                    }

                    yield Ok(chunk);
                }
                Err(e) => {
                    yield Err(e);
                    break;
                }
            }
        }

        if !tail_buf.is_empty() {
            if let Ok(line) = std::str::from_utf8(&tail_buf) {
                if let Some(usage) = extract_usage_from_sse_line(line) {
                    finalizer.latest_usage = Some(usage);
                }
            }
        }
    })
}

pub(crate) fn extract_usage_from_sse_line(line: &str) -> Option<CanonicalUsage> {
    let trimmed = line.trim();
    let json_part = trimmed.strip_prefix("data:")?.trim();
    if json_part.is_empty()
        || json_part == "[DONE]"
        || (!json_part.contains("usageMetadata") && !json_part.contains("\"usage\""))
    {
        return None;
    }
    let val: Value = serde_json::from_str(json_part).ok()?;
    extract_usage_from_json_value(&val)
}

pub(crate) fn extract_usage_from_bytes(bytes: &[u8]) -> Option<CanonicalUsage> {
    if bytes.is_empty() {
        return None;
    }
    let val: Value = serde_json::from_slice(bytes).ok()?;
    extract_usage_from_json_value(&val)
}

pub(crate) fn extract_usage_from_json_value(val: &Value) -> Option<CanonicalUsage> {
    let usage = val
        .get("usageMetadata")
        .or_else(|| val.get("usage"))
        .or_else(|| val.get("response").and_then(|r| r.get("usageMetadata")))
        .or_else(|| val.get("response").and_then(|r| r.get("usage")))?;

    let has_tokens = usage.get("promptTokenCount").is_some()
        || usage.get("candidatesTokenCount").is_some()
        || usage.get("totalTokenCount").is_some()
        || usage.get("input_tokens").is_some()
        || usage.get("output_tokens").is_some();
    if !has_tokens {
        return None;
    }
    Some(CanonicalUsage::from_gemini(usage))
}

/// Clean byte-identical pass-through for setup/account `/v1internal:*` RPC endpoints
/// (e.g. `loadCodeAssist`, `onboardUser`, `fetchUserInfo`, `retrieveUserQuota`).
async fn handle_v1internal_passthrough(
    state: AppState,
    rpc_method: &str,
    query: Option<String>,
    headers: HeaderMap,
    body_bytes: Bytes,
) -> Response {
    let incoming_ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let raw_body = if body_bytes.is_empty() && incoming_ct.is_none() {
        Bytes::from_static(b"{}")
    } else {
        body_bytes
    };

    let extra_headers = collect_extra_headers(&headers);
    // Setup/auth/account RPCs (loadCodeAssist, onboardUser, fetchUserInfo, etc.)
    // must bypass model-quota filtering: prefer the client's own Bearer token first,
    // and only fall back to any healthy pool account without model-quota restriction.
    let token_and_acc = if let Some(client_tok) = extract_bearer_token(&headers) {
        Some((client_tok, None))
    } else if let Ok((tok, _proj, _email, acc_id, _)) = state
        .token_manager
        .get_token("gemini", false, None, "")
        .await
    {
        Some((tok, Some(acc_id)))
    } else if let Ok((tok, _proj, _email, acc_id, _)) = state
        .token_manager
        .get_token("gemini", false, None, "gemini-3.8-flash")
        .await
    {
        Some((tok, Some(acc_id)))
    } else {
        None
    };

    let Some((token, acc_id)) = token_and_acc else {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "No valid Google token available for v1internal pass-through",
        );
    };

    match state
        .upstream
        .call_v1_internal_raw_bytes(
            rpc_method,
            &token,
            raw_body,
            incoming_ct.as_deref(),
            query.as_deref(),
            extra_headers,
            acc_id.as_deref(),
        )
        .await
    {
        Ok(call_res) => {
            let status = call_res.response.status();
            let ct = call_res
                .response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            let bytes = call_res.response.bytes().await.unwrap_or_default();
            Response::builder()
                .status(status)
                .header(axum::http::header::CONTENT_TYPE, ct)
                .body(Body::from(bytes))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(e) => json_error(StatusCode::BAD_GATEWAY, &e),
    }
}

pub(crate) fn is_pool_ai_rpc(rpc_method: &str) -> bool {
    matches!(
        rpc_method,
        "streamGenerateContent" | "generateContent" | "countTokens"
    )
}

fn extract_v1internal_method(path: &str) -> Option<&str> {
    let trimmed = path.trim_start_matches('/');
    trimmed.strip_prefix("v1internal:")
}

fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    auth.strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn collect_extra_headers(headers: &HeaderMap) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (k, v) in headers.iter() {
        let name = k.as_str().to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "host" | "content-length" | "content-type" | "authorization" | "connection"
        ) {
            continue;
        }
        if let Ok(val) = v.to_str() {
            map.insert(k.as_str().to_string(), val.to_string());
        }
    }
    map
}

fn classify_quota_group(model: &str) -> &'static str {
    let lower = model.to_ascii_lowercase();
    if lower.contains("claude") {
        "claude"
    } else if lower.contains("image") {
        "image_gen"
    } else {
        "gemini"
    }
}

pub(crate) fn extract_model_from_v1internal_body(body: &Value) -> String {
    body.get("model")
        .and_then(Value::as_str)
        .or_else(|| {
            body.get("request")
                .and_then(|r| r.get("model"))
                .and_then(Value::as_str)
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub(crate) fn rewrite_model_in_v1internal_body(body: &mut Value, target_model: &str) {
    if let Some(obj) = body.as_object_mut() {
        if obj.contains_key("model") {
            obj.insert("model".to_string(), json!(target_model));
        }
        if let Some(req) = obj.get_mut("request").and_then(Value::as_object_mut) {
            if req.contains_key("model") {
                req.insert("model".to_string(), json!(target_model));
            }
        }
    }
}

pub(crate) fn rewrite_project_in_body(body: &mut Value, project_id: &str) {
    if project_id.trim().is_empty() {
        return;
    }
    if let Some(obj) = body.as_object_mut() {
        if obj.contains_key("project") {
            obj.insert("project".to_string(), json!(project_id));
        }
    }
}

pub(crate) fn remove_project_from_body(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.remove("project");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Forbidden403Kind {
    ServiceDisabled,
    LocationUnsupported,
    ValidationRequired,
    TosOrForbidden,
}

pub(crate) fn classify_403_error(err_body: &str) -> Forbidden403Kind {
    let upper = err_body.to_ascii_uppercase();
    if upper.contains("SERVICE_DISABLED") {
        return Forbidden403Kind::ServiceDisabled;
    }
    if crate::models::account::is_location_unsupported_error(err_body) {
        return Forbidden403Kind::LocationUnsupported;
    }
    let lower = err_body.to_ascii_lowercase();
    if upper.contains("VALIDATION_REQUIRED")
        || lower.contains("verify your account")
        || lower.contains("validation_url")
        || lower.contains("further action is required")
        || lower.contains("account_verification")
        || lower.contains("phone_verification")
    {
        return Forbidden403Kind::ValidationRequired;
    }
    Forbidden403Kind::TosOrForbidden
}

pub(crate) fn is_thought_signature_400(err_body: &str) -> bool {
    let lower = err_body.to_ascii_lowercase();
    (lower.contains("thought") || lower.contains("thinking"))
        && (lower.contains("signature") || lower.contains("corrupted"))
}

fn json_raw_ok(json_text: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        )
        .body(Body::from(json_text))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({ "error": msg }))).into_response()
}

pub(crate) fn format_audit_request_body(body_json: &Value, storage_mode: &str) -> String {
    const MAX_FULL_AUDIT_BYTES: usize = 65_536;
    if storage_mode == "full" {
        let raw = serde_json::to_string(body_json).unwrap_or_else(|_| "{}".to_string());
        if raw.len() > MAX_FULL_AUDIT_BYTES {
            format!(
                "{}...[truncated {} bytes]",
                crate::proxy::mappers::common_utils::safe_truncate_str(&raw, MAX_FULL_AUDIT_BYTES),
                raw.len() - MAX_FULL_AUDIT_BYTES
            )
        } else {
            raw
        }
    } else {
        let raw = serde_json::to_string(body_json).unwrap_or_else(|_| "{}".to_string());
        crate::proxy::payload_audit::apply_storage_mode_to_body(Some(raw), "simple")
            .unwrap_or_else(|| "{}".to_string())
    }
}

pub(crate) fn current_log_timestamp_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[allow(clippy::too_many_arguments)]
fn record_bridge_log(
    state: &AppState,
    rpc_method: &str,
    status: u16,
    duration: u64,
    model: Option<&str>,
    mapped_model: Option<&str>,
    account_email: Option<&str>,
    error: Option<&str>,
    usage: Option<&CanonicalUsage>,
    ttft_ms: Option<f64>,
    session_id: Option<&str>,
    request_body: Option<String>,
) {
    let response_headers =
        ttft_ms.map(|ms| json!({ "x-timing-ttft-ms": format!("{:.1}", ms) }).to_string());

    let log = ProxyRequestLog {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: current_log_timestamp_millis(),
        method: "POST".to_string(),
        url: format!("/v1internal:{}", rpc_method),
        status,
        duration,
        model: model.filter(|s| !s.is_empty()).map(str::to_string),
        mapped_model: mapped_model.filter(|s| !s.is_empty()).map(str::to_string),
        upstream_model: mapped_model.filter(|s| !s.is_empty()).map(str::to_string),
        account_email: account_email.map(str::to_string),
        client_ip: Some("127.0.0.1".to_string()),
        error: error.map(|s| {
            if s.len() > 500 {
                format!(
                    "{}...",
                    crate::proxy::mappers::common_utils::safe_truncate_str(s, 500)
                )
            } else {
                s.to_string()
            }
        }),
        request_body,
        upstream_request_body: None,
        response_body: None,
        request_headers: None,
        upstream_request_headers: None,
        response_headers,
        input_tokens: usage.map(|u| u.total_input_tokens),
        output_tokens: usage.map(|u| u.output_tokens),
        cached_tokens: usage.map(|u| u.cached_tokens),
        reasoning_tokens: usage.map(|u| u.reasoning_tokens),
        protocol: Some("v1internal".to_string()),
        username: None,
        session_id: session_id.map(str::to_string),
    };
    if let Some(email) = account_email.filter(|s| !s.is_empty()) {
        crate::modules::auto_refresh::mark_account_quota_dirty(email);
    }
    let monitor = state.monitor.clone();
    tokio::spawn(async move {
        monitor.log_request(log).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_v1internal_method() {
        assert_eq!(
            extract_v1internal_method("/v1internal:fetchAvailableModels"),
            Some("fetchAvailableModels")
        );
        assert_eq!(
            extract_v1internal_method("/v1internal:streamGenerateContent"),
            Some("streamGenerateContent")
        );
        assert_eq!(extract_v1internal_method("/v1/chat/completions"), None);
    }

    #[test]
    fn test_load_code_assist_bypasses_model_quota_pool() {
        assert!(is_pool_ai_rpc("streamGenerateContent"));
        assert!(is_pool_ai_rpc("generateContent"));
        assert!(is_pool_ai_rpc("countTokens"));
        assert!(!is_pool_ai_rpc("loadCodeAssist"));
        assert!(!is_pool_ai_rpc("onboardUser"));
        assert!(!is_pool_ai_rpc("fetchUserInfo"));
        assert!(!is_pool_ai_rpc("getCodeAssistGlobalUserSetting"));
        assert!(!is_pool_ai_rpc("recordCodeAssistMetrics"));
    }

    #[test]
    fn test_extract_and_rewrite_model_and_project() {
        let mut body = json!({
            "project": "old-proj",
            "model": "gemini-3.8-flash-high",
            "request": { "contents": [] }
        });
        assert_eq!(
            extract_model_from_v1internal_body(&body),
            "gemini-3.8-flash-high"
        );
        rewrite_model_in_v1internal_body(&mut body, "gemini-3.8-flash-tiered");
        rewrite_project_in_body(&mut body, "new-proj-123");
        assert_eq!(body["model"], "gemini-3.8-flash-tiered");
        assert_eq!(body["project"], "new-proj-123");
        remove_project_from_body(&mut body);
        assert!(body.get("project").is_none());
    }

    #[test]
    fn test_is_thought_signature_400() {
        assert!(is_thought_signature_400(
            "Invalid `signature` in `thinking` block"
        ));
        assert!(is_thought_signature_400("Corrupted thought signature"));
        assert!(!is_thought_signature_400("Invalid JSON syntax"));
    }

    #[test]
    fn test_classify_403_error() {
        assert_eq!(
            classify_403_error(
                r#"{"error":{"status":"PERMISSION_DENIED","message":"SERVICE_DISABLED"}}"#
            ),
            Forbidden403Kind::ServiceDisabled
        );
        assert_eq!(
            classify_403_error(
                r#"{"error":{"code":403,"message":"VALIDATION_REQUIRED","details":[{"metadata":{"validation_url":"https://accounts.google.com/v"}}]}} "#
            ),
            Forbidden403Kind::ValidationRequired
        );
        assert_eq!(
            classify_403_error("Please verify your account to continue."),
            Forbidden403Kind::ValidationRequired
        );
        assert_eq!(
            classify_403_error("User location is not supported for the API use."),
            Forbidden403Kind::LocationUnsupported
        );
        assert_eq!(
            classify_403_error(
                "This account has been suspended due to Violation of Terms of Service."
            ),
            Forbidden403Kind::TosOrForbidden
        );
    }

    #[test]
    fn test_sse_usage_metadata_parser() {
        // Wrapped v1internal response envelope
        let wrapped_line = r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"hi"}]}}],"usageMetadata":{"promptTokenCount":1200,"candidatesTokenCount":45,"cachedContentTokenCount":800,"thoughtsTokenCount":155}}}"#;
        let u = extract_usage_from_sse_line(wrapped_line).expect("should parse wrapped usage");
        assert_eq!(u.total_input_tokens, 1200);
        assert_eq!(u.cached_tokens, 800);
        assert_eq!(u.reasoning_tokens, 155);
        assert_eq!(u.output_tokens, 200); // 45 + 155

        // Top-level Gemini envelope
        let direct_line =
            r#"data: {"usageMetadata":{"promptTokenCount":300,"candidatesTokenCount":50}}"#;
        let u2 = extract_usage_from_sse_line(direct_line).expect("should parse direct usage");
        assert_eq!(u2.total_input_tokens, 300);
        assert_eq!(u2.output_tokens, 50);
        assert_eq!(u2.cached_tokens, 0);

        // Non-usage chunk & [DONE]
        assert!(extract_usage_from_sse_line("data: [DONE]").is_none());
        assert!(extract_usage_from_sse_line(r#"data: {"candidates":[]}"#).is_none());
    }

    #[test]
    fn test_adaptive_ttfb_timeout_secs() {
        let plain_body = json!({ "request": { "contents": [{ "parts": [{ "text": "hello" }] }] } });
        assert_eq!(
            adaptive_ttfb_timeout_secs("gemini-2.5-flash", &plain_body, 120),
            22
        );
        assert_eq!(
            adaptive_ttfb_timeout_secs("claude-sonnet-4-6", &plain_body, 120),
            60
        );

        let tools_body = json!({
            "request": {
                "contents": [{ "parts": [{ "text": "search" }] }],
                "tools": [{ "functionDeclarations": [{ "name": "read_file" }] }]
            }
        });
        assert_eq!(
            adaptive_ttfb_timeout_secs("gemini-2.5-flash", &tools_body, 500),
            35
        );

        let thinking_body = json!({
            "request": {
                "generationConfig": { "thinkingConfig": { "thinkingBudget": 8192 } }
            }
        });
        assert_eq!(
            adaptive_ttfb_timeout_secs("gemini-3.8-flash-tiered", &thinking_body, 20_000),
            60
        );
    }

    #[test]
    fn test_dual_track_exclusion_fallback() {
        let mut tried = HashSet::new();
        // Gemini track with >1 accounts excludes pinned Claude Lead
        let excl =
            build_dual_track_initial_exclusions(true, "gemini", 3, Some("acc-claude-pro"), &tried)
                .expect("should exclude claude lead when pool > 1");
        assert!(excl.contains("acc-claude-pro"));

        // Single-account pool must NOT exclude the only account
        assert!(build_dual_track_initial_exclusions(
            true,
            "gemini",
            1,
            Some("acc-claude-pro"),
            &tried
        )
        .is_none());

        // Claude track never excludes its own slot
        assert!(build_dual_track_initial_exclusions(
            true,
            "claude",
            3,
            Some("acc-claude-pro"),
            &tried
        )
        .is_none());

        // Disabled dual-track returns None
        assert!(build_dual_track_initial_exclusions(
            false,
            "gemini",
            3,
            Some("acc-claude-pro"),
            &tried
        )
        .is_none());

        // Already tried returns None (no redundant retry)
        tried.insert("acc-claude-pro".to_string());
        assert!(build_dual_track_initial_exclusions(
            true,
            "gemini",
            3,
            Some("acc-claude-pro"),
            &tried
        )
        .is_none());
    }

    #[test]
    fn test_non_content_v1internal_payload_not_wrapped_with_envelope() {
        use crate::proxy::upstream::client::UpstreamClient;

        for rpc in [
            "loadCodeAssist",
            "fetchAvailableModels",
            "onboardUser",
            "fetchUserInfo",
            "retrieveUserQuota",
        ] {
            let original = json!({
                "metadata": {
                    "ideType": "ANTIGRAVITY",
                    "platform": "WINDOWS_AMD64"
                }
            });
            let mut body = original.clone();
            UpstreamClient::prepare_v1_internal_json_envelope(rpc, &mut body);
            assert_eq!(body, original, "{} must not be mutated with envelope", rpc);
            assert!(body.get("project").is_none());
            assert!(body.get("requestId").is_none());
            assert!(body.get("request").is_none());
            assert!(body.get("userAgent").is_none());
        }
    }

    #[test]
    fn test_fetch_models_tier_rank_ultra_pro_free() {
        assert_eq!(fetch_models_tier_rank(Some("ULTRA")), 0);
        assert_eq!(fetch_models_tier_rank(Some("ultra")), 0);
        assert_eq!(fetch_models_tier_rank(Some("PRO")), 1);
        assert_eq!(fetch_models_tier_rank(Some("FREE")), 2);
        assert_eq!(fetch_models_tier_rank(None), 2);
    }

    #[test]
    fn test_audit_request_body_and_millis_timestamp() {
        // Timestamp must be Unix epoch in milliseconds (> 1_700_000_000_000), matching monitor.rs
        let ts = current_log_timestamp_millis();
        assert!(
            ts > 1_700_000_000_000,
            "expected millisecond timestamp, got {}",
            ts
        );

        let payload = json!({
            "project": "ag-proj-1",
            "model": "claude-sonnet-4-6",
            "request": {
                "contents": [
                    {
                        "role": "user",
                        "parts": [
                            { "text": "Inspect image" },
                            {
                                "inlineData": {
                                    "mimeType": "image/png",
                                    "data": "A".repeat(4096)
                                }
                            }
                        ]
                    }
                ]
            }
        });

        let simple = format_audit_request_body(&payload, "simple");
        assert!(simple.contains("Inspect image"));
        assert!(
            !simple.contains(&"A".repeat(512)),
            "simple mode must strip raw inlineData base64 bytes"
        );

        let full = format_audit_request_body(&payload, "full");
        assert!(full.contains("claude-sonnet-4-6"));
        assert!(full.contains(&"A".repeat(512)));
    }
}
