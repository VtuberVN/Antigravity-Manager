import { request as invoke } from '../utils/request';

export interface AsarStatus {
    is_patched: boolean;
    heartbeat_active: boolean;
    bridge_port: number;
    asar_paths: string[];
    backup_paths: string[];
    patched_count: number;
}

export interface PatchResult {
    success: boolean;
    message: string;
    patched_count: number;
    asar_paths: string[];
}

export interface AsarBridgeStatus {
    running: boolean;
    port: number;
    base_url: string;
}

export interface CatalogModelEntry {
    id: string;
    displayName: string;
    enabled: boolean;
    ownedBy: string;
    providerId: string;
    providerName: string;
    targetModel: string;
    contextWindow: number;
    maxInputTokens: number;
    maxOutputTokens: number;
    supportsThinking: boolean;
    supportsVision: boolean;
    supportsAudio: boolean;
    supportsVideo: boolean;
    supportsFunctionCalling: boolean;
    inputPricing: number;
    outputPricing: number;
    description: string;
    order: number;
}

export async function getAsarStatus(): Promise<AsarStatus> {
    return await invoke<AsarStatus>('get_asar_status');
}

export async function patchAsar(port?: number, terminateProcesses?: boolean): Promise<PatchResult> {
    return await invoke<PatchResult>('patch_asar', { port, terminateProcesses });
}

export async function unpatchAsar(port?: number, terminateProcesses?: boolean): Promise<PatchResult> {
    return await invoke<PatchResult>('unpatch_asar', { port, terminateProcesses });
}

export async function toggleAsarHeartbeat(active: boolean, hotSwitch?: boolean): Promise<AsarStatus> {
    return await invoke<AsarStatus>('toggle_asar_heartbeat', { active, hotSwitch });
}

export async function setAsarBridgePort(port: number): Promise<number> {
    return await invoke<number>('set_asar_bridge_port', { port });
}

export async function getAsarBridgeStatus(): Promise<AsarBridgeStatus> {
    return await invoke<AsarBridgeStatus>('get_asar_bridge_status');
}

export async function startAsarBridge(port?: number): Promise<AsarBridgeStatus> {
    return await invoke<AsarBridgeStatus>('start_asar_bridge', { port });
}

export async function stopAsarBridge(): Promise<void> {
    return await invoke<void>('stop_asar_bridge');
}

export async function listCatalogModels(): Promise<CatalogModelEntry[]> {
    return await invoke<CatalogModelEntry[]>('list_catalog_models');
}

export async function toggleCatalogModel(id: string, enabled: boolean): Promise<boolean> {
    return await invoke<boolean>('toggle_catalog_model', { id, enabled });
}

export async function upsertCatalogModel(entry: CatalogModelEntry): Promise<CatalogModelEntry> {
    return await invoke<CatalogModelEntry>('upsert_catalog_model', { entry });
}

export async function deleteCatalogModel(id: string): Promise<boolean> {
    return await invoke<boolean>('delete_catalog_model', { id });
}

export async function saveCatalogModels(entries: CatalogModelEntry[]): Promise<CatalogModelEntry[]> {
    return await invoke<CatalogModelEntry[]>('save_catalog_models', { entries });
}

export async function reloadCatalogModels(): Promise<boolean> {
    return await invoke<boolean>('reload_catalog_models');
}

export interface AsarActiveSlot {
    slot_key: string;
    family: string;
    tier_kind: string;
    account_id: string;
    email: string;
    models: string[];
    in_flight: number;
    last_used_ts: number;
}

export interface AsarInFlightStream {
    stream_id: number;
    slot_key: string;
    account_id: string;
    email: string;
    requested_model: string;
    target_model: string;
    elapsed_ms: number;
}

export interface AsarTelemetrySnapshot {
    in_flight: number;
    asar_in_flight_account_ids: string[];
    total_requests: number;
    failover_count: number;
    total_input_tokens: number;
    total_output_tokens: number;
    total_cached_tokens: number;
    last_active_account_id: string | null;
    last_active_email: string | null;
    last_active_model: string | null;
    last_active_timestamp: number | null;
    dual_track_enabled: boolean;
    claude_slot_account_id: string | null;
    claude_slot_email: string | null;
    gemini_slot_account_id: string | null;
    gemini_slot_email: string | null;
    proxy_last_account_id: string | null;
    proxy_last_email: string | null;
    proxy_in_flight_account_ids: string[];
    native_current_account_id: string | null;
    equivalent_fallback_enabled?: boolean;
    cross_family_fallback_enabled?: boolean;
    fallback_downgrade_count?: number;
    last_fallback_route?: string | null;
    active_slots?: AsarActiveSlot[];
    separate_lite_slot?: boolean;
    in_flight_streams?: AsarInFlightStream[];
}

export async function getCatalogPath(): Promise<string> {
    return await invoke<string>('get_catalog_path');
}

export async function syncCatalogFromAccounts(): Promise<number> {
    return await invoke<number>('sync_catalog_from_accounts');
}

export async function getAsarTelemetry(): Promise<AsarTelemetrySnapshot> {
    return await invoke<AsarTelemetrySnapshot>('get_asar_telemetry');
}

export async function setAsarDualTrack(enabled: boolean): Promise<AsarTelemetrySnapshot> {
    return await invoke<AsarTelemetrySnapshot>('set_asar_dual_track', { enabled });
}

export async function resetAsarSlots(): Promise<AsarTelemetrySnapshot> {
    return await invoke<AsarTelemetrySnapshot>('reset_asar_slots');
}

export async function setAsarEquivalentFallback(enabled: boolean): Promise<AsarTelemetrySnapshot> {
    return await invoke<AsarTelemetrySnapshot>('set_asar_equivalent_fallback', { enabled });
}

export async function setAsarCrossFamilyFallback(enabled: boolean): Promise<AsarTelemetrySnapshot> {
    return await invoke<AsarTelemetrySnapshot>('set_asar_cross_family_fallback', { enabled });
}

export async function setAsarSeparateLiteSlot(enabled: boolean): Promise<AsarTelemetrySnapshot> {
    return await invoke<AsarTelemetrySnapshot>('set_asar_separate_lite_slot', { enabled });
}

export async function refreshIdeModels(): Promise<string> {
    return await invoke<string>('refresh_ide_models');
}
