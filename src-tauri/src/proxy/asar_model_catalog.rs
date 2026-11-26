//! Dynamic Model Catalog & `v1internal:fetchAvailableModels` Injection Engine
//!
//! Manages an independent `models_catalog.json` in the application data directory
//! with automatic mtime-based hot-reload (no IDE restart required) and transforms
//! Google Cloud Code PA `fetchAvailableModels` responses without `models/` prefix dupes,
//! preserving Google's multi-group `agentModelSorts` and native model metadata.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

const CATALOG_FILE_NAME: &str = "models_catalog.json";
const BASELINE_MODELS_JSON: &str = include_str!("../../resources/official_models.json");
const DESC_AUTO_LIVE: &str = "Auto-discovered from live Google fetchAvailableModels";
const DESC_SYNCED_POOL: &str = "Synced from account pool";

fn default_true() -> bool {
    true
}

fn default_owned_by() -> String {
    "google".to_string()
}

fn default_provider_id() -> String {
    "default_google_pool".to_string()
}

fn default_provider_name() -> String {
    "Google Account Pool".to_string()
}

fn default_context_window() -> u64 {
    1_048_576
}

fn default_max_output() -> u64 {
    65_536
}

fn default_input_pricing() -> f64 {
    0.075
}

fn default_output_pricing() -> f64 {
    0.30
}

fn default_order() -> i32 {
    100
}

/// Configuration entry for a model in `models_catalog.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModelEntry {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_owned_by")]
    pub owned_by: String,
    #[serde(default = "default_provider_id")]
    pub provider_id: String,
    #[serde(default = "default_provider_name")]
    pub provider_name: String,
    #[serde(default)]
    pub target_model: String,
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    #[serde(default = "default_context_window")]
    pub max_input_tokens: u64,
    #[serde(default = "default_max_output")]
    pub max_output_tokens: u64,
    #[serde(default)]
    pub supports_thinking: bool,
    #[serde(default = "default_true")]
    pub supports_vision: bool,
    #[serde(default = "default_true")]
    pub supports_audio: bool,
    #[serde(default = "default_true")]
    pub supports_video: bool,
    #[serde(default = "default_true")]
    pub supports_function_calling: bool,
    #[serde(default = "default_input_pricing")]
    pub input_pricing: f64,
    #[serde(default = "default_output_pricing")]
    pub output_pricing: f64,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_order")]
    pub order: i32,
}

#[derive(Debug)]
struct CatalogState {
    path: PathBuf,
    entries: Vec<CatalogModelEntry>,
    last_mtime: Option<SystemTime>,
}

/// Thread-safe catalog store with file persistence and automatic mtime hot-reload.
pub struct AsarModelCatalog {
    state: RwLock<CatalogState>,
    /// Native Google `models` node snapshots keyed by lowercase model ID (from `official_models.json`,
    /// account `ModelQuota` metadata, and live `fetchAvailableModels` responses).
    live_nodes: RwLock<HashMap<String, Map<String, Value>>>,
}

static GLOBAL_CATALOG: Lazy<AsarModelCatalog> = Lazy::new(|| {
    let path = resolve_default_catalog_path();
    let cat = AsarModelCatalog::open(path);
    let _ = cat.sync_from_accounts();
    cat
});

fn resolve_default_catalog_path() -> PathBuf {
    match crate::modules::account::get_data_dir() {
        Ok(dir) => dir.join(CATALOG_FILE_NAME),
        Err(_) => dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("antigravity-tools")
            .join(CATALOG_FILE_NAME),
    }
}

fn account_tier_rank(tier: Option<&str>) -> u8 {
    match tier.unwrap_or("").trim().to_ascii_uppercase().as_str() {
        "ULTRA" => 0,
        "PRO" => 1,
        _ => 2,
    }
}

impl AsarModelCatalog {
    /// Open or initialize a catalog backed by `path`.
    pub fn open(path: PathBuf) -> Self {
        let mut entries = Vec::new();
        let mut last_mtime = None;
        let existed_on_disk = path.exists();

        if existed_on_disk {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(parsed) = serde_json::from_str::<Vec<CatalogModelEntry>>(&content) {
                    entries = parsed;
                    last_mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
                }
            }
        }

        let modified = normalize_and_ensure_defaults(&mut entries);
        if !existed_on_disk || modified {
            if let Ok(mtime) = write_entries_to_disk(&path, &entries) {
                last_mtime = mtime;
            }
        }

        let mut initial_live_nodes = HashMap::new();
        for (raw_id, val) in official_models_map() {
            if let Value::Object(obj) = val {
                let clean = strip_models_prefix(raw_id.trim()).to_ascii_lowercase();
                if !clean.is_empty() {
                    initial_live_nodes.entry(clean).or_insert(obj);
                }
            }
        }

