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
use sha2::{Digest, Sha256};
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
pub struct AsarActiveSlot {
    pub slot_key: String, // "claude_full" | "claude_basic" | "gemini_full" | "gemini_basic" | "gemini_lite"
    pub family: String,   // "Claude" | "Gemini"
    pub tier_kind: String, // "Full" | "Basic" | "Lite"
    pub account_id: String,
    pub email: String,
    pub models: Vec<String>, // Distinct short readable model labels served on this account in this slot
    pub in_flight: u64,      // Live active streaming count on this account
    pub last_used_ts: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsarInFlightStream {
    pub stream_id: u64,
    pub slot_key: String,
    pub account_id: String,
    pub email: String,
    pub requested_model: String,
    pub target_model: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone)]
struct ActiveStreamRecord {
    stream_id: u64,
    slot_key: String,
    account_id: String,
    email: String,
    requested_model: String,
    target_model: String,
    started_at: std::time::Instant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsarTelemetrySnapshot {
    pub in_flight: u64,
    pub asar_in_flight_account_ids: Vec<String>,
    pub in_flight_streams: Vec<AsarInFlightStream>,
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
    pub separate_lite_slot: bool,
    pub active_slots: Vec<AsarActiveSlot>,
    pub claude_slot_account_id: Option<String>,
    pub claude_slot_email: Option<String>,
    pub gemini_slot_account_id: Option<String>,
    pub gemini_slot_email: Option<String>,
    pub proxy_last_account_id: Option<String>,
    pub proxy_last_email: Option<String>,
    pub proxy_in_flight_account_ids: Vec<String>,
    pub native_current_account_id: Option<String>,
    pub equivalent_fallback_enabled: bool,
    pub cross_family_fallback_enabled: bool,
    pub fallback_downgrade_count: u64,
    pub last_fallback_route: Option<String>,
}

#[derive(Debug, Clone)]
struct LastActiveRecord {
    account_id: String,
    email: String,
    model: String,
    timestamp: i64,
}

const SLOT_ORDER: [&str; 5] = [
    "claude_full",
    "claude_basic",
    "gemini_full",
    "gemini_basic",
    "gemini_lite",
];

#[derive(Debug, Clone)]
struct SlotState {
    account_id: String,
    email: String,
    models: Vec<String>,
    last_used_ts: i64,
}

pub(crate) fn resolve_slot_key(
    is_claude_track: bool,
    model: &str,
    separate_lite_slot: bool,
) -> &'static str {
    let is_canary = is_flagship_canary_model(model);
    if is_claude_track {
        if is_canary {
            "claude_full"
        } else {
            "claude_basic"
        }
    } else if is_canary {
        "gemini_full"
    } else if separate_lite_slot && is_flash_lite_model(model) {
        "gemini_lite"
    } else {
        "gemini_basic"
    }
}

