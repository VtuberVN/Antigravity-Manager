use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
#[cfg(not(target_os = "android"))]
use sysinfo::{ProcessesToUpdate, System};
use tracing::{info, warn};

#[cfg(target_os = "windows")]
use std::os::windows::fs::OpenOptionsExt;

pub const DEFAULT_BRIDGE_PORT: u16 = 8989;
const HEARTBEAT_FILENAME: &str = "ag_rot.act";
const BRIDGE_CONFIG_FILENAME: &str = "asar_bridge.json";
const LOCKED_ASAR_MSG: &str = "Không thể ghi đè app.asar do Antigravity IDE đang mở khóa file. Hãy bật 'Tự động đóng Antigravity trước khi Vá / Gỡ vá' rồi thử lại!";

static BRIDGE_PORT: AtomicU16 = AtomicU16::new(DEFAULT_BRIDGE_PORT);
static BRIDGE_PORT_LOADED: AtomicBool = AtomicBool::new(false);
static HEARTBEAT_FILE: Lazy<Mutex<Option<File>>> = Lazy::new(|| Mutex::new(None));

fn default_bridge_flag_true() -> bool {
    true
}

fn default_bridge_port_value() -> u16 {
    DEFAULT_BRIDGE_PORT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AsarBridgeConfig {
    #[serde(default = "default_bridge_port_value")]
    pub port: u16,
    /// Dual-Track (`Claude Lead` / `Gemini Worker`) slot pinning toggle, persisted across restarts.
    #[serde(default = "default_bridge_flag_true")]
    pub dual_track_enabled: bool,
    /// Graceful Equivalent Model Fallback (Carbon/Neon/Argon -> Hydrogen, Claude 5.5 -> 4.6) toggle.
    #[serde(default = "default_bridge_flag_true")]
    pub equivalent_fallback_enabled: bool,
    /// Separate sticky Basic account slot for background `*-flash-lite*` micro-models.
    #[serde(default = "default_bridge_flag_true")]
    pub separate_lite_slot: bool,
    /// Cross-family rescue fallback (Claude/GPT <-> Gemini) when an entire model family is exhausted.
    #[serde(default = "default_bridge_flag_true")]
    pub cross_family_fallback_enabled: bool,
    /// Auto-terminate running Antigravity IDE / language_server processes before patching/restoring.
    #[serde(default = "default_bridge_flag_true")]
    pub terminate_processes: bool,
    /// Auto-bootstrap healthy pool credentials and rescue onboarding RPCs when ASAR bridge is active.
    #[serde(default = "default_bridge_flag_true")]
    pub skip_ide_login_when_asar: bool,
}

impl Default for AsarBridgeConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_BRIDGE_PORT,
            dual_track_enabled: true,
            equivalent_fallback_enabled: true,
            separate_lite_slot: true,
            cross_family_fallback_enabled: true,
            terminate_processes: true,
            skip_ide_login_when_asar: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchResult {
    pub success: bool,
    pub message: String,
    pub patched_count: usize,
    pub asar_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsarFileDetail {
    pub path: String,
    pub size_bytes: u64,
    pub sha256_short: String,
    pub original_sha256_short: Option<String>,
    pub is_patched: bool,
    pub patch_method: Option<String>,
    pub patch_offset: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsarStatus {
    pub is_patched: bool,
    pub heartbeat_active: bool,
    pub bridge_port: u16,
    pub asar_paths: Vec<String>,
    pub backup_paths: Vec<String>,
    pub patched_count: usize,
    pub terminate_processes: bool,
    pub file_details: Vec<AsarFileDetail>,
}

fn bridge_config_path() -> Option<PathBuf> {
    crate::modules::account::get_data_dir()
        .ok()
        .map(|dir| dir.join(BRIDGE_CONFIG_FILENAME))
}

/// Returns the configured ASAR bridge port (default 8989), loading from disk on first call.
pub fn get_asar_bridge_port() -> u16 {
    if !BRIDGE_PORT_LOADED.swap(true, Ordering::SeqCst) {
        if let Some(path) = bridge_config_path() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(cfg) = serde_json::from_str::<AsarBridgeConfig>(&content) {
                    if cfg.port >= 1024 {
                        BRIDGE_PORT.store(cfg.port, Ordering::SeqCst);
                    }
                }
            }
        }
    }
    BRIDGE_PORT.load(Ordering::SeqCst)
}

/// Updates and persists the ASAR bridge port.
pub fn set_asar_bridge_port(port: u16) -> Result<u16, String> {
    if port < 1024 {
        return Err("Bridge port must be between 1024 and 65535".to_string());
    }
    BRIDGE_PORT.store(port, Ordering::SeqCst);
    BRIDGE_PORT_LOADED.store(true, Ordering::SeqCst);

    let mut cfg = load_bridge_config();
    cfg.port = port;
    save_bridge_config(&cfg)?;

    info!("ASAR bridge port updated to {}", port);
    Ok(port)
}

fn load_bridge_config() -> AsarBridgeConfig {
    bridge_config_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|content| serde_json::from_str::<AsarBridgeConfig>(&content).ok())
        .unwrap_or_default()
}

fn save_bridge_config(cfg: &AsarBridgeConfig) -> Result<(), String> {
    if let Some(path) = bridge_config_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let json = serde_json::to_string_pretty(cfg)
            .map_err(|e| format!("Failed to serialize bridge config: {}", e))?;
        fs::write(&path, json).map_err(|e| format!("Failed to write bridge config: {}", e))?;
    }
    Ok(())
}

/// Persisted bridge toggles `(dual_track_enabled, equivalent_fallback_enabled)`; both default to `true`.
pub fn get_bridge_flags() -> (bool, bool) {
    let cfg = load_bridge_config();
    (cfg.dual_track_enabled, cfg.equivalent_fallback_enabled)
}