        Self {
            state: RwLock::new(CatalogState {
                path,
                entries,
                last_mtime,
            }),
            live_nodes: RwLock::new(initial_live_nodes),
        }
    }

    pub fn catalog_path(&self) -> PathBuf {
        self.state.read().unwrap().path.clone()
    }

    /// Hot-reload from disk if the file's modification time changed.
    pub fn reload_if_changed(&self) -> Result<bool, String> {
        let path = {
            let guard = self.state.read().unwrap();
            guard.path.clone()
        };

        if !path.exists() {
            return Ok(false);
        }

        let disk_mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
        {
            let guard = self.state.read().unwrap();
            if disk_mtime.is_some() && disk_mtime == guard.last_mtime {
                return Ok(false);
            }
        }

        self.force_reload()
    }

    /// Unconditionally reload `models_catalog.json` from disk.
    pub fn force_reload(&self) -> Result<bool, String> {
        let mut guard = self.state.write().unwrap();
        if !guard.path.exists() {
            return Ok(false);
        }

        let content = fs::read_to_string(&guard.path)
            .map_err(|e| format!("Failed to read {}: {}", guard.path.display(), e))?;
        let mut parsed: Vec<CatalogModelEntry> = serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse {}: {}", guard.path.display(), e))?;

        let modified = normalize_and_ensure_defaults(&mut parsed);
        let mut mtime = fs::metadata(&guard.path).and_then(|m| m.modified()).ok();
        if modified {
            if let Ok(new_mtime) = write_entries_to_disk(&guard.path, &parsed) {
                mtime = new_mtime;
            }
        }

        guard.entries = parsed;
        guard.last_mtime = mtime;
        Ok(true)
    }

    pub fn get_all_models(&self) -> Vec<CatalogModelEntry> {
        let _ = self.reload_if_changed();
        let guard = self.state.read().unwrap();
        let mut list = guard.entries.clone();
        sort_entries(&mut list);
        list
    }

    /// Returns enabled models belonging to the Google Pool (excludes external providers).
    pub fn get_active_google_models(&self) -> Vec<CatalogModelEntry> {
        let _ = self.reload_if_changed();
        let guard = self.state.read().unwrap();
        let mut list: Vec<CatalogModelEntry> = guard
            .entries
            .iter()
            .filter(|m| m.enabled && is_google_pool_provider(&m.provider_id))
            .cloned()
            .collect();
        sort_entries(&mut list);
        list
    }

    pub fn get_model(&self, id: &str) -> Option<CatalogModelEntry> {
        let _ = self.reload_if_changed();
        let clean = strip_models_prefix(id.trim());
        let guard = self.state.read().unwrap();
        guard
            .entries
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(clean))
            .cloned()
    }

    pub fn toggle_model(&self, id: &str, enabled: bool) -> Result<bool, String> {
        let _ = self.reload_if_changed();
        let clean = strip_models_prefix(id.trim());
        if clean.is_empty() {
            return Ok(false);
        }

        let mut guard = self.state.write().unwrap();
        if let Some(entry) = guard
            .entries
            .iter_mut()
            .find(|m| m.id.eq_ignore_ascii_case(clean))
        {
            entry.enabled = enabled;
            let mtime = write_entries_to_disk(&guard.path, &guard.entries)?;
            guard.last_mtime = mtime;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn upsert_model(&self, mut entry: CatalogModelEntry) -> Result<CatalogModelEntry, String> {
        let _ = self.reload_if_changed();
        let clean_id = strip_models_prefix(entry.id.trim()).to_string();
        if clean_id.is_empty() {
            return Err("Model id cannot be empty".to_string());
        }
        entry.id = clean_id.clone();
        if entry.display_name.trim().is_empty() {
            entry.display_name = clean_id.clone();
        }
        if entry.target_model.trim().is_empty() {
            entry.target_model = clean_id.clone();
        }
        if entry.provider_id.trim().is_empty() {
            entry.provider_id = default_provider_id();
        }
        if entry.provider_name.trim().is_empty() {
            entry.provider_name = default_provider_name();
        }

        let mut guard = self.state.write().unwrap();
        if let Some(existing) = guard
            .entries
            .iter_mut()
            .find(|m| m.id.eq_ignore_ascii_case(&clean_id))
        {
            *existing = entry.clone();
        } else {
            guard.entries.push(entry.clone());
        }
        sort_entries(&mut guard.entries);
        let mtime = write_entries_to_disk(&guard.path, &guard.entries)?;
        guard.last_mtime = mtime;
        Ok(entry)
    }

    pub fn delete_model(&self, id: &str) -> Result<bool, String> {
        let _ = self.reload_if_changed();
        let clean = strip_models_prefix(id.trim());
        if clean.is_empty() {
            return Ok(false);
        }

        let mut guard = self.state.write().unwrap();
        let before = guard.entries.len();
        guard.entries.retain(|m| !m.id.eq_ignore_ascii_case(clean));
        if guard.entries.len() == before {
            return Ok(false);
        }
        let mtime = write_entries_to_disk(&guard.path, &guard.entries)?;
        guard.last_mtime = mtime;
        Ok(true)
    }

    pub fn replace_all_models(
        &self,
        mut entries: Vec<CatalogModelEntry>,
    ) -> Result<Vec<CatalogModelEntry>, String> {
        normalize_and_ensure_defaults(&mut entries);
        let mut guard = self.state.write().unwrap();
        let mtime = write_entries_to_disk(&guard.path, &entries)?;
        guard.entries = entries.clone();
        guard.last_mtime = mtime;
        Ok(entries)
    }

    /// Resolve a requested model ID to its configured `targetModel` (pass-through if not mapped).
    pub fn resolve_target_model(&self, requested_model: &str) -> String {
        let clean = strip_models_prefix(requested_model.trim());
        let clean = clean
            .strip_suffix(":online")
            .or_else(|| clean.strip_suffix(":search"))
            .unwrap_or(clean)
            .trim();
        if let Some(entry) = self.get_model(clean) {
            if !entry.target_model.trim().is_empty() {
                return entry.target_model.trim().to_string();
            }
        }
        clean.to_string()
    }

    /// Merge live Google `fetchAvailableModels` response with active Google Pool catalog models,
    /// strip internal non-chat buckets & disabled models, preserve multi-group `agentModelSorts`,
    /// and preserve native Google fields + `tieredModelIds.flashLite`.
    pub fn inject_models_into_fetch_response(&self, original_json: &str) -> String {
        let _ = self.reload_if_changed();
        let snapshot = {
            let guard = self.state.read().unwrap();
            let mut list = guard.entries.clone();
            sort_entries(&mut list);
            list
        };
        let live_snapshot = self.live_nodes.read().unwrap().clone();
        let restored = restore_richest_upstream(original_json);
        inject_with_catalog(&restored, &snapshot, &live_snapshot)
    }

    /// Auto-discover user-selectable chat models from a live Google `fetchAvailableModels` JSON response:
    /// - Caches ALL `models` nodes in `live_nodes` (preserving `MODEL_PLACEHOLDER_M*`, `thinkingLevel`,
    ///   `modelExperiments`, `tagTitle`, `supportedMimeTypes` for subagents/checkpoints/pool union).
    /// - ONLY auto-adds to `models_catalog.json` models that appear in `agentModelSorts[*].groups[*].modelIds`
    ///   (or have `"recommended": true` AND pass `is_selectable_pool_chat_model`), never overwriting `enabled: false`.
    /// - Purges stale auto-synced catalog entries that fail selectability checks.
    pub fn ingest_live_fetch_response(&self, raw_json: &str) -> usize {
        let Ok(Value::Object(mut root)) = serde_json::from_str::<Value>(raw_json) else {
            return 0;
        };
        let sort_model_ids = collect_sort_model_ids(&root);
        remember_richest_upstream(&root);
        let Some(Value::Object(models_map)) = root.remove("models") else {
            return 0;
        };

        let _ = self.reload_if_changed();
        let mut eligible: Vec<(String, Map<String, Value>)> = Vec::new();

        {
            let mut nodes_guard = self.live_nodes.write().unwrap();
            for (raw_key, val) in models_map {
                let clean_id = strip_models_prefix(raw_key.trim()).to_string();
                if clean_id.is_empty() || is_dead_model(&clean_id) {
                    continue;
                }
                if let Value::Object(obj) = val {
                    let lower = clean_id.to_ascii_lowercase();
                    nodes_guard.insert(lower.clone(), obj.clone());

                    let in_sorts = sort_model_ids.contains(&lower);
                    let recommended_and_compliant = obj
                        .get("recommended")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        && is_selectable_pool_chat_model(&clean_id);

                    if (in_sorts && !is_retired_or_non_chat_catalog_id(&clean_id))
                        || recommended_and_compliant
                    {
                        eligible.push((clean_id, obj));
                    }
                }
            }
        }

        let mut guard = self.state.write().unwrap();
        let before_len = guard.entries.len();
        guard.entries.retain(|e| {
            let is_auto = e.description == DESC_AUTO_LIVE || e.description == DESC_SYNCED_POOL;
            if !is_auto {
                return !is_retired_or_non_chat_catalog_id(&e.id);
            }
            let lower = e.id.to_ascii_lowercase();
            sort_model_ids.contains(&lower) || is_selectable_pool_chat_model(&e.id)
        });
        let mut changed = guard.entries.len() != before_len;

        let mut existing_ids: HashSet<String> = guard
            .entries
            .iter()
            .map(|e| e.id.to_ascii_lowercase())
            .collect();
        let mut added = 0usize;

        for (clean_id, m_node) in eligible {
            let lower = clean_id.to_ascii_lowercase();
            if !existing_ids.insert(lower.clone()) {
                continue;
            }

            let provider_str = m_node
                .get("modelProvider")
                .or_else(|| m_node.get("apiProvider"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let is_claude =
                lower.contains("claude") || provider_str.to_ascii_uppercase().contains("ANTHROPIC");
            let is_openai =
                lower.contains("gpt") || provider_str.to_ascii_uppercase().contains("OPENAI");
            let owned_by = if is_claude {
                "anthropic"
            } else if is_openai {
                "openai"
            } else {
                "google"
            }
            .to_string();

            let display_name = m_node
                .get("displayName")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&clean_id)
                .to_string();

            let context_window = m_node
                .get("maxTokens")
                .and_then(Value::as_u64)
                .unwrap_or(if is_claude { 200_000 } else { 1_048_576 });

            let max_output_tokens = m_node
                .get("maxOutputTokens")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| {
                    crate::proxy::model_specs::get_max_output_tokens(&clean_id, None)
                });

            let supports_vision = m_node
                .get("supportsImages")
                .and_then(Value::as_bool)
                .unwrap_or(true);

            let supports_thinking = m_node
                .get("supportsThinking")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| {
                    m_node.contains_key("thinkingLevel")
                        || m_node.contains_key("thinkingBudget")
                        || lower.contains("think")
                        || lower.contains("-high")
                        || lower.contains("-medium")
                        || lower.contains("-low")
                        || lower.contains("xhigh")
                });

            let supports_video = m_node
                .get("supportsVideo")
                .and_then(Value::as_bool)
                .unwrap_or(!is_claude);

            guard.entries.push(CatalogModelEntry {
                id: clean_id.clone(),
                display_name,
                enabled: true,
                owned_by,
                provider_id: default_provider_id(),
                provider_name: default_provider_name(),
                target_model: clean_id,
                context_window,
                max_input_tokens: context_window,
                max_output_tokens,
                supports_thinking,
                supports_vision,
                supports_audio: !is_claude,
                supports_video,
                supports_function_calling: true,
                input_pricing: if is_claude { 3.0 } else { 0.15 },
                output_pricing: if is_claude { 15.0 } else { 0.60 },
                description: DESC_AUTO_LIVE.to_string(),
                order: 70,
            });
            added += 1;
            changed = true;
        }

        if changed {
            sort_entries(&mut guard.entries);
            if let Ok(mtime) = write_entries_to_disk(&guard.path, &guard.entries) {
                guard.last_mtime = mtime;
            }
        }

        added
    }

    /// Synthetic baseline fallback when Google upstream is unreachable.
    pub fn create_synthetic_available_models_response(&self) -> String {
        self.inject_models_into_fetch_response(BASELINE_MODELS_JSON)
    }

    /// Sync usable chat models strictly from healthy accounts in the pool (prioritizing ULTRA > PRO > FREE),
    /// using `official_models.json` ONLY as a metadata lookup table (never importing unowned archive entries).
    pub fn sync_from_accounts(&self) -> Result<usize, String> {
        let mut accounts = crate::modules::account::list_accounts().unwrap_or_default();
        accounts.sort_by_key(|a| {
            let tier = a
                .quota
                .as_ref()
                .and_then(|q| q.subscription_tier.as_deref());
            (account_tier_rank(tier), a.priority)
        });

        let mut account_models = Vec::new();
        {
            let mut nodes_guard = self.live_nodes.write().unwrap();
            for acc in accounts {
                if acc.disabled || acc.banned || acc.validation_blocked {
                    continue;
                }
                if let Some(quota) = acc.quota {
                    if quota.is_forbidden {
                        continue;
                    }
                    for m in quota.models {
                        let clean_lower = strip_models_prefix(m.name.trim()).to_ascii_lowercase();
                        if !clean_lower.is_empty() && !nodes_guard.contains_key(&clean_lower) {
                            if let Some(ref proto_enum) = m.model {
                                let mut cached_node = Map::new();
                                cached_node
                                    .insert("model".to_string(), Value::String(proto_enum.clone()));
                                if let Some(ref dn) = m.display_name {
                                    cached_node.insert(
                                        "displayName".to_string(),
                                        Value::String(dn.clone()),
                                    );
                                }
                                if let Some(si) = m.supports_images {
                                    cached_node
                                        .insert("supportsImages".to_string(), Value::Bool(si));
                                }
                                if let Some(st) = m.supports_thinking {
                                    cached_node
                                        .insert("supportsThinking".to_string(), Value::Bool(st));
                                }
                                if let Some(tb) = m.thinking_budget {
                                    cached_node.insert("thinkingBudget".to_string(), json!(tb));
                                }
                                if let Some(mt) = m.max_tokens {
                                    cached_node.insert("maxTokens".to_string(), json!(mt));
                                }
                                if let Some(mot) = m.max_output_tokens {
                                    cached_node.insert("maxOutputTokens".to_string(), json!(mot));
                                }
                                if let Some(ref mimes) = m.supported_mime_types {
                                    let mime_obj: Map<String, Value> = mimes
                                        .iter()
                                        .map(|(k, v)| (k.clone(), Value::Bool(*v)))
                                        .collect();
                                    cached_node.insert(
                                        "supportedMimeTypes".to_string(),
                                        Value::Object(mime_obj),
                                    );
                                }
                                nodes_guard.insert(clean_lower, cached_node);
                            }
                        }
                        account_models.push((
                            m.name,
                            m.display_name,
                            m.supports_images,
                            m.supports_thinking,
                        ));
                    }
                }
            }
        }
        self.sync_from_account_models(account_models)
    }

    /// Core sync implementation taking `(name, display_name, supports_images, supports_thinking)` tuples
    /// from owned pool accounts ONLY (uses `official_models.json` strictly for metadata enrichment).
    pub fn sync_from_account_models<I>(&self, account_models: I) -> Result<usize, String>
    where
        I: IntoIterator<Item = (String, Option<String>, Option<bool>, Option<bool>)>,
    {
        let _ = self.reload_if_changed();
        let official_map = official_models_map();

        type RawAccountCandidate = (String, Option<String>, Option<bool>, Option<bool>);
        let raw_candidates: Vec<RawAccountCandidate> = account_models.into_iter().collect();

        // Collect all family roots that have explicit tier variants (-low/-medium/-high/-xhigh/-extra-low)
        // so bare roots (e.g. `gemini-4-carbon`, `gemini-4-argon`, `claude-sonnet-5-5`) are excluded.
        let mut tiered_families: HashSet<String> = HashSet::new();
        for (raw_name, _, _, _) in &raw_candidates {
            let clean = strip_models_prefix(raw_name.trim()).to_ascii_lowercase();
            let base = crate::proxy::common::model_mapping::strip_tier_suffix(&clean);
            if base != clean {
                tiered_families.insert(base);
            }
        }

        let mut guard = self.state.write().unwrap();
        for existing in &guard.entries {
            let lower = existing.id.to_ascii_lowercase();
            let base = crate::proxy::common::model_mapping::strip_tier_suffix(&lower);
            if base != lower {
                tiered_families.insert(base);
            }
        }

        let before_len = guard.entries.len();
        guard
            .entries
            .retain(|e| !is_retired_or_non_chat_catalog_id(&e.id));
        let mut changed = guard.entries.len() != before_len;

        let mut existing_ids: HashSet<String> = guard
            .entries
            .iter()
            .map(|e| e.id.to_ascii_lowercase())
            .collect();
        let mut added = 0usize;

        for (raw_name, display_opt, imgs_opt, think_opt) in raw_candidates {
            let clean_id = strip_models_prefix(raw_name.trim()).to_string();
            if !is_selectable_pool_chat_model(&clean_id) {
                continue;
            }
            let lower = clean_id.to_ascii_lowercase();
            let base = crate::proxy::common::model_mapping::strip_tier_suffix(&lower);
            if base == lower && tiered_families.contains(&lower) {
                continue;
            }
            if !existing_ids.insert(lower.clone()) {
                continue;
            }

            let official = official_map.get(&clean_id).or_else(|| {
                official_map
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(&clean_id))
                    .map(|(_, v)| v)
            });

            let is_claude = lower.contains("claude");
            let is_openai = lower.contains("gpt");
            let owned_by = if is_claude {
                "anthropic"
            } else if is_openai {
                "openai"
            } else {
                "google"
            }
            .to_string();

            let display_name = display_opt
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    official
                        .and_then(|v| v.get("displayName"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| clean_id.clone());

            let context_window = official
                .and_then(|v| v.get("maxTokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(if is_claude { 200_000 } else { 1_048_576 });

            let max_output_tokens = official
                .and_then(|v| v.get("maxOutputTokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or_else(|| {
                    crate::proxy::model_specs::get_max_output_tokens(&clean_id, None)
                });

            let supports_vision = imgs_opt
                .or_else(|| {
                    official
                        .and_then(|v| v.get("supportsImages"))
                        .and_then(|v| v.as_bool())
                })
                .unwrap_or(true);

            let supports_thinking = think_opt
                .or_else(|| {
                    official
                        .and_then(|v| v.get("supportsThinking"))
                        .and_then(|v| v.as_bool())
                })
                .unwrap_or_else(|| {
                    lower.contains("think")
                        || lower.contains("-high")
                        || lower.contains("-medium")
                        || lower.contains("-low")
                        || lower.contains("xhigh")
                });

            let supports_video = official
                .and_then(|v| v.get("supportsVideo"))
                .and_then(|v| v.as_bool())
                .unwrap_or(!is_claude && !is_openai);

            guard.entries.push(CatalogModelEntry {
                id: clean_id.clone(),
                display_name,
                enabled: true,
                owned_by,
                provider_id: default_provider_id(),
                provider_name: default_provider_name(),
                target_model: clean_id,
                context_window,
                max_input_tokens: context_window,
                max_output_tokens,
                supports_thinking,
                supports_vision,
                supports_audio: !is_claude && !is_openai,
                supports_video,
                supports_function_calling: true,
                input_pricing: if is_claude { 3.0 } else { 0.15 },
                output_pricing: if is_claude { 15.0 } else { 0.60 },
                description: DESC_SYNCED_POOL.to_string(),
                order: 75,
            });
            added += 1;
            changed = true;
        }

        if changed {
            sort_entries(&mut guard.entries);
            let mtime = write_entries_to_disk(&guard.path, &guard.entries)?;
            guard.last_mtime = mtime;
        }

        Ok(added)
    }
}

fn collect_sort_model_ids(root_obj: &Map<String, Value>) -> HashSet<String> {
    let mut ids = HashSet::new();
    if let Some(Value::Array(sorts)) = root_obj.get("agentModelSorts") {
        for sort in sorts {
            if let Some(Value::Array(groups)) = sort.get("groups") {
                for g in groups {
                    if let Some(Value::Array(model_ids)) = g.get("modelIds") {
                        for m in model_ids {
                            if let Some(s) = m.as_str() {
                                let clean = strip_models_prefix(s.trim()).to_ascii_lowercase();
                                if !clean.is_empty() {
                                    ids.insert(clean);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    ids
}

pub fn sync_models_from_accounts() -> Result<usize, String> {
    GLOBAL_CATALOG.sync_from_accounts()
}

fn official_models_map() -> Map<String, Value> {
    match serde_json::from_str::<Value>(BASELINE_MODELS_JSON) {
        Ok(Value::Object(mut root)) => match root.remove("models") {
            Some(Value::Object(models)) => models,
            _ => Map::new(),
        },
        _ => Map::new(),
    }
}

// ============================================================================
// Global Public API (used by v1internal-bridge & Tauri commands)
// ============================================================================

pub fn get_catalog_path() -> PathBuf {
    GLOBAL_CATALOG.catalog_path()
}

pub fn reload_catalog() -> Result<bool, String> {
    GLOBAL_CATALOG.force_reload()
}

pub fn get_all_models() -> Vec<CatalogModelEntry> {
    GLOBAL_CATALOG.get_all_models()
}

pub fn get_active_google_models() -> Vec<CatalogModelEntry> {
    GLOBAL_CATALOG.get_active_google_models()
}

pub fn get_model(id: &str) -> Option<CatalogModelEntry> {
    GLOBAL_CATALOG.get_model(id)
}

pub fn toggle_model(id: &str, enabled: bool) -> Result<bool, String> {
    GLOBAL_CATALOG.toggle_model(id, enabled)
}

pub fn upsert_model(entry: CatalogModelEntry) -> Result<CatalogModelEntry, String> {
    GLOBAL_CATALOG.upsert_model(entry)
}

pub fn delete_model(id: &str) -> Result<bool, String> {
    GLOBAL_CATALOG.delete_model(id)
}

pub fn replace_all_models(
    entries: Vec<CatalogModelEntry>,
) -> Result<Vec<CatalogModelEntry>, String> {
    GLOBAL_CATALOG.replace_all_models(entries)
}

pub fn resolve_target_model(requested_model: &str) -> String {
    GLOBAL_CATALOG.resolve_target_model(requested_model)
}

pub fn inject_models_into_fetch_response(original_json: &str) -> String {
    GLOBAL_CATALOG.inject_models_into_fetch_response(original_json)
}

pub fn ingest_live_fetch_available_models(raw_json: &str) -> usize {
    GLOBAL_CATALOG.ingest_live_fetch_response(raw_json)
}

// ============================================================================
// Layer 1 — Richest-catalog cache (keeps Carbon / Neon / Argon / Claude 5.5 visible in the IDE
// even if a Basic account happens to answer `fetchAvailableModels`).
// ============================================================================

/// `true` for flagship canary models only exposed on "Full" accounts
/// (`carbon`, `neon`, `argon`, `claude-*-5-5`, `claude-*-5-*`).
pub(crate) fn is_flagship_canary_model(model: &str) -> bool {
    let lower = strip_models_prefix(model.trim()).to_ascii_lowercase();
    lower.contains("carbon")
        || lower.contains("neon")
        || lower.contains("argon")
        || lower.contains("-5-5")
        || (lower.contains("claude") && lower.contains("-5-"))
}

#[derive(Clone)]
struct RichestUpstream {
    score: usize,
    models: Map<String, Value>,
    sorts: Value,
    grouped_models: Map<String, Value>,
    experiment_ids: Option<Value>,
    shared_model_experiments: Option<Value>,
}

fn richness_score(models: &Map<String, Value>) -> usize {
    let flagship = models
        .keys()
        .filter(|k| is_flagship_canary_model(k))
        .count();
    flagship * 1000 + models.len()
}

fn richest_upstream_path() -> PathBuf {
    get_catalog_path()
        .parent()
        .map(|p| p.join("asar_richest_upstream.json"))
        .unwrap_or_else(|| PathBuf::from("asar_richest_upstream.json"))
}

fn is_valid_grouped_model_entry(val: &Value) -> bool {
    let Some(obj) = val.as_object() else {
        return false;
    };
    let Some(levels_val) = obj.get("supportedThinkingLevels") else {
        return true;
    };
    let Some(levels) = levels_val.as_array() else {
        return false;
    };
    levels.iter().all(|item| {
        item.as_object().is_some_and(|lvl| {
            lvl.get("thinkingSuffix")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
                && lvl
                    .get("thinkingDisplayName")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty())
        })
    })
}

fn sanitize_grouped_models_map(grouped: &mut Map<String, Value>) {
    grouped.retain(|_, v| is_valid_grouped_model_entry(v));
}

fn load_richest_from_disk() -> Option<RichestUpstream> {
    if cfg!(test) {
        return None;
    }
    let content = fs::read_to_string(richest_upstream_path()).ok()?;
    let Value::Object(root) = serde_json::from_str::<Value>(&content).ok()? else {
        return None;
    };
    let Some(Value::Object(models)) = root.get("models").cloned() else {
        return None;
    };
    // Do NOT reject cache files that omit `groupedModels`; strip any poisoned bare-string entries.
    let mut grouped_models = root
        .get("groupedModels")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    sanitize_grouped_models_map(&mut grouped_models);
    let sorts = root
        .get("agentModelSorts")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let experiment_ids = root.get("experimentIds").cloned();
    let shared_model_experiments = root.get("sharedModelExperiments").cloned();
    Some(RichestUpstream {
        score: richness_score(&models),
        models,
        sorts,
        grouped_models,
        experiment_ids,
        shared_model_experiments,
    })
}

static RICHEST_UPSTREAM: Lazy<RwLock<Option<RichestUpstream>>> =
    Lazy::new(|| RwLock::new(load_richest_from_disk()));

/// Remember the richest live `fetchAvailableModels` payload (models + `agentModelSorts` + `groupedModels` + experiments) seen so far.
fn remember_richest_upstream(root: &Map<String, Value>) {
    if cfg!(test) {
        return;
    }
    let Some(Value::Object(models)) = root.get("models") else {
        return;
    };
    let score = richness_score(models);
    let sorts = root
        .get("agentModelSorts")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let mut grouped_models = root
        .get("groupedModels")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    sanitize_grouped_models_map(&mut grouped_models);
    let experiment_ids = root.get("experimentIds").cloned();
    let shared_model_experiments = root.get("sharedModelExperiments").cloned();

    let Ok(mut guard) = RICHEST_UPSTREAM.write() else {
        return;
    };
    let existing_score = guard.as_ref().map(|r| r.score);
    let existing_has_groups = guard.as_ref().is_some_and(|r| !r.grouped_models.is_empty());
    if existing_score.is_some_and(|s| score < s) {
        return;
    }
    let should_persist = existing_score.is_none_or(|s| score > s)
        || (!existing_has_groups && !grouped_models.is_empty());
    *guard = Some(RichestUpstream {
        score,
        models: models.clone(),
        sorts: sorts.clone(),
        grouped_models: grouped_models.clone(),
        experiment_ids: experiment_ids.clone(),
        shared_model_experiments: shared_model_experiments.clone(),
    });
    drop(guard);

    if should_persist {
        let mut payload = Map::new();
        payload.insert("models".to_string(), Value::Object(models.clone()));
        payload.insert("agentModelSorts".to_string(), sorts);
        if !grouped_models.is_empty() {
            payload.insert("groupedModels".to_string(), Value::Object(grouped_models));
        }
        if let Some(exp) = experiment_ids {
            payload.insert("experimentIds".to_string(), exp);
        }
        if let Some(shared) = shared_model_experiments {
            payload.insert("sharedModelExperiments".to_string(), shared);
        }
        let path = richest_upstream_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(path, Value::Object(payload).to_string());
    }
}

/// Restore missing flagship entries, `groupedModels`, sort groups, and experiments into a poorer upstream response.
fn merge_richest_into_response(original_json: &str, richest: &RichestUpstream) -> String {
    let Ok(Value::Object(mut root)) = serde_json::from_str::<Value>(original_json) else {
        return original_json.to_string();
    };
    let original_models = root
        .get("models")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if richness_score(&original_models) >= richest.score {
        return original_json.to_string();
    }

    let mut merged = original_models;
    for (k, v) in &richest.models {
        merged.entry(k.clone()).or_insert_with(|| v.clone());
    }
    root.insert("models".to_string(), Value::Object(merged));
    if richest.sorts.as_array().is_some_and(|a| !a.is_empty()) {
        root.insert("agentModelSorts".to_string(), richest.sorts.clone());
    }
    if !richest.grouped_models.is_empty() {
        let mut grouped = root
            .get("groupedModels")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        sanitize_grouped_models_map(&mut grouped);
        for (k, v) in &richest.grouped_models {
            if is_valid_grouped_model_entry(v) {
                grouped.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        if !grouped.is_empty() {
            root.insert("groupedModels".to_string(), Value::Object(grouped));
        }
    }
    if let Some(exp) = &richest.experiment_ids {
        root.entry("experimentIds".to_string())
            .or_insert_with(|| exp.clone());
    }
    if let Some(shared) = &richest.shared_model_experiments {
        root.entry("sharedModelExperiments".to_string())
            .or_insert_with(|| shared.clone());
    }
    Value::Object(root).to_string()
}

fn restore_richest_upstream(original_json: &str) -> String {
    let richest = RICHEST_UPSTREAM.read().ok().and_then(|g| g.clone());
    match richest {
        Some(r) => merge_richest_into_response(original_json, &r),
        None => original_json.to_string(),
    }
}

pub fn ingest_pool_model_ids<I: IntoIterator<Item = String>>(ids: I) -> usize {
    let tuples = ids.into_iter().map(|id| (id, None, None, None));
    GLOBAL_CATALOG.sync_from_account_models(tuples).unwrap_or(0)
}

pub fn create_synthetic_available_models_response() -> String {
    GLOBAL_CATALOG.create_synthetic_available_models_response()
}

// ============================================================================
// Core Injection & Normalization Engine
// ============================================================================

fn strip_models_prefix(id: &str) -> &str {
    if id.len() >= 7 && id[..7].eq_ignore_ascii_case("models/") {
        &id[7..]
    } else {
        id
    }
}

fn is_google_pool_provider(provider_id: &str) -> bool {
    let trimmed = provider_id.trim();
    trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("default_google_pool")
        || trimmed.eq_ignore_ascii_case("google")
}

/// Zero-hardcode bucket filter for Google's `models` map:
/// Returns true ONLY for empty IDs or non-model quota/tab buckets
/// (`chat_*`, `tab_*`, `*-disabled`, `*_disabled`, `*-5h`, `*-weekly`, `3p-*`).
/// Hidden internal models like `gemini-flash-worker-agy`, `gemini-4-hydrogen-tiered`,
/// and `gemini-3.5-flash-lite` return `false` here so they STAY in `models` for
/// subagents / checkpoints / `tieredModelIds.flashLite`, while never entering `agentModelSorts`.
pub fn is_dead_model(raw_id: &str) -> bool {
    let id = strip_models_prefix(raw_id.trim()).to_ascii_lowercase();
    if id.is_empty() {
        return true;
    }

    id.starts_with("chat_")
        || id.starts_with("tab_")
        || id.ends_with("-disabled")
        || id.ends_with("_disabled")
        || id.ends_with("-5h")
        || id.ends_with("-weekly")
        || id.starts_with("3p-")
}

/// Retired archive models & internal worker/image/tiered/bare IDs that must NOT
/// appear in `models_catalog.json` or `agentModelSorts` user chat dropdown.
pub fn is_retired_or_non_chat_catalog_id(raw_id: &str) -> bool {
    let id = strip_models_prefix(raw_id.trim()).to_ascii_lowercase();
    if is_dead_model(&id) {
        return true;
    }

    if matches!(
        id.as_str(),
        "gemini-2.5-pro"
            | "gemini-2.5-flash-thinking"
            | "gemini-2.5-flash-lite"
            | "gemini-4-hydrogen-sonar"
            | "gemini-pro-agent"
            | "claude-opus-5-5"
            | "claude-sonnet-5-5"
            | "gemini-3.8-flash"
            | "gpt-oss-120b"
            | "gemini-3.5-flash"
            | "gemini-3.5-flash-high"
            | "gemini-3.5-flash-medium"
            | "gemini-3.5-flash-low"
            | "gemini-3.5-flash-extra-low"
            | "gemini-3.5-flash-lite"
    ) {
        return true;
    }

    id.starts_with("gemini-3-flash")
        || id.starts_with("gemini-3.1-pro")
        || id.starts_with("gemini-3.6-")
        || id.starts_with("gemini-3.7-")
        || id.starts_with("gemini-flash-worker")
        || id.contains("-image")
        || id.ends_with("-tiered")
}

fn is_selectable_pool_chat_model(raw_id: &str) -> bool {
    let clean = strip_models_prefix(raw_id.trim());
    !is_retired_or_non_chat_catalog_id(clean)
        && crate::proxy::common::model_mapping::is_model_compliant_with_baseline(clean)
}

fn is_legacy_seeded_entry(entry: &CatalogModelEntry) -> bool {
    let desc = entry.description.trim();
    if desc == "Synced from account pool & official baseline"
        || desc.starts_with("Google Gemini ")
        || desc == "Claude Sonnet 4.6 (Google Cloud Code PA)"
        || desc == "Claude Opus 4.6 Thinking (Google Cloud Code PA)"
        || desc == "GPT-OSS 120B Medium"
        || desc == "Gemini 4 Neon (Frontier Reasoning & High Speed Coding)"
    {
        return true;
    }
    false
}

fn sort_entries(entries: &mut [CatalogModelEntry]) {
    entries.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.id.cmp(&b.id)));
}

fn write_entries_to_disk(
    path: &Path,
    entries: &[CatalogModelEntry],
) -> Result<Option<SystemTime>, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create dir {}: {}", parent.display(), e))?;
    }
    let mut sorted = entries.to_vec();
    sort_entries(&mut sorted);
    let json = serde_json::to_string_pretty(&sorted)
        .map_err(|e| format!("Failed to serialize catalog: {}", e))?;
    fs::write(path, json).map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
    Ok(fs::metadata(path).and_then(|m| m.modified()).ok())
}

fn normalize_and_ensure_defaults(entries: &mut Vec<CatalogModelEntry>) -> bool {
    let mut modified = false;
    let before = entries.len();
    entries.retain(|e| !is_retired_or_non_chat_catalog_id(&e.id) && !is_legacy_seeded_entry(e));
    if entries.len() != before {
        modified = true;
    }

    for entry in entries.iter_mut() {
        let clean = strip_models_prefix(entry.id.trim()).to_string();
        if clean != entry.id {
            entry.id = clean;
            modified = true;
        }
        if (entry.id.eq_ignore_ascii_case("claude-sonnet-4-6")
            || entry.id.eq_ignore_ascii_case("claude-opus-4-6-thinking"))
            && (entry.context_window != 200_000 || entry.supports_audio || entry.supports_video)
        {
            entry.context_window = 200_000;
            entry.max_input_tokens = 200_000;
            entry.supports_audio = false;
            entry.supports_video = false;
            modified = true;
        }
    }

    sort_entries(entries);
    modified
}

fn standard_supported_mime_types() -> Map<String, Value> {
    const MIMES: &[&str] = &[
        "audio/opus",
        "video/audio/wav",
        "application/json",
        "text/css",
        "audio/wave",
        "audio/l16",
        "application/x-ipynb+json",
        "image/jpeg",
        "text/javascript",
        "audio/mp3",
        "text/xml",
        "audio/x-wav",
        "audio/webm",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "image/heif",
        "audio/vnd.wave",
        "application/msword",
        "audio/flac",
        "audio/aac",
        "text/x-python",
        "video/jpeg2000",
        "application/x-python-code",
        "video/audio/s16le",
        "audio/wav",
        "text/csv",
        "image/webp",
        "text/plain",
        "application/x-javascript",
        "audio/m4a",
        "audio/mp4",
        "audio/mpeg",
        "application/vnd.ms-excel",
        "video/webm",
        "application/rtf",
        "image/png",
        "application/vnd.ms-powerpoint",
        "video/text/timestamp",
        "text/x-typescript",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "image/heic",
        "text/html",
        "application/pdf",
        "video/videoframe/jpeg2000",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "text/markdown",
        "text/x-python-script",
        "video/mp4",
        "text/rtf",
        "audio/ogg",
        "audio/webm;codecs=opus",
    ];
    let mut map = Map::new();
    for m in MIMES {
        map.insert((*m).to_string(), Value::Bool(true));
    }
    map
}

fn infer_proto_enum(id: &str) -> &'static str {
    let lower = id.to_ascii_lowercase();
    if lower == "gemini-2.5-flash" {
        "MODEL_GOOGLE_GEMINI_2_5_FLASH"
    } else if lower.contains("claude") {
        if lower.contains("opus") || lower.contains("thinking") {
            "MODEL_CLAUDE_4_OPUS_THINKING"
        } else {
            "MODEL_CLAUDE_4_SONNET"
        }
    } else if lower.contains("gpt-oss") {
        "MODEL_OPENAI_GPT_OSS_120B_MEDIUM"
    } else if lower.contains("pro") || lower.contains("gemini-4") {
        "MODEL_GOOGLE_GEMINI_2_5_PRO"
    } else {
        "MODEL_GOOGLE_GEMINI_2_5_FLASH"
    }
}

fn build_model_node(entry: &CatalogModelEntry) -> Map<String, Value> {
    let lower = entry.id.to_ascii_lowercase();
    let is_claude = lower.contains("claude") || entry.owned_by.eq_ignore_ascii_case("anthropic");
    let is_openai = lower.contains("gpt-oss") || entry.owned_by.eq_ignore_ascii_case("openai");

    let api_provider = if is_claude {
        "API_PROVIDER_ANTHROPIC_VERTEX"
    } else if is_openai {
        "API_PROVIDER_OPENAI_VERTEX"
    } else {
        "API_PROVIDER_GOOGLE_GEMINI"
    };
    let model_provider = if is_claude {
        "MODEL_PROVIDER_ANTHROPIC"
    } else if is_openai {
        "MODEL_PROVIDER_OPENAI"
    } else {
        "MODEL_PROVIDER_GOOGLE"
    };

    let mut node = Map::new();
    node.insert(
        "displayName".to_string(),
        Value::String(if entry.display_name.is_empty() {
            entry.id.clone()
        } else {
            entry.display_name.clone()
        }),
    );
    node.insert(
        "model".to_string(),
        Value::String(infer_proto_enum(&entry.id).to_string()),
    );
    node.insert(
        "apiProvider".to_string(),
        Value::String(api_provider.to_string()),
    );
    node.insert(
        "modelProvider".to_string(),
        Value::String(model_provider.to_string()),
    );
    node.insert(
        "maxTokens".to_string(),
        Value::Number(entry.context_window.into()),
    );
    node.insert(
        "maxOutputTokens".to_string(),
        Value::Number(entry.max_output_tokens.into()),
    );
    node.insert(
        "supportsImages".to_string(),
        Value::Bool(entry.supports_vision),
    );
    node.insert(
        "supportsThinking".to_string(),
        Value::Bool(entry.supports_thinking),
    );
    node.insert(
        "supportsVideo".to_string(),
        Value::Bool(!is_claude && entry.supports_video),
    );
    if entry.supports_thinking {
        node.insert("thinkingBudget".to_string(), Value::Number(8192.into()));
        node.insert("minThinkingBudget".to_string(), Value::Number(32.into()));
    }
    node.insert(
        "supportedMimeTypes".to_string(),
        Value::Object(standard_supported_mime_types()),
    );
    node.insert("quotaInfo".to_string(), json!({ "remainingFraction": 1.0 }));
    node
}

fn inject_with_catalog(
    original_json: &str,
    catalog: &[CatalogModelEntry],
    live_nodes: &HashMap<String, Map<String, Value>>,
) -> String {
    let mut root_obj = match serde_json::from_str::<Value>(original_json) {
        Ok(Value::Object(obj)) => obj,
        _ => match serde_json::from_str::<Value>(BASELINE_MODELS_JSON) {
            Ok(Value::Object(obj)) => obj,
            _ => Map::new(),
        },
    };

    if !root_obj.get("models").is_some_and(Value::is_object) {
        root_obj.insert("models".to_string(), Value::Object(Map::new()));
    }

    let std_mimes = standard_supported_mime_types();
    let mut models_obj = match root_obj.remove("models") {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };

    // 1. Strip internal non-model buckets & user-disabled catalog models, while preserving
    //    bare keys ONLY (NO `models/` prefix duplicates!) and keeping Google's native fields
    //    (`model` enum, `thinkingLevel`, `modelExperiments`, `tagTitle`, `supportedMimeTypes`,
    //    hidden internal subagent/checkpoint models like `gemini-flash-worker-agy` &
    //    `gemini-4-hydrogen-tiered`) completely intact!
    let disabled_ids: HashSet<String> = catalog
        .iter()
        .filter(|e| !e.enabled && is_google_pool_provider(&e.provider_id))
        .map(|e| e.id.to_ascii_lowercase())
        .collect();

    let existing_keys: Vec<String> = models_obj.keys().cloned().collect();
    let mut normalized_models = Map::new();

    for key in existing_keys {
        let val = match models_obj.remove(&key) {
            Some(v) => v,
            None => continue,
        };
        let clean_id = strip_models_prefix(&key).to_string();
        if is_dead_model(&clean_id) || disabled_ids.contains(&clean_id.to_ascii_lowercase()) {
            continue;
        }

        if let Value::Object(mut m_node) = val {
            let lower = clean_id.to_ascii_lowercase();

            if !m_node.contains_key("supportedMimeTypes") {
                m_node.insert(
                    "supportedMimeTypes".to_string(),
                    Value::Object(std_mimes.clone()),
                );
            }

            if lower == "gemini-2.5-flash"
                && m_node.get("displayName").and_then(|v| v.as_str())
                    == Some("Gemini 3.5 Flash Lite")
            {
                m_node.insert("displayName".to_string(), json!("Gemini 2.5 Flash"));
            }

            normalized_models
                .entry(clean_id)
                .or_insert_with(|| Value::Object(m_node));
        }
    }

    // 2. Merge active Google Pool catalog models (bare keys ONLY — zero `models/` dupes!)
    let mut injected_ids = Vec::new();
    for entry in catalog {
        if !entry.enabled || !is_google_pool_provider(&entry.provider_id) {
            continue;
        }
        let clean_id = strip_models_prefix(entry.id.trim()).to_string();
        if is_retired_or_non_chat_catalog_id(&clean_id) {
            continue;
        }
        if !normalized_models.contains_key(&clean_id) {
            let lower = clean_id.to_ascii_lowercase();
            let node = if let Some(cached_native) = live_nodes.get(&lower) {
                let mut merged = build_model_node(entry);
                for (k, v) in cached_native {
                    merged.insert(k.clone(), v.clone());
                }
                merged
                    .entry("quotaInfo".to_string())
                    .or_insert_with(|| json!({ "remainingFraction": 1.0 }));
                Value::Object(merged)
            } else {
                Value::Object(build_model_node(entry))
            };
            normalized_models.insert(clean_id.clone(), node);
        }
        if !injected_ids
            .iter()
            .any(|id: &String| id.eq_ignore_ascii_case(&clean_id))
        {
            injected_ids.push(clean_id);
        }
    }

    // 3. Sync multi-group `agentModelSorts` cleanly (NEVER dumping hidden `models_obj` keys!)
    sync_agent_model_sorts(
        &mut root_obj,
        &normalized_models,
        &injected_ids,
        &disabled_ids,
    );

    // 4. Ensure defaultAgentModelId
    if !root_obj
        .get("defaultAgentModelId")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty() && normalized_models.contains_key(id))
    {
        let fallback_default = root_obj
            .get("agentModelSorts")
            .and_then(|s| s.get(0))
            .and_then(|s| s.get("groups"))
            .and_then(|g| g.get(0))
            .and_then(|g| g.get("modelIds"))
            .and_then(|m| m.get(0))
            .and_then(Value::as_str)
            .unwrap_or("gemini-3.8-flash-medium")
            .to_string();
        root_obj.insert("defaultAgentModelId".to_string(), json!(fallback_default));
    }

    // 5. Preserve tieredModelIds.flashLite (critical for search_web and /teamwork)
    sync_tiered_model_ids(&mut root_obj, &normalized_models);

    // 6. Ensure `groupedModels` contains every active tiered family (Carbon, Neon, Argon, Claude 5.5, Hydrogen, 3.8 Flash)
    ensure_grouped_models_complete(&mut root_obj, &normalized_models);

    root_obj.remove("deprecatedModelIds");
    if !root_obj.get("commandModelIds").is_some_and(Value::is_array) {
        let cmd_default = root_obj
            .get("defaultAgentModelId")
            .and_then(Value::as_str)
            .unwrap_or("gemini-3.8-flash-medium")
            .to_string();
        root_obj.insert("commandModelIds".to_string(), json!([cmd_default]));
    }
    if !root_obj.get("tabModelIds").is_some_and(Value::is_array) {
        root_obj.insert(
            "tabModelIds".to_string(),
            json!(["gemini-3.5-flash-lite", "chat_20706"]),
        );
    }

    root_obj.insert("models".to_string(), Value::Object(normalized_models));
    Value::Object(root_obj).to_string()
}

fn family_display_name_from_id(
    family_id: &str,
    sample_node: Option<&Map<String, Value>>,
) -> String {
    if let Some(dn) = sample_node
        .and_then(|n| n.get("displayName"))
        .and_then(Value::as_str)
    {
        let trimmed = dn.trim();
        let stripped = [
            " (Low)",
            " (Medium)",
            " (High)",
            " (XHigh)",
            " Low",
            " Medium",
            " High",
            " XHigh",
        ]
        .iter()
        .find_map(|suffix| trimmed.strip_suffix(suffix))
        .unwrap_or(trimmed);
        if !stripped.is_empty() {
            return stripped.to_string();
        }
    }

    // Fallback: format `claude-sonnet-5-5` -> `Claude Sonnet 5.5`, `gemini-4-carbon` -> `Gemini 4 Carbon`
    let parts: Vec<&str> = family_id.split('-').collect();
    let mut out = Vec::with_capacity(parts.len());
    let mut i = 0;
    while i < parts.len() {
        let p = parts[i];
        if i + 1 < parts.len()
            && p.len() == 1
            && p.chars().all(|c| c.is_ascii_digit())
            && parts[i + 1].len() == 1
            && parts[i + 1].chars().all(|c| c.is_ascii_digit())
        {
            out.push(format!("{}.{}", p, parts[i + 1]));
            i += 2;
            continue;
        }
        let mut chars = p.chars();
        let titled = match chars.next() {
            Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
            None => String::new(),
        };
        out.push(titled);
        i += 1;
    }
    out.join(" ")
}

/// Ensures `root_obj["groupedModels"]` has an entry for every active tiered model family
/// (`<family>-low`, `-medium`, `-high`, `-xhigh`) present in `normalized_models`,
/// using the exact Google `FetchAvailableModelsResponse` protojson schema:
/// `supportedThinkingLevels: [{ "thinkingSuffix": "low", "thinkingDisplayName": "Low" }, ...]`.
fn ensure_grouped_models_complete(
    root_obj: &mut Map<String, Value>,
    normalized_models: &Map<String, Value>,
) {
    const TIER_SUFFIXES: [(&str, &str, &str, usize); 4] = [
        ("-low", "low", "Low", 0),
        ("-medium", "medium", "Medium", 1),
        ("-high", "high", "High", 2),
        ("-xhigh", "xhigh", "Extra High", 3),
    ];

    let mut families: HashMap<
        String,
        (
            Vec<(&'static str, &'static str, usize)>,
            Option<&Map<String, Value>>,
        ),
    > = HashMap::new();

    for (model_id, val) in normalized_models {
        let lower = model_id.to_ascii_lowercase();
        for &(suffix, thinking_suffix, thinking_display, order_idx) in &TIER_SUFFIXES {
            if let Some(prefix_len) = lower.strip_suffix(suffix).map(|s| s.len()) {
                let family_id = model_id[..prefix_len].to_string();
                if family_id.to_ascii_lowercase().ends_with("-tiered") {
                    break;
                }
                let entry = families
                    .entry(family_id)
                    .or_insert_with(|| (Vec::new(), None));
                if !entry.0.iter().any(|(s, _, _)| *s == thinking_suffix) {
                    entry.0.push((thinking_suffix, thinking_display, order_idx));
                }
                if entry.1.is_none() || suffix == "-medium" {
                    entry.1 = val.as_object();
                }
                break;
            }
        }
    }

    let mut grouped = match root_obj.remove("groupedModels") {
        Some(Value::Object(mut m)) => {
            sanitize_grouped_models_map(&mut m);
            m
        }
        _ => Map::new(),
    };

    for (family_id, (mut levels, sample_node)) in families {
        if levels.len() < 2 || grouped.contains_key(&family_id) {
            continue;
        }
        levels.sort_by_key(|(_, _, idx)| *idx);
        let display_name = family_display_name_from_id(&family_id, sample_node);
        let level_objs: Vec<Value> = levels
            .into_iter()
            .map(|(thinking_suffix, thinking_display, _)| {
                json!({
                    "thinkingSuffix": thinking_suffix,
                    "thinkingDisplayName": thinking_display,
                })
            })
            .collect();
        grouped.insert(
            family_id,
            json!({
                "displayName": display_name,
                "supportedThinkingLevels": level_objs,
            }),
        );
    }

    if !grouped.is_empty() {
        root_obj.insert("groupedModels".to_string(), Value::Object(grouped));
    }
}

fn sync_agent_model_sorts(
    root_obj: &mut Map<String, Value>,
    models_obj: &Map<String, Value>,
    injected_ids: &[String],
    disabled_ids: &HashSet<String>,
) {
    let had_upstream_sorts = root_obj
        .get("agentModelSorts")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());

    let mut sorts = match root_obj.remove("agentModelSorts") {
        Some(Value::Array(arr)) if !arr.is_empty() => arr,
        _ => vec![json!({
            "displayName": "Recommended",
            "groups": []
        })],
    };

    if !sorts[0].is_object() {
        sorts[0] = json!({
            "displayName": "Recommended",
            "groups": []
        });
    }

    let mut seen_across_groups: HashSet<String> = HashSet::new();

    // Preserve ALL groups across `sorts`, filtering disabled/dead/missing entries and deduplicating
    for sort_val in sorts.iter_mut() {
        let Some(sort_obj) = sort_val.as_object_mut() else {
            continue;
        };
        let raw_groups = match sort_obj.remove("groups") {
            Some(Value::Array(g)) => g,
            _ => Vec::new(),
        };

        let mut cleaned_groups: Vec<Value> = Vec::new();
        for group_val in raw_groups {
            let Some(mut group_obj) = group_val.as_object().cloned() else {
                continue;
            };
            let Some(Value::Array(raw_ids)) = group_obj.remove("modelIds") else {
                continue;
            };

            let mut group_clean_ids: Vec<Value> = Vec::new();
            for id_val in raw_ids {
                let Some(raw_str) = id_val.as_str() else {
                    continue;
                };
                let clean = strip_models_prefix(raw_str.trim());
                if clean.is_empty() || clean.starts_with("MODEL_") || is_dead_model(clean) {
                    continue;
                }
                let lower = clean.to_ascii_lowercase();
                if disabled_ids.contains(&lower) || !models_obj.contains_key(clean) {
                    continue;
                }
                if seen_across_groups.insert(lower) {
                    group_clean_ids.push(Value::String(clean.to_string()));
                }
            }

            if !group_clean_ids.is_empty() {
                group_obj.insert("modelIds".to_string(), Value::Array(group_clean_ids));
                cleaned_groups.push(Value::Object(group_obj));
            }
        }

        sort_obj.insert("groups".to_string(), Value::Array(cleaned_groups));
    }

    // Group any missing `injected_ids` (Pool union or user custom models) by base family and append
    let mut missing_injected: Vec<String> = Vec::new();
    for id in injected_ids {
        let clean = strip_models_prefix(id.trim());
        let lower = clean.to_ascii_lowercase();
        if !clean.is_empty()
            && !disabled_ids.contains(&lower)
            && models_obj.contains_key(clean)
            && seen_across_groups.insert(lower)
        {
            missing_injected.push(clean.to_string());
        }
    }

    // Offline synthetic fallback ONLY: if upstream had no `agentModelSorts` AND catalog had no `injected_ids`,
    // build clean family groups strictly from `recommended == true` selectable chat models in `models_obj`.
    if !had_upstream_sorts && seen_across_groups.is_empty() && missing_injected.is_empty() {
        let mut candidate_keys: Vec<String> = models_obj
            .iter()
            .filter_map(|(k, v)| {
                let rec = v
                    .get("recommended")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if rec && is_selectable_pool_chat_model(k) {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();
        candidate_keys.sort();
        for k in candidate_keys {
            if seen_across_groups.insert(k.to_ascii_lowercase()) {
                missing_injected.push(k);
            }
        }
    }

    if !missing_injected.is_empty() {
        let mut family_order: Vec<String> = Vec::new();
        let mut family_map: HashMap<String, Vec<Value>> = HashMap::new();
        for id in missing_injected {
            let family =
                crate::proxy::common::model_mapping::strip_tier_suffix(&id.to_ascii_lowercase());
            if !family_map.contains_key(&family) {
                family_order.push(family.clone());
            }
            family_map
                .entry(family)
                .or_default()
                .push(Value::String(id));
        }

        if let Some(first_sort) = sorts[0].as_object_mut() {
            let groups_arr = first_sort
                .entry("groups".to_string())
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .unwrap();
            for fam in family_order {
                if let Some(ids) = family_map.remove(&fam) {
                    groups_arr.push(json!({ "modelIds": ids }));
                }
            }
        }
    }

    root_obj.insert("agentModelSorts".to_string(), Value::Array(sorts));
}

fn sync_tiered_model_ids(root_obj: &mut Map<String, Value>, models_obj: &Map<String, Value>) {
    let pick_if_present = |candidates: &[&str], fallback: &str| -> Value {
        for c in candidates {
            if models_obj.contains_key(*c) {
                return json!([c]);
            }
        }
        json!([fallback])
    };

    let flash_lite_fallback = if models_obj.contains_key("gemini-3.5-flash-lite") {
        "gemini-3.5-flash-lite"
    } else {
        "gemini-2.5-flash-lite"
    };
    let flash_arr = pick_if_present(
        &[
            "gemini-3.8-flash-medium",
            "gemini-3.8-flash-low",
            "gemini-2.5-flash",
        ],
        "gemini-3.8-flash-medium",
    );
    let pro_arr = pick_if_present(
        &[
            "gemini-4-argon-medium",
            "gemini-4-argon-high",
            "gemini-4-neon",
        ],
        "gemini-4-argon-medium",
    );

    match root_obj.get_mut("tieredModelIds") {
        Some(Value::Object(tiered)) => {
            let has_valid_flash_lite = tiered
                .get("flashLite")
                .and_then(|v| v.as_array())
                .is_some_and(|a| !a.is_empty());
            if !has_valid_flash_lite {
                tiered.insert("flashLite".to_string(), json!([flash_lite_fallback]));
            }
            if !tiered
                .get("flash")
                .and_then(Value::as_array)
                .is_some_and(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .any(|id| models_obj.contains_key(id))
                })
            {
                tiered.insert("flash".to_string(), flash_arr);
            }
            if !tiered
                .get("pro")
                .and_then(Value::as_array)
                .is_some_and(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .any(|id| models_obj.contains_key(id))
                })
            {
                tiered.insert("pro".to_string(), pro_arr);
            }
        }
        _ => {
            root_obj.insert(
                "tieredModelIds".to_string(),
                json!({
                    "flashLite": [flash_lite_fallback],
                    "flash": flash_arr,
                    "pro": pro_arr,
                }),
            );
        }
    }
}

/// Default catalog starts empty — real models are populated purely from live Google
/// `fetchAvailableModels` responses and healthy accounts owned in the Pool.
pub fn default_catalog() -> Vec<CatalogModelEntry> {
    Vec::new()
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_layer1_richest_upstream_restores_flagship_models_and_sorts() {
        let richest_json = json!({
            "models": {
                "gemini-4-hydrogen-medium": { "displayName": "Hydrogen" },
                "gemini-4-carbon-medium": { "displayName": "Carbon" },
                "claude-opus-5-5-high": { "displayName": "Opus 5.5" }
            },
            "agentModelSorts": [{
                "displayName": "Recommended",
                "groups": [{ "modelIds": [
                    "gemini-4-carbon-medium",
                    "claude-opus-5-5-high",
                    "gemini-4-hydrogen-medium"
                ]}]
            }]
        });
        let Value::Object(root) = richest_json else {
            panic!("object")
        };
        let Some(Value::Object(models)) = root.get("models").cloned() else {
            panic!("models")
        };
        let richest = RichestUpstream {
            score: richness_score(&models),
            models,
            sorts: root.get("agentModelSorts").cloned().unwrap(),
            grouped_models: json!({
                "gemini-4-carbon": {
                    "displayName": "Gemini 4 Carbon",
                    "supportedThinkingLevels": [
                        { "thinkingSuffix": "low", "thinkingDisplayName": "Low" },
                        { "thinkingSuffix": "medium", "thinkingDisplayName": "Medium" },
                        { "thinkingSuffix": "high", "thinkingDisplayName": "High" },
                        { "thinkingSuffix": "xhigh", "thinkingDisplayName": "Extra High" }
                    ]
                },
                "claude-opus-5-5": {
                    "displayName": "Claude Opus 5.5",
                    "supportedThinkingLevels": [
                        { "thinkingSuffix": "low", "thinkingDisplayName": "Low" },
                        { "thinkingSuffix": "medium", "thinkingDisplayName": "Medium" },
                        { "thinkingSuffix": "high", "thinkingDisplayName": "High" }
                    ]
                }
            })
            .as_object()
            .cloned()
            .unwrap(),
            experiment_ids: Some(json!(["exp_carbon_canary"])),
            shared_model_experiments: Some(json!({ "carbon": true })),
        };
        assert!(richest.score >= 2000);

        // A Basic account answers with only Hydrogen.
        let basic = json!({
            "models": { "gemini-4-hydrogen-medium": { "displayName": "Hydrogen" } },
            "groupedModels": {
                "gemini-4-hydrogen": {
                    "displayName": "Gemini 4 Hydrogen",
                    "supportedThinkingLevels": [
                        { "thinkingSuffix": "low", "thinkingDisplayName": "Low" },
                        { "thinkingSuffix": "high", "thinkingDisplayName": "High" }
                    ]
                }
            },
            "agentModelSorts": [{
                "displayName": "Recommended",
                "groups": [{ "modelIds": ["gemini-4-hydrogen-medium"] }]
            }]
        })
        .to_string();

        let merged: Value =
            serde_json::from_str(&merge_richest_into_response(&basic, &richest)).unwrap();
        let models = merged["models"].as_object().unwrap();
        assert!(models.contains_key("gemini-4-carbon-medium"));
        assert!(models.contains_key("claude-opus-5-5-high"));
        let grouped = merged["groupedModels"].as_object().unwrap();
        assert!(grouped.contains_key("gemini-4-hydrogen"));
        assert!(grouped.contains_key("gemini-4-carbon"));
        assert!(grouped.contains_key("claude-opus-5-5"));
        assert_eq!(merged["experimentIds"], json!(["exp_carbon_canary"]));
        assert_eq!(merged["sharedModelExperiments"], json!({ "carbon": true }));
        let ids = merged["agentModelSorts"][0]["groups"][0]["modelIds"]
            .as_array()
            .unwrap();
        assert!(ids.iter().any(|v| v == "gemini-4-carbon-medium"));

        // A response that is already as rich is returned untouched.
        let full = json!({ "models": richest.models.clone() }).to_string();
        assert_eq!(merge_richest_into_response(&full, &richest), full);
    }

    #[test]
    fn test_ensure_grouped_models_complete_synthesizes_missing_families() {
        let basic = json!({
            "models": {
                "gemini-4-hydrogen-low": { "displayName": "Gemini 4 Hydrogen (Low)" },
                "gemini-4-hydrogen-high": { "displayName": "Gemini 4 Hydrogen (High)" },
                "gemini-4-carbon-medium": { "displayName": "Gemini 4 Carbon (Medium)" },
                "gemini-4-carbon-high": { "displayName": "Gemini 4 Carbon (High)" },
                "claude-sonnet-5-5-low": { "displayName": "Claude Sonnet 5.5 (Low)" },
                "claude-sonnet-5-5-medium": { "displayName": "Claude Sonnet 5.5 (Medium)" },
                "gemini-4-hydrogen-tiered-high": { "displayName": "Internal Tiered High" },
                "gemini-4-hydrogen-tiered-xhigh": { "displayName": "Internal Tiered XHigh" }
            },
            "groupedModels": {
                "gemini-4-hydrogen": {
                    "displayName": "Custom Hydrogen",
                    "supportedThinkingLevels": [
                        { "thinkingSuffix": "low", "thinkingDisplayName": "Low" },
                        { "thinkingSuffix": "high", "thinkingDisplayName": "High" }
                    ]
                },
                "gemini-4-carbon": {
                    "displayName": "Poisoned Carbon",
                    "supportedThinkingLevels": ["MEDIUM", "HIGH"]
                }
            }
        })
        .to_string();

        let out: Value =
            serde_json::from_str(&inject_with_catalog(&basic, &[], &HashMap::new())).unwrap();
        let grouped = out["groupedModels"].as_object().unwrap();
        // Existing valid entry is preserved untouched
        assert_eq!(
            grouped["gemini-4-hydrogen"]["displayName"],
            "Custom Hydrogen"
        );
        // Poisoned bare-string entry is stripped & re-synthesized with valid proto objects
        assert_eq!(grouped["gemini-4-carbon"]["displayName"], "Gemini 4 Carbon");
        assert_eq!(
            grouped["gemini-4-carbon"]["supportedThinkingLevels"],
            json!([
                { "thinkingSuffix": "medium", "thinkingDisplayName": "Medium" },
                { "thinkingSuffix": "high", "thinkingDisplayName": "High" }
            ])
        );
        assert_eq!(
            grouped["claude-sonnet-5-5"]["displayName"],
            "Claude Sonnet 5.5"
        );
        assert_eq!(
            grouped["claude-sonnet-5-5"]["supportedThinkingLevels"],
            json!([
                { "thinkingSuffix": "low", "thinkingDisplayName": "Low" },
                { "thinkingSuffix": "medium", "thinkingDisplayName": "Medium" }
            ])
        );
        // Internal `-tiered` family must NOT be added to groupedModels
        assert!(!grouped.contains_key("gemini-4-hydrogen-tiered"));
    }

    #[test]
    fn test_grouped_models_never_emit_bare_string_tokens() {
        let (_dir, cat) = temp_catalog();
        for (id, name) in [
            ("gemini-4-carbon-low", "Gemini 4 Carbon (Low)"),
            ("gemini-4-carbon-medium", "Gemini 4 Carbon (Medium)"),
            ("gemini-4-carbon-high", "Gemini 4 Carbon (High)"),
            ("gemini-4-neon-xhigh", "Gemini 4 Neon (XHigh)"),
            ("gemini-4-neon-low", "Gemini 4 Neon (Low)"),
        ] {
            cat.upsert_model(CatalogModelEntry {
                id: id.to_string(),
                display_name: name.to_string(),
                enabled: true,
                owned_by: "google".to_string(),
                provider_id: "default_google_pool".to_string(),
                provider_name: "Google Account Pool".to_string(),
                target_model: id.to_string(),
                context_window: 1_048_576,
                max_input_tokens: 1_048_576,
                max_output_tokens: 65_536,
                supports_thinking: true,
                supports_vision: true,
                supports_audio: true,
                supports_video: true,
                supports_function_calling: true,
                input_pricing: 0.15,
                output_pricing: 0.60,
                description: DESC_SYNCED_POOL.to_string(),
                order: 10,
            })
            .unwrap();
        }

        let check_valid_grouped = |raw_json: &str| {
            let parsed: Value = serde_json::from_str(raw_json).unwrap();
            if let Some(grouped) = parsed.get("groupedModels").and_then(Value::as_object) {
                for (family, entry) in grouped {
                    assert!(
                        is_valid_grouped_model_entry(entry),
                        "groupedModels[{}] has invalid proto schema: {}",
                        family,
                        entry
                    );
                    if let Some(levels) = entry
                        .get("supportedThinkingLevels")
                        .and_then(Value::as_array)
                    {
                        for item in levels {
                            assert!(
                                item.is_object(),
                                "groupedModels[{}] emitted bare token instead of proto object: {}",
                                family,
                                item
                            );
                        }
                    }
                }
            }
        };

        let injected = cat.inject_models_into_fetch_response(r#"{"models":{}}"#);
        check_valid_grouped(&injected);

        let synthetic = cat.create_synthetic_available_models_response();
        check_valid_grouped(&synthetic);
    }

    #[test]
    fn test_flagship_canary_model_detection() {
        assert!(is_flagship_canary_model("gemini-4-carbon-medium"));
        assert!(is_flagship_canary_model("models/gemini-4-neon-high"));
        assert!(is_flagship_canary_model("claude-sonnet-5-5-low"));
        assert!(!is_flagship_canary_model("gemini-4-hydrogen-medium"));
        assert!(!is_flagship_canary_model("gemini-3.5-flash-lite"));
        assert!(!is_flagship_canary_model("claude-sonnet-4-6"));
    }

    fn temp_catalog() -> (TempDir, AsarModelCatalog) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("models_catalog.json");
        let cat = AsarModelCatalog::open(path);
        (dir, cat)
    }

    #[test]
    fn test_crud_persistence_and_legacy_purge() {
        let (dir, cat) = temp_catalog();
        assert!(dir.path().join("models_catalog.json").exists());
        // default_catalog() is empty — no fake models seeded
        assert!(cat.get_all_models().is_empty());

        // Add custom model
        let custom = CatalogModelEntry {
            id: "gemini-4-custom-pro".to_string(),
            display_name: "Gemini 4 Custom Pro".to_string(),
            enabled: true,
            owned_by: "google".to_string(),
            provider_id: "default_google_pool".to_string(),
            provider_name: "Google Account Pool".to_string(),
            target_model: "gemini-4-argon-high".to_string(),
            context_window: 1_048_576,
            max_input_tokens: 1_048_576,
            max_output_tokens: 65_536,
            supports_thinking: true,
            supports_vision: true,
            supports_audio: true,
            supports_video: true,
            supports_function_calling: true,
            input_pricing: 0.1,
            output_pricing: 0.2,
            description: "Custom user model".to_string(),
            order: 10,
        };
        cat.upsert_model(custom).unwrap();
        assert!(cat.get_model("gemini-4-custom-pro").is_some());

        // Toggle off
        assert!(cat.toggle_model("gemini-4-custom-pro", false).unwrap());
        assert!(!cat.get_model("gemini-4-custom-pro").unwrap().enabled);

        // Write legacy archive / fake default entries to disk and verify open() purges them
        let polluted = vec![
            cat.get_model("gemini-4-custom-pro").unwrap(),
            CatalogModelEntry {
                id: "gemini-4-hydrogen-sonar".to_string(),
                display_name: "Fake Sonar".to_string(),
                enabled: true,
                owned_by: "google".to_string(),
                provider_id: "default_google_pool".to_string(),
                provider_name: "Google Account Pool".to_string(),
                target_model: "gemini-4-hydrogen-sonar".to_string(),
                context_window: 1_048_576,
                max_input_tokens: 1_048_576,
                max_output_tokens: 65_536,
                supports_thinking: true,
                supports_vision: true,
                supports_audio: true,
                supports_video: true,
                supports_function_calling: true,
                input_pricing: 0.1,
                output_pricing: 0.2,
                description: "Google Gemini 4 Hydrogen Sonar - Deep Research & Web Grounding"
                    .to_string(),
                order: 15,
            },
            CatalogModelEntry {
                id: "gemini-3.1-pro-high".to_string(),
                display_name: "Old 3.1 Pro".to_string(),
                enabled: true,
                owned_by: "google".to_string(),
                provider_id: "default_google_pool".to_string(),
                provider_name: "Google Account Pool".to_string(),
                target_model: "gemini-3.1-pro-high".to_string(),
                context_window: 1_048_576,
                max_input_tokens: 1_048_576,
                max_output_tokens: 65_536,
                supports_thinking: true,
                supports_vision: true,
                supports_audio: true,
                supports_video: true,
                supports_function_calling: true,
                input_pricing: 0.1,
                output_pricing: 0.2,
                description: "Synced from account pool & official baseline".to_string(),
                order: 75,
            },
        ];
        fs::write(
            dir.path().join("models_catalog.json"),
            serde_json::to_string_pretty(&polluted).unwrap(),
        )
        .unwrap();

        let cat2 = AsarModelCatalog::open(dir.path().join("models_catalog.json"));
        assert!(!cat2.get_model("gemini-4-custom-pro").unwrap().enabled);
        assert!(cat2.get_model("gemini-4-hydrogen-sonar").is_none());
        assert!(cat2.get_model("gemini-3.1-pro-high").is_none());

        assert!(cat2.delete_model("gemini-4-custom-pro").unwrap());
        assert!(cat2.get_model("gemini-4-custom-pro").is_none());
    }

    #[test]
    fn test_hot_reload_on_external_file_edit() {
        let (dir, cat) = temp_catalog();
        let path = dir.path().join("models_catalog.json");

        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut list = cat.get_all_models();
        list.push(CatalogModelEntry {
            id: "gemini-4-hot-reloaded".to_string(),
            display_name: "Hot Reloaded".to_string(),
            enabled: true,
            owned_by: "google".to_string(),
            provider_id: "default_google_pool".to_string(),
            provider_name: "Google Account Pool".to_string(),
            target_model: "gemini-4-argon-medium".to_string(),
            context_window: 1_048_576,
            max_input_tokens: 1_048_576,
            max_output_tokens: 65_536,
            supports_thinking: false,
            supports_vision: true,
            supports_audio: true,
            supports_video: true,
            supports_function_calling: true,
            input_pricing: 0.1,
            output_pricing: 0.2,
            description: "User custom".to_string(),
            order: 90,
        });
        fs::write(&path, serde_json::to_string_pretty(&list).unwrap()).unwrap();

        assert!(cat.get_model("gemini-4-hot-reloaded").is_some());
    }

    #[test]
    fn test_no_models_prefix_and_hidden_models_never_leak_into_sorts() {
        let (_dir, cat) = temp_catalog();
        // Simulate a Pool-unioned Ultra model not in the current account's upstream response
        cat.upsert_model(CatalogModelEntry {
            id: "gemini-4-carbon-high".to_string(),
            display_name: "Gemini 4 Carbon (High)".to_string(),
            enabled: true,
            owned_by: "google".to_string(),
            provider_id: "default_google_pool".to_string(),
            provider_name: "Google Account Pool".to_string(),
            target_model: "gemini-4-carbon-high".to_string(),
            context_window: 1_048_576,
            max_input_tokens: 1_048_576,
            max_output_tokens: 262_144,
            supports_thinking: true,
            supports_vision: true,
            supports_audio: true,
            supports_video: true,
            supports_function_calling: true,
            input_pricing: 0.15,
            output_pricing: 0.60,
            description: DESC_SYNCED_POOL.to_string(),
            order: 10,
        })
        .unwrap();

        let upstream = json!({
            "models": {
                "chat_20706": { "displayName": "Internal Chat" },
                "gemini-4-argon-high": { "displayName": "Gemini 4 Argon (High)", "model": "MODEL_PLACEHOLDER_M417" },
                "gemini-4-argon-medium": { "displayName": "Gemini 4 Argon (Medium)", "model": "MODEL_PLACEHOLDER_M418" },
                "claude-sonnet-5-5-high": { "displayName": "Claude Sonnet 5.5 (High)", "model": "MODEL_PLACEHOLDER_M401" },
                "gemini-flash-worker-agy": { "displayName": "Internal Subagent Worker", "model": "MODEL_PLACEHOLDER_M50" },
                "gemini-4-hydrogen-tiered": { "displayName": "Internal Tiered", "model": "MODEL_PLACEHOLDER_M430" },
                "gemini-3.5-flash-lite": { "displayName": "Gemini 3.5 Flash Lite", "model": "MODEL_PLACEHOLDER_M198" }
            },
            "agentModelSorts": [{
                "displayName": "Recommended",
                "groups": [
                    { "modelIds": ["gemini-4-argon-high", "gemini-4-argon-medium", "gemini-4-argon-high"] },
                    { "modelIds": ["claude-sonnet-5-5-high"] }
                ]
            }],
            "tieredModelIds": {
                "flashLite": ["gemini-3.5-flash-lite"],
                "flash": ["gemini-4-argon-medium"],
                "pro": ["gemini-4-argon-high"]
            }
        })
        .to_string();

        let out_str = cat.inject_models_into_fetch_response(&upstream);
        let out: Value = serde_json::from_str(&out_str).unwrap();
        let models = out["models"].as_object().unwrap();

        // (1) Zero `models/` prefixed keys in `models`
        assert!(!models.keys().any(|k| k.starts_with("models/")));

        // (2) Hidden internal models stay in `models` for subagents/checkpoints/flashLite
        assert!(models.contains_key("gemini-flash-worker-agy"));
        assert!(models.contains_key("gemini-4-hydrogen-tiered"));
        assert!(models.contains_key("gemini-3.5-flash-lite"));
        // Non-model bucket `chat_20706` stripped from `models`
        assert!(!models.contains_key("chat_20706"));
        // Pool-unioned model injected cleanly without `models/` prefix
        assert!(models.contains_key("gemini-4-carbon-high"));

        // (3) Multi-group `agentModelSorts` preserved without duplicates, and hidden models NEVER leak!
        let groups = out["agentModelSorts"][0]["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 3); // 2 upstream groups + 1 appended family group for `gemini-4-carbon-high`
        assert_eq!(
            groups[0]["modelIds"],
            json!(["gemini-4-argon-high", "gemini-4-argon-medium"])
        );
        assert_eq!(groups[1]["modelIds"], json!(["claude-sonnet-5-5-high"]));
        assert_eq!(groups[2]["modelIds"], json!(["gemini-4-carbon-high"]));

        let all_sort_ids: Vec<&str> = groups
            .iter()
            .flat_map(|g| g["modelIds"].as_array().unwrap())
            .filter_map(Value::as_str)
            .collect();
        assert!(!all_sort_ids.contains(&"gemini-flash-worker-agy"));
        assert!(!all_sort_ids.contains(&"gemini-4-hydrogen-tiered"));
        assert!(!all_sort_ids.contains(&"gemini-3.5-flash-lite"));

        // Preserve tieredModelIds.flashLite
        assert_eq!(
            out["tieredModelIds"]["flashLite"],
            json!(["gemini-3.5-flash-lite"])
        );
    }

    #[test]
    fn test_future_model_auto_discovery_and_native_fields_preserved() {
        let (_dir, cat) = temp_catalog();

        let live_google_json = json!({
            "models": {
                "gemini-5-omni-high": {
                    "displayName": "Gemini 5 Omni (High)",
                    "model": "MODEL_PLACEHOLDER_M999",
                    "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                    "modelProvider": "MODEL_PROVIDER_GOOGLE",
                    "thinkingLevel": 3,
                    "modelExperiments": [{ "experimentId": 777 }],
                    "tagTitle": "New",
                    "supportedMimeTypes": { "text/plain": true, "image/png": true },
                    "maxTokens": 4194304,
                    "maxOutputTokens": 131072
                },
                "gemini-5-omni-medium": {
                    "displayName": "Gemini 5 Omni (Medium)",
                    "model": "MODEL_PLACEHOLDER_M998",
                    "modelProvider": "MODEL_PROVIDER_GOOGLE",
                    "thinkingLevel": 2
                },
                "claude-sonnet-6-0": {
                    "displayName": "Claude Sonnet 6.0",
                    "model": "MODEL_PLACEHOLDER_M1001",
                    "modelProvider": "MODEL_PROVIDER_ANTHROPIC"
                },
                "gemini-flash-worker-agy": {
                    "displayName": "Internal Worker",
                    "model": "MODEL_PLACEHOLDER_M50"
                }
            },
            "agentModelSorts": [{
                "displayName": "Recommended",
                "groups": [
                    { "modelIds": ["gemini-5-omni-high", "gemini-5-omni-medium"] },
                    { "modelIds": ["claude-sonnet-6-0"] }
                ]
            }]
        })
        .to_string();

        // 1. Auto-discover future models in `agentModelSorts` (while skipping hidden `gemini-flash-worker-agy`)
        let added = cat.ingest_live_fetch_response(&live_google_json);
        assert_eq!(added, 3);
        assert!(cat.get_model("gemini-5-omni-high").is_some());
        assert!(cat.get_model("gemini-5-omni-medium").is_some());
        assert_eq!(
            cat.get_model("claude-sonnet-6-0").unwrap().owned_by,
            "anthropic"
        );
        assert!(cat.get_model("gemini-flash-worker-agy").is_none());

        // 2. Native fields preserved both on live account and when injected for a Free account
        let free_account_json = json!({
            "models": {
                "gemini-3.8-flash-medium": { "displayName": "Gemini 3.8 Flash (Medium)", "model": "MODEL_PLACEHOLDER_M72" }
            },
            "agentModelSorts": [{
                "displayName": "Recommended",
                "groups": [{ "modelIds": ["gemini-3.8-flash-medium"] }]
            }]
        })
        .to_string();
        let out_free: Value =
            serde_json::from_str(&cat.inject_models_into_fetch_response(&free_account_json))
                .unwrap();
        assert_eq!(
            out_free["models"]["gemini-5-omni-high"]["model"],
            json!("MODEL_PLACEHOLDER_M999")
        );
        assert_eq!(
            out_free["models"]["gemini-5-omni-high"]["thinkingLevel"],
            json!(3)
        );
        assert_eq!(
            out_free["models"]["gemini-5-omni-high"]["modelExperiments"],
            json!([{ "experimentId": 777 }])
        );

        // 3. Disabling in catalog removes it AND re-ingesting NEVER overwrites `enabled: false`
        assert!(cat.toggle_model("gemini-5-omni-high", false).unwrap());
        assert_eq!(cat.ingest_live_fetch_response(&live_google_json), 0);
        assert!(!cat.get_model("gemini-5-omni-high").unwrap().enabled);

        let out_disabled: Value =
            serde_json::from_str(&cat.inject_models_into_fetch_response(&live_google_json))
                .unwrap();
        assert!(out_disabled["models"].get("gemini-5-omni-high").is_none());
    }

    #[test]
    fn test_external_provider_models_excluded_from_google_injection() {
        let (_dir, cat) = temp_catalog();
        cat.upsert_model(CatalogModelEntry {
            id: "deepseek-v3-external".to_string(),
            display_name: "DeepSeek V3".to_string(),
            enabled: true,
            owned_by: "deepseek".to_string(),
            provider_id: "nvidia_nim".to_string(),
            provider_name: "Nvidia NIM".to_string(),
            target_model: "deepseek-ai/deepseek-v3".to_string(),
            context_window: 131_072,
            max_input_tokens: 131_072,
            max_output_tokens: 8192,
            supports_thinking: false,
            supports_vision: false,
            supports_audio: false,
            supports_video: false,
            supports_function_calling: true,
            input_pricing: 0.0,
            output_pricing: 0.0,
            description: "External".to_string(),
            order: 80,
        })
        .unwrap();

        let out_str = cat.create_synthetic_available_models_response();
        let out: Value = serde_json::from_str(&out_str).unwrap();
        let models = out["models"].as_object().unwrap();
        assert!(!models.contains_key("deepseek-v3-external"));
        assert!(!models.keys().any(|k| k.starts_with("models/")));
    }

    #[test]
    fn test_sync_models_from_accounts_strict_pool_only() {
        let (_dir, cat) = temp_catalog();

        cat.upsert_model(CatalogModelEntry {
            id: "gemini-4-argon-low".to_string(),
            display_name: "Gemini 4 Argon (Low)".to_string(),
            enabled: false,
            owned_by: "google".to_string(),
            provider_id: "default_google_pool".to_string(),
            provider_name: "Google Account Pool".to_string(),
            target_model: "gemini-4-argon-low".to_string(),
            context_window: 1_048_576,
            max_input_tokens: 1_048_576,
            max_output_tokens: 65_536,
            supports_thinking: true,
            supports_vision: true,
            supports_audio: true,
            supports_video: true,
            supports_function_calling: true,
            input_pricing: 0.15,
            output_pricing: 0.60,
            description: "User disabled".to_string(),
            order: 20,
        })
        .unwrap();

        let mock_account_models = vec![
            (
                "gemini-4-carbon-high".to_string(),
                Some("Gemini 4 Carbon (High)".to_string()),
                Some(true),
                Some(true),
            ),
            // Bare family root must be skipped when `-high` exists
            ("gemini-4-carbon".to_string(), None, Some(true), Some(true)),
            // Archive / non-chat / image / tiered / bucket models must be skipped
            ("gemini-3.1-pro-high".to_string(), None, None, None),
            ("gemini-3.1-flash-image".to_string(), None, None, None),
            ("gemini-4-hydrogen-tiered".to_string(), None, None, None),
            ("chat_20706".to_string(), None, None, None),
            ("model-disabled".to_string(), None, None, None),
            ("claude-sonnet-4-6-5h".to_string(), None, None, None),
            (
                "gemini-4-argon-low".to_string(),
                None,
                Some(true),
                Some(true),
            ),
        ];

        let added = cat.sync_from_account_models(mock_account_models).unwrap();
        assert_eq!(added, 1);

        // User's disabled flag preserved
        assert!(!cat.get_model("gemini-4-argon-low").unwrap().enabled);

        // Only owned compliant chat variant added (official_models.json NOT bulk-imported!)
        let carbon = cat.get_model("gemini-4-carbon-high").unwrap();
        assert!(carbon.enabled);
        assert_eq!(carbon.max_output_tokens, 262_144);
        assert!(cat.get_model("gemini-4-carbon").is_none());
        assert!(cat.get_model("gemini-3.1-pro-high").is_none());
        assert!(cat.get_model("gemini-3.1-flash-image").is_none());
        assert!(cat.get_model("gemini-4-hydrogen-tiered").is_none());
        // Unowned model from official_models.json must NOT be imported
        assert!(cat.get_model("claude-opus-5-5-high").is_none());
    }
}
