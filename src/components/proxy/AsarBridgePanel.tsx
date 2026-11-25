import { useState, useEffect, useCallback } from 'react';
import { useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import {
    Activity,
    CheckCircle2,
    Database,
    Edit2,
    ExternalLink,
    FileCode2,
    GitBranch,
    HardDrive,
    Play,
    Plus,
    RefreshCw,
    RotateCcw,
    Save,
    Search,
    ShieldCheck,
    Square,
    Trash2,
    Wrench,
    X,
    XCircle,
} from 'lucide-react';
import {
    AsarBridgeStatus,
    AsarStatus,
    AsarTelemetrySnapshot,
    CatalogModelEntry,
    deleteCatalogModel,
    getAsarBridgeStatus,
    getAsarStatus,
    getAsarTelemetry,
    getCatalogPath,
    listCatalogModels,
    patchAsar,
    reloadCatalogModels,
    resetAsarSlots,
    saveCatalogModels,
    setAsarBridgePort,
    setAsarDualTrack,
    startAsarBridge,
    stopAsarBridge,
    syncCatalogFromAccounts,
    toggleAsarHeartbeat,
    toggleCatalogModel,
    unpatchAsar,
    upsertCatalogModel,
} from '../../services/asarService';
import { showToast } from '../common/ToastContainer';
import { cn } from '../../utils/cn';
import { formatCompactNumber } from '../../utils/format';

const DEFAULT_PORT = 8989;

const BTN_BASE =
    'inline-flex items-center justify-center gap-1.5 px-3 py-1.5 rounded-lg text-xs font-medium transition-all disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer whitespace-nowrap';
const BTN_PRIMARY_EMERALD = `${BTN_BASE} bg-emerald-600 hover:bg-emerald-700 text-white shadow-xs`;
const BTN_PRIMARY_BLUE = `${BTN_BASE} bg-blue-600 hover:bg-blue-700 text-white shadow-xs`;
const BTN_SECONDARY = `${BTN_BASE} border border-gray-200 dark:border-gray-700 bg-white dark:bg-base-200 hover:bg-gray-100 dark:hover:bg-base-300 text-gray-700 dark:text-gray-200`;
const BTN_WARNING = `${BTN_BASE} border border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400 hover:bg-amber-500/20`;
const BTN_DANGER = `${BTN_BASE} border border-rose-500/40 bg-rose-500/10 text-rose-600 dark:text-rose-400 hover:bg-rose-500/20`;
const BTN_ICON =
    'inline-flex items-center justify-center p-1.5 rounded-lg text-gray-500 dark:text-gray-400 hover:bg-gray-100 dark:hover:bg-base-300 transition-colors disabled:opacity-50 cursor-pointer';

function createEmptyModel(order: number): CatalogModelEntry {
    return {
        id: '',
        displayName: '',
        enabled: true,
        ownedBy: 'google',
        providerId: 'google',
        providerName: 'Google Pool',
        targetModel: '',
        contextWindow: 1000000,
        maxInputTokens: 1000000,
        maxOutputTokens: 65536,
        supportsThinking: true,
        supportsVision: true,
        supportsAudio: false,
        supportsVideo: false,
        supportsFunctionCalling: true,
        inputPricing: 0,
        outputPricing: 0,
        description: '',
        order,
    };
}

export default function AsarBridgePanel() {
    const { t } = useTranslation();
    const navigate = useNavigate();

    const [asarStatus, setAsarStatus] = useState<AsarStatus | null>(null);
    const [bridgeStatus, setBridgeStatus] = useState<AsarBridgeStatus | null>(null);
    const [telemetry, setTelemetry] = useState<AsarTelemetrySnapshot | null>(null);
    const [portInput, setPortInput] = useState<number>(DEFAULT_PORT);
    const [terminateProcesses, setTerminateProcesses] = useState<boolean>(false);

    const [catalogModels, setCatalogModels] = useState<CatalogModelEntry[]>([]);
    const [catalogPath, setCatalogPath] = useState<string>('');
    const [searchQuery, setSearchQuery] = useState<string>('');

    const [loading, setLoading] = useState<boolean>(false);
    const [actionLoading, setActionLoading] = useState<string | null>(null);

    const [editingModel, setEditingModel] = useState<CatalogModelEntry | null>(null);
    const [isNewModel, setIsNewModel] = useState<boolean>(false);

    const refreshAll = useCallback(async () => {
        setLoading(true);
        try {
            await syncCatalogFromAccounts().catch(() => 0);
            const [asar, bridge, models, path, snap] = await Promise.all([
                getAsarStatus(),
                getAsarBridgeStatus(),
                listCatalogModels(),
                getCatalogPath().catch(() => ''),
                getAsarTelemetry().catch(() => null),
            ]);
            setAsarStatus(asar);
            setBridgeStatus(bridge);
            if (snap) setTelemetry(snap);
            setPortInput(asar.bridge_port || bridge.port || DEFAULT_PORT);
            setCatalogModels(models);
            setCatalogPath(path);
        } catch (error) {
            console.error('Failed to load ASAR Bridge status:', error);
            showToast(
                t('proxy.asar.load_failed', {
                    defaultValue: 'Failed to load ASAR Bridge status',
                }) + `: ${String(error)}`,
                'error',
            );
        } finally {
            setLoading(false);
        }
    }, [t]);

    useEffect(() => {
        refreshAll();
    }, [refreshAll]);

    useEffect(() => {
        let cancelled = false;
        const pollTelemetry = async () => {
            try {
                const snap = await getAsarTelemetry();
                if (!cancelled) setTelemetry(snap);
            } catch {
                // Ignore transient polling errors
            }
        };
        const timer = setInterval(pollTelemetry, 2500);
        return () => {
            cancelled = true;
            clearInterval(timer);
        };
    }, []);

    const handleToggleDualTrack = async (enabled: boolean) => {
        setActionLoading('dual-track');
        try {
            const snap = await setAsarDualTrack(enabled);
            setTelemetry(snap);
            showToast(
                enabled
                    ? t('proxy.asar.dual_track_on', {
                          defaultValue: 'Dual-Track enabled (Claude Lead / Gemini Worker isolated)',
                      })
                    : t('proxy.asar.dual_track_off', {
                          defaultValue: 'Dual-Track disabled',
                      }),
                'success',
            );
        } catch (error) {
            showToast(String(error), 'error');
        } finally {
            setActionLoading(null);
        }
    };

    const handleResetSlots = async () => {
        setActionLoading('reset-slots');
        try {
            const snap = await resetAsarSlots();
            setTelemetry(snap);
            showToast(
                t('proxy.asar.slots_reset', {
                    defaultValue: 'Dual-Track pinned slots reset',
                }),
                'info',
            );
        } catch (error) {
            showToast(String(error), 'error');
        } finally {
            setActionLoading(null);
        }
    };

    const handleSavePort = async () => {
        if (!Number.isInteger(portInput) || portInput < 1024 || portInput > 65535) {
            showToast(
                t('proxy.asar.invalid_port', {
                    defaultValue: 'Port must be between 1024 and 65535',
                }),
                'error',
            );
            return;
        }
        setActionLoading('port');
        try {
            const savedPort = await setAsarBridgePort(portInput);
            setPortInput(savedPort);
            showToast(
                t('proxy.asar.port_saved', {
                    defaultValue: `Bridge port updated to ${savedPort}`,
                }),
                'success',
            );
            await refreshAll();
        } catch (error) {
            showToast(String(error), 'error');
        } finally {
            setActionLoading(null);
        }
    };

    const handlePatch = async () => {
        setActionLoading('patch');
        try {
            const res = await patchAsar(portInput, terminateProcesses);
            if (res.success) {
                showToast(
                    res.message ||
                        t('proxy.asar.patch_success', {
                            defaultValue: `Patched ${res.patched_count} app.asar file(s)`,
                        }),
                    'success',
                );
            } else {
                showToast(res.message || 'Patch failed', 'error');
            }
            await refreshAll();
        } catch (error) {
            showToast(String(error), 'error');
        } finally {
            setActionLoading(null);
        }
    };

    const handleRestore = async () => {
        setActionLoading('restore');
        try {
            const res = await unpatchAsar(portInput, terminateProcesses);
            if (res.success) {
                showToast(
                    res.message ||
                        t('proxy.asar.restore_success', {
                            defaultValue: `Restored ${res.patched_count} app.asar file(s)`,
                        }),
                    'success',
                );
            } else {
                showToast(res.message || 'Restore failed', 'error');
            }
            await refreshAll();
        } catch (error) {
            showToast(String(error), 'error');
        } finally {
            setActionLoading(null);
        }
    };

    const handleToggleHeartbeat = async (nextActive: boolean) => {
        setActionLoading('heartbeat');
        try {
            const updated = await toggleAsarHeartbeat(nextActive, false);
            setAsarStatus(updated);
            showToast(
                nextActive
                    ? t('proxy.asar.heartbeat_started', {
                          defaultValue: 'Heartbeat (ag_rot.act) activated',
                      })
                    : t('proxy.asar.heartbeat_stopped', {
                          defaultValue: 'Heartbeat (ag_rot.act) stopped',
                      }),
                'success',
            );
        } catch (error) {
            showToast(String(error), 'error');
        } finally {
            setActionLoading(null);
        }
    };

    const handleToggleBridgeServer = async () => {
        setActionLoading('bridge');
        try {
            if (bridgeStatus?.running) {
                await stopAsarBridge();
                showToast(
                    t('proxy.asar.bridge_stopped', {
                        defaultValue: 'ASAR Bridge server stopped',
                    }),
                    'info',
                );
            } else {
                const status = await startAsarBridge(portInput);
                setBridgeStatus(status);
                showToast(
                    t('proxy.asar.bridge_started', {
                        defaultValue: `ASAR Bridge running on ${status.base_url}`,
                    }),
                    'success',
                );
            }
            await refreshAll();
        } catch (error) {
            showToast(String(error), 'error');
        } finally {
            setActionLoading(null);
        }
    };

    const handleToggleModel = async (id: string, enabled: boolean) => {
        setActionLoading(`toggle-${id}`);
        try {
            await toggleCatalogModel(id, enabled);
            setCatalogModels((prev) =>
                prev.map((m) => (m.id === id ? { ...m, enabled } : m)),
            );
            showToast(
                t('proxy.asar.model_toggled', {
                    defaultValue: `${id} ${enabled ? 'enabled' : 'disabled'} (hot-applied)`,
                }),
                'success',
            );
        } catch (error) {
            showToast(String(error), 'error');
        } finally {
            setActionLoading(null);
        }
    };

    const handleSyncPool = async () => {
        setActionLoading('sync-pool');
        try {
            const added = await syncCatalogFromAccounts();
            const models = await listCatalogModels();
            setCatalogModels(models);
            showToast(
                t('proxy.asar.pool_synced', {
                    defaultValue:
                        added > 0
                            ? `Synced ${added} new model(s) from accounts`
                            : `Catalog up to date (${models.length} models)`,
                }),
                'success',
            );
        } catch (error) {
            showToast(String(error), 'error');
        } finally {
            setActionLoading(null);
        }
    };

    const handleReloadCatalog = async () => {
        setActionLoading('reload-catalog');
        try {
            await reloadCatalogModels();
            const models = await listCatalogModels();
            setCatalogModels(models);
            showToast(
                t('proxy.asar.catalog_reloaded', {
                    defaultValue: 'Model catalog reloaded from disk',
                }),
                'success',
            );
        } catch (error) {
            showToast(String(error), 'error');
        } finally {
            setActionLoading(null);
        }
    };

    const handleSaveAllCatalog = async () => {
        setActionLoading('save-catalog');
        try {
            const saved = await saveCatalogModels(catalogModels);
            setCatalogModels(saved);
            showToast(
                t('proxy.asar.catalog_saved', {
                    defaultValue: 'Model catalog saved & hot-applied (no IDE reload needed)',
                }),
                'success',
            );
        } catch (error) {
            showToast(String(error), 'error');
        } finally {
            setActionLoading(null);
        }
    };

    const handleSaveEditingModel = async () => {
        if (!editingModel) return;
        const trimmedId = editingModel.id.trim();
        if (!trimmedId) {
            showToast(
                t('proxy.asar.model_id_required', {
                    defaultValue: 'Model ID is required',
                }),
                'error',
            );
            return;
        }
        setActionLoading('upsert-model');
        try {
            const payload: CatalogModelEntry = {
                ...editingModel,
                id: trimmedId,
                displayName: editingModel.displayName.trim() || trimmedId,
                targetModel: editingModel.targetModel.trim() || trimmedId,
            };
            const saved = await upsertCatalogModel(payload);
            setCatalogModels((prev) => {
                const exists = prev.some((m) => m.id === saved.id);
                if (exists) {
                    return prev.map((m) => (m.id === saved.id ? saved : m));
                }
                return [...prev, saved];
            });
            setEditingModel(null);
            setIsNewModel(false);
            showToast(
                t('proxy.asar.model_saved', {
                    defaultValue: `Model ${saved.id} saved & hot-applied`,
                }),
                'success',
            );
        } catch (error) {
            showToast(String(error), 'error');
        } finally {
            setActionLoading(null);
        }
    };

    const handleDeleteModel = async (id: string) => {
        setActionLoading(`delete-${id}`);
        try {
            await deleteCatalogModel(id);
            setCatalogModels((prev) => prev.filter((m) => m.id !== id));
            showToast(
                t('proxy.asar.model_deleted', {
                    defaultValue: `Removed ${id} from catalog`,
                }),
                'info',
            );
        } catch (error) {
            showToast(String(error), 'error');
        } finally {
            setActionLoading(null);
        }
    };

    const filteredModels = catalogModels.filter((m) => {
        if (!searchQuery.trim()) return true;
        const q = searchQuery.toLowerCase();
        return (
            m.id.toLowerCase().includes(q) ||
            m.displayName.toLowerCase().includes(q) ||
            m.targetModel.toLowerCase().includes(q)
        );
    });

    const isPatched = asarStatus?.is_patched ?? false;
    const heartbeatActive = asarStatus?.heartbeat_active ?? false;
    const bridgeRunning = bridgeStatus?.running ?? false;
    const inFlight = telemetry?.in_flight ?? 0;
    const dualTrackEnabled = telemetry?.dual_track_enabled ?? true;

    return (
        <div className="space-y-4">
            {/* Status Overview Row */}
            <div className="grid grid-cols-1 md:grid-cols-3 gap-3">
                {/* ASAR Patch Status */}
                <div className="p-3.5 rounded-xl border border-gray-200/80 dark:border-base-300 bg-gray-50/70 dark:bg-base-200/60 flex items-center justify-between gap-3">
                    <div className="flex items-center gap-2.5 min-w-0">
                        {isPatched ? (
                            <CheckCircle2 className="w-5 h-5 text-emerald-500 shrink-0" />
                        ) : (
                            <XCircle className="w-5 h-5 text-gray-400 shrink-0" />
                        )}
                        <div className="min-w-0">
                            <div className="text-xs font-semibold text-gray-800 dark:text-gray-200">
                                {t('proxy.asar.patch_status', { defaultValue: 'app.asar Patch' })}
                            </div>
                            <div className="text-[11px] text-gray-500 dark:text-gray-400">
                                {isPatched
                                    ? t('proxy.asar.patched_count', {
                                          defaultValue: `Patched (${asarStatus?.patched_count ?? 0}/${asarStatus?.asar_paths.length ?? 0})`,
                                      })
                                    : t('proxy.asar.unpatched', { defaultValue: 'Unpatched' })}
                            </div>
                        </div>
                    </div>
                    <span
                        className={cn(
                            'text-[11px] px-2.5 py-0.5 rounded-full font-medium border shrink-0',
                            isPatched
                                ? 'bg-emerald-50 text-emerald-700 border-emerald-200 dark:bg-emerald-950/50 dark:text-emerald-400 dark:border-emerald-800/60'
                                : 'bg-gray-100 text-gray-600 border-gray-200 dark:bg-base-300 dark:text-gray-400 dark:border-base-300',
                        )}
                    >
                        {isPatched
                            ? t('proxy.asar.status_patched', { defaultValue: 'Patched' })
                            : t('proxy.asar.status_unpatched', { defaultValue: 'Unpatched' })}
                    </span>
                </div>

                {/* Heartbeat ag_rot.act Status */}
                <div className="p-3.5 rounded-xl border border-gray-200/80 dark:border-base-300 bg-gray-50/70 dark:bg-base-200/60 flex items-center justify-between gap-3">
                    <div className="flex items-center gap-2.5 min-w-0">
                        <Activity
                            className={cn(
                                'w-5 h-5 shrink-0',
                                heartbeatActive ? 'text-emerald-500 animate-pulse' : 'text-gray-400',
                            )}
                        />
                        <div className="min-w-0">
                            <div className="text-xs font-semibold text-gray-800 dark:text-gray-200">
                                {t('proxy.asar.heartbeat_title', { defaultValue: 'Heartbeat (ag_rot.act)' })}
                            </div>
                            <div className="text-[11px] text-gray-500 dark:text-gray-400">
                                {heartbeatActive
                                    ? t('proxy.asar.heartbeat_on', { defaultValue: 'Active lock file' })
                                    : t('proxy.asar.heartbeat_off', { defaultValue: 'Inactive' })}
                            </div>
                        </div>
                    </div>
                    <input
                        type="checkbox"
                        className="toggle toggle-sm toggle-success shrink-0"
                        checked={heartbeatActive}
                        disabled={actionLoading === 'heartbeat'}
                        onChange={(e) => handleToggleHeartbeat(e.target.checked)}
                    />
                </div>

                {/* Bridge HTTP Server Status */}
                <div className="p-3.5 rounded-xl border border-gray-200/80 dark:border-base-300 bg-gray-50/70 dark:bg-base-200/60 flex items-center justify-between gap-3">
                    <div className="flex items-center gap-2.5 min-w-0">
                        <ShieldCheck
                            className={cn(
                                'w-5 h-5 shrink-0',
                                bridgeRunning ? 'text-blue-500' : 'text-gray-400',
                            )}
                        />
                        <div className="min-w-0">
                            <div className="text-xs font-semibold text-gray-800 dark:text-gray-200">
                                {t('proxy.asar.bridge_server', { defaultValue: 'Bridge Server' })}
                            </div>
                            <div className="text-[11px] text-gray-500 dark:text-gray-400 font-mono truncate">
                                {bridgeRunning
                                    ? bridgeStatus?.base_url
                                    : `127.0.0.1:${portInput}`}
                            </div>
                        </div>
                    </div>
                    <button
                        type="button"
                        onClick={handleToggleBridgeServer}
                        disabled={actionLoading === 'bridge'}
                        className={bridgeRunning ? BTN_DANGER : BTN_PRIMARY_EMERALD}
                    >
                        {bridgeRunning ? <Square size={12} /> : <Play size={12} />}
                        {bridgeRunning
                            ? t('common.stop', { defaultValue: 'Stop' })
                            : t('common.start', { defaultValue: 'Start' })}
                    </button>
                </div>
            </div>

            {/* Live Mini-Telemetry Strip + Dual-Track Bar */}
            <div className="p-3.5 rounded-xl border border-emerald-200/70 dark:border-emerald-900/40 bg-emerald-50/30 dark:bg-emerald-950/15 space-y-3">
                <div className="flex flex-wrap items-center justify-between gap-3">
                    <div className="flex flex-wrap items-center gap-4 text-xs">
                        {/* In-Flight */}
                        <div className="flex items-center gap-1.5">
                            <span
                                className={cn(
                                    'w-2 h-2 rounded-full',
                                    inFlight > 0 ? 'bg-emerald-500 animate-ping' : 'bg-gray-400',
                                )}
                            />
                            <span className="text-gray-500 dark:text-gray-400">In-Flight:</span>
                            <span
                                className={cn(
                                    'font-bold font-mono',
                                    inFlight > 0
                                        ? 'text-emerald-600 dark:text-emerald-400'
                                        : 'text-gray-800 dark:text-gray-200',
                                )}
                            >
                                {inFlight}
                            </span>
                        </div>

                        <span className="text-gray-300 dark:text-gray-700">|</span>

                        {/* IDE Requests */}
                        <div className="flex items-center gap-1.5">
                            <span className="text-gray-500 dark:text-gray-400">IDE Requests:</span>
                            <span className="font-bold font-mono text-gray-800 dark:text-gray-200">
                                {formatCompactNumber(telemetry?.total_requests ?? 0)}
                            </span>
                        </div>

                        <span className="text-gray-300 dark:text-gray-700">|</span>

                        {/* Auto-Failovers */}
                        <div className="flex items-center gap-1.5">
                            <span className="text-gray-500 dark:text-gray-400">Auto-Failovers:</span>
                            <span
                                className={cn(
                                    'font-bold font-mono',
                                    (telemetry?.failover_count ?? 0) > 0
                                        ? 'text-amber-600 dark:text-amber-400'
                                        : 'text-gray-800 dark:text-gray-200',
                                )}
                            >
                                {telemetry?.failover_count ?? 0}
                            </span>
                        </div>

                        <span className="text-gray-300 dark:text-gray-700">|</span>

                        {/* IDE Tokens */}
                        <div className="flex items-center gap-1.5" title="Input / Output / Cached Tokens">
                            <span className="text-gray-500 dark:text-gray-400">IDE Tokens (In/Out/Cache):</span>
                            <span className="font-bold font-mono text-blue-600 dark:text-blue-400">
                                {formatCompactNumber(telemetry?.total_input_tokens ?? 0)}
                                {' / '}
                                {formatCompactNumber(telemetry?.total_output_tokens ?? 0)}
                                {' / '}
                                {formatCompactNumber(telemetry?.total_cached_tokens ?? 0)}
                            </span>
                        </div>

                        {telemetry?.last_active_email && (
                            <>
                                <span className="text-gray-300 dark:text-gray-700">|</span>
                                <div className="flex items-center gap-1.5 min-w-0">
                                    <span className="text-gray-500 dark:text-gray-400">Active:</span>
                                    <span className="font-semibold text-emerald-700 dark:text-emerald-300 truncate max-w-[180px]">
                                        {telemetry.last_active_email}
                                    </span>
                                    {telemetry.last_active_model && (
                                        <span className="text-[10px] font-mono px-1.5 py-0.5 rounded bg-emerald-100/80 dark:bg-emerald-900/40 text-emerald-700 dark:text-emerald-300">
                                            {telemetry.last_active_model}
                                        </span>
                                    )}
                                </div>
                            </>
                        )}
                    </div>

                    <button
                        type="button"
                        onClick={() => navigate('/monitor', { state: { protocol: 'v1internal' } })}
                        className={BTN_SECONDARY}
                    >
                        <ExternalLink size={12} />
                        {t('proxy.asar.open_monitor', { defaultValue: 'Nhật ký IDE (Monitor)' })}
                    </button>
                </div>

                {/* Dual-Track Control Bar */}
                <div className="pt-2.5 border-t border-emerald-200/60 dark:border-emerald-900/40 flex flex-wrap items-center justify-between gap-3">
                    <div className="flex flex-wrap items-center gap-3">
                        <label className="inline-flex items-center gap-2 cursor-pointer select-none">
                            <input
                                type="checkbox"
                                className="toggle toggle-xs toggle-success"
                                checked={dualTrackEnabled}
                                disabled={actionLoading === 'dual-track'}
                                onChange={(e) => handleToggleDualTrack(e.target.checked)}
                            />
                            <span className="text-xs font-semibold text-gray-800 dark:text-gray-200 inline-flex items-center gap-1">
                                <GitBranch size={13} className="text-emerald-500" />
                                {t('proxy.asar.dual_track_label', {
                                    defaultValue: 'Dual-Track (Tách làn Claude Lead & Gemini Worker)',
                                })}
                            </span>
                        </label>

                        {dualTrackEnabled && (
                            <div className="flex flex-wrap items-center gap-2 text-[11px]">
                                <span className="px-2 py-0.5 rounded-md bg-amber-100/80 dark:bg-amber-950/50 text-amber-800 dark:text-amber-300 border border-amber-300/60 dark:border-amber-800/60 font-medium">
                                    👑 Claude Slot:{' '}
                                    <span className="font-mono font-bold">
                                        {telemetry?.claude_slot_email || 'Auto'}
                                    </span>
                                </span>
                                <span className="px-2 py-0.5 rounded-md bg-cyan-100/80 dark:bg-cyan-950/50 text-cyan-800 dark:text-cyan-300 border border-cyan-300/60 dark:border-cyan-800/60 font-medium">
                                    ⚡ Gemini Slot:{' '}
                                    <span className="font-mono font-bold">
                                        {telemetry?.gemini_slot_email || 'Auto'}
                                    </span>
                                </span>
                            </div>
                        )}
                    </div>

                    {dualTrackEnabled && (
                        <button
                            type="button"
                            onClick={handleResetSlots}
                            disabled={actionLoading === 'reset-slots'}
                            className={BTN_SECONDARY}
                        >
                            <RotateCcw size={12} />
                            {t('proxy.asar.reset_slots', { defaultValue: 'Reset Slots' })}
                        </button>
                    )}
                </div>
            </div>

            {/* Patch Controls & Port Configuration */}
            <div className="p-4 rounded-xl border border-gray-200/80 dark:border-base-300 bg-white dark:bg-base-100 space-y-3.5">
                <div className="flex flex-wrap items-center justify-between gap-3">
                    <div className="flex items-center gap-2">
                        <label className="text-xs font-semibold text-gray-700 dark:text-gray-300">
                            {t('proxy.asar.bridge_port', { defaultValue: 'Bridge Port' })}:
                        </label>
                        <input
                            type="number"
                            min={1024}
                            max={65535}
                            value={portInput}
                            onChange={(e) => setPortInput(Number(e.target.value))}
                            className="w-24 px-2.5 py-1.5 text-xs font-mono border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-base-200 text-gray-900 dark:text-base-content focus:ring-2 focus:ring-blue-500/20 focus:border-blue-500 outline-none"
                        />
                        <button
                            type="button"
                            onClick={handleSavePort}
                            disabled={actionLoading === 'port'}
                            className={BTN_SECONDARY}
                        >
                            <Save size={12} />
                            {t('common.save', { defaultValue: 'Save' })}
                        </button>
                    </div>

                    <div className="flex flex-wrap items-center gap-2">
                        <button
                            type="button"
                            onClick={refreshAll}
                            disabled={loading}
                            className={BTN_SECONDARY}
                        >
                            <RefreshCw size={12} className={loading ? 'animate-spin' : ''} />
                            {t('common.refresh', { defaultValue: 'Refresh' })}
                        </button>
                        <button
                            type="button"
                            onClick={handlePatch}
                            disabled={actionLoading !== null}
                            className={BTN_PRIMARY_EMERALD}
                        >
                            <Wrench size={12} />
                            {actionLoading === 'patch'
                                ? t('proxy.asar.patching', { defaultValue: 'Patching...' })
                                : t('proxy.asar.patch_btn', { defaultValue: 'Patch app.asar' })}
                        </button>
                        <button
                            type="button"
                            onClick={handleRestore}
                            disabled={actionLoading !== null}
                            className={BTN_WARNING}
                        >
                            <RotateCcw size={12} />
                            {actionLoading === 'restore'
                                ? t('proxy.asar.restoring', { defaultValue: 'Restoring...' })
                                : t('proxy.asar.restore_btn', { defaultValue: 'Restore app.asar' })}
                        </button>
                    </div>
                </div>

                <label className="flex items-center gap-2.5 text-xs text-gray-600 dark:text-gray-300 cursor-pointer select-none">
                    <input
                        type="checkbox"
                        className="w-4 h-4 rounded border-gray-300 dark:border-gray-600 accent-emerald-500 cursor-pointer"
                        checked={terminateProcesses}
                        onChange={(e) => setTerminateProcesses(e.target.checked)}
                    />
                    {t('proxy.asar.terminate_processes', {
                        defaultValue: 'Terminate running Antigravity processes before Patch / Restore (recommended if file is locked)',
                    })}
                </label>

                {/* Detected app.asar paths */}
                <div className="pt-2.5 border-t border-gray-100 dark:border-base-200 space-y-1.5">
                    <div className="text-[11px] font-semibold text-gray-600 dark:text-gray-400 flex items-center gap-1.5">
                        <HardDrive size={12} />
                        {t('proxy.asar.detected_paths', {
                            defaultValue: `Detected app.asar Paths (${asarStatus?.asar_paths.length ?? 0})`,
                        })}
                    </div>
                    {asarStatus && asarStatus.asar_paths.length > 0 ? (
                        <div className="space-y-1 max-h-28 overflow-y-auto">
                            {asarStatus.asar_paths.map((p) => {
                                const hasBackup = asarStatus.backup_paths.some((b) =>
                                    b.startsWith(p),
                                );
                                return (
                                    <div
                                        key={p}
                                        className="flex items-center justify-between gap-2 text-[11px] font-mono px-2.5 py-1.5 rounded-lg bg-gray-50 dark:bg-base-200 text-gray-700 dark:text-gray-300"
                                    >
                                        <span className="truncate">{p}</span>
                                        {hasBackup && (
                                            <span className="shrink-0 text-[10px] px-1.5 py-0.5 rounded bg-blue-50 text-blue-600 dark:bg-blue-950/50 dark:text-blue-400">
                                                .original
                                            </span>
                                        )}
                                    </div>
                                );
                            })}
                        </div>
                    ) : (
                        <div className="text-xs text-gray-400 italic">
                            {t('proxy.asar.no_paths', {
                                defaultValue: 'No Antigravity app.asar installations detected on this system.',
                            })}
                        </div>
                    )}
                </div>
            </div>

            {/* Hot Model Catalog Manager */}
            <div className="p-4 rounded-xl border border-gray-200/80 dark:border-base-300 bg-white dark:bg-base-100 space-y-3.5">
                <div className="flex flex-wrap items-center justify-between gap-3">
                    <div>
                        <div className="text-xs font-bold text-gray-900 dark:text-white flex items-center gap-1.5">
                            <FileCode2 size={14} className="text-purple-500" />
                            {t('proxy.asar.catalog_title', {
                                defaultValue: `Hot Model Catalog (${catalogModels.length} models)`,
                            })}
                        </div>
                        {catalogPath && (
                            <div className="text-[11px] font-mono text-gray-400 truncate max-w-md mt-0.5">
                                {catalogPath}
                            </div>
                        )}
                    </div>

                    <div className="flex flex-wrap items-center gap-2">
                        <div className="relative">
                            <Search
                                size={12}
                                className="absolute left-2.5 top-1/2 -translate-y-1/2 text-gray-400"
                            />
                            <input
                                type="text"
                                value={searchQuery}
                                onChange={(e) => setSearchQuery(e.target.value)}
                                placeholder={t('common.search', { defaultValue: 'Search models...' })}
                                className="pl-7 pr-2.5 py-1.5 text-xs border border-gray-200 dark:border-gray-700 rounded-lg bg-gray-50 dark:bg-base-200 text-gray-900 dark:text-base-content w-44 outline-none focus:ring-2 focus:ring-blue-500/20 focus:border-blue-500"
                            />
                        </div>
                        <button
                            type="button"
                            onClick={handleSyncPool}
                            disabled={actionLoading !== null}
                            className={BTN_SECONDARY}
                            title={t('proxy.asar.sync_pool_tooltip', {
                                defaultValue: 'Import all models from your account quotas',
                            })}
                        >
                            <Database size={12} />
                            {actionLoading === 'sync-pool'
                                ? t('proxy.asar.syncing', { defaultValue: 'Syncing...' })
                                : t('proxy.asar.sync_pool', { defaultValue: 'Sync Pool' })}
                        </button>
                        <button
                            type="button"
                            onClick={handleReloadCatalog}
                            disabled={actionLoading !== null}
                            className={BTN_SECONDARY}
                        >
                            <RefreshCw size={12} />
                            {t('proxy.asar.reload', { defaultValue: 'Reload' })}
                        </button>
                        <button
                            type="button"
                            onClick={() => {
                                setIsNewModel(true);
                                setEditingModel(createEmptyModel(catalogModels.length + 1));
                            }}
                            className={BTN_SECONDARY}
                        >
                            <Plus size={12} />
                            {t('proxy.asar.add_model', { defaultValue: 'Add Model' })}
                        </button>
                        <button
                            type="button"
                            onClick={handleSaveAllCatalog}
                            disabled={actionLoading !== null}
                            className={BTN_PRIMARY_BLUE}
                        >
                            <Save size={12} />
                            {t('proxy.asar.save_hot_apply', { defaultValue: 'Save & Hot-Apply' })}
                        </button>
                    </div>
                </div>

                {/* Inline Add / Edit Form */}
                {editingModel && (
                    <div className="p-3.5 rounded-xl border border-blue-200 dark:border-blue-800/60 bg-blue-50/40 dark:bg-blue-950/20 space-y-3">
                        <div className="flex items-center justify-between">
                            <span className="text-xs font-bold text-gray-800 dark:text-gray-200">
                                {isNewModel
                                    ? t('proxy.asar.new_model', { defaultValue: 'Add Catalog Model' })
                                    : t('proxy.asar.edit_model', {
                                          defaultValue: `Edit Model: ${editingModel.id}`,
                                      })}
                            </span>
                            <button
                                type="button"
                                onClick={() => {
                                    setEditingModel(null);
                                    setIsNewModel(false);
                                }}
                                className={BTN_ICON}
                            >
                                <X size={14} />
                            </button>
                        </div>

                        <div className="grid grid-cols-1 md:grid-cols-3 gap-2.5">
                            <div>
                                <label className="block text-[11px] font-medium text-gray-600 dark:text-gray-400 mb-1">
                                    Model ID *
                                </label>
                                <input
                                    type="text"
                                    disabled={!isNewModel}
                                    value={editingModel.id}
                                    onChange={(e) =>
                                        setEditingModel({ ...editingModel, id: e.target.value })
                                    }
                                    placeholder="e.g. gemini-3-pro-high"
                                    className="w-full px-2.5 py-1.5 text-xs font-mono border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-base-200 disabled:opacity-60 outline-none"
                                />
                            </div>
                            <div>
                                <label className="block text-[11px] font-medium text-gray-600 dark:text-gray-400 mb-1">
                                    Display Name
                                </label>
                                <input
                                    type="text"
                                    value={editingModel.displayName}
                                    onChange={(e) =>
                                        setEditingModel({
                                            ...editingModel,
                                            displayName: e.target.value,
                                        })
                                    }
                                    placeholder="e.g. Gemini 3 Pro (High)"
                                    className="w-full px-2.5 py-1.5 text-xs border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-base-200 outline-none"
                                />
                            </div>
                            <div>
                                <label className="block text-[11px] font-medium text-gray-600 dark:text-gray-400 mb-1">
                                    Target Model (Routing)
                                </label>
                                <input
                                    type="text"
                                    value={editingModel.targetModel}
                                    onChange={(e) =>
                                        setEditingModel({
                                            ...editingModel,
                                            targetModel: e.target.value,
                                        })
                                    }
                                    placeholder="Same as Model ID if blank"
                                    className="w-full px-2.5 py-1.5 text-xs font-mono border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-base-200 outline-none"
                                />
                            </div>
                        </div>

                        <div className="grid grid-cols-2 md:grid-cols-4 gap-2.5">
                            <div>
                                <label className="block text-[11px] font-medium text-gray-600 dark:text-gray-400 mb-1">
                                    Context Window
                                </label>
                                <input
                                    type="number"
                                    value={editingModel.contextWindow}
                                    onChange={(e) =>
                                        setEditingModel({
                                            ...editingModel,
                                            contextWindow: Number(e.target.value) || 0,
                                        })
                                    }
                                    className="w-full px-2.5 py-1.5 text-xs font-mono border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-base-200 outline-none"
                                />
                            </div>
                            <div>
                                <label className="block text-[11px] font-medium text-gray-600 dark:text-gray-400 mb-1">
                                    Max Output Tokens
                                </label>
                                <input
                                    type="number"
                                    value={editingModel.maxOutputTokens}
                                    onChange={(e) =>
                                        setEditingModel({
                                            ...editingModel,
                                            maxOutputTokens: Number(e.target.value) || 0,
                                        })
                                    }
                                    className="w-full px-2.5 py-1.5 text-xs font-mono border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-base-200 outline-none"
                                />
                            </div>
                            <div>
                                <label className="block text-[11px] font-medium text-gray-600 dark:text-gray-400 mb-1">
                                    Order
                                </label>
                                <input
                                    type="number"
                                    value={editingModel.order}
                                    onChange={(e) =>
                                        setEditingModel({
                                            ...editingModel,
                                            order: Number(e.target.value) || 0,
                                        })
                                    }
                                    className="w-full px-2.5 py-1.5 text-xs font-mono border border-gray-300 dark:border-gray-600 rounded-lg bg-white dark:bg-base-200 outline-none"
                                />
                            </div>
                            <div className="flex items-end gap-3 pb-1.5">
                                <label className="flex items-center gap-1.5 text-xs cursor-pointer">
                                    <input
                                        type="checkbox"
                                        className="w-4 h-4 rounded border-gray-300 dark:border-gray-600 accent-emerald-500 cursor-pointer"
                                        checked={editingModel.supportsThinking}
                                        onChange={(e) =>
                                            setEditingModel({
                                                ...editingModel,
                                                supportsThinking: e.target.checked,
                                            })
                                        }
                                    />
                                    Thinking
                                </label>
                                <label className="flex items-center gap-1.5 text-xs cursor-pointer">
                                    <input
                                        type="checkbox"
                                        className="w-4 h-4 rounded border-gray-300 dark:border-gray-600 accent-emerald-500 cursor-pointer"
                                        checked={editingModel.supportsVision}
                                        onChange={(e) =>
                                            setEditingModel({
                                                ...editingModel,
                                                supportsVision: e.target.checked,
                                            })
                                        }
                                    />
                                    Vision
                                </label>
                            </div>
                        </div>

                        <div className="flex justify-end gap-2">
                            <button
                                type="button"
                                onClick={() => {
                                    setEditingModel(null);
                                    setIsNewModel(false);
                                }}
                                className={BTN_SECONDARY}
                            >
                                {t('common.cancel', { defaultValue: 'Cancel' })}
                            </button>
                            <button
                                type="button"
                                onClick={handleSaveEditingModel}
                                disabled={actionLoading === 'upsert-model'}
                                className={BTN_PRIMARY_BLUE}
                            >
                                <Save size={12} />
                                {t('common.save', { defaultValue: 'Save' })}
                            </button>
                        </div>
                    </div>
                )}

                {/* Model List Table */}
                <div className="max-h-72 overflow-y-auto border border-gray-100 dark:border-base-200 rounded-lg divide-y divide-gray-100 dark:divide-base-200">
                    {filteredModels.length === 0 ? (
                        <div className="p-6 text-center text-xs text-gray-400">
                            {t('proxy.asar.no_models', {
                                defaultValue: 'No models in catalog. Click "Sync Pool" or "Add Model".',
                            })}
                        </div>
                    ) : (
                        filteredModels.map((model) => (
                            <div
                                key={model.id}
                                className="px-3 py-2 flex items-center justify-between gap-3 hover:bg-gray-50/80 dark:hover:bg-base-200/50 transition-colors"
                            >
                                <div className="min-w-0 flex-1">
                                    <div className="flex items-center gap-2">
                                        <span className="text-xs font-semibold text-gray-900 dark:text-white truncate">
                                            {model.displayName || model.id}
                                        </span>
                                        <span className="text-[10px] font-mono px-1.5 py-0.5 rounded bg-gray-100 dark:bg-base-300 text-gray-600 dark:text-gray-300">
                                            {model.id}
                                        </span>
                                        {model.targetModel && model.targetModel !== model.id && (
                                            <span className="text-[10px] font-mono text-blue-600 dark:text-blue-400">
                                                → {model.targetModel}
                                            </span>
                                        )}
                                    </div>
                                    <div className="text-[11px] text-gray-400 flex items-center gap-2 mt-0.5">
                                        <span>{model.providerName || model.ownedBy}</span>
                                        <span>•</span>
                                        <span>
                                            {Math.round((model.contextWindow || 0) / 1000)}k ctx
                                        </span>
                                        {model.supportsThinking && (
                                            <>
                                                <span>•</span>
                                                <span className="text-purple-500">Thinking</span>
                                            </>
                                        )}
                                    </div>
                                </div>

                                <div className="flex items-center gap-2 shrink-0">
                                    <input
                                        type="checkbox"
                                        className="toggle toggle-xs toggle-primary"
                                        checked={model.enabled}
                                        disabled={actionLoading === `toggle-${model.id}`}
                                        onChange={(e) =>
                                            handleToggleModel(model.id, e.target.checked)
                                        }
                                    />
                                    <button
                                        type="button"
                                        onClick={() => {
                                            setIsNewModel(false);
                                            setEditingModel({ ...model });
                                        }}
                                        className={cn(BTN_ICON, 'hover:text-blue-500')}
                                        title={t('common.edit', { defaultValue: 'Edit' })}
                                    >
                                        <Edit2 size={13} />
                                    </button>
                                    <button
                                        type="button"
                                        onClick={() => handleDeleteModel(model.id)}
                                        disabled={actionLoading === `delete-${model.id}`}
                                        className={cn(BTN_ICON, 'hover:text-rose-500')}
                                        title={t('common.delete', { defaultValue: 'Delete' })}
                                    >
                                        <Trash2 size={13} />
                                    </button>
                                </div>
                            </div>
                        ))
                    )}
                </div>
            </div>
        </div>
    );
}