/// Persist the Dual-Track toggle without touching other bridge settings.
pub fn persist_dual_track_flag(enabled: bool) -> Result<(), String> {
    let mut cfg = load_bridge_config();
    if cfg.dual_track_enabled == enabled {
        return Ok(());
    }
    cfg.dual_track_enabled = enabled;
    save_bridge_config(&cfg)
}

/// Persist the Equivalent Model Fallback toggle without touching other bridge settings.
pub fn persist_equivalent_fallback_flag(enabled: bool) -> Result<(), String> {
    let mut cfg = load_bridge_config();
    if cfg.equivalent_fallback_enabled == enabled {
        return Ok(());
    }
    cfg.equivalent_fallback_enabled = enabled;
    save_bridge_config(&cfg)
}

/// Persisted `separate_lite_slot` toggle; defaults to `true`.
pub fn get_separate_lite_slot_flag() -> bool {
    load_bridge_config().separate_lite_slot
}

/// Persist the `separate_lite_slot` toggle without touching other bridge settings.
pub fn persist_separate_lite_slot_flag(enabled: bool) -> Result<(), String> {
    let mut cfg = load_bridge_config();
    if cfg.separate_lite_slot == enabled {
        return Ok(());
    }
    cfg.separate_lite_slot = enabled;
    save_bridge_config(&cfg)
}

/// Persisted `cross_family_fallback_enabled` toggle; defaults to `true`.
pub fn get_cross_family_fallback_flag() -> bool {
    load_bridge_config().cross_family_fallback_enabled
}

/// Persist the `cross_family_fallback_enabled` toggle without touching other bridge settings.
pub fn persist_cross_family_fallback_flag(enabled: bool) -> Result<(), String> {
    let mut cfg = load_bridge_config();
    if cfg.cross_family_fallback_enabled == enabled {
        return Ok(());
    }
    cfg.cross_family_fallback_enabled = enabled;
    save_bridge_config(&cfg)
}

/// Persisted `terminate_processes` toggle; defaults to `true`.
pub fn get_terminate_processes_flag() -> bool {
    load_bridge_config().terminate_processes
}

/// Persist the `terminate_processes` toggle without touching other bridge settings.
pub fn persist_terminate_processes_flag(enabled: bool) -> Result<(), String> {
    let mut cfg = load_bridge_config();
    if cfg.terminate_processes == enabled {
        return Ok(());
    }
    cfg.terminate_processes = enabled;
    save_bridge_config(&cfg)
}

/// Persisted `skip_ide_login_when_asar` toggle; defaults to `true`.
pub fn get_skip_ide_login_flag() -> bool {
    load_bridge_config().skip_ide_login_when_asar
}

/// Persist the `skip_ide_login_when_asar` toggle without touching other bridge settings.
pub fn persist_skip_ide_login_flag(enabled: bool) -> Result<(), String> {
    let mut cfg = load_bridge_config();
    if cfg.skip_ide_login_when_asar == enabled {
        return Ok(());
    }
    cfg.skip_ide_login_when_asar = enabled;
    save_bridge_config(&cfg)
}

