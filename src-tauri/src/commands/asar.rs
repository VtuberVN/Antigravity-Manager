use crate::modules::asar_patch::{self, AsarStatus, PatchResult};
use crate::proxy::asar_bridge::{self, AsarBridgeStatus, AsarTelemetrySnapshot};
use crate::proxy::asar_model_catalog::{self, CatalogModelEntry};
use tauri::State;

#[tauri::command]
pub async fn get_asar_status() -> Result<AsarStatus, String> {
    tokio::task::spawn_blocking(asar_patch::get_asar_status)
        .await
        .map_err(|e| format!("Task panicked: {}", e))
}

#[tauri::command]
pub async fn patch_asar(
    port: Option<u16>,
    terminate_processes: Option<bool>,
) -> Result<PatchResult, String> {
    let terminate = terminate_processes.unwrap_or(true);
    tokio::task::spawn_blocking(move || asar_patch::patch_asar_direct(port, terminate))
        .await
        .map_err(|e| format!("Task panicked: {}", e))
}

#[tauri::command]
pub async fn unpatch_asar(
    port: Option<u16>,
    terminate_processes: Option<bool>,
) -> Result<PatchResult, String> {
    let terminate = terminate_processes.unwrap_or(true);
    tokio::task::spawn_blocking(move || asar_patch::restore_original_asar(port, terminate))
        .await
        .map_err(|e| format!("Task panicked: {}", e))
}

#[tauri::command]
pub async fn toggle_asar_heartbeat(
    active: bool,
    hot_switch: Option<bool>,
) -> Result<AsarStatus, String> {
    let hs = hot_switch.unwrap_or(false);
    tokio::task::spawn_blocking(move || {
        if active {
            asar_patch::start_heartbeat(hs);
        } else {
            asar_patch::stop_heartbeat(hs);
        }
        asar_patch::get_asar_status()
    })
    .await
    .map_err(|e| format!("Task panicked: {}", e))
}

#[tauri::command]
pub async fn set_asar_bridge_port(
    port: u16,
    proxy_state: State<'_, crate::commands::proxy::ProxyServiceState>,
) -> Result<u16, String> {
    let saved_port = tokio::task::spawn_blocking(move || asar_patch::set_asar_bridge_port(port))
        .await
        .map_err(|e| format!("Task panicked: {}", e))??;

    // Also persist to AppConfig.proxy.asar_bridge_port
    if let Ok(mut cfg) = crate::modules::config::load_app_config() {
        cfg.proxy.asar_bridge_port = saved_port;
        let _ = crate::modules::config::save_app_config(&cfg);
    }

    // Restart bridge if currently running so it binds to the new port
    let is_running = proxy_state.asar_bridge.read().await.is_some();
    if is_running {
        let _ = crate::commands::proxy::ensure_asar_bridge_started(saved_port, &proxy_state).await;
    }

    Ok(saved_port)
}

#[tauri::command]
pub async fn get_asar_bridge_status(
    proxy_state: State<'_, crate::commands::proxy::ProxyServiceState>,
) -> Result<AsarBridgeStatus, String> {
    let guard = proxy_state.asar_bridge.read().await;
    if let Some(handle) = guard.as_ref() {
        Ok(AsarBridgeStatus {
            running: true,
            port: handle.port,
            base_url: format!("http://127.0.0.1:{}", handle.port),
        })
    } else {
        let port = asar_patch::get_asar_bridge_port();
        Ok(AsarBridgeStatus {
            running: false,
            port,
            base_url: format!("http://127.0.0.1:{}", port),
        })
    }
}

#[tauri::command]
pub async fn start_asar_bridge(
    port: Option<u16>,
    proxy_state: State<'_, crate::commands::proxy::ProxyServiceState>,
) -> Result<AsarBridgeStatus, String> {
    let target_port = port.unwrap_or_else(asar_patch::get_asar_bridge_port);
    let status =
        crate::commands::proxy::ensure_asar_bridge_started(target_port, &proxy_state).await?;
    tokio::task::spawn_blocking(|| asar_patch::start_heartbeat(false))
        .await
        .map_err(|e| format!("Task panicked: {}", e))?;
    if let Ok(mut cfg) = crate::modules::config::load_app_config() {
        cfg.proxy.asar_bridge_enabled = true;
        cfg.proxy.asar_bridge_port = target_port;
        let _ = crate::modules::config::save_app_config(&cfg);
    }
    Ok(status)
}

