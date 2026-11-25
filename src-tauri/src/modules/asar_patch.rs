use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
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

static BRIDGE_PORT: AtomicU16 = AtomicU16::new(DEFAULT_BRIDGE_PORT);
static BRIDGE_PORT_LOADED: AtomicBool = AtomicBool::new(false);
static HEARTBEAT_FILE: Lazy<Mutex<Option<File>>> = Lazy::new(|| Mutex::new(None));

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AsarBridgeConfig {
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchResult {
    pub success: bool,
    pub message: String,
    pub patched_count: usize,
    pub asar_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsarStatus {
    pub is_patched: bool,
    pub heartbeat_active: bool,
    pub bridge_port: u16,
    pub asar_paths: Vec<String>,
    pub backup_paths: Vec<String>,
    pub patched_count: usize,
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

    if let Some(path) = bridge_config_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let cfg = AsarBridgeConfig { port };
        let json = serde_json::to_string_pretty(&cfg)
            .map_err(|e| format!("Failed to serialize bridge config: {}", e))?;
        fs::write(&path, json).map_err(|e| format!("Failed to write bridge config: {}", e))?;
    }

    info!("ASAR bridge port updated to {}", port);
    Ok(port)
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

/// Applies the smart dynamic auto-fallback patch that checks `ag_rot.act` in TEMP at runtime.
/// Preserves the exact byte length of the replaced slice by padding with `/*...*/`.
pub fn apply_smart_patch(data: &mut [u8], proxy_port: u16) -> bool {
    let needle_start = b"'--api_server_url'";
    let needle_end = b"pass them when the server is up.";

    let Some(idx1) = find_subslice(data, needle_start) else {
        return false;
    };
    let Some(rel_idx2) = find_subslice(&data[idx1..], needle_end) else {
        return false;
    };

    let end_idx = idx1 + rel_idx2 + needle_end.len();
    let orig_len = end_idx - idx1;
    let target_proxy = format!("http://127.0.0.1:{}", proxy_port);

    let repl_code = format!(
        "'--api_server_url',\n            (fs.existsSync((process.env.TEMP||process.env.TMPDIR||'/tmp')+'/ag_rot.act')?'{}':'https://generativelanguage.googleapis.com'),\n            '--cloud_code_endpoint',\n            (fs.existsSync((process.env.TEMP||process.env.TMPDIR||'/tmp')+'/ag_rot.act')?'{}':'https://daily-cloudcode-pa.googleapis.com'),\n            '--enable_sidecars',\n        ];\n        /*",
        target_proxy, target_proxy
    );

    let Some(pad_needed) = orig_len.checked_sub(repl_code.len() + 2) else {
        return false;
    };

    let mut final_repl = String::with_capacity(orig_len);
    final_repl.push_str(&repl_code);
    for _ in 0..pad_needed {
        final_repl.push('*');
    }
    final_repl.push_str("*/");

    let r_bytes = final_repl.as_bytes();
    if r_bytes.len() != orig_len {
        return false;
    }

    data[idx1..end_idx].copy_from_slice(r_bytes);
    true
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

/// Terminates running Antigravity / Antigravity IDE / language_server processes before patching/restoring.
pub fn terminate_antigravity_processes() {
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
            let exe = proc_info
                .exe()
                .map(|p| p.to_string_lossy().to_lowercase())
                .unwrap_or_default();

            if name.contains("tools")
                || name.contains("manager")
                || exe.contains("antigravity-manager")
            {
                continue;
            }

            if name == "antigravity"
                || name == "antigravity.exe"
                || name == "antigravity ide"
                || name == "antigravity ide.exe"
                || name.contains("language_server")
            {
                let _ = proc_info.kill();
            }
        }
    }
}

fn is_file_patched(asar_path: &Path, proxy_port: u16) -> bool {
    let Ok(bytes) = fs::read(asar_path) else {
        return false;
    };
    let target_proxy = format!("http://127.0.0.1:{}", proxy_port);
    find_subslice(&bytes, target_proxy.as_bytes()).is_some()
        || find_subslice(&bytes, HEARTBEAT_FILENAME.as_bytes()).is_some()
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

    for p in &paths {
        if is_file_patched(p, port) {
            patched_count += 1;
        }
        asar_paths.push(p.to_string_lossy().to_string());
        let backup = PathBuf::from(format!("{}.original", p.display()));
        if backup.exists() {
            backup_paths.push(backup.to_string_lossy().to_string());
        }
    }

    AsarStatus {
        is_patched: patched_count > 0,
        heartbeat_active: is_heartbeat_active(),
        bridge_port: port,
        asar_paths,
        backup_paths,
        patched_count,
    }
}

/// Patches all detected `app.asar` files in-place (preserving exact file size) and starts heartbeat.
pub fn patch_asar_direct(proxy_port: Option<u16>, terminate_processes: bool) -> PatchResult {
    let port = proxy_port.unwrap_or_else(get_asar_bridge_port);

    if terminate_processes {
        terminate_antigravity_processes();
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    let paths = get_all_asar_paths();
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

    for asar in &paths {
        let backup = PathBuf::from(format!("{}.original", asar.display()));
        if !backup.exists() {
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
            Ok(bytes) if verify_asar_header(&bytes) => bytes,
            _ => match fs::read(asar) {
                Ok(bytes) if verify_asar_header(&bytes) => {
                    let _ = fs::copy(asar, &backup);
                    bytes
                }
                _ => {
                    return PatchResult {
                        success: false,
                        message: format!("Invalid ASAR header in {}", asar.display()),
                        patched_count: patched,
                        asar_paths: patched_paths,
                    };
                }
            },
        };

        let original_len = data.len();
        if !apply_smart_patch(&mut data, port) {
            if let Err(e) = apply_fallback_url_patch(&mut data, port) {
                return PatchResult {
                    success: false,
                    message: format!("Failed fallback patch on {}: {}", asar.display(), e),
                    patched_count: patched,
                    asar_paths: patched_paths,
                };
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

        if let Err(e) = fs::write(asar, &data) {
            return PatchResult {
                success: false,
                message: format!("Failed to write patched {}: {}", asar.display(), e),
                patched_count: patched,
                asar_paths: patched_paths,
            };
        }

        patched += 1;
        patched_paths.push(asar.to_string_lossy().to_string());
    }

    start_heartbeat(false);

    PatchResult {
        success: true,
        message: format!(
            "Successfully patched {} app.asar file(s) for 127.0.0.1:{}",
            patched, port
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
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    let paths = get_all_asar_paths();
    if paths.is_empty() {
        return PatchResult {
            success: false,
            message: "No Antigravity app.asar files found.".to_string(),
            patched_count: 0,
            asar_paths: vec![],
        };
    }

    let target_proxy = format!("http://127.0.0.1:{}", port);
    let proxy_pattern = format!("'{}'/*", target_proxy);
    let mut restored = 0usize;
    let mut restored_paths = Vec::new();

    for asar in &paths {
        let backup = PathBuf::from(format!("{}.original", asar.display()));
        let mut backup_restored = false;

        if backup.exists() {
            if let Ok(backup_bytes) = fs::read(&backup) {
                let backup_is_patched = find_subslice(&backup_bytes, proxy_pattern.as_bytes())
                    .is_some()
                    || find_subslice(&backup_bytes, HEARTBEAT_FILENAME.as_bytes()).is_some();

                if !backup_is_patched && verify_asar_header(&backup_bytes) {
                    if let Err(e) = fs::copy(&backup, asar) {
                        return PatchResult {
                            success: false,
                            message: format!("Failed to restore {}: {}", asar.display(), e),
                            patched_count: restored,
                            asar_paths: restored_paths,
                        };
                    }
                    restored += 1;
                    restored_paths.push(asar.to_string_lossy().to_string());
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

            if replacements > 0 {
                if let Err(e) = fs::write(asar, &data) {
                    return PatchResult {
                        success: false,
                        message: format!("Failed to write {}: {}", asar.display(), e),
                        patched_count: restored,
                        asar_paths: restored_paths,
                    };
                }
            }
            restored += 1;
            restored_paths.push(asar.to_string_lossy().to_string());
        }
    }

    stop_heartbeat(false);

    PatchResult {
        success: true,
        message: format!("Restored {} app.asar file(s) to original state", restored),
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
}