/// Auto-detects all installed Antigravity / Antigravity IDE `resources/app.asar` paths.
pub fn get_all_asar_paths() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "windows")]
    {
        if let Ok(local_app) = std::env::var("LOCALAPPDATA") {
            let base = PathBuf::from(local_app).join("Programs");
            candidates.push(base.join("antigravity").join("resources").join("app.asar"));
            candidates.push(base.join("Antigravity").join("resources").join("app.asar"));
            candidates.push(
                base.join("Antigravity IDE")
                    .join("resources")
                    .join("app.asar"),
            );
            candidates.push(
                base.join("antigravity-ide")
                    .join("resources")
                    .join("app.asar"),
            );
        }
        if let Ok(prog_files) = std::env::var("ProgramFiles") {
            let base = PathBuf::from(prog_files);
            candidates.push(base.join("Antigravity").join("resources").join("app.asar"));
            candidates.push(
                base.join("Antigravity IDE")
                    .join("resources")
                    .join("app.asar"),
            );
        }
        if let Ok(prog_files_x86) = std::env::var("ProgramFiles(x86)") {
            let base = PathBuf::from(prog_files_x86);
            candidates.push(base.join("Antigravity").join("resources").join("app.asar"));
            candidates.push(
                base.join("Antigravity IDE")
                    .join("resources")
                    .join("app.asar"),
            );
        }
    }

    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from(
            "/Applications/Antigravity.app/Contents/Resources/app.asar",
        ));
        candidates.push(PathBuf::from(
            "/Applications/Antigravity IDE.app/Contents/Resources/app.asar",
        ));
        if let Some(home) = dirs::home_dir() {
            candidates.push(
                home.join("Applications")
                    .join("Antigravity.app")
                    .join("Contents")
                    .join("Resources")
                    .join("app.asar"),
            );
            candidates.push(
                home.join("Applications")
                    .join("Antigravity IDE.app")
                    .join("Contents")
                    .join("Resources")
                    .join("app.asar"),
            );
        }
    }

    #[cfg(target_os = "linux")]
    {
        candidates.push(PathBuf::from("/usr/share/antigravity/resources/app.asar"));
        candidates.push(PathBuf::from("/opt/antigravity/resources/app.asar"));
        candidates.push(PathBuf::from("/opt/Antigravity/resources/app.asar"));
        candidates.push(PathBuf::from(
            "/usr/share/antigravity-ide/resources/app.asar",
        ));
        candidates.push(PathBuf::from("/opt/antigravity-ide/resources/app.asar"));
        if let Some(home) = dirs::home_dir() {
            candidates.push(
                home.join(".local")
                    .join("share")
                    .join("antigravity")
                    .join("resources")
                    .join("app.asar"),
            );
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut result: Vec<PathBuf> = Vec::new();

    for path in candidates {
        if path.is_file() {
            let canonical = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            #[cfg(target_os = "windows")]
            let key = canonical.to_string_lossy().to_lowercase();
            #[cfg(not(target_os = "windows"))]
            let key = canonical.to_string_lossy().to_string();

            if seen.insert(key) {
                result.push(path);
            }
        }
    }

    result
}

/// Verifies the ASAR header magic: 4-byte LE uint32 == 4, and byte 16 == '{' (0x7B).
pub fn verify_asar_header(data: &[u8]) -> bool {
    if data.len() < 17 {
        return false;
    }
    let header_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    header_size == 4 && data[16] == 0x7B
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Replaces all occurrences of `target` with `replacement` in-place.
/// Returns an error if `target.len() != replacement.len()`.
pub fn replace_bytes_in_place(
    data: &mut [u8],
    target: &[u8],
    replacement: &[u8],
) -> Result<usize, String> {
    if target.is_empty() {
        return Ok(0);
    }
    if target.len() != replacement.len() {
        return Err(format!(
            "Length mismatch: target is {} bytes, replacement is {} bytes",
            target.len(),
            replacement.len()
        ));
    }

    let mut match_count = 0usize;
    let mut offset = 0usize;

    while offset + target.len() <= data.len() {
        if let Some(rel_idx) = find_subslice(&data[offset..], target) {
            let abs_idx = offset + rel_idx;
            data[abs_idx..abs_idx + replacement.len()].copy_from_slice(replacement);
            match_count += 1;
            offset = abs_idx + replacement.len();
        } else {
            break;
        }
    }

    Ok(match_count)
}

fn pad_js_comment_block(code_with_open_comment: &str, orig_len: usize) -> Option<Vec<u8>> {
    let pad_needed = orig_len.checked_sub(code_with_open_comment.len() + 2)?;
    let mut out = String::with_capacity(orig_len);
    out.push_str(code_with_open_comment);
    for _ in 0..pad_needed {
        out.push('*');
    }
    out.push_str("*/");
    let bytes = out.into_bytes();
    (bytes.len() == orig_len).then_some(bytes)
}

/// Applies the smart dynamic auto-fallback patch that checks `ag_rot.act` in TEMP at runtime.
/// Supports both legacy `baseLsArgs` array layout and Antigravity 2.35.1 `LS_API_SERVER_URL` layout.
/// Preserves the exact byte length of the replaced slice by padding with `/*...*/`.
pub fn apply_smart_patch(data: &mut [u8], proxy_port: u16) -> bool {
    let target_proxy = format!("http://127.0.0.1:{}", proxy_port);

    // 1. Legacy layout (`'--api_server_url' ... pass them when the server is up.`)
    let legacy_start = b"'--api_server_url'";
    let legacy_end = b"pass them when the server is up.";
    if let Some(idx1) = find_subslice(data, legacy_start) {
        if let Some(rel_idx2) = find_subslice(&data[idx1..], legacy_end) {
            let end_idx = idx1 + rel_idx2 + legacy_end.len();
            let orig_len = end_idx - idx1;
            let repl_code = format!(
                "'--api_server_url',\n            (fs.existsSync((process.env.TEMP||process.env.TMPDIR||'/tmp')+'/ag_rot.act')?'{}':'https://generativelanguage.googleapis.com'),\n            '--cloud_code_endpoint',\n            (fs.existsSync((process.env.TEMP||process.env.TMPDIR||'/tmp')+'/ag_rot.act')?'{}':'https://daily-cloudcode-pa.googleapis.com'),\n            '--enable_sidecars',\n        ];\n        /*",
                target_proxy, target_proxy
            );
            if let Some(r_bytes) = pad_js_comment_block(&repl_code, orig_len) {
                data[idx1..end_idx].copy_from_slice(&r_bytes);
                return true;
            }
        }
    }

    // 2. Antigravity 2.35.1 layout (`// Every LS's base flags... exports.LS_CLOUD_CODE_ENDPOINT = ...;`)
    let v235_start = b"// Every LS's base flags. No Electron imports: pure modules use this file.";
    let v235_end = b"exports.LS_CLOUD_CODE_ENDPOINT = 'https://daily-cloudcode-pa.googleapis.com';";
    if let Some(idx1) = find_subslice(data, v235_start) {
        if let Some(rel_idx2) = find_subslice(&data[idx1..], v235_end) {
            let end_idx = idx1 + rel_idx2 + v235_end.len();
            let orig_len = end_idx - idx1;
            let repl_code = format!(
                "Object.defineProperty(exports,\"__esModule\",{{value:true}});exports.baseLsArgs=baseLsArgs;var _e=process.env,_u=require(\"fs\").existsSync((_e.TEMP||_e.TMPDIR||\"/tmp\")+\"/ag_rot.act\")?\"{}\":\"\";exports.LS_API_SERVER_URL=_u||\"https://generativelanguage.googleapis.com\";exports.LS_CLOUD_CODE_ENDPOINT=_u||\"https://daily-cloudcode-pa.googleapis.com\";/*",
                target_proxy
            );
            if let Some(r_bytes) = pad_js_comment_block(&repl_code, orig_len) {
                data[idx1..end_idx].copy_from_slice(&r_bytes);
                return true;
            }
        }
    }

    false
}

fn pad_proxy_replacement(target_proxy: &str, total_len: usize) -> Result<String, String> {
    let prefix = format!("'{}'/*", target_proxy);
    let min_len = prefix.len() + 2;
    if total_len < min_len {
        return Err(format!(
            "Target length {} is too short for proxy URL {}",
            total_len, target_proxy
        ));
    }
    let spaces = total_len - prefix.len() - 2;
    Ok(format!("{}{}*/", prefix, " ".repeat(spaces)))
}

/// Applies fallback static URL replacement in-place with exact byte-length padding.
pub fn apply_fallback_url_patch(data: &mut [u8], proxy_port: u16) -> Result<usize, String> {
    let target_proxy = format!("http://127.0.0.1:{}", proxy_port);
    let targets = [
        (
            "'https://generativelanguage.googleapis.com'",
            pad_proxy_replacement(&target_proxy, 43)?,
        ),
        (
            "'https://daily-cloudcode-pa.googleapis.com'",
            pad_proxy_replacement(&target_proxy, 43)?,
        ),
        (
            "'https://cloudcode-pa.googleapis.com'",
            pad_proxy_replacement(&target_proxy, 37)?,
        ),
    ];

    let mut total = 0usize;
    for (target_str, repl_str) in targets {
        total += replace_bytes_in_place(data, target_str.as_bytes(), repl_str.as_bytes())?;
    }
    Ok(total)
}

pub fn heartbeat_file_path() -> PathBuf {
    std::env::temp_dir().join(HEARTBEAT_FILENAME)
}

/// Starts the `ag_rot.act` heartbeat lock file in temp dir.
/// On Windows, uses `FILE_FLAG_DELETE_ON_CLOSE` so it is automatically cleaned up on crash/exit.
pub fn start_heartbeat(hot_switch: bool) {
    {
        let mut guard = HEARTBEAT_FILE.lock();
        if guard.is_none() {
            let path = heartbeat_file_path();
            let mut opts = OpenOptions::new();
            opts.read(true).write(true).create(true).truncate(true);

            #[cfg(target_os = "windows")]
            {
                const FILE_SHARE_READ: u32 = 0x00000001;
                const FILE_SHARE_WRITE: u32 = 0x00000002;
                const FILE_SHARE_DELETE: u32 = 0x00000004;
                const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x04000000;
                opts.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
                opts.custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
            }

            match opts.open(&path) {
                Ok(file) => {
                    *guard = Some(file);
                    info!("ASAR heartbeat started at {}", path.display());
                }
                Err(e) => {
                    warn!(
                        "Failed to create ASAR heartbeat file {}: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
    }

    if hot_switch {
        hot_switch_running_antigravity();
    }
}

/// Stops and removes the `ag_rot.act` heartbeat file.
pub fn stop_heartbeat(hot_switch: bool) {
    {
        let mut guard = HEARTBEAT_FILE.lock();
        *guard = None;
    }
    let path = heartbeat_file_path();
    if path.exists() {
        let _ = fs::remove_file(&path);
    }
    info!("ASAR heartbeat stopped");

    if hot_switch {
        hot_switch_running_antigravity();
    }
}

pub fn is_heartbeat_active() -> bool {
    HEARTBEAT_FILE.lock().is_some() || heartbeat_file_path().exists()
}

/// Kills running `language_server` processes so Antigravity respawns them immediately
/// without reloading the IDE window.
pub fn hot_switch_running_antigravity() {
    #[cfg(not(target_os = "android"))]
    {
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::All);
        let my_pid = std::process::id();

        for (pid, proc_info) in sys.processes() {
            if pid.as_u32() == my_pid {
                continue;
            }
            let name = proc_info.name().to_string_lossy().to_lowercase();
            if name.contains("language_server") {
                let _ = proc_info.kill();
            }
        }
    }
}

/// Kills running `language_server` processes ONLY if their command line does NOT already
/// point to `127.0.0.1:{port}` (e.g. spawned while Manager was closed).
pub fn hot_switch_if_unbridged(port: u16) {
    #[cfg(not(target_os = "android"))]
    {
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::All);
        let my_pid = std::process::id();
        let expected_bridge = format!("127.0.0.1:{}", port);
        let mut killed = 0usize;

        for (pid, proc_info) in sys.processes() {
            if pid.as_u32() == my_pid {
                continue;
            }
            let name = proc_info.name().to_string_lossy().to_lowercase();
            if name.contains("language_server") {
                let cmdline = proc_info
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !cmdline.contains(&expected_bridge) && proc_info.kill() {
                    killed += 1;
                }
            }
        }
        if killed > 0 {
            tracing::info!(
                "[ASAR-Patch] Hot-switched {} unbridged language_server process(es) onto {}",
                killed,
                expected_bridge
            );
        }
    }
}

#[cfg(not(target_os = "android"))]
fn is_target_antigravity_process(name: &str, exe: &str) -> bool {
    if name.contains("tools") || name.contains("manager") || exe.contains("antigravity-manager") {
        return false;
    }
    name == "antigravity"
        || name == "antigravity.exe"
        || name == "antigravity ide"
        || name == "antigravity ide.exe"
        || name.contains("language_server")
}

/// Terminates running Antigravity / Antigravity IDE / language_server processes before patching/restoring
/// and waits up to 3000ms (15 x 200ms) for them to exit so Windows releases file locks on `app.asar`.
pub fn terminate_antigravity_processes() {
    #[cfg(not(target_os = "android"))]
    {
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::All);
        let my_pid = std::process::id();
        let mut killed_any = false;

        for (pid, proc_info) in sys.processes() {
            if pid.as_u32() == my_pid {
                continue;
            }
            let name = proc_info.name().to_string_lossy().to_lowercase();
            let exe = proc_info
                .exe()
                .map(|p| p.to_string_lossy().to_lowercase())
                .unwrap_or_default();

            if is_target_antigravity_process(&name, &exe) && proc_info.kill() {
                killed_any = true;
            }
        }

        if killed_any {
            for _ in 0..15 {
                std::thread::sleep(std::time::Duration::from_millis(200));
                sys.refresh_processes(ProcessesToUpdate::All);
                let any_alive = sys.processes().iter().any(|(pid, proc_info)| {
                    if pid.as_u32() == my_pid {
                        return false;
                    }
                    let name = proc_info.name().to_string_lossy().to_lowercase();
                    let exe = proc_info
                        .exe()
                        .map(|p| p.to_string_lossy().to_lowercase())
                        .unwrap_or_default();
                    is_target_antigravity_process(&name, &exe)
                });
                if !any_alive {
                    break;
                }
            }
        }
    }
}

fn sha256_short(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest[..6].iter().map(|b| format!("{:02x}", b)).collect()
}

fn find_any_static_patch_offset(bytes: &[u8]) -> Option<usize> {
    const PREFIX: &[u8] = b"'http://127.0.0.1:";
    let mut offset = 0usize;
    while offset + PREFIX.len() + 4 <= bytes.len() {
        let Some(rel) = find_subslice(&bytes[offset..], PREFIX) else {
            break;
        };
        let abs = offset + rel;
        let after = &bytes[abs + PREFIX.len()..];
        let digits = after.iter().take_while(|b| b.is_ascii_digit()).count();
        if (1..=5).contains(&digits) && after[digits..].starts_with(b"'/*") {
            return Some(abs);
        }
        offset = abs + PREFIX.len();
    }
    None
}

fn detect_patch_info(bytes: &[u8], proxy_port: u16) -> (bool, Option<String>, Option<usize>) {
    if let Some(offset) = find_subslice(bytes, HEARTBEAT_FILENAME.as_bytes()) {
        return (true, Some("smart_heartbeat".to_string()), Some(offset));
    }
    let port_pattern = format!("'http://127.0.0.1:{}'/*", proxy_port);
    if let Some(offset) = find_subslice(bytes, port_pattern.as_bytes())
        .or_else(|| find_any_static_patch_offset(bytes))
    {
        return (true, Some("static_url".to_string()), Some(offset));
    }
    (false, None, None)
}

pub fn is_any_asar_patched_bytes(bytes: &[u8]) -> bool {
    find_subslice(bytes, HEARTBEAT_FILENAME.as_bytes()).is_some()
        || find_any_static_patch_offset(bytes).is_some()
}

fn is_file_patched(asar_path: &Path, proxy_port: u16) -> bool {
    let Ok(bytes) = fs::read(asar_path) else {
        return false;
    };
    detect_patch_info(&bytes, proxy_port).0
}

fn build_file_detail(asar_path: &Path, proxy_port: u16) -> AsarFileDetail {
    let disk_bytes = fs::read(asar_path).unwrap_or_default();
    let size_bytes = disk_bytes.len() as u64;
    let sha_short = if disk_bytes.is_empty() {
        String::new()
    } else {
        sha256_short(&disk_bytes)
    };
    let (is_patched, patch_method, patch_offset) = detect_patch_info(&disk_bytes, proxy_port);

    let backup_path = PathBuf::from(format!("{}.original", asar_path.display()));
    let original_sha256_short = fs::read(&backup_path)
        .ok()
        .filter(|b| !b.is_empty())
        .map(|b| sha256_short(&b));

    AsarFileDetail {
        path: asar_path.to_string_lossy().to_string(),
        size_bytes,
        sha256_short: sha_short,
        original_sha256_short,
        is_patched,
        patch_method,
        patch_offset,
    }
}

fn format_write_error(asar: &Path, err: &std::io::Error) -> String {
    #[cfg(target_os = "windows")]
    if err.raw_os_error() == Some(32) || err.kind() == std::io::ErrorKind::PermissionDenied {
        return format!("{} ({}: {})", LOCKED_ASAR_MSG, asar.display(), err);
    }
    #[cfg(not(target_os = "windows"))]
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        return format!("{} ({}: {})", LOCKED_ASAR_MSG, asar.display(), err);
    }
    format!("Failed to write {}: {}", asar.display(), err)
}

fn write_asar_sync(asar: &Path, data: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(asar)
        .map_err(|e| format_write_error(asar, &e))?;
    file.write_all(data)
        .map_err(|e| format_write_error(asar, &e))?;
    file.sync_all().map_err(|e| format_write_error(asar, &e))?;
    Ok(())
}

/// Returns true if any detected `app.asar` contains the ASAR bridge patch.
pub fn is_patched(proxy_port: Option<u16>) -> bool {
    let port = proxy_port.unwrap_or_else(get_asar_bridge_port);
    get_all_asar_paths()
        .iter()
        .any(|p| is_file_patched(p, port))
}

/// Returns full ASAR status for the UI and bridge startup checks.
pub fn get_asar_status() -> AsarStatus {
    let port = get_asar_bridge_port();
    let paths = get_all_asar_paths();
    let mut patched_count = 0usize;
    let mut asar_paths = Vec::with_capacity(paths.len());
    let mut backup_paths = Vec::new();
    let mut file_details = Vec::with_capacity(paths.len());

    for p in &paths {
        let detail = build_file_detail(p, port);
        if detail.is_patched {
            patched_count += 1;
        }
        asar_paths.push(p.to_string_lossy().to_string());
        let backup = PathBuf::from(format!("{}.original", p.display()));
        if backup.exists() {
            backup_paths.push(backup.to_string_lossy().to_string());
        }
        file_details.push(detail);
    }

    AsarStatus {
        is_patched: patched_count > 0,
        heartbeat_active: is_heartbeat_active(),
        bridge_port: port,
        asar_paths,
        backup_paths,
        patched_count,
        terminate_processes: get_terminate_processes_flag(),
        file_details,
    }
}

/// Patches all detected `app.asar` files in-place (preserving exact file size) and starts heartbeat.
pub fn patch_asar_direct(proxy_port: Option<u16>, terminate_processes: bool) -> PatchResult {
    let port = proxy_port.unwrap_or_else(get_asar_bridge_port);

    if terminate_processes {
        terminate_antigravity_processes();
    }

    let paths = get_all_asar_paths();
    let res = patch_asar_paths(&paths, port);
    if res.success {
        start_heartbeat(false);
    }
    res
}

fn patch_asar_paths(paths: &[PathBuf], port: u16) -> PatchResult {
    if paths.is_empty() {
        return PatchResult {
            success: false,
            message: "No Antigravity app.asar files found.".to_string(),
            patched_count: 0,
            asar_paths: vec![],
        };
    }

    let mut patched = 0usize;
    let mut patched_paths = Vec::new();
    let mut summaries = Vec::new();
    let target_proxy = format!("http://127.0.0.1:{}", port);

    for asar in paths {
        let backup = PathBuf::from(format!("{}.original", asar.display()));
        let current_bytes = match fs::read(asar) {
            Ok(bytes) => bytes,
            Err(e) => {
                return PatchResult {
                    success: false,
                    message: format!("Failed to read {}: {}", asar.display(), e),
                    patched_count: patched,
                    asar_paths: patched_paths,
                };
            }
        };

        if !backup.exists() && !is_any_asar_patched_bytes(&current_bytes) {
            if let Err(e) = fs::copy(asar, &backup) {
                return PatchResult {
                    success: false,
                    message: format!("Failed to create backup {}: {}", backup.display(), e),
                    patched_count: patched,
                    asar_paths: patched_paths,
                };
            }
        }

        let mut data = match fs::read(&backup) {
            Ok(bytes) if verify_asar_header(&bytes) && !is_any_asar_patched_bytes(&bytes) => bytes,
            _ => {
                if verify_asar_header(&current_bytes) {
                    if !is_any_asar_patched_bytes(&current_bytes) && !backup.exists() {
                        let _ = fs::copy(asar, &backup);
                    }
                    current_bytes
                } else {
                    return PatchResult {
                        success: false,
                        message: format!("Invalid ASAR header in {}", asar.display()),
                        patched_count: patched,
                        asar_paths: patched_paths,
                    };
                }
            }
        };

        let original_len = data.len();
        if !apply_smart_patch(&mut data, port) {
            match apply_fallback_url_patch(&mut data, port) {
                Ok(n) if n > 0 => {}
                Ok(_) => {
                    if !is_any_asar_patched_bytes(&data) {
                        return PatchResult {
                            success: false,
                            message: format!("No patchable endpoints found in {}", asar.display()),
                            patched_count: patched,
                            asar_paths: patched_paths,
                        };
                    }
                }
                Err(e) => {
                    return PatchResult {
                        success: false,
                        message: format!("Failed fallback patch on {}: {}", asar.display(), e),
                        patched_count: patched,
                        asar_paths: patched_paths,
                    };
                }
            }
        }

        if data.len() != original_len {
            return PatchResult {
                success: false,
                message: format!(
                    "Critical ASAR size mismatch: {} != {}",
                    data.len(),
                    original_len
                ),
                patched_count: patched,
                asar_paths: patched_paths,
            };
        }

        if let Err(msg) = write_asar_sync(asar, &data) {
            return PatchResult {
                success: false,
                message: msg,
                patched_count: patched,
                asar_paths: patched_paths,
            };
        }

        // Read-back verification from disk after sync_all()
        let disk_bytes = match fs::read(asar) {
            Ok(b) => b,
            Err(e) => {
                return PatchResult {
                    success: false,
                    message: format!("Failed read-back verification on {}: {}", asar.display(), e),
                    patched_count: patched,
                    asar_paths: patched_paths,
                };
            }
        };
        let (is_patched_on_disk, method_opt, offset_opt) = detect_patch_info(&disk_bytes, port);
        let has_target_proxy = find_subslice(&disk_bytes, target_proxy.as_bytes()).is_some();
        if !is_patched_on_disk || !has_target_proxy {
            return PatchResult {
                success: false,
                message: format!(
                    "Read-back verification failed for {}: patch markers not found on disk",
                    asar.display()
                ),
                patched_count: patched,
                asar_paths: patched_paths,
            };
        }

        let sha_short = sha256_short(&disk_bytes);
        let offset_str = offset_opt
            .map(|o| format!("0x{:x}", o))
            .unwrap_or_else(|| "n/a".to_string());
        let method_str = method_opt.unwrap_or_else(|| "unknown".to_string());
        summaries.push(format!(
            "{} (SHA256: {}, {}, offset: {})",
            asar.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| asar.display().to_string()),
            sha_short,
            method_str,
            offset_str
        ));

        patched += 1;
        patched_paths.push(asar.to_string_lossy().to_string());
    }

    PatchResult {
        success: true,
        message: format!(
            "Successfully patched {} app.asar file(s) for 127.0.0.1:{} [{}]",
            patched,
            port,
            summaries.join("; ")
        ),
        patched_count: patched,
        asar_paths: patched_paths,
    }
}

/// Restores all detected `app.asar` files from `.original` backups (or in-place unpatch) and stops heartbeat.
pub fn restore_original_asar(proxy_port: Option<u16>, terminate_processes: bool) -> PatchResult {
    let port = proxy_port.unwrap_or_else(get_asar_bridge_port);

    if terminate_processes {
        terminate_antigravity_processes();
    }

    let paths = get_all_asar_paths();
    let res = restore_asar_paths(&paths, port);
    if res.success {
        stop_heartbeat(false);
    }
    res
}

fn restore_asar_paths(paths: &[PathBuf], port: u16) -> PatchResult {
    if paths.is_empty() {
        return PatchResult {
            success: false,
            message: "No Antigravity app.asar files found.".to_string(),
            patched_count: 0,
            asar_paths: vec![],
        };
    }

    let target_proxy = format!("http://127.0.0.1:{}", port);
    let mut restored = 0usize;
    let mut restored_paths = Vec::new();
    let mut summaries = Vec::new();

    for asar in paths {
        let backup = PathBuf::from(format!("{}.original", asar.display()));
        let mut backup_restored = false;
        let mut expected_backup_sha: Option<String> = None;

        if backup.exists() {
            if let Ok(backup_bytes) = fs::read(&backup) {
                if !is_any_asar_patched_bytes(&backup_bytes) && verify_asar_header(&backup_bytes) {
                    if let Err(msg) = write_asar_sync(asar, &backup_bytes) {
                        return PatchResult {
                            success: false,
                            message: msg,
                            patched_count: restored,
                            asar_paths: restored_paths,
                        };
                    }
                    expected_backup_sha = Some(sha256_short(&backup_bytes));
                    backup_restored = true;
                }
            }
        }

        if !backup_restored {
            let mut data = match fs::read(asar) {
                Ok(b) => b,
                Err(e) => {
                    return PatchResult {
                        success: false,
                        message: format!("Failed to read {}: {}", asar.display(), e),
                        patched_count: restored,
                        asar_paths: restored_paths,
                    };
                }
            };

            let targets = [
                (
                    pad_proxy_replacement(&target_proxy, 43).unwrap_or_default(),
                    "'https://generativelanguage.googleapis.com'",
                ),
                (
                    pad_proxy_replacement(&target_proxy, 43).unwrap_or_default(),
                    "'https://daily-cloudcode-pa.googleapis.com'",
                ),
                (
                    pad_proxy_replacement(&target_proxy, 37).unwrap_or_default(),
                    "'https://cloudcode-pa.googleapis.com'",
                ),
            ];

            let mut replacements = 0usize;
            for (t_str, r_str) in targets {
                if t_str.len() == r_str.len() {
                    replacements +=
                        replace_bytes_in_place(&mut data, t_str.as_bytes(), r_str.as_bytes())
                            .unwrap_or(0);
                }
            }

            if replacements == 0 && is_any_asar_patched_bytes(&data) {
                return PatchResult {
                    success: false,
                    message: format!(
                        "Cannot unpatch {}: .original backup is missing/invalid and smart_heartbeat patch cannot be reverted in-place.",
                        asar.display()
                    ),
                    patched_count: restored,
                    asar_paths: restored_paths,
                };
            }

            if replacements > 0 {
                if let Err(msg) = write_asar_sync(asar, &data) {
                    return PatchResult {
                        success: false,
                        message: msg,
                        patched_count: restored,
                        asar_paths: restored_paths,
                    };
                }
            }
        }

        // Read-back verification from disk after sync_all()
        let disk_bytes = match fs::read(asar) {
            Ok(b) => b,
            Err(e) => {
                return PatchResult {
                    success: false,
                    message: format!(
                        "Failed read-back verification after restore on {}: {}",
                        asar.display(),
                        e
                    ),
                    patched_count: restored,
                    asar_paths: restored_paths,
                };
            }
        };

        if is_any_asar_patched_bytes(&disk_bytes) {
            return PatchResult {
                success: false,
                message: format!(
                    "Read-back verification failed for {}: patch markers still remain on disk.",
                    asar.display()
                ),
                patched_count: restored,
                asar_paths: restored_paths,
            };
        }

        let disk_sha = sha256_short(&disk_bytes);
        if let Some(ref orig_sha) = expected_backup_sha {
            if &disk_sha != orig_sha {
                return PatchResult {
                    success: false,
                    message: format!(
                        "Read-back SHA256 mismatch on {}: disk {} != .original {}",
                        asar.display(),
                        disk_sha,
                        orig_sha
                    ),
                    patched_count: restored,
                    asar_paths: restored_paths,
                };
            }
            summaries.push(format!(
                "{} (SHA256: {} == .original)",
                asar.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| asar.display().to_string()),
                disk_sha
            ));
        } else {
            summaries.push(format!(
                "{} (SHA256: {})",
                asar.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| asar.display().to_string()),
                disk_sha
            ));
        }

        restored += 1;
        restored_paths.push(asar.to_string_lossy().to_string());
    }

    PatchResult {
        success: true,
        message: format!(
            "Restored {} app.asar file(s) to original state [{}]",
            restored,
            summaries.join("; ")
        ),
        patched_count: restored,
        asar_paths: restored_paths,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replace_bytes_in_place_exact_length() {
        let mut buf = b"hello 'https://cloudcode-pa.googleapis.com' world 'https://cloudcode-pa.googleapis.com'!".to_vec();
        let orig_len = buf.len();
        let target = b"'https://cloudcode-pa.googleapis.com'";
        let repl = pad_proxy_replacement("http://127.0.0.1:8989", 37).unwrap();
        assert_eq!(repl.len(), target.len());

        let count = replace_bytes_in_place(&mut buf, target, repl.as_bytes()).unwrap();
        assert_eq!(count, 2);
        assert_eq!(buf.len(), orig_len);
        assert!(find_subslice(&buf, b"http://127.0.0.1:8989").is_some());
        assert!(find_subslice(&buf, target).is_none());
    }

    #[test]
    fn test_replace_bytes_in_place_rejects_length_mismatch() {
        let mut buf = b"abcdef".to_vec();
        let err = replace_bytes_in_place(&mut buf, b"abc", b"abcd").unwrap_err();
        assert!(err.contains("Length mismatch"));
    }

    #[test]
    fn test_smart_patch_preserves_exact_length() {
        let prefix = b"const args = [\n            ";
        let middle = b"'--api_server_url',\n            'https://generativelanguage.googleapis.com',\n            '--cloud_code_endpoint',\n            'https://daily-cloudcode-pa.googleapis.com',\n            '--enable_sidecars',\n            '--extension_server_port',\n            String(port),\n            '--csrf_token',\n            csrfToken,\n        ];\n        // Additional flags and comments: 0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789\n        // pass them when the server is up.";
        let suffix = b"\n        return args;";

        let mut data = Vec::new();
        data.extend_from_slice(prefix);
        data.extend_from_slice(middle);
        data.extend_from_slice(suffix);
        let orig_len = data.len();

        let applied = apply_smart_patch(&mut data, 8989);
        assert!(applied);
        assert_eq!(data.len(), orig_len);
        assert!(data.starts_with(prefix));
        assert!(data.ends_with(suffix));
        assert!(find_subslice(&data, b"ag_rot.act").is_some());
        assert!(find_subslice(&data, b"http://127.0.0.1:8989").is_some());
        assert!(find_subslice(&data, b"*/").is_some());
    }

    #[test]
    fn test_fallback_url_patch_preserves_length() {
        let mut data = b"url1='https://generativelanguage.googleapis.com';url2='https://daily-cloudcode-pa.googleapis.com';url3='https://cloudcode-pa.googleapis.com';".to_vec();
        let orig_len = data.len();
        let count = apply_fallback_url_patch(&mut data, 18989).unwrap();
        assert_eq!(count, 3);
        assert_eq!(data.len(), orig_len);
    }

    #[test]
    fn test_verify_asar_header() {
        let mut valid = vec![0u8; 20];
        valid[0] = 4;
        valid[16] = b'{';
        assert!(verify_asar_header(&valid));

        valid[16] = b'[';
        assert!(!verify_asar_header(&valid));
        assert!(!verify_asar_header(&[4, 0, 0, 0]));
    }

    fn make_synthetic_asar_bytes() -> Vec<u8> {
        let mut data = vec![0u8; 20];
        data[0] = 4;
        data[16] = b'{';
        let body = b"\"files\":{}}\nconst HOLE_PROXY_RULES = 'http://127.0.0.1:9';\nconst args = ['--api_server_url',\n            'https://generativelanguage.googleapis.com',\n            '--cloud_code_endpoint',\n            'https://daily-cloudcode-pa.googleapis.com',\n            '--enable_sidecars',\n            '--extension_server_port',\n            String(port),\n            '--csrf_token',\n            csrfToken,\n        ];\n        // Additional flags and comments: 0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789\n        // pass them when the server is up.";
        data.extend_from_slice(body);
        data
    }

    #[test]
    fn test_patch_and_restore_roundtrip_verifies_sha256_and_offset() {
        let dir = tempfile::TempDir::new().unwrap();
        let asar = dir.path().join("app.asar");
        let original_bytes = make_synthetic_asar_bytes();
        let original_sha = sha256_short(&original_bytes);
        fs::write(&asar, &original_bytes).unwrap();

        // Original blackhole rule must NOT trigger false-positive patch detection
        assert!(!is_any_asar_patched_bytes(&original_bytes));

        let patch_res = patch_asar_paths(std::slice::from_ref(&asar), 8989);
        assert!(patch_res.success, "patch failed: {}", patch_res.message);
        assert_eq!(patch_res.patched_count, 1);
        assert!(patch_res.message.contains("SHA256:"));
        assert!(patch_res.message.contains("offset: 0x"));

        let detail_after_patch = build_file_detail(&asar, 8989);
        assert!(detail_after_patch.is_patched);
        assert_eq!(
            detail_after_patch.patch_method.as_deref(),
            Some("smart_heartbeat")
        );
        assert!(detail_after_patch.patch_offset.is_some());
        assert_ne!(detail_after_patch.sha256_short, original_sha);
        assert_eq!(
            detail_after_patch.original_sha256_short.as_deref(),
            Some(original_sha.as_str())
        );

        let restore_res = restore_asar_paths(std::slice::from_ref(&asar), 8989);
        assert!(
            restore_res.success,
            "restore failed: {}",
            restore_res.message
        );
        assert!(restore_res.message.contains(&original_sha));
        assert!(restore_res.message.contains("== .original"));

        let disk_after_restore = fs::read(&asar).unwrap();
        assert_eq!(disk_after_restore, original_bytes);
        assert_eq!(sha256_short(&disk_after_restore), original_sha);
        let detail_after_restore = build_file_detail(&asar, 8989);
        assert!(!detail_after_restore.is_patched);
    }

    #[test]
    fn test_restore_smart_patch_without_original_returns_failure() {
        let dir = tempfile::TempDir::new().unwrap();
        let asar = dir.path().join("app.asar");
        let original_bytes = make_synthetic_asar_bytes();
        fs::write(&asar, &original_bytes).unwrap();

        let patch_res = patch_asar_paths(std::slice::from_ref(&asar), 8989);
        assert!(patch_res.success);

        // Remove .original backup so restore cannot use backup
        let backup = dir.path().join("app.asar.original");
        fs::remove_file(&backup).unwrap();

        let restore_res = restore_asar_paths(std::slice::from_ref(&asar), 8989);
        assert!(
            !restore_res.success,
            "restore without .original on smart_heartbeat must fail instead of faking success"
        );
        assert!(restore_res.message.contains("smart_heartbeat"));
        assert!(is_file_patched(&asar, 8989));
    }

    #[test]
    fn test_any_static_patch_detection_ignores_blackhole_proxy_rule() {
        let clean = b"const HOLE_PROXY_RULES = 'http://127.0.0.1:9';";
        assert!(!is_any_asar_patched_bytes(clean));

        let patched = b"const url = 'http://127.0.0.1:8989'/*           */;";
        assert!(is_any_asar_patched_bytes(patched));
    }

    #[test]
    fn test_smart_patch_supports_antigravity_2_35_1_layout() {
        let prefix = b"\"use strict\";\n";
        let middle = b"// Every LS's base flags. No Electron imports: pure modules use this file.\nObject.defineProperty(exports, \"__esModule\", { value: true });\nexports.LS_CLOUD_CODE_ENDPOINT = exports.LS_API_SERVER_URL = void 0;\nexports.baseLsArgs = baseLsArgs;\nexports.LS_API_SERVER_URL = 'https://generativelanguage.googleapis.com';\nexports.LS_CLOUD_CODE_ENDPOINT = 'https://daily-cloudcode-pa.googleapis.com';";
        let suffix = b"\nfunction baseLsArgs(opts) { return [exports.LS_API_SERVER_URL, exports.LS_CLOUD_CODE_ENDPOINT]; }";

        let mut data = Vec::new();
        data.extend_from_slice(prefix);
        data.extend_from_slice(middle);
        data.extend_from_slice(suffix);
        let orig_len = data.len();

        let applied = apply_smart_patch(&mut data, 8989);
        assert!(applied, "2.35.1 smart patch must succeed");
        assert_eq!(data.len(), orig_len);
        assert!(data.starts_with(prefix));
        assert!(data.ends_with(suffix));
        assert!(find_subslice(&data, b"ag_rot.act").is_some());
        assert!(find_subslice(&data, b"http://127.0.0.1:8989").is_some());
        assert!(find_subslice(&data, b"exports.LS_API_SERVER_URL=").is_some());
        assert!(find_subslice(&data, b"exports.LS_CLOUD_CODE_ENDPOINT=").is_some());
        assert!(is_any_asar_patched_bytes(&data));
    }
}