pub(crate) fn short_slot_model_label(model: &str) -> String {
    let lower = model.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return String::new();
    }
    if lower.contains("carbon") {
        return "Carbon".to_string();
    }
    if lower.contains("neon") {
        return "Neon".to_string();
    }
    if lower.contains("argon") {
        return "Argon".to_string();
    }
    if lower.contains("hydrogen") {
        return "Hydrogen".to_string();
    }
    if lower.contains("orchestrator") {
        return "Orchestrator".to_string();
    }
    if lower.contains("3.5-flash-lite") || lower.contains("3-5-flash-lite") {
        return "3.5 Flash Lite".to_string();
    }
    if lower.contains("flash-lite") {
        return "Flash Lite".to_string();
    }
    if lower.contains("3.8-flash") || lower.contains("3-8-flash") {
        return "3.8 Flash".to_string();
    }
    if lower.contains("2.5-flash") || lower.contains("2-5-flash") {
        return "2.5 Flash".to_string();
    }
    if lower.contains("claude") {
        if lower.contains("opus") && lower.contains("5-5") {
            return "Opus 5.5".to_string();
        }
        if lower.contains("sonnet") && lower.contains("5-5") {
            return "Sonnet 5.5".to_string();
        }
        if lower.contains("opus") && lower.contains("4-6") {
            return "Opus 4.6".to_string();
        }
        if lower.contains("sonnet") && lower.contains("4-6") {
            return "Sonnet 4.6".to_string();
        }
        if lower.contains("haiku") {
            return "Haiku".to_string();
        }
    }
    if lower.contains("gpt-oss") {
        return "GPT-OSS".to_string();
    }

    let stripped = lower
        .trim_end_matches("-thinking")
        .trim_end_matches("-xhigh")
        .trim_end_matches("-high")
        .trim_end_matches("-medium")
        .trim_end_matches("-low");
    stripped
        .split('-')
        .filter(|s| !s.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn slot_meta(slot_key: &str) -> (&'static str, &'static str) {
    match slot_key {
        "claude_full" => ("Claude", "Full"),
        "claude_basic" => ("Claude", "Basic"),
        "gemini_full" => ("Gemini", "Full"),
        "gemini_lite" => ("Gemini", "Lite"),
        _ => ("Gemini", "Basic"),
    }
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
    equivalent_fallback_enabled: AtomicBool,
    cross_family_fallback_enabled: AtomicBool,
    separate_lite_slot: AtomicBool,
    fallback_downgrade_count: AtomicU64,
    last_fallback_route: RwLock<Option<String>>,
    /// 5 named sticky slots: claude_full, claude_basic, gemini_full, gemini_basic, gemini_lite
    slots: RwLock<HashMap<&'static str, SlotState>>,
    /// Live concurrent AI streams currently in flight
    next_stream_id: AtomicU64,
    active_streams: std::sync::Mutex<Vec<ActiveStreamRecord>>,
    /// Legacy locks kept for direct unit-test compatibility
    claude_slot: RwLock<Option<(String, String)>>,
    gemini_slot: RwLock<Option<(String, String)>>,
    gemini_basic_slot: RwLock<Option<(String, String)>>,
}

impl AsarTelemetryHub {
    fn new() -> Self {
        let (
            persisted_dual_track,
            persisted_fallback,
            persisted_separate_lite,
            persisted_cross_family,
        ) = if cfg!(test) {
            (true, true, true, true)
        } else {
            let (dt, ef) = crate::modules::asar_patch::get_bridge_flags();
            let sl = crate::modules::asar_patch::get_separate_lite_slot_flag();
            let cf = crate::modules::asar_patch::get_cross_family_fallback_flag();
            (dt, ef, sl, cf)
        };
        Self {
            in_flight: AtomicU64::new(0),
            in_flight_by_account: DashMap::new(),
            total_requests: AtomicU64::new(0),
            failover_count: AtomicU64::new(0),
            total_input_tokens: AtomicU64::new(0),
            total_output_tokens: AtomicU64::new(0),
            total_cached_tokens: AtomicU64::new(0),
            last_active: RwLock::new(None),
            dual_track_enabled: AtomicBool::new(persisted_dual_track),
            equivalent_fallback_enabled: AtomicBool::new(persisted_fallback),
            cross_family_fallback_enabled: AtomicBool::new(persisted_cross_family),
            separate_lite_slot: AtomicBool::new(persisted_separate_lite),
            fallback_downgrade_count: AtomicU64::new(0),
            last_fallback_route: RwLock::new(None),
            slots: RwLock::new(HashMap::new()),
            next_stream_id: AtomicU64::new(0),
            active_streams: std::sync::Mutex::new(Vec::new()),
            claude_slot: RwLock::new(None),
            gemini_slot: RwLock::new(None),
            gemini_basic_slot: RwLock::new(None),
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

    fn is_equivalent_fallback_enabled(&self) -> bool {
        self.equivalent_fallback_enabled.load(Ordering::Relaxed)
    }

    fn set_equivalent_fallback(&self, enabled: bool) {
        self.equivalent_fallback_enabled
            .store(enabled, Ordering::Relaxed);
    }

    fn is_cross_family_fallback_enabled(&self) -> bool {
        self.cross_family_fallback_enabled.load(Ordering::Relaxed)
    }

    fn set_cross_family_fallback(&self, enabled: bool) {
        self.cross_family_fallback_enabled
            .store(enabled, Ordering::Relaxed);
    }

    fn is_separate_lite_slot_enabled(&self) -> bool {
        self.separate_lite_slot.load(Ordering::Relaxed)
    }

    fn set_separate_lite_slot(&self, enabled: bool) {
        self.separate_lite_slot.store(enabled, Ordering::Relaxed);
        if !enabled {
            if let Ok(mut map) = self.slots.write() {
                map.remove("gemini_lite");
            }
        }
    }

    fn record_fallback_route(&self, route: String) {
        self.fallback_downgrade_count
            .fetch_add(1, Ordering::Relaxed);
        if let Ok(mut g) = self.last_fallback_route.write() {
            *g = Some(route);
        }
    }

    /// Records the last active model/account. Background `*-flash-lite*` micro-model calls
    /// (IDE title/summary generation) never overwrite an existing real active record.
    fn record_active_guarded(&self, account_id: &str, email: &str, model: &str) {
        if is_flash_lite_model(model) {
            let has_existing = self.last_active.read().map(|g| g.is_some()).unwrap_or(true);
            if has_existing {
                return;
            }
        }
        self.record_active(account_id, email, model);
    }

    fn reset_slots(&self) {
        if let Ok(mut map) = self.slots.write() {
            map.clear();
        }
        if let Ok(mut c) = self.claude_slot.write() {
            *c = None;
        }
        if let Ok(mut g) = self.gemini_slot.write() {
            *g = None;
        }
        if let Ok(mut b) = self.gemini_basic_slot.write() {
            *b = None;
        }
    }

    fn get_slot_by_key(&self, slot_key: &str) -> Option<(String, String)> {
        self.slots.read().ok().and_then(|map| {
            map.get(slot_key)
                .map(|s| (s.account_id.clone(), s.email.clone()))
        })
    }

    fn set_slot_by_key(
        &self,
        slot_key: &'static str,
        account_id: String,
        email: String,
        model: &str,
    ) {
        let now = chrono::Utc::now().timestamp();
        let label = short_slot_model_label(model);
        if let Ok(mut map) = self.slots.write() {
            match map.get_mut(slot_key) {
                Some(existing) if existing.account_id == account_id => {
                    existing.email = email.clone();
                    existing.last_used_ts = now;
                    if !label.is_empty() && !existing.models.iter().any(|m| m == &label) {
                        if existing.models.len() >= 4 {
                            existing.models.remove(0);
                        }
                        existing.models.push(label);
                    }
                }
                _ => {
                    let models = if label.is_empty() {
                        Vec::new()
                    } else {
                        vec![label]
                    };
                    map.insert(
                        slot_key,
                        SlotState {
                            account_id: account_id.clone(),
                            email: email.clone(),
                            models,
                            last_used_ts: now,
                        },
                    );
                }
            }
        }
        // Keep legacy fields synchronized
        match slot_key {
            "claude_full" | "claude_basic" => self.set_slot(true, account_id, email),
            "gemini_full" => self.set_slot(false, account_id, email),
            "gemini_basic" => self.set_gemini_basic_slot(account_id, email),
            _ => {}
        }
    }

    fn clear_slot_by_key_if_matches(&self, slot_key: &str, account_id: &str) {
        if let Ok(mut map) = self.slots.write() {
            if map
                .get(slot_key)
                .is_some_and(|s| s.account_id == account_id)
            {
                map.remove(slot_key);
            }
        }
        match slot_key {
            "claude_full" | "claude_basic" => self.clear_slot_if_matches(true, account_id),
            "gemini_full" => self.clear_slot_if_matches(false, account_id),
            "gemini_basic" => self.clear_gemini_basic_slot_if_matches(account_id),
            _ => {}
        }
    }

    fn active_slots_snapshot(&self) -> Vec<AsarActiveSlot> {
        let Ok(map) = self.slots.read() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for key in SLOT_ORDER {
            if let Some(s) = map.get(key) {
                let (family, tier_kind) = slot_meta(key);
                let in_flight = self
                    .in_flight_by_account
                    .get(&s.account_id)
                    .map(|c| c.load(Ordering::Relaxed) as u64)
                    .unwrap_or(0);
                out.push(AsarActiveSlot {
                    slot_key: key.to_string(),
                    family: family.to_string(),
                    tier_kind: tier_kind.to_string(),
                    account_id: s.account_id.clone(),
                    email: s.email.clone(),
                    models: s.models.clone(),
                    in_flight,
                    last_used_ts: s.last_used_ts,
                });
            }
        }
        out
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

    fn get_gemini_basic_slot(&self) -> Option<(String, String)> {
        self.gemini_basic_slot.read().ok().and_then(|g| g.clone())
    }

    fn set_gemini_basic_slot(&self, account_id: String, email: String) {
        if let Ok(mut g) = self.gemini_basic_slot.write() {
            *g = Some((account_id, email));
        }
    }

    fn clear_gemini_basic_slot_if_matches(&self, account_id: &str) {
        if let Ok(mut g) = self.gemini_basic_slot.write() {
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

    fn active_streams_snapshot(&self) -> Vec<AsarInFlightStream> {
        let Ok(guard) = self.active_streams.lock() else {
            return Vec::new();
        };
        guard
            .iter()
            .map(|rec| AsarInFlightStream {
                stream_id: rec.stream_id,
                slot_key: rec.slot_key.clone(),
                account_id: rec.account_id.clone(),
                email: rec.email.clone(),
                requested_model: rec.requested_model.clone(),
                target_model: rec.target_model.clone(),
                elapsed_ms: rec.started_at.elapsed().as_millis() as u64,
            })
            .collect()
    }
}

static ASAR_TELEMETRY: Lazy<AsarTelemetryHub> = Lazy::new(AsarTelemetryHub::new);

struct AsarInFlightGuard {
    account_id: Option<String>,
    stream_id: Option<u64>,
}

impl AsarInFlightGuard {
    fn acquire(
        slot_key: &str,
        account_id: String,
        email: String,
        requested_model: String,
        target_model: String,
    ) -> Self {
        ASAR_TELEMETRY.in_flight.fetch_add(1, Ordering::SeqCst);
        ASAR_TELEMETRY
            .in_flight_by_account
            .entry(account_id.clone())
            .or_insert_with(|| AtomicUsize::new(0))
            .fetch_add(1, Ordering::SeqCst);
        let stream_id = ASAR_TELEMETRY
            .next_stream_id
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        if let Ok(mut guard) = ASAR_TELEMETRY.active_streams.lock() {
            guard.push(ActiveStreamRecord {
                stream_id,
                slot_key: slot_key.to_string(),
                account_id: account_id.clone(),
                email,
                requested_model,
                target_model,
                started_at: std::time::Instant::now(),
            });
        }
        Self {
            account_id: Some(account_id),
            stream_id: Some(stream_id),
        }
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
        if let Some(sid) = self.stream_id {
            if let Ok(mut guard) = ASAR_TELEMETRY.active_streams.lock() {
                guard.retain(|rec| rec.stream_id != sid);
            }
        }
    }
}

pub fn set_dual_track_enabled(enabled: bool) {
    ASAR_TELEMETRY.set_dual_track(enabled);
    if let Err(e) = crate::modules::asar_patch::persist_dual_track_flag(enabled) {
        tracing::warn!("[ASAR-Bridge] Failed to persist dual_track flag: {}", e);
    }
}

pub fn set_equivalent_fallback_enabled(enabled: bool) {
    ASAR_TELEMETRY.set_equivalent_fallback(enabled);
    if let Err(e) = crate::modules::asar_patch::persist_equivalent_fallback_flag(enabled) {
        tracing::warn!(
            "[ASAR-Bridge] Failed to persist equivalent_fallback flag: {}",
            e
        );
    }
}

pub fn set_cross_family_fallback_enabled(enabled: bool) {
    ASAR_TELEMETRY.set_cross_family_fallback(enabled);
    if let Err(e) = crate::modules::asar_patch::persist_cross_family_fallback_flag(enabled) {
        tracing::warn!(
            "[ASAR-Bridge] Failed to persist cross_family_fallback flag: {}",
            e
        );
    }
}

pub fn set_separate_lite_slot_enabled(enabled: bool) {
    ASAR_TELEMETRY.set_separate_lite_slot(enabled);
    if let Err(e) = crate::modules::asar_patch::persist_separate_lite_slot_flag(enabled) {
        tracing::warn!(
            "[ASAR-Bridge] Failed to persist separate_lite_slot flag: {}",
            e
        );
    }
}

/// `true` for flagship canary models only available on "Full" accounts
/// (`carbon`, `neon`, `argon`, `claude-*-5-5`).
pub(crate) fn is_flagship_canary_model(model: &str) -> bool {
    asar_model_catalog::is_flagship_canary_model(model)
}

/// `true` for Antigravity IDE's internal background micro-model (title / summary generation).
pub(crate) fn is_flash_lite_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("flash-lite")
}

/// Map an exhausted flagship canary model to its graceful equivalent on Basic accounts.
pub(crate) fn resolve_equivalent_fallback_model(target_model: &str) -> Option<String> {
    let lower = target_model.trim().to_ascii_lowercase();
    let lower = lower.strip_prefix("models/").unwrap_or(&lower);

    if lower.contains("gemini-4-carbon")
        || lower.contains("gemini-4-neon")
        || lower.contains("gemini-4-argon")
    {
        let suffix = if lower.ends_with("-xhigh") {
            "-xhigh"
        } else if lower.ends_with("-high") {
            "-high"
        } else if lower.ends_with("-low") {
            "-low"
        } else {
            "-medium"
        };
        return Some(format!("gemini-4-hydrogen{}", suffix));
    }
    if lower.starts_with("claude-opus-5") {
        return Some("claude-opus-4-6-thinking".to_string());
    }
    if lower == "claude-opus-4-6-thinking" {
        return Some("claude-sonnet-4-6".to_string());
    }
    if lower.starts_with("claude-sonnet-5") {
        return Some("claude-sonnet-4-6".to_string());
    }
    if lower.starts_with("gemini-pro-orchestrator") {
        return Some("gemini-4-hydrogen-high".to_string());
    }
    None
}

fn infer_gemini_tier_suffix(
    requested_model: &str,
    current_target: &str,
    body_json: &Value,
) -> &'static str {
    let probe = if !requested_model.trim().is_empty() {
        requested_model
    } else {
        current_target
    };
    let lower = probe.trim().to_ascii_lowercase();
    if lower.ends_with("-low") {
        return "-low";
    }
    if lower.ends_with("-xhigh") || lower.ends_with("-high") {
        return "-high";
    }
    if lower.ends_with("-medium") {
        return "-medium";
    }
    let thinking_level = body_json
        .get("generationConfig")
        .or_else(|| {
            body_json
                .get("request")
                .and_then(|r| r.get("generationConfig"))
        })
        .and_then(|gc| gc.get("thinkingConfig"))
        .and_then(|tc| tc.get("thinkingLevel"))
        .and_then(Value::as_str);
    if let Some(level) = thinking_level {
        match level.trim().to_ascii_uppercase().as_str() {
            "LOW" => return "-low",
            "HIGH" | "XHIGH" => return "-high",
            "MEDIUM" => return "-medium",
            _ => {}
        }
    }
    "-medium"
}

/// Multi-stage fallback selector:
/// 1) Same-family equivalent downgrade (if `equivalent_enabled`),
/// 2) Cross-family rescue (if `cross_family_enabled`): Claude/GPT-OSS -> Gemini Carbon -> Gemini Hydrogen,
///    or interactive non-flash-lite Gemini -> Claude Sonnet 5.5 -> Claude Sonnet 4.6.
pub(crate) fn resolve_next_fallback_model(
    current_target: &str,
    requested_model: &str,
    body_json: &Value,
    visited: &HashSet<String>,
    equivalent_enabled: bool,
    cross_family_enabled: bool,
) -> Option<String> {
    if equivalent_enabled {
        if let Some(eq_model) = resolve_equivalent_fallback_model(current_target) {
            let eq_lower = eq_model.to_ascii_lowercase();
            if !eq_lower.eq_ignore_ascii_case(current_target) && !visited.contains(&eq_lower) {
                return Some(eq_model);
            }
        }
    }

    if !cross_family_enabled {
        return None;
    }

    let lower = current_target.trim().to_ascii_lowercase();
    let lower = lower.strip_prefix("models/").unwrap_or(&lower);

    if (lower.starts_with("claude-") || lower.starts_with("gpt-oss-"))
        && !visited.iter().any(|m| m.starts_with("gemini-"))
    {
        let tier = infer_gemini_tier_suffix(requested_model, current_target, body_json);
        let carbon = format!("gemini-4-carbon{}", tier);
        if !visited.contains(&carbon) {
            return Some(carbon);
        }
        let hydrogen = format!("gemini-4-hydrogen{}", tier);
        if !visited.contains(&hydrogen) {
            return Some(hydrogen);
        }
    } else if lower.starts_with("gemini-")
        && !is_flash_lite_model(lower)
        && !is_flash_lite_model(requested_model)
        && !visited
            .iter()
            .any(|m| m.starts_with("claude-") || m.starts_with("gpt-oss-"))
    {
        if !visited.contains("claude-sonnet-5-5-medium") {
            return Some("claude-sonnet-5-5-medium".to_string());
        }
        if !visited.contains("claude-sonnet-4-6") {
            return Some("claude-sonnet-4-6".to_string());
        }
    }

    None
}

fn sanitize_generation_config_for_target(gen_cfg: &mut Value, target_model: &str) {
    let Some(gc) = gen_cfg.as_object_mut() else {
        return;
    };
    let lower = target_model.trim().to_ascii_lowercase();
    let lower = lower.strip_prefix("models/").unwrap_or(&lower);

    if lower.starts_with("gemini-") {
        if let Some(max_tok) = gc.get("maxOutputTokens").and_then(Value::as_u64) {
            if max_tok > 65_536 {
                gc.insert("maxOutputTokens".to_string(), json!(65_536));
            }
        }
        if let Some(tc) = gc.get_mut("thinkingConfig").and_then(Value::as_object_mut) {
            if let Some(level) = tc
                .get("thinkingLevel")
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                tc.remove("thinkingBudget");
                let normalized = match level.trim().to_ascii_uppercase().as_str() {
                    "LOW" => "LOW",
                    "MEDIUM" => "MEDIUM",
                    "HIGH" => "HIGH",
                    "XHIGH" => {
                        if lower.ends_with("-xhigh") {
                            "XHIGH"
                        } else {
                            "HIGH"
                        }
                    }
                    _ => "HIGH",
                };
                tc.insert("thinkingLevel".to_string(), json!(normalized));
            }
        }
    } else if lower.starts_with("claude-") {
        if let Some(max_tok) = gc.get("maxOutputTokens").and_then(Value::as_u64) {
            if max_tok > 64_000 {
                gc.insert("maxOutputTokens".to_string(), json!(64_000));
            }
        }
        if lower == "claude-sonnet-4-6" {
            gc.remove("thinkingConfig");
        }
    }
}

pub(crate) fn sanitize_body_for_fallback_target(
    body_json: &mut Value,
    target_model: &str,
    client_session_id: Option<&str>,
) {
    rewrite_model_in_v1internal_body(body_json, target_model);
    InboundThinkingPipeline::strip_signatures_for_retry(body_json, client_session_id, target_model);
    InboundThinkingPipeline::sanitize_inbound_signatures(body_json, target_model);

    if let Some(gc) = body_json.get_mut("generationConfig") {
        sanitize_generation_config_for_target(gc, target_model);
    }
    if let Some(gc) = body_json
        .get_mut("request")
        .and_then(|r| r.get_mut("generationConfig"))
    {
        sanitize_generation_config_for_target(gc, target_model);
    }
}

/// Sort key for `fetchAvailableModels` upstream account selection:
/// `ULTRA > PRO > FREE`, then accounts holding flagship canary models, then richest catalog,
/// then lowest manual priority.
pub(crate) fn fetch_models_sort_key(
    tier_rank: u8,
    has_flagship_canary: bool,
    available_models_len: usize,
    priority: u8,
) -> (u8, u8, usize, u8) {
    (
        tier_rank,
        u8::from(!has_flagship_canary),
        usize::MAX - available_models_len,
        priority,
    )
}

pub fn reset_dual_track_slots() {
    ASAR_TELEMETRY.reset_slots();
}

/// Derive a deterministic, per-account session ID from a raw session value.
///
/// Returns `"sess_"` + first 24 hex chars of `SHA-256("ag_asar:<account_id>:<raw_session>")`.
/// Guarantees:
/// - Same `(account_id, raw_session)` → same output on every call → 100% prompt-cache hit
///   for repeated turns on the same account.
/// - Different `account_id` for the same `raw_session` → different output → no cross-account
///   correlation leak on Google's servers.
pub(crate) fn isolate_session_for_account(account_id: &str, raw_session: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("ag_asar:{}:{}", account_id, raw_session).as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest
        .iter()
        .flat_map(|b| {
            [
                char::from_digit((b >> 4) as u32, 16).unwrap_or('0'),
                char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'),
            ]
        })
        .take(24)
        .collect();
    format!("sess_{}", hex)
}

pub async fn snapshot_asar_telemetry(state: Option<&AppState>) -> AsarTelemetrySnapshot {
    let last_active = ASAR_TELEMETRY
        .last_active
        .read()
        .ok()
        .and_then(|g| g.clone());
    let claude_slot = ASAR_TELEMETRY.get_slot(true);
    // For the ⚡ Gemini row, prefer gemini_slot (Full flagship); fall back to gemini_basic_slot
    // so the UI stays at 2 rows with no schema change.
    let gemini_slot = ASAR_TELEMETRY
        .get_slot(false)
        .or_else(|| ASAR_TELEMETRY.get_gemini_basic_slot());

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
        in_flight_streams: ASAR_TELEMETRY.active_streams_snapshot(),
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
        separate_lite_slot: ASAR_TELEMETRY.is_separate_lite_slot_enabled(),
        active_slots: ASAR_TELEMETRY.active_slots_snapshot(),
        claude_slot_account_id: claude_slot.as_ref().map(|(id, _)| id.clone()),
        claude_slot_email: claude_slot.as_ref().map(|(_, email)| email.clone()),
        gemini_slot_account_id: gemini_slot.as_ref().map(|(id, _)| id.clone()),
        gemini_slot_email: gemini_slot.as_ref().map(|(_, email)| email.clone()),
        proxy_last_account_id,
        proxy_last_email,
        proxy_in_flight_account_ids,
        native_current_account_id,
        equivalent_fallback_enabled: ASAR_TELEMETRY.is_equivalent_fallback_enabled(),
        cross_family_fallback_enabled: ASAR_TELEMETRY.is_cross_family_fallback_enabled(),
        fallback_downgrade_count: ASAR_TELEMETRY
            .fallback_downgrade_count
            .load(Ordering::Relaxed),
        last_fallback_route: ASAR_TELEMETRY
            .last_fallback_route
            .read()
            .ok()
            .and_then(|g| g.clone()),
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
    let mut candidates: Vec<((u8, u8, usize, u8), String)> = state
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
            let has_canary = tok
                .available_models
                .iter()
                .any(|m| is_flagship_canary_model(m));
            Some((
                fetch_models_sort_key(
                    fetch_models_tier_rank(tier.as_deref()),
                    has_canary,
                    tok.available_models.len(),
                    tok.priority,
                ),
                id,
            ))
        })
        .collect();
    candidates.sort_by_key(|a| a.0);
    candidates.into_iter().map(|(_, id)| id).collect()
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

    // Try up to 5 accounts in richest-catalog-first order using metadata tokens (ignoring 0% chat quota / rate-limit)
    for target_acc_id in ordered_ids.iter().take(5) {
        if tried.contains(target_acc_id) {
            continue;
        }
        match state
            .token_manager
            .get_metadata_token_by_id(target_acc_id)
            .await
        {
            Ok((access_token, email, account_id)) => {
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
                            "[ASAR-Bridge] fetchAvailableModels (metadata) upstream {} for {}, trying next account",
                            status,
                            email
                        );
                        tried.insert(account_id);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[ASAR-Bridge] fetchAvailableModels (metadata) error for {}: {}",
                            email,
                            e
                        );
                        tried.insert(account_id);
                    }
                }
            }
            Err(_) => {
                // Will still be eligible for get_token_filtered fallback below
            }
        }
    }

    // Fallback: try healthy accounts via get_token_filtered in tier order
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

    let mut quota_group = classify_quota_group(&target_model);
    let mut is_claude_track = quota_group == "claude";
    // 5-slot routing: claude_full, claude_basic, gemini_full, gemini_basic, gemini_lite
    let mut slot_key = resolve_slot_key(
        is_claude_track,
        &target_model,
        ASAR_TELEMETRY.is_separate_lite_slot_enabled(),
    );
    let dual_track = is_ai_request && ASAR_TELEMETRY.is_dual_track_enabled();

    let client_session_id = headers
        .get("x-session-id")
        .or_else(|| headers.get("x-vscode-sessionid"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut effective_session_id = if dual_track {
        Some(format!("asar_slot_{}", slot_key))
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

    const MAX_FALLBACK_HOPS: usize = 3;
    let mut fallback_hops: usize = 0;
    let mut visited_models: HashSet<String> = HashSet::new();
    visited_models.insert(target_model.to_ascii_lowercase());
    let mut fallback_chain = if !requested_model.trim().is_empty() {
        requested_model.clone()
    } else {
        target_model.clone()
    };

    'fallback_pass: loop {
        for _attempt in 0..max_account_attempts {
            let token_res = select_account_for_request(
                &state,
                quota_group,
                slot_key,
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

                // Per-account session isolation: rewrite sessionId fields in the body so
                // concurrent accounts never share the same UUID on Google's servers, while
                // repeated turns on the same account keep an identical hash → cache hit.
                if is_ai_request {
                    isolate_session_in_body(&mut req_body, &account_id);
                }

                // Per-account session header isolation + forward with scrubbed headers.
                let mut per_account_headers = extra_headers.clone();
                if is_ai_request {
                    isolate_session_headers(&mut per_account_headers, &account_id);
                }

                let ttfb_secs = adaptive_ttfb_timeout_secs(&target_model, &req_body, body_len);
                let in_flight_guard = if is_ai_request {
                    Some(AsarInFlightGuard::acquire(
                        slot_key,
                        account_id.clone(),
                        email.clone(),
                        requested_model.clone(),
                        target_model.clone(),
                    ))
                } else {
                    None
                };

                let upstream_fut = state.upstream.call_v1_internal_with_headers(
                    rpc_method,
                    &access_token,
                    req_body.clone(),
                    query.as_deref(),
                    per_account_headers.clone(),
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
                            ASAR_TELEMETRY.clear_slot_by_key_if_matches(slot_key, &account_id);
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
                            ASAR_TELEMETRY.clear_slot_by_key_if_matches(slot_key, &account_id);
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
                        ASAR_TELEMETRY.record_active_guarded(&account_id, &email, &target_model);
                        if dual_track {
                            ASAR_TELEMETRY.set_slot_by_key(
                                slot_key,
                                account_id.clone(),
                                email.clone(),
                                &target_model,
                            );
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
                            trace_id: format!(
                                "asar_{}",
                                chrono::Utc::now().timestamp_subsec_millis()
                            ),
                            budget: None,
                        };

                        let healed_stream = wrap_stream_with_empty_thinking_auto_heal(
                            Box::pin(raw_stream),
                            heal_ctx,
                        );

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
                            builder = builder
                                .header(axum::http::header::CONTENT_TYPE, "text/event-stream");
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
                if status == StatusCode::TOO_MANY_REQUESTS
                    || status == StatusCode::SERVICE_UNAVAILABLE
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
                        ASAR_TELEMETRY.clear_slot_by_key_if_matches(slot_key, &account_id);
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
                                tracing::error!(
                                    "[ASAR-Bridge] Failed to set validation block: {}",
                                    e
                                );
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
                                tracing::error!(
                                    "[ASAR-Bridge] Failed to set forbidden status: {}",
                                    e
                                );
                            }
                        }
                    }
                    ASAR_TELEMETRY.record_failover();
                    if dual_track {
                        ASAR_TELEMETRY.clear_slot_by_key_if_matches(slot_key, &account_id);
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
                        ASAR_TELEMETRY.clear_slot_by_key_if_matches(slot_key, &account_id);
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
                        ASAR_TELEMETRY.clear_slot_by_key_if_matches(slot_key, &account_id);
                    }
                    state.token_manager.mark_model_unsupported(
                        &account_id,
                        &target_model,
                        Some(900),
                    );
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

        // Layer 3 — Multi-Stage Fallback Cascade + Cross-Family Rescue:
        // Hop 1: Same-family equivalent (Carbon/Neon/Argon -> Hydrogen, Claude 5.5 -> 4.6, Opus 4.6 -> Sonnet 4.6)
        // Hop 2 & 3: Cross-family rescue (Claude/GPT <-> Gemini Carbon -> Gemini Hydrogen)
        if is_ai_request && fallback_hops < MAX_FALLBACK_HOPS {
            if let Some(next_model) = resolve_next_fallback_model(
                &target_model,
                &requested_model,
                &body_json,
                &visited_models,
                ASAR_TELEMETRY.is_equivalent_fallback_enabled(),
                ASAR_TELEMETRY.is_cross_family_fallback_enabled(),
            ) {
                tracing::warn!(
                    "[ASAR-Bridge] All accounts failed for {}; fallback hop {} -> {}",
                    target_model,
                    fallback_hops + 1,
                    next_model
                );
                fallback_chain = format!("{} -> {}", fallback_chain, next_model);
                ASAR_TELEMETRY.record_fallback_route(fallback_chain.clone());

                sanitize_body_for_fallback_target(
                    &mut body_json,
                    &next_model,
                    client_session_id.as_deref(),
                );
                target_model = next_model;
                visited_models.insert(target_model.to_ascii_lowercase());

                quota_group = classify_quota_group(&target_model);
                is_claude_track = quota_group == "claude";
                slot_key = resolve_slot_key(
                    is_claude_track,
                    &target_model,
                    ASAR_TELEMETRY.is_separate_lite_slot_enabled(),
                );
                if dual_track {
                    effective_session_id = Some(format!("asar_slot_{}", slot_key));
                }
                tried_accounts.clear();
                force_rotate = false;
                fallback_hops += 1;
                continue 'fallback_pass;
            }
        }
        break;
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

    if let Ok(parsed) = serde_json::from_str::<Value>(&last_error_body) {
        if parsed.is_object() && parsed.get("error").is_some() {
            return Response::builder()
                .status(last_error_status)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(last_error_body))
                .unwrap_or_else(|_| StatusCode::SERVICE_UNAVAILABLE.into_response());
        }
    }

    let rpc_status = match last_error_status {
        StatusCode::TOO_MANY_REQUESTS => "RESOURCE_EXHAUSTED",
        StatusCode::BAD_REQUEST => "INVALID_ARGUMENT",
        StatusCode::UNAUTHORIZED => "UNAUTHENTICATED",
        StatusCode::FORBIDDEN => "PERMISSION_DENIED",
        StatusCode::NOT_FOUND => "NOT_FOUND",
        StatusCode::GATEWAY_TIMEOUT => "DEADLINE_EXCEEDED",
        _ => "UNAVAILABLE",
    };
    (
        last_error_status,
        axum::Json(json!({
            "error": {
                "code": last_error_status.as_u16(),
                "message": last_error_body,
                "status": rpc_status,
            }
        })),
    )
        .into_response()
}

#[allow(clippy::too_many_arguments)]
async fn select_account_for_request(
    state: &AppState,
    quota_group: &str,
    slot_key: &str,
    dual_track: bool,
    force_rotate: bool,
    session_id: Option<&str>,
    target_model: &str,
    pool_len: usize,
    tried_accounts: &HashSet<String>,
) -> Result<(String, String, String, String, u64), String> {
    // 1. If Dual-Track is active and this slot already has a pinned healthy account, prefer it first
    //    (preserves 99% prompt-cache across all 5 slots: claude_full, claude_basic, gemini_full, gemini_basic, gemini_lite).
    if dual_track && !force_rotate {
        if let Some((slot_acc_id, _slot_email)) = ASAR_TELEMETRY.get_slot_by_key(slot_key) {
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
                    ASAR_TELEMETRY.clear_slot_by_key_if_matches(slot_key, &slot_acc_id);
                }
            }
        }
    }

    // 2. Layer 2 — Capability-Tiered Burn Order: common (non-flagship) models burn Basic accounts
    //    first and keep Full accounts (Carbon / Neon / Argon / Claude 5.5) in reserve.
    if !is_flagship_canary_model(target_model) {
        let enabled_ids = state.token_manager.enabled_account_ids();
        let canary_ids: HashSet<String> = enabled_ids
            .iter()
            .filter(|id| {
                state.token_manager.get_token_by_id(id).is_some_and(|tok| {
                    tok.available_models
                        .iter()
                        .any(|m| is_flagship_canary_model(m))
                })
            })
            .cloned()
            .collect();
        let has_basic_candidate = enabled_ids
            .iter()
            .any(|id| !canary_ids.contains(id) && !tried_accounts.contains(id));
        if !canary_ids.is_empty() && has_basic_candidate {
            let mut exclusions = tried_accounts.clone();
            exclusions.extend(canary_ids);
            if let Ok(tuple) = get_token_with_25_fallback(
                state,
                quota_group,
                force_rotate,
                session_id,
                target_model,
                &exclusions,
            )
            .await
            {
                return Ok(tuple);
            }
        }
    }

    // 3. On Gemini worker track, avoid burning the pinned Claude Lead account when >1 accounts exist
    let claude_slot_id = ASAR_TELEMETRY
        .get_slot_by_key("claude_full")
        .or_else(|| ASAR_TELEMETRY.get_slot_by_key("claude_basic"))
        .map(|(id, _)| id);
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

    // 4. Standard pool selection (including fallback to Claude Lead if no other Gemini account is available)
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