#[tauri::command]
pub async fn stop_asar_bridge(
    proxy_state: State<'_, crate::commands::proxy::ProxyServiceState>,
) -> Result<(), String> {
    crate::commands::proxy::stop_asar_bridge_internal(&proxy_state).await;
    tokio::task::spawn_blocking(|| asar_patch::stop_heartbeat(false))
        .await
        .map_err(|e| format!("Task panicked: {}", e))?;
    if let Ok(mut cfg) = crate::modules::config::load_app_config() {
        cfg.proxy.asar_bridge_enabled = false;
        let _ = crate::modules::config::save_app_config(&cfg);
    }
    Ok(())
}

#[tauri::command]
pub async fn list_catalog_models() -> Result<Vec<CatalogModelEntry>, String> {
    tokio::task::spawn_blocking(asar_model_catalog::get_all_models)
        .await
        .map_err(|e| format!("Task panicked: {}", e))
}

#[tauri::command]
pub async fn toggle_catalog_model(id: String, enabled: bool) -> Result<bool, String> {
    tokio::task::spawn_blocking(move || asar_model_catalog::toggle_model(&id, enabled))
        .await
        .map_err(|e| format!("Task panicked: {}", e))?
}

#[tauri::command]
pub async fn upsert_catalog_model(entry: CatalogModelEntry) -> Result<CatalogModelEntry, String> {
    tokio::task::spawn_blocking(move || asar_model_catalog::upsert_model(entry))
        .await
        .map_err(|e| format!("Task panicked: {}", e))?
}

#[tauri::command]
pub async fn delete_catalog_model(id: String) -> Result<bool, String> {
    tokio::task::spawn_blocking(move || asar_model_catalog::delete_model(&id))
        .await
        .map_err(|e| format!("Task panicked: {}", e))?
}

#[tauri::command]
pub async fn save_catalog_models(
    entries: Vec<CatalogModelEntry>,
) -> Result<Vec<CatalogModelEntry>, String> {
    tokio::task::spawn_blocking(move || asar_model_catalog::replace_all_models(entries))
        .await
        .map_err(|e| format!("Task panicked: {}", e))?
}

#[tauri::command]
pub async fn reload_catalog_models() -> Result<bool, String> {
    tokio::task::spawn_blocking(asar_model_catalog::reload_catalog)
        .await
        .map_err(|e| format!("Task panicked: {}", e))?
}

#[tauri::command]
pub async fn sync_catalog_from_accounts() -> Result<usize, String> {
    tokio::task::spawn_blocking(asar_model_catalog::sync_models_from_accounts)
        .await
        .map_err(|e| format!("Task panicked: {}", e))?
}

#[tauri::command]
pub async fn get_catalog_path() -> Result<String, String> {
    tokio::task::spawn_blocking(|| {
        asar_model_catalog::get_catalog_path()
            .to_string_lossy()
            .to_string()
    })
    .await
    .map_err(|e| format!("Task panicked: {}", e))
}

async fn current_app_state(
    proxy_state: &crate::commands::proxy::ProxyServiceState,
) -> Option<crate::proxy::server::AppState> {
    if let Ok(admin_lock) = proxy_state.admin_server.try_read() {
        admin_lock.as_ref().map(|a| a.axum_server.state.clone())
    } else {
        None
    }
}

#[tauri::command]
pub async fn get_asar_telemetry(
    proxy_state: State<'_, crate::commands::proxy::ProxyServiceState>,
) -> Result<AsarTelemetrySnapshot, String> {
    let app_state = current_app_state(&proxy_state).await;
    Ok(asar_bridge::snapshot_asar_telemetry(app_state.as_ref()).await)
}

#[tauri::command]
pub async fn set_asar_dual_track(
    enabled: bool,
    proxy_state: State<'_, crate::commands::proxy::ProxyServiceState>,
) -> Result<AsarTelemetrySnapshot, String> {
    asar_bridge::set_dual_track_enabled(enabled);
    let app_state = current_app_state(&proxy_state).await;
    Ok(asar_bridge::snapshot_asar_telemetry(app_state.as_ref()).await)
}

#[tauri::command]
pub async fn set_asar_equivalent_fallback(
    enabled: bool,
    proxy_state: State<'_, crate::commands::proxy::ProxyServiceState>,
) -> Result<AsarTelemetrySnapshot, String> {
    asar_bridge::set_equivalent_fallback_enabled(enabled);
    let app_state = current_app_state(&proxy_state).await;
    Ok(asar_bridge::snapshot_asar_telemetry(app_state.as_ref()).await)
}

#[tauri::command]
pub async fn reset_asar_slots(
    proxy_state: State<'_, crate::commands::proxy::ProxyServiceState>,
) -> Result<AsarTelemetrySnapshot, String> {
    asar_bridge::reset_dual_track_slots();
    let app_state = current_app_state(&proxy_state).await;
    Ok(asar_bridge::snapshot_asar_telemetry(app_state.as_ref()).await)
}