/// Rewrite `"sessionId"` / `"session_id"` at top level and inside a `"request"` envelope
/// so each pool account receives a deterministic per-account session token.
/// This ensures repeated turns on the **same** account hash to the same value (100% cache hit)
/// while concurrent accounts on the same raw session never share a token on Google's servers.
pub(crate) fn isolate_session_in_body(body: &mut Value, account_id: &str) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    for key in ["sessionId", "session_id"] {
        if let Some(Value::String(s)) = obj.get(key).cloned() {
            obj.insert(
                key.to_string(),
                Value::String(isolate_session_for_account(account_id, &s)),
            );
        }
    }
    if let Some(Value::Object(req)) = obj.get_mut("request") {
        for key in ["sessionId", "session_id"] {
            if let Some(Value::String(s)) = req.get(key).cloned() {
                req.insert(
                    key.to_string(),
                    Value::String(isolate_session_for_account(account_id, &s)),
                );
            }
        }
    }
}

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
    /// Headers that could leak IDE-native project IDs or cross-account browser correlation.
    /// These are stripped before forwarding to any pool account.
    const STRIP_HEADERS: &[&str] = &[
        "host",
        "content-length",
        "content-type",
        "authorization",
        "connection",
        // Cross-account correlation headers — must never be forwarded as-is
        "x-client-data",
        "x-goog-request-params",
        "x-vscode-machineid",
        "x-machine-id",
    ];
    let mut map = HashMap::new();
    for (k, v) in headers.iter() {
        let name = k.as_str().to_ascii_lowercase();
        if STRIP_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if let Ok(val) = v.to_str() {
            map.insert(k.as_str().to_string(), val.to_string());
        }
    }
    map
}

/// Apply per-account session isolation to `extra_headers` **in-place** before forwarding.
/// Rewrites `x-session-id` / `x-vscode-sessionid` values so each account receives its own
/// deterministic session token (100% prompt-cache on the same account, no cross-account leak).
fn isolate_session_headers(headers: &mut HashMap<String, String>, account_id: &str) {
    for key in ["x-session-id", "x-vscode-sessionid"] {
        if let Some(orig) = headers.get(key).cloned() {
            headers.insert(
                key.to_string(),
                isolate_session_for_account(account_id, &orig),
            );
        }
    }
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
    fn test_layer1_richest_catalog_first_sort_key() {
        // PRO Full (26 models, canary) must beat PRO Basic (15 models) for fetchAvailableModels.
        let full = fetch_models_sort_key(1, true, 26, 50);
        let basic = fetch_models_sort_key(1, false, 15, 1);
        assert!(full < basic);
        // Tier still dominates: ULTRA Basic beats PRO Full.
        let ultra_basic = fetch_models_sort_key(0, false, 15, 50);
        assert!(ultra_basic < full);
        // Among canary accounts, the richer catalog wins before manual priority.
        let richer = fetch_models_sort_key(1, true, 30, 90);
        let poorer = fetch_models_sort_key(1, true, 26, 1);
        assert!(richer < poorer);
    }

    #[test]
    fn test_layer2_flagship_canary_and_flash_lite_detection() {
        for m in [
            "gemini-4-carbon-medium",
            "gemini-4-neon-xhigh",
            "gemini-4-argon-high",
            "claude-opus-5-5-high",
            "claude-sonnet-5-5-low",
        ] {
            assert!(is_flagship_canary_model(m), "{m} must be flagship canary");
        }
        for m in [
            "gemini-3.5-flash-lite",
            "gemini-3.8-flash-high",
            "gemini-4-hydrogen-medium",
            "claude-sonnet-4-6",
            "claude-opus-4-6-thinking",
        ] {
            assert!(!is_flagship_canary_model(m), "{m} must be common model");
        }
        assert!(is_flash_lite_model("gemini-3.5-flash-lite"));
        assert!(is_flash_lite_model("Gemini-3.1-Flash-Lite"));
        assert!(!is_flash_lite_model("gemini-3.8-flash-high"));
    }

    #[test]
    fn test_layer2_flash_lite_never_overwrites_last_active_record() {
        let hub = AsarTelemetryHub::new();
        // First ever call may be recorded, even if flash-lite.
        hub.record_active_guarded("acc-a", "a@x.com", "gemini-3.5-flash-lite");
        assert_eq!(
            hub.last_active.read().unwrap().as_ref().unwrap().model,
            "gemini-3.5-flash-lite"
        );
        // Real model overwrites.
        hub.record_active_guarded("acc-b", "b@x.com", "gemini-4-carbon-medium");
        // Subsequent flash-lite must not overwrite the real record.
        hub.record_active_guarded("acc-c", "c@x.com", "gemini-3.5-flash-lite");
        let rec = hub.last_active.read().unwrap().clone().unwrap();
        assert_eq!(rec.model, "gemini-4-carbon-medium");
        assert_eq!(rec.account_id, "acc-b");
    }

    #[test]
    fn test_layer3_equivalent_fallback_model_mapping() {
        assert_eq!(
            resolve_equivalent_fallback_model("gemini-4-carbon-medium").as_deref(),
            Some("gemini-4-hydrogen-medium")
        );
        assert_eq!(
            resolve_equivalent_fallback_model("gemini-4-neon-xhigh").as_deref(),
            Some("gemini-4-hydrogen-xhigh")
        );
        assert_eq!(
            resolve_equivalent_fallback_model("gemini-4-argon-high").as_deref(),
            Some("gemini-4-hydrogen-high")
        );
        assert_eq!(
            resolve_equivalent_fallback_model("gemini-4-carbon-low").as_deref(),
            Some("gemini-4-hydrogen-low")
        );
        assert_eq!(
            resolve_equivalent_fallback_model("gemini-4-carbon").as_deref(),
            Some("gemini-4-hydrogen-medium")
        );
        assert_eq!(
            resolve_equivalent_fallback_model("claude-opus-5-5-high").as_deref(),
            Some("claude-opus-4-6-thinking")
        );
        assert_eq!(
            resolve_equivalent_fallback_model("claude-sonnet-5-5-medium").as_deref(),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            resolve_equivalent_fallback_model("gemini-pro-orchestrator-argon").as_deref(),
            Some("gemini-4-hydrogen-high")
        );
        // Already-basic models have no fallback.
        assert!(resolve_equivalent_fallback_model("gemini-4-hydrogen-medium").is_none());
        assert!(resolve_equivalent_fallback_model("claude-sonnet-4-6").is_none());
        assert!(resolve_equivalent_fallback_model("gemini-3.8-flash-high").is_none());
    }

    #[test]
    fn test_layer3_fallback_telemetry_counter_and_default_enabled() {
        let hub = AsarTelemetryHub::new();
        assert!(hub.is_equivalent_fallback_enabled());
        hub.record_fallback_route("gemini-4-carbon-medium -> gemini-4-hydrogen-medium".into());
        hub.record_fallback_route("claude-opus-5-5-high -> claude-opus-4-6-thinking".into());
        assert_eq!(hub.fallback_downgrade_count.load(Ordering::Relaxed), 2);
        assert_eq!(
            hub.last_fallback_route.read().unwrap().as_deref(),
            Some("claude-opus-5-5-high -> claude-opus-4-6-thinking")
        );
        hub.set_equivalent_fallback(false);
        assert!(!hub.is_equivalent_fallback_enabled());
    }

    #[test]
    fn test_multi_stage_cross_family_fallback_cascade() {
        let req_body = json!({
            "model": "claude-sonnet-5-5-high",
            "request": {
                "model": "claude-sonnet-5-5-high",
                "generationConfig": {
                    "maxOutputTokens": 128000,
                    "thinkingConfig": {
                        "includeThoughts": true,
                        "thinkingLevel": "HIGH"
                    }
                }
            }
        });

        let mut visited = HashSet::new();
        let mut current = "claude-sonnet-5-5-high".to_string();
        visited.insert(current.to_ascii_lowercase());

        // Hop 1: claude-sonnet-5-5-high -> claude-sonnet-4-6
        let hop1 = resolve_next_fallback_model(
            &current,
            "claude-sonnet-5-5-high",
            &req_body,
            &visited,
            true,
            true,
        )
        .expect("hop 1 expected");
        assert_eq!(hop1, "claude-sonnet-4-6");
        visited.insert(hop1.to_ascii_lowercase());
        current = hop1;

        // Hop 2: claude-sonnet-4-6 -> gemini-4-carbon-high
        let hop2 = resolve_next_fallback_model(
            &current,
            "claude-sonnet-5-5-high",
            &req_body,
            &visited,
            true,
            true,
        )
        .expect("hop 2 expected");
        assert_eq!(hop2, "gemini-4-carbon-high");
        visited.insert(hop2.to_ascii_lowercase());
        current = hop2;

        // Hop 3: gemini-4-carbon-high -> gemini-4-hydrogen-high
        let hop3 = resolve_next_fallback_model(
            &current,
            "claude-sonnet-5-5-high",
            &req_body,
            &visited,
            true,
            true,
        )
        .expect("hop 3 expected");
        assert_eq!(hop3, "gemini-4-hydrogen-high");
        visited.insert(hop3.to_ascii_lowercase());
        current = hop3;

        // No 4th hop back to claude-sonnet-5-5 or claude-sonnet-4-6 (already visited)
        assert!(resolve_next_fallback_model(
            &current,
            "claude-sonnet-5-5-high",
            &req_body,
            &visited,
            true,
            true,
        )
        .is_none());

        // Background flash-lite never crosses into Claude
        let empty_visited = HashSet::new();
        assert!(resolve_next_fallback_model(
            "gemini-3.5-flash-lite",
            "gemini-3.5-flash-lite",
            &json!({}),
            &empty_visited,
            true,
            true,
        )
        .is_none());
    }

    #[test]
    fn test_sanitize_body_for_fallback_target_clamps_tokens_and_thinking() {
        let mut gemini_body = json!({
            "model": "claude-sonnet-5-5-high",
            "generationConfig": {
                "maxOutputTokens": 128000,
                "thinkingConfig": {
                    "includeThoughts": true,
                    "thinkingLevel": "XHIGH",
                    "thinkingBudget": 16000
                }
            },
            "request": {
                "model": "claude-sonnet-5-5-high",
                "generationConfig": {
                    "maxOutputTokens": 128000,
                    "thinkingConfig": {
                        "includeThoughts": true,
                        "thinkingLevel": "HIGH",
                        "thinkingBudget": 16000
                    }
                }
            }
        });

        sanitize_body_for_fallback_target(&mut gemini_body, "gemini-4-carbon-high", None);
        assert_eq!(gemini_body["model"], "gemini-4-carbon-high");
        assert_eq!(gemini_body["request"]["model"], "gemini-4-carbon-high");
        assert_eq!(gemini_body["generationConfig"]["maxOutputTokens"], 65536);
        assert_eq!(
            gemini_body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );
        assert!(gemini_body["generationConfig"]["thinkingConfig"]
            .get("thinkingBudget")
            .is_none());
        assert_eq!(
            gemini_body["request"]["generationConfig"]["maxOutputTokens"],
            65536
        );

        let mut claude_46_body = json!({
            "model": "claude-sonnet-5-5-high",
            "request": {
                "model": "claude-sonnet-5-5-high",
                "generationConfig": {
                    "maxOutputTokens": 128000,
                    "thinkingConfig": {
                        "includeThoughts": true,
                        "thinkingLevel": "HIGH"
                    }
                }
            }
        });
        sanitize_body_for_fallback_target(&mut claude_46_body, "claude-sonnet-4-6", None);
        assert_eq!(claude_46_body["request"]["model"], "claude-sonnet-4-6");
        assert_eq!(
            claude_46_body["request"]["generationConfig"]["maxOutputTokens"],
            64000
        );
        assert!(claude_46_body["request"]["generationConfig"]
            .get("thinkingConfig")
            .is_none());
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

    // ── New tests for Tri-Slot Sticky, Session Isolation & Header Scrubbing ────────────

    #[test]
    fn test_isolate_session_for_account_deterministic_and_distinct() {
        // Same account + session → identical output on repeated calls (cache-hit guarantee)
        let s1 = isolate_session_for_account("acc-123", "raw-uuid-abc");
        let s2 = isolate_session_for_account("acc-123", "raw-uuid-abc");
        assert_eq!(s1, s2, "must be deterministic for same account+session");

        // Output has the sess_ prefix
        assert!(s1.starts_with("sess_"), "must start with sess_");
        // 24 hex chars after prefix
        assert_eq!(s1.len(), "sess_".len() + 24);

        // Different account_id → different output (no cross-account correlation)
        let s3 = isolate_session_for_account("acc-456", "raw-uuid-abc");
        assert_ne!(
            s1, s3,
            "different accounts must produce distinct session tokens"
        );

        // Different raw_session → different output
        let s4 = isolate_session_for_account("acc-123", "raw-uuid-xyz");
        assert_ne!(s1, s4, "different raw_session must produce distinct token");
    }

    #[test]
    fn test_isolate_session_in_body_top_level_and_request_envelope() {
        let account_id = "acc-test-789";
        let raw = "orig-session-uuid";

        let mut body = json!({
            "sessionId": raw,
            "model": "gemini-4-carbon-medium",
            "request": {
                "sessionId": raw,
                "contents": []
            }
        });
        isolate_session_in_body(&mut body, account_id);

        let top_sess = body["sessionId"].as_str().unwrap();
        let req_sess = body["request"]["sessionId"].as_str().unwrap();
        let expected = isolate_session_for_account(account_id, raw);

        assert_eq!(top_sess, expected, "top-level sessionId must be rewritten");
        assert_eq!(req_sess, expected, "request.sessionId must be rewritten");

        // Model untouched
        assert_eq!(body["model"].as_str().unwrap(), "gemini-4-carbon-medium");

        // Idempotent: rewriting the already-hashed value with same account gives a NEW hash,
        // but the second rewrite uses the hashed value as raw_session, which is fine since
        // in production the body is cloned fresh for every attempt from body_json.
    }

    #[test]
    fn test_isolate_session_in_body_snake_case_field() {
        let account_id = "acc-snake";
        let raw = "snake-session-id-orig";
        let mut body = json!({ "session_id": raw, "request": { "session_id": raw } });
        isolate_session_in_body(&mut body, account_id);
        let expected = isolate_session_for_account(account_id, raw);
        assert_eq!(body["session_id"].as_str().unwrap(), expected);
        assert_eq!(body["request"]["session_id"].as_str().unwrap(), expected);
    }

    #[test]
    fn test_collect_extra_headers_scrubs_correlation_headers() {
        use axum::http::HeaderMap;
        let mut hdrs = HeaderMap::new();
        hdrs.insert("x-client-data", "some-chrome-data".parse().unwrap());
        hdrs.insert(
            "x-goog-request-params",
            "project=native-proj".parse().unwrap(),
        );
        hdrs.insert("x-vscode-machineid", "machine-abc123".parse().unwrap());
        hdrs.insert("x-machine-id", "another-id".parse().unwrap());
        hdrs.insert("x-session-id", "sess-raw-1234".parse().unwrap());
        hdrs.insert("authorization", "Bearer tok".parse().unwrap());
        hdrs.insert("x-custom-header", "keep-me".parse().unwrap());

        let out = collect_extra_headers(&hdrs);

        // Stripped headers
        assert!(
            !out.contains_key("x-client-data"),
            "x-client-data must be stripped"
        );
        assert!(
            !out.contains_key("x-goog-request-params"),
            "x-goog-request-params must be stripped"
        );
        assert!(
            !out.contains_key("x-vscode-machineid"),
            "x-vscode-machineid must be stripped"
        );
        assert!(
            !out.contains_key("x-machine-id"),
            "x-machine-id must be stripped"
        );
        assert!(
            !out.contains_key("authorization"),
            "authorization must be stripped"
        );

        // Session header passes through (isolate_session_headers rewrites it later per-account)
        assert!(
            out.contains_key("x-session-id"),
            "x-session-id must pass through collect_extra_headers"
        );

        // Custom headers retained
        assert_eq!(
            out.get("x-custom-header").map(|s| s.as_str()),
            Some("keep-me")
        );
    }

    #[test]
    fn test_isolate_session_headers_rewrites_session_header_per_account() {
        let raw_session = "raw-session-9999";
        let account_id = "acc-rewrite-test";

        let mut hdrs = std::collections::HashMap::new();
        hdrs.insert("x-session-id".to_string(), raw_session.to_string());
        hdrs.insert("x-vscode-sessionid".to_string(), raw_session.to_string());
        hdrs.insert("x-custom".to_string(), "untouched".to_string());

        isolate_session_headers(&mut hdrs, account_id);

        let expected = isolate_session_for_account(account_id, raw_session);
        assert_eq!(
            hdrs.get("x-session-id").map(|s| s.as_str()),
            Some(expected.as_str()),
            "x-session-id must be rewritten to per-account hash"
        );
        assert_eq!(
            hdrs.get("x-vscode-sessionid").map(|s| s.as_str()),
            Some(expected.as_str()),
            "x-vscode-sessionid must be rewritten to per-account hash"
        );
        // Non-session headers untouched
        assert_eq!(hdrs.get("x-custom").map(|s| s.as_str()), Some("untouched"));
    }

    #[test]
    fn test_gemini_basic_slot_lifecycle_in_hub() {
        let hub = AsarTelemetryHub::new();

        // Initially no basic slot
        assert!(hub.get_gemini_basic_slot().is_none());

        // Set basic slot
        hub.set_gemini_basic_slot("acc-basic-1".to_string(), "basic1@example.com".to_string());
        assert_eq!(
            hub.get_gemini_basic_slot().map(|(id, _)| id).as_deref(),
            Some("acc-basic-1")
        );

        // clear_gemini_basic_slot_if_matches with wrong id → no change
        hub.clear_gemini_basic_slot_if_matches("acc-basic-99");
        assert!(hub.get_gemini_basic_slot().is_some());

        // clear_gemini_basic_slot_if_matches with correct id → cleared
        hub.clear_gemini_basic_slot_if_matches("acc-basic-1");
        assert!(hub.get_gemini_basic_slot().is_none());

        // reset_slots() clears all three slots at once
        hub.set_gemini_basic_slot("acc-basic-2".to_string(), "b2@x.com".to_string());
        if let Ok(mut g) = hub.claude_slot.write() {
            *g = Some(("acc-claude".to_string(), "c@x.com".to_string()));
        }
        if let Ok(mut g) = hub.gemini_slot.write() {
            *g = Some(("acc-full".to_string(), "f@x.com".to_string()));
        }
        hub.reset_slots();
        assert!(hub.get_slot(true).is_none(), "claude_slot must be cleared");
        assert!(hub.get_slot(false).is_none(), "gemini_slot must be cleared");
        assert!(
            hub.get_gemini_basic_slot().is_none(),
            "gemini_basic_slot must be cleared"
        );
    }

    #[test]
    fn test_snapshot_gemini_slot_fallback_to_basic() {
        let hub = AsarTelemetryHub::new();

        // gemini_slot empty, gemini_basic_slot set → snapshot should fall back to basic
        hub.set_gemini_basic_slot("acc-b".to_string(), "b@x.com".to_string());
        // Simulate what snapshot_asar_telemetry does for the gemini_slot field:
        let gemini_slot_for_snapshot = hub.get_slot(false).or_else(|| hub.get_gemini_basic_slot());
        assert_eq!(
            gemini_slot_for_snapshot.as_ref().map(|(id, _)| id.as_str()),
            Some("acc-b"),
            "snapshot gemini_slot must fall back to gemini_basic_slot when gemini_slot is empty"
        );

        // gemini_slot set → gemini_slot wins
        hub.set_slot(false, "acc-full".to_string(), "full@x.com".to_string());
        let gemini_slot_for_snapshot2 = hub.get_slot(false).or_else(|| hub.get_gemini_basic_slot());
        assert_eq!(
            gemini_slot_for_snapshot2
                .as_ref()
                .map(|(id, _)| id.as_str()),
            Some("acc-full"),
            "snapshot gemini_slot must prefer gemini_slot over gemini_basic_slot"
        );
    }

    #[test]
    fn test_gemini_pro_orchestrator_fallback() {
        // Explicitly verify #139 mapping is present
        assert_eq!(
            resolve_equivalent_fallback_model("gemini-pro-orchestrator").as_deref(),
            Some("gemini-4-hydrogen-high")
        );
        assert_eq!(
            resolve_equivalent_fallback_model("gemini-pro-orchestrator-v2").as_deref(),
            Some("gemini-4-hydrogen-high")
        );
        assert_eq!(
            resolve_equivalent_fallback_model("models/gemini-pro-orchestrator-argon").as_deref(),
            Some("gemini-4-hydrogen-high")
        );
    }

    #[test]
    fn test_five_slot_routing_and_active_slots_snapshot() {
        assert_eq!(
            resolve_slot_key(true, "claude-opus-5-5-high", true),
            "claude_full"
        );
        assert_eq!(
            resolve_slot_key(true, "claude-sonnet-4-6", true),
            "claude_basic"
        );
        assert_eq!(
            resolve_slot_key(false, "gemini-4-carbon-medium", true),
            "gemini_full"
        );
        assert_eq!(
            resolve_slot_key(false, "gemini-4-hydrogen-high", true),
            "gemini_basic"
        );
        assert_eq!(
            resolve_slot_key(false, "gemini-3.5-flash-lite", true),
            "gemini_lite"
        );
        assert_eq!(
            resolve_slot_key(false, "gemini-3.5-flash-lite", false),
            "gemini_basic"
        );

        assert_eq!(short_slot_model_label("gemini-4-carbon-medium"), "Carbon");
        assert_eq!(short_slot_model_label("gemini-4-neon-high"), "Neon");
        assert_eq!(short_slot_model_label("gemini-4-hydrogen-high"), "Hydrogen");
        assert_eq!(
            short_slot_model_label("gemini-3.8-flash-medium"),
            "3.8 Flash"
        );
        assert_eq!(
            short_slot_model_label("gemini-3.5-flash-lite"),
            "3.5 Flash Lite"
        );
        assert_eq!(short_slot_model_label("claude-opus-5-5-high"), "Opus 5.5");
        assert_eq!(short_slot_model_label("claude-sonnet-4-6"), "Sonnet 4.6");

        let hub = AsarTelemetryHub::new();
        hub.set_slot_by_key(
            "gemini_full",
            "acc-full".to_string(),
            "full@x.com".to_string(),
            "gemini-4-carbon-medium",
        );
        hub.set_slot_by_key(
            "gemini_full",
            "acc-full".to_string(),
            "full@x.com".to_string(),
            "gemini-4-neon-high",
        );
        hub.set_slot_by_key(
            "gemini_basic",
            "acc-basic".to_string(),
            "basic@x.com".to_string(),
            "gemini-4-hydrogen-high",
        );
        hub.set_slot_by_key(
            "claude_full",
            "acc-claude".to_string(),
            "claude@x.com".to_string(),
            "claude-sonnet-5-5-medium",
        );

        let slots = hub.active_slots_snapshot();
        assert_eq!(slots.len(), 3);
        assert_eq!(slots[0].slot_key, "claude_full");
        assert_eq!(slots[0].models, vec!["Sonnet 5.5"]);
        assert_eq!(slots[1].slot_key, "gemini_full");
        assert_eq!(slots[1].models, vec!["Carbon", "Neon"]);
        assert_eq!(slots[2].slot_key, "gemini_basic");
        assert_eq!(slots[2].models, vec!["Hydrogen"]);

        // Rotating gemini_full to a new account resets its model list
        hub.set_slot_by_key(
            "gemini_full",
            "acc-full-2".to_string(),
            "full2@x.com".to_string(),
            "gemini-4-argon-high",
        );
        let slots2 = hub.active_slots_snapshot();
        assert_eq!(slots2[1].account_id, "acc-full-2");
        assert_eq!(slots2[1].models, vec!["Argon"]);
    }

    #[test]
    fn test_in_flight_guard_registers_and_drops_stream() {
        let before = ASAR_TELEMETRY.active_streams_snapshot().len();
        {
            let _guard = AsarInFlightGuard::acquire(
                "gemini_full",
                "acc-stream-test".to_string(),
                "stream@x.com".to_string(),
                "gemini-4-carbon-medium".to_string(),
                "gemini-4-carbon-medium".to_string(),
            );
            let during = ASAR_TELEMETRY.active_streams_snapshot();
            assert_eq!(during.len(), before + 1);
            let rec = during
                .iter()
                .find(|s| s.account_id == "acc-stream-test")
                .expect("stream record present while guard is alive");
            assert_eq!(rec.slot_key, "gemini_full");
            assert_eq!(rec.email, "stream@x.com");
            assert_eq!(rec.requested_model, "gemini-4-carbon-medium");
            assert_eq!(rec.target_model, "gemini-4-carbon-medium");
        }
        let after = ASAR_TELEMETRY.active_streams_snapshot();
        assert!(!after.iter().any(|s| s.account_id == "acc-stream-test"));
    }
}
