import { useEffect, useState, useMemo, useCallback } from 'react';
import {
    Maximize2, RefreshCw, ShieldAlert, Tag, Users,
    Laptop, Code2, Terminal, Blocks, Mail, Gem, Diamond, Circle, HelpCircle,
    ArrowRight, Check, ChevronDown, Shuffle, Power, Feather, Copy, Pin, X,
    Sparkles, Bot, Zap, RotateCcw
} from 'lucide-react';
import { Gemini } from '@lobehub/icons';
import { useViewStore } from '../../stores/useViewStore';
import { useAccountStore } from '../../stores/useAccountStore';
import { isTauri } from '../../utils/env';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { invoke } from '@tauri-apps/api/core';
import { useTranslation } from 'react-i18next';
import clsx from 'clsx';
import { formatCompactNumber, formatTimeRemaining } from '../../utils/format';
import {
    AsarBridgeStatus,
    AsarStatus,
    AsarTelemetrySnapshot,
    getAsarBridgeStatus,
    getAsarStatus,
    getAsarTelemetry,
    resetAsarSlots,
    setAsarDualTrack,
    startAsarBridge,
    stopAsarBridge,
    toggleAsarHeartbeat,
} from '../../services/asarService';
import {
    DashboardQuotaView,
    getGroupQuotaSummaries,
    GroupQuotaSummary
} from '../../utils/quotaDisplay';
import { getAccountTier, type Account } from '../../types/account';
import { isAccountAbnormal, isAccountNeedsVerification } from '../../utils/accountStatus';
import { getLiveLimitForModel } from '../../utils/liveLimit';
import { isArgonModel } from '../../utils/modelCategory';
import { GroupQuotaCard } from '../accounts/GroupQuotaCard';
import { useConfigStore } from '../../stores/useConfigStore';

type MiniTab = 'classic' | 'ide' | 'asar' | 'agy' | 'acp' | 'pool';

interface MiniQuotaRowProps {
    displayName: string;
    percentage: number;
    colorClass?: 'emerald' | 'cyan' | 'purple';
    tag?: React.ReactNode;
    resetText?: string;
    resetTooltip?: string;
    isWeeklyExhausted?: boolean;
    icon?: React.ReactNode;
    subtitle?: string;
}

function MiniQuotaRow({
    displayName,
    percentage,
    colorClass = 'emerald',
    tag,
    resetText,
    resetTooltip,
    isWeeklyExhausted = false,
    icon,
    subtitle,
}: MiniQuotaRowProps) {
    const p = Math.max(0, Math.min(100, percentage));

    const getStatusColor = (pct: number) => {
        if (isWeeklyExhausted) return 'text-rose-500';
        if (pct >= 50) return 'text-emerald-500';
        if (pct >= 20) return 'text-amber-500';
        return 'text-rose-500';
    };

    const getBarColor = (pct: number) => {
        if (isWeeklyExhausted) return 'bg-gradient-to-r from-rose-400 to-rose-500';
        if (pct >= 50) {
            if (colorClass === 'cyan') return 'bg-gradient-to-r from-cyan-400 to-cyan-500';
            if (colorClass === 'purple') return 'bg-gradient-to-r from-purple-400 to-purple-500';
            return 'bg-gradient-to-r from-emerald-400 to-emerald-500';
        }
        if (pct >= 20) {
            if (colorClass === 'cyan') return 'bg-gradient-to-r from-orange-400 to-orange-500';
            return 'bg-gradient-to-r from-amber-400 to-amber-500';
        }
        return 'bg-gradient-to-r from-rose-400 to-rose-500';
    };

    return (
        <div className="space-y-1.5">
            <div className="flex justify-between items-center">
                <div className="flex items-center gap-1.5 min-w-0">
                    {icon}
                    <span className="text-xs font-semibold text-gray-700 dark:text-gray-300 truncate max-w-[130px]" title={displayName}>
                        {displayName}
                    </span>
                    {subtitle && (
                        <span className="text-[10px] text-gray-400 dark:text-zinc-500 font-normal truncate max-w-[110px]" title={subtitle}>
                            ({subtitle})
                        </span>
                    )}
                </div>
                <div className="flex items-center gap-1.5 shrink-0">
                    {tag}
                    {resetText && (
                        <span className="text-[10px] text-blue-600 dark:text-blue-400 font-mono" title={resetTooltip}>
                            {resetText}
                        </span>
                    )}
                    <span className={clsx("text-xs font-bold font-mono", getStatusColor(p))}>
                        {p}%
                    </span>
                </div>
            </div>
            <div className="w-full bg-gray-100 dark:bg-white/10 rounded-full h-1.5 overflow-hidden">
                <div
                    style={{ width: `${p}%` }}
                    className={clsx("h-full rounded-full transition-all duration-700 ease-out shadow-[0_0_8px_currentColor]", getBarColor(p))}
                />
            </div>
        </div>
    );
}

export default function MiniView() {
    const { setMiniView } = useViewStore();
    const {
        accounts,
        currentAccount,
        activeClients,
        refreshQuota,
        refreshAllQuotas,
        fetchAccounts,
        fetchCurrentAccount,
        switchAccount
    } = useAccountStore();
    const { config, saveConfig, loadConfig } = useConfigStore();
    const { t } = useTranslation();

    const [isRefreshing, setIsRefreshing] = useState(false);
    const [isSwitching, setIsSwitching] = useState(false);
    const [showAccountList, setShowAccountList] = useState(false);
    const [copied, setCopied] = useState(false);
    // 默认是不停留的（Popover 模式），只有用户显式点击图钉才保持常驻悬浮
    const [isPinned, setIsPinned] = useState(false);

    // 默认定位到活跃客户端 Tab，优先 classic/ide/asar/agy/acp，绝不落入 pool
    const [activeTab, setActiveTab] = useState<MiniTab>(() => {
        const saved = localStorage.getItem('mini_view_tab');
        if (saved === 'classic' || saved === 'ide' || saved === 'asar' || saved === 'agy' || saved === 'acp') {
            return saved;
        }
        return 'classic';
    });

    const [asarStatus, setAsarStatus] = useState<AsarStatus | null>(null);
    const [bridgeStatus, setBridgeStatus] = useState<AsarBridgeStatus | null>(null);
    const [asarTelemetry, setAsarTelemetry] = useState<AsarTelemetrySnapshot | null>(null);
    const [asarBusy, setAsarBusy] = useState<string | null>(null);

    const fetchAsarMiniState = useCallback(async () => {
        if (!isTauri()) return;
        try {
            const [st, br, tel] = await Promise.all([
                getAsarStatus().catch(() => null),
                getAsarBridgeStatus().catch(() => null),
                getAsarTelemetry().catch(() => null),
            ]);
            if (st) setAsarStatus(st);
            if (br) setBridgeStatus(br);
            if (tel) setAsarTelemetry(tel);
        } catch {
            // Ignore transient polling errors
        }
    }, []);

    useEffect(() => {
        if (!isTauri()) return;
        fetchAsarMiniState();
        const timer = setInterval(fetchAsarMiniState, 2500);
        return () => clearInterval(timer);
    }, [fetchAsarMiniState]);

    const [quotaView, setQuotaView] = useState<DashboardQuotaView>(() => {
        const saved = localStorage.getItem('dashboard_quota_view');
        return (saved === '5h' || saved === 'weekly' || saved === 'weighted') ? saved : 'weighted';
    });

    useEffect(() => {
        localStorage.setItem('mini_view_tab', activeTab);
    }, [activeTab]);

    useEffect(() => {
        localStorage.setItem('dashboard_quota_view', quotaView);
    }, [quotaView]);

    useEffect(() => {
        if (!config) {
            loadConfig();
        }
    }, [config, loadConfig]);

    useEffect(() => {
        fetchAccounts();
        fetchCurrentAccount();
        const handleFocus = () => {
            loadConfig();
            fetchAccounts();
            fetchCurrentAccount();
        };
        window.addEventListener('focus', handleFocus);
        return () => window.removeEventListener('focus', handleFocus);
    }, [fetchAccounts, fetchCurrentAccount, loadConfig]);

    // Client accounts resolution
    const getClientAccountId = (tab: 'classic' | 'ide' | 'agy' | 'acp') => {
        switch (tab) {
            case 'classic':
                return activeClients?.classic_account_id;
            case 'ide':
                return activeClients?.ide_account_id;
            case 'agy':
                return activeClients?.agy_account_id;
            case 'acp':
                return activeClients?.acp_account_id;
        }
    };

    const displayedAccount = useMemo(() => {
        if (activeTab === 'pool' || activeTab === 'asar') return null;
        const currentClientAccountId = getClientAccountId(activeTab);
        if (currentClientAccountId && accounts.length > 0) {
            const found = accounts.find((a) => a.id === currentClientAccountId);
            if (found) return found;
        }
        // Fallback for classic: if no specific id matched, fallback to currentAccount
        if (activeTab === 'classic' && currentAccount) {
            return currentAccount;
        }
        return null;
    }, [activeTab, activeClients, accounts, currentAccount]);

    // 选中/切换不同客户端 Tab 或账号时，实时驱动托盘标题和提示显示对应账号的核心配额
    useEffect(() => {
        if (isTauri()) {
            invoke('update_tray_for_account', { accountId: displayedAccount?.id ?? null }).catch(() => {});
        }
    }, [displayedAccount?.id, displayedAccount?.quota]);

    const tier = displayedAccount ? getAccountTier(displayedAccount) : 'free';

    const groupSummaries: GroupQuotaSummary[] = useMemo(() => {
        if (!displayedAccount) return [];
        return getGroupQuotaSummaries(
            displayedAccount.quota?.quota_groups,
            displayedAccount.quota?.models,
            tier
        );
    }, [displayedAccount, tier]);

    const isGroupProtected = (key: 'gemini' | 'claude' | 'argon') => {
        if (!displayedAccount?.protected_models || displayedAccount.protected_models.length === 0)
            return false;
        if (key === 'argon') {
            return displayedAccount.protected_models.some(isArgonModel);
        }
        if (key === 'gemini') {
            return displayedAccount.protected_models.some(
                (m: string) => m.toLowerCase().includes('gemini') && !isArgonModel(m)
            );
        }
        return displayedAccount.protected_models.some(
            (m: string) => m.toLowerCase().includes('claude') || m.toLowerCase().includes('gpt')
        );
    };

    const handleRefresh = async () => {
        if (isRefreshing) return;
        setIsRefreshing(true);
        try {
            if (activeTab === 'asar') {
                await fetchAsarMiniState();
            } else if (activeTab === 'pool') {
                await refreshAllQuotas();
                await fetchAccounts();
                await fetchCurrentAccount();
            } else if (displayedAccount) {
                await refreshQuota(displayedAccount.id);
                await fetchCurrentAccount();
            }
        } catch (e) {
            console.error('Refresh failed:', e);
        } finally {
            setTimeout(() => setIsRefreshing(false), 800);
        }
    };

    const handleMiniToggleHeartbeat = async () => {
        if (asarBusy) return;
        setAsarBusy('hb');
        try {
            const next = !(asarStatus?.heartbeat_active ?? false);
            const st = await toggleAsarHeartbeat(next, false);
            setAsarStatus(st);
        } catch (e) {
            console.error('Failed to toggle ASAR heartbeat:', e);
        } finally {
            setAsarBusy(null);
        }
    };

    const handleMiniToggleBridge = async () => {
        if (asarBusy) return;
        setAsarBusy('bridge');
        try {
            if (bridgeStatus?.running) {
                await stopAsarBridge();
            } else {
                await startAsarBridge(asarStatus?.bridge_port || bridgeStatus?.port || 8989);
            }
            await fetchAsarMiniState();
        } catch (e) {
            console.error('Failed to toggle ASAR bridge:', e);
        } finally {
            setAsarBusy(null);
        }
    };

    const handleMiniToggleDualTrack = async () => {
        if (asarBusy) return;
        setAsarBusy('dt');
        try {
            const next = !(asarTelemetry?.dual_track_enabled ?? true);
            const snap = await setAsarDualTrack(next);
            setAsarTelemetry(snap);
        } catch (e) {
            console.error('Failed to toggle Dual-Track:', e);
        } finally {
            setAsarBusy(null);
        }
    };

    const handleMiniResetSlots = async () => {
        if (asarBusy) return;
        setAsarBusy('slots');
        try {
            const snap = await resetAsarSlots();
            setAsarTelemetry(snap);
        } catch (e) {
            console.error('Failed to reset ASAR slots:', e);
        } finally {
            setAsarBusy(null);
        }
    };

    // [Fix #3958] 定时全量刷新已完全下沉至后端调度器 (modules::auto_refresh)；
    // 全局 accounts://refreshed 广播事件由 App.tsx 根级统一监听并调用 fetchAccounts(true)，
    // 悬浮窗无需额外绑定重复监听器，彻底杜绝多窗口并发与卸载内存泄漏。

    useEffect(() => {
        if (isTauri()) {
            invoke<boolean>('is_mini_view_pinned')
                .then((pinned) => setIsPinned(pinned))
                .catch(() => {});
        }
    }, []);

    // 保持迷你窗口置顶状态与图钉同步
    useEffect(() => {
        if (isTauri()) {
            getCurrentWindow().setAlwaysOnTop(isPinned).catch(() => {});
        }
    }, [isPinned]);

    // 支持 ESC 快捷键一键收起迷你悬浮窗至托盘
    useEffect(() => {
        const handleKeyDown = (e: KeyboardEvent) => {
            if (e.key === 'Escape') {
                if (isTauri()) {
                    invoke('hide_mini_view').catch(() => {});
                }
            }
        };
        window.addEventListener('keydown', handleKeyDown);
        return () => window.removeEventListener('keydown', handleKeyDown);
    }, []);

    const handleTogglePin = async () => {
        const next = !isPinned;
        setIsPinned(next);
        if (isTauri()) {
            try {
                await invoke('set_mini_view_pinned', { pinned: next });
                await getCurrentWindow().setAlwaysOnTop(next);
            } catch (e) {
                console.error('Failed to toggle pin always-on-top:', e);
            }
        }
    };

    const handleClose = async () => {
        if (isTauri()) {
            await invoke('hide_mini_view').catch(() => {});
        } else {
            setMiniView(false);
        }
    };

    const handleMaximize = () => {
        if (isTauri()) {
            // 原子化唤起主控制台并在未置顶时安全收起迷你视窗，由后端主线程统一调度避免消息死锁
            invoke('switch_to_main_window').catch((err) => {
                console.error('Failed to switch to main window:', err);
            });
        } else {
            setMiniView(false);
        }
    };

    const handleQuitApp = async () => {
        if (isTauri()) {
            try {
                await invoke('exit_app');
            } catch (e) {
                console.error('Failed to exit app:', e);
            }
        }
    };

    const handleToggleLightweight = async () => {
        if (!config) return;
        try {
            await saveConfig({
                ...config,
                lightweight_mode: !config.lightweight_mode,
            }, true);
        } catch (e) {
            console.error('Failed to toggle lightweight mode:', e);
        }
    };

    const handleCopyEmail = (email?: string) => {
        if (!email) return;
        navigator.clipboard.writeText(email);
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
    };

    const availableAccounts = useMemo(() => {
        return accounts.filter(
            a => !a.disabled && !a.proxy_disabled && !isAccountAbnormal(a) && !isAccountNeedsVerification(a)
        );
    }, [accounts]);

    const switchableAccounts = useMemo(() => {
        return accounts.filter(
            a => !a.disabled && !a.banned && !isAccountNeedsVerification(a)
        );
    }, [accounts]);

    const activePool = useMemo(() => {
        return availableAccounts.length > 0
            ? availableAccounts
            : accounts.filter(a => !isAccountAbnormal(a));
    }, [availableAccounts, accounts]);

    const handleSwitchAccount = async (accountId: string) => {
        if (isSwitching || activeTab === 'pool' || activeTab === 'asar') return;
        setIsSwitching(true);
        setShowAccountList(false);
        try {
            await switchAccount(accountId, activeTab);
            await fetchAccounts();
            await fetchCurrentAccount();
        } catch (err) {
            console.error('Failed to switch account in mini view:', err);
        } finally {
            setTimeout(() => setIsSwitching(false), 800);
        }
    };

    const handleQuickNextAccount = async () => {
        if (isSwitching || activeTab === 'pool' || activeTab === 'asar' || accounts.length <= 1) return;
        const poolToUse = availableAccounts.length > 0 ? availableAccounts : activePool;
        if (poolToUse.length === 0) return;
        const currentId = displayedAccount?.id;
        const currentIndex = poolToUse.findIndex((a: Account) => a.id === currentId);
        const nextIndex = (currentIndex + 1) % poolToUse.length;
        const nextAcc = poolToUse[nextIndex];
        if (nextAcc) {
            await handleSwitchAccount(nextAcc.id);
        }
    };

    const poolMetrics = useMemo(() => {
        const calculateGroupMetrics = (target: 'gemini' | 'claude' | 'argon') => {
            if (activePool.length === 0) {
                return {
                    percentage: 0,
                    count: 0,
                    zeroCount: 0,
                    constrainedCount: 0,
                    coolingCount: 0,
                    nearestReset: null as string | null,
                    isWeeklyExhausted: false,
                };
            }

            let sum5h = 0, count5h = 0;
            let sumWeekly = 0, countWeekly = 0;
            let sumWeighted = 0, totalWeight = 0;
            let zeroCount = 0;
            let constrainedCount = 0;
            let coolingCount = 0;
            let nearestResetMs: number | null = null;
            let matchedAccountsCount = 0;

            for (const acc of activePool) {
                const tier = getAccountTier(acc);
                const summaries = getGroupQuotaSummaries(acc.quota?.quota_groups, acc.quota?.models, tier);
                const group = summaries.find((s) => s.key === target);
                if (!group) continue;

                matchedAccountsCount++;

                const q5h = group.fiveHour?.percentage ?? null;
                const qWeekly = group.weekly?.percentage ?? null;
                const reset5h = group.fiveHour?.resetTime;
                const resetWeekly = group.weekly?.resetTime;

                if (q5h !== null && q5h >= 0) {
                    sum5h += q5h;
                    count5h++;
                }

                if (qWeekly !== null && qWeekly >= 0) {
                    sumWeekly += qWeekly;
                    countWeekly++;
                    if (qWeekly <= 0) {
                        zeroCount++;
                    }
                }

                // 双向木桶约束计算 (对齐 Dashboard.tsx)
                if (qWeekly !== null && q5h !== null && qWeekly < q5h) {
                    constrainedCount++;
                }
                if (qWeekly !== null && qWeekly > 0 && q5h !== null && q5h <= 0) {
                    coolingCount++;
                }

                // 综合短板有效值 (加权基准)
                let effectiveVal = q5h ?? 0;
                if (qWeekly !== null && qWeekly <= 0) {
                    effectiveVal = 0;
                } else if (qWeekly !== null && q5h !== null) {
                    effectiveVal = Math.min(q5h, qWeekly);
                } else if (qWeekly !== null) {
                    effectiveVal = qWeekly;
                }

                const weight = tier === 'ultra' ? 2.0 : tier === 'pro' ? 1.5 : 1.0;
                sumWeighted += effectiveVal * weight;
                totalWeight += weight;

                // 依据当前 quotaView 选出最近重置时间
                let candResetTime: string | undefined = undefined;
                if (quotaView === '5h') {
                    candResetTime = reset5h || resetWeekly;
                } else if (quotaView === 'weekly') {
                    candResetTime = resetWeekly || reset5h;
                } else {
                    candResetTime = (qWeekly !== null && qWeekly <= 0) ? resetWeekly : (reset5h || resetWeekly);
                }

                if (candResetTime) {
                    const ms = new Date(candResetTime).getTime();
                    if (!isNaN(ms) && ms > Date.now()) {
                        if (nearestResetMs === null || ms < nearestResetMs) {
                            nearestResetMs = ms;
                        }
                    }
                }
            }

            const avg5h = count5h > 0 ? Math.round(sum5h / count5h) : 0;
            const avgWeekly = countWeekly > 0 ? Math.round(sumWeekly / countWeekly) : 0;
            const weightedEffective = totalWeight > 0 ? Math.round(sumWeighted / totalWeight) : 0;

            const percentage = quotaView === '5h'
                ? avg5h
                : quotaView === 'weekly'
                    ? avgWeekly
                    : weightedEffective;

            const nearestReset = nearestResetMs ? new Date(nearestResetMs).toISOString() : null;

            return {
                percentage,
                count: matchedAccountsCount,
                zeroCount,
                constrainedCount,
                coolingCount,
                nearestReset,
                isWeeklyExhausted: zeroCount > 0,
            };
        };

        return {
            gemini: calculateGroupMetrics('gemini'),
            claude: calculateGroupMetrics('claude'),
            argon: calculateGroupMetrics('argon'),
        };
    }, [activePool, quotaView]);

    const renderPoolMetricRow = (
        displayName: string,
        metric: {
            percentage: number;
            count: number;
            zeroCount: number;
            constrainedCount: number;
            coolingCount: number;
            nearestReset: string | null;
            isWeeklyExhausted: boolean;
        },
        colorClass: 'emerald' | 'cyan' | 'purple',
        icon?: React.ReactNode,
        subtitle?: string
    ) => {
        const tag = (
            <div className="flex items-center gap-1 shrink-0">
                {metric.zeroCount > 0 && (
                    <span
                        className="text-[9px] font-bold text-rose-500 bg-rose-50 dark:bg-rose-900/30 px-1 py-0.5 rounded"
                        title={t('dashboard.mini_pool_zero_tooltip', '{{count}} 个账号额度见底/熔断', { count: metric.zeroCount })}
                    >
                        {t('dashboard.mini_pool_zero_tag', '{{count}}枯竭', { count: metric.zeroCount })}
                    </span>
                )}
                {quotaView === '5h' && metric.constrainedCount > 0 && (
                    <span
                        className="text-[9px] font-bold text-amber-600 dark:text-amber-400"
                        title={t('dashboard.mini_pool_constrained_tooltip', '{{count}} 个账号 5H 额度受 7 天周配额短板压制', { count: metric.constrainedCount })}
                    >
                        [{t('dashboard.mini_tag_constrained', '周限')}: {t('dashboard.account_count_unit', '{{count}}个', { count: metric.constrainedCount })}]
                    </span>
                )}
                {quotaView === 'weekly' && metric.coolingCount > 0 && (
                    <span
                        className="text-[9px] font-bold text-blue-600 dark:text-blue-400"
                        title={t('dashboard.mini_pool_cooling_tooltip', '{{count}} 个账号处于 5H 冷却态', { count: metric.coolingCount })}
                    >
                        [{t('dashboard.mini_tag_cooling', '冷却')}: {t('dashboard.account_count_unit', '{{count}}个', { count: metric.coolingCount })}]
                    </span>
                )}
            </div>
        );

        return (
            <MiniQuotaRow
                key={displayName}
                displayName={displayName}
                percentage={metric.percentage}
                colorClass={colorClass}
                tag={tag}
                icon={icon}
                subtitle={subtitle}
                isWeeklyExhausted={metric.isWeeklyExhausted && metric.percentage <= 0}
                resetText={metric.nearestReset ? `R: ${formatTimeRemaining(metric.nearestReset)}` : undefined}
                resetTooltip={metric.nearestReset ? `${t('dashboard.nearest_reset', '最近重置')}: ${new Date(metric.nearestReset).toLocaleTimeString()}` : undefined}
            />
        );
    };

    const getTabStatus = (tabKey: 'classic' | 'ide' | 'agy' | 'acp') => {
        const id = getClientAccountId(tabKey);
        if (!id && tabKey === 'classic' && currentAccount) {
            const isHealthy = !currentAccount.disabled && !isAccountAbnormal(currentAccount);
            return { hasAccount: true, isHealthy };
        }
        if (!id) return { hasAccount: false, isHealthy: false };
        const acc = accounts.find((a) => a.id === id);
        if (!acc) return { hasAccount: false, isHealthy: false };
        const isHealthy = !acc.disabled && !isAccountAbnormal(acc);
        return { hasAccount: true, isHealthy };
    };

    const tabs = [
        {
            key: 'classic' as const,
            label: t('accounts.client_classic', '经典款'),
            icon: Laptop,
            tooltip: t('accounts.client_classic_tooltip', '当前 Antigravity 经典款客户端登录账号'),
            activeDot: 'bg-blue-400',
        },
        {
            key: 'ide' as const,
            label: t('accounts.client_ide', 'IDE'),
            icon: Code2,
            tooltip: t('accounts.client_ide_tooltip', '当前 Antigravity IDE 客户端登录账号'),
            activeDot: 'bg-sky-400',
        },
        {
            key: 'asar' as const,
            label: 'ASAR',
            icon: Zap,
            tooltip: 'ASAR Zero-Reload Bridge (127.0.0.1:8989)',
            activeDot: 'bg-emerald-400',
        },
        {
            key: 'agy' as const,
            label: t('accounts.client_cli', 'CLI'),
            icon: Terminal,
            tooltip: t('accounts.client_cli_tooltip', '当前 Antigravity CLI (agy) 登录账号'),
            activeDot: 'bg-emerald-400',
        },
        {
            key: 'acp' as const,
            label: t('accounts.client_acp', 'Plugin'),
            icon: Blocks,
            tooltip: t('accounts.client_acp_tooltip', 'Active account in Antigravity IDE Plugin (ACP)'),
            activeDot: 'bg-violet-400',
        },
        {
            key: 'pool' as const,
            label: t('dashboard.mini_scope_pool_short', '池'),
            icon: Users,
            tooltip: t('dashboard.pool_matrix_view', '全账号池矩阵'),
            activeDot: 'bg-indigo-400',
        },
    ];

    return (
        <div className={clsx(
            "w-full h-full flex flex-col overflow-hidden select-none",
            isTauri()
                ? "bg-white dark:bg-[#15161a] rounded-[8px]"
                : "bg-white/95 dark:bg-[#15161a]/95 backdrop-blur-2xl rounded-2xl border border-gray-200/80 dark:border-white/[0.12] shadow-2xl"
        )}>
            {/* 1. 顶栏：macOS 风格分段 Tab 与快捷工具 */}
            <div
                className={clsx(
                    "flex-none flex items-center justify-between gap-1 px-2 py-1.5 bg-gray-50/80 dark:bg-white/[0.03] border-b border-gray-100 dark:border-white/[0.06] select-none cursor-default",
                    isTauri() ? "rounded-t-[8px]" : "rounded-t-2xl"
                )}
                data-tauri-drag-region
                onMouseDown={(e) => {
                    const target = e.target as HTMLElement;
                    if (target.closest('button, a, input, select, textarea, [data-no-drag="true"]')) {
                        return;
                    }
                    if (isTauri()) {
                        getCurrentWindow().startDragging().catch(() => {});
                    }
                }}
            >
                {/* 应用客户端 Tab + ASAR + 全池胶囊 (min-w-0 可横向滑动，绝不挤压右侧图钉按钮) */}
                <div
                    className="flex items-center min-w-0 overflow-x-auto [scrollbar-width:none] [&::-webkit-scrollbar]:hidden bg-black/[0.04] dark:bg-white/[0.07] p-0.5 rounded-xl gap-0.5"
                    data-tauri-drag-region="false"
                    onMouseDown={(e) => e.stopPropagation()}
                >
                    {tabs.map((tab) => {
                        const Icon = tab.icon;
                        const isActive = activeTab === tab.key;
                        const status = (tab.key !== 'pool' && tab.key !== 'asar') ? getTabStatus(tab.key) : null;
                        const isAsarReady = Boolean(asarStatus?.is_patched && bridgeStatus?.running);
                        const isAsarStreaming = (asarTelemetry?.in_flight ?? 0) > 0;
                        return (
                            <button
                                key={tab.key}
                                type="button"
                                data-tauri-drag-region="false"
                                onMouseDown={(e) => e.stopPropagation()}
                                onClick={(e) => {
                                    e.stopPropagation();
                                    setActiveTab(tab.key);
                                    setShowAccountList(false);
                                }}
                                title={tab.tooltip}
                                className={clsx(
                                    "relative flex items-center gap-1 px-1.5 py-0.5 rounded-lg text-[10.5px] whitespace-nowrap shrink-0 transition-all cursor-pointer outline-none focus:outline-none focus:ring-0 focus-visible:ring-0 select-none",
                                    isActive
                                        ? tab.key === 'asar'
                                            ? "bg-white dark:bg-zinc-800 text-emerald-600 dark:text-emerald-400 font-bold shadow-xs border border-emerald-500/25"
                                            : "bg-white dark:bg-zinc-800 text-blue-600 dark:text-blue-400 font-bold shadow-xs border border-black/[0.06] dark:border-white/[0.1]"
                                        : "text-gray-500 dark:text-gray-400 hover:text-gray-900 dark:hover:text-white font-medium hover:bg-black/[0.02] dark:hover:bg-white/[0.03]"
                                )}
                            >
                                <Icon className={clsx("w-3 h-3 shrink-0", tab.key === 'asar' && isAsarReady && "text-emerald-500")} />
                                <span className="shrink-0">{tab.label}</span>
                                {tab.key === 'asar' && (
                                    <span
                                        className={clsx(
                                            "w-1.5 h-1.5 rounded-full shrink-0 transition-colors",
                                            isAsarStreaming
                                                ? "bg-emerald-500 animate-ping"
                                                : isAsarReady
                                                    ? "bg-emerald-500 shadow-[0_0_6px_rgba(16,185,129,0.6)]"
                                                    : bridgeStatus?.running || asarStatus?.is_patched
                                                        ? "bg-amber-500"
                                                        : "bg-gray-300 dark:bg-zinc-600"
                                        )}
                                    />
                                )}
                                {status && (
                                    <span
                                        className={clsx(
                                            "w-1.5 h-1.5 rounded-full shrink-0 transition-colors",
                                            !status.hasAccount
                                                ? "bg-gray-300 dark:bg-zinc-600"
                                                : status.isHealthy
                                                    ? "bg-emerald-500 shadow-[0_0_6px_rgba(16,185,129,0.5)]"
                                                    : "bg-amber-500 shadow-[0_0_6px_rgba(245,158,11,0.5)]"
                                        )}
                                    />
                                )}
                            </button>
                        );
                    })}
                </div>

                {/* 居中透明拖拽区：按住可拖动整个悬浮窗 */}
                <div
                    className="flex-1 h-full min-w-[4px] cursor-move"
                    data-tauri-drag-region
                    onMouseDown={(e) => {
                        e.preventDefault();
                        if (isTauri()) {
                            getCurrentWindow().startDragging().catch(() => {});
                        }
                    }}
                />

                {/* 右侧核心操作：刷新 / 图钉固定 / 关闭收起 */}
                <div
                    className="flex items-center gap-0.5 shrink-0"
                    data-tauri-drag-region="false"
                    onMouseDown={(e) => e.stopPropagation()}
                >
                    <button
                        type="button"
                        data-tauri-drag-region="false"
                        onMouseDown={(e) => e.stopPropagation()}
                        onClick={handleRefresh}
                        className="p-1 rounded-lg text-gray-500 hover:text-gray-900 dark:text-gray-400 dark:hover:text-white hover:bg-black/[0.04] dark:hover:bg-white/[0.08] transition-colors cursor-pointer outline-none focus:outline-none focus:ring-0 focus-visible:ring-0"
                        title={t('common.refresh', '刷新当前额度')}
                    >
                        <RefreshCw size={12} className={clsx(isRefreshing && "animate-spin text-blue-500")} />
                    </button>
                    <button
                        type="button"
                        data-tauri-drag-region="false"
                        onMouseDown={(e) => e.stopPropagation()}
                        onClick={handleTogglePin}
                        className={clsx(
                            "p-1 rounded-lg border transition-all cursor-pointer outline-none focus:outline-none focus:ring-0 focus-visible:ring-0",
                            isPinned
                                ? "text-blue-600 bg-blue-50 border-blue-200 dark:bg-blue-500/20 dark:text-blue-300 dark:border-blue-500/40 shadow-2xs"
                                : "text-gray-500 border-transparent hover:text-gray-800 dark:text-gray-300 dark:hover:text-white hover:bg-black/[0.05] dark:hover:bg-white/[0.08]"
                        )}
                        title={isPinned ? t('tray.unpin_desc', 'Đã ghim luôn nổi (bấm để bỏ ghim)') : t('tray.pin_desc', 'Ghim cửa sổ luôn nổi trên màn hình (Always on Top)')}
                    >
                        <Pin size={12} className={clsx("transition-transform", isPinned ? "fill-current rotate-45" : "")} />
                    </button>
                    <button
                        type="button"
                        data-tauri-drag-region="false"
                        onMouseDown={(e) => e.stopPropagation()}
                        onClick={handleClose}
                        className="p-1 rounded-lg text-gray-400 hover:text-gray-700 dark:hover:text-gray-200 hover:bg-black/[0.04] dark:hover:bg-white/[0.08] transition-colors cursor-pointer outline-none focus:outline-none focus:ring-0 focus-visible:ring-0"
                        title={t('common.close', '收起至托盘 (Esc)')}
                    >
                        <X size={13} />
                    </button>
                </div>
            </div>

            {/* 2. 核心内容区：尺寸固定，杜绝任何外部窗体拉伸 */}
            <div className="flex-1 overflow-y-auto overflow-x-hidden p-3 space-y-2.5 scrollbar-thin scrollbar-track-transparent scrollbar-thumb-gray-200 dark:scrollbar-thumb-white/10">
                {(activeTab === 'classic' || activeTab === 'ide') && bridgeStatus?.running && asarStatus?.heartbeat_active && (
                    <button
                        type="button"
                        onClick={() => setActiveTab('asar')}
                        className="w-full flex items-center justify-between gap-2 px-2.5 py-1.5 rounded-xl bg-emerald-50/90 dark:bg-emerald-950/35 hover:bg-emerald-100/80 dark:hover:bg-emerald-950/55 border border-emerald-200/80 dark:border-emerald-700/40 text-[10.5px] text-emerald-800 dark:text-emerald-300 transition-colors cursor-pointer"
                        title="Bấm để mở bảng điều khiển nhanh ASAR Zero-Reload Bridge"
                    >
                        <div className="flex items-center gap-1.5 min-w-0 truncate">
                            <span
                                className={clsx(
                                    "w-1.5 h-1.5 rounded-full shrink-0",
                                    (asarTelemetry?.in_flight ?? 0) > 0
                                        ? "bg-emerald-500 animate-ping"
                                        : "bg-emerald-500"
                                )}
                            />
                            <span className="font-semibold truncate">
                                ⚡ ASAR :{bridgeStatus.port} đang gánh:{' '}
                                <span className="font-mono">
                                    {asarTelemetry?.last_active_email || 'Sẵn sàng'}
                                </span>
                            </span>
                        </div>
                        <span className="shrink-0 font-mono font-bold text-[10px] px-1.5 py-0.5 rounded bg-emerald-500/15 text-emerald-700 dark:text-emerald-300">
                            {asarTelemetry?.in_flight ?? 0} luồng
                        </span>
                    </button>
                )}

                {activeTab === 'asar' ? (
                    /* ASAR Zero-Reload Bridge Mini Quick-View */
                    <div className="space-y-2.5">
                        {/* Quick Toggles Row */}
                        <div className="grid grid-cols-4 gap-1.5">
                            {/* Patch Status */}
                            <div className="p-2 rounded-xl bg-black/[0.02] dark:bg-white/[0.04] border border-black/[0.05] dark:border-white/[0.08] flex flex-col justify-between gap-1">
                                <span className="text-[9.5px] text-gray-500 dark:text-gray-400 truncate">app.asar</span>
                                <span
                                    className={clsx(
                                        "text-[10px] font-bold truncate",
                                        asarStatus?.is_patched
                                            ? "text-emerald-600 dark:text-emerald-400"
                                            : "text-gray-500 dark:text-gray-400"
                                    )}
                                >
                                    {asarStatus?.is_patched ? '✓ Patched' : 'Unpatched'}
                                </span>
                            </div>

                            {/* Heartbeat Toggle */}
                            <button
                                type="button"
                                disabled={asarBusy !== null}
                                onClick={handleMiniToggleHeartbeat}
                                className={clsx(
                                    "p-2 rounded-xl border text-left flex flex-col justify-between gap-1 transition-all cursor-pointer",
                                    asarStatus?.heartbeat_active
                                        ? "bg-emerald-50/80 dark:bg-emerald-950/30 border-emerald-300/70 dark:border-emerald-700/50"
                                        : "bg-black/[0.02] dark:bg-white/[0.04] border-black/[0.05] dark:border-white/[0.08]"
                                )}
                                title="Bật/tắt Heartbeat (ag_rot.act)"
                            >
                                <span className="text-[9.5px] text-gray-500 dark:text-gray-400 truncate">Heartbeat</span>
                                <span
                                    className={clsx(
                                        "text-[10px] font-bold",
                                        asarStatus?.heartbeat_active
                                            ? "text-emerald-600 dark:text-emerald-400"
                                            : "text-gray-500 dark:text-gray-400"
                                    )}
                                >
                                    {asarStatus?.heartbeat_active ? '● ON' : '○ OFF'}
                                </span>
                            </button>

                            {/* Bridge Server Toggle */}
                            <button
                                type="button"
                                disabled={asarBusy !== null}
                                onClick={handleMiniToggleBridge}
                                className={clsx(
                                    "p-2 rounded-xl border text-left flex flex-col justify-between gap-1 transition-all cursor-pointer",
                                    bridgeStatus?.running
                                        ? "bg-blue-50/80 dark:bg-blue-950/30 border-blue-300/70 dark:border-blue-700/50"
                                        : "bg-black/[0.02] dark:bg-white/[0.04] border-black/[0.05] dark:border-white/[0.08]"
                                )}
                                title="Bật/tắt cổng Bridge :8989"
                            >
                                <span className="text-[9.5px] text-gray-500 dark:text-gray-400 truncate">
                                    :{bridgeStatus?.port || asarStatus?.bridge_port || 8989}
                                </span>
                                <span
                                    className={clsx(
                                        "text-[10px] font-bold",
                                        bridgeStatus?.running
                                            ? "text-blue-600 dark:text-blue-400"
                                            : "text-gray-500 dark:text-gray-400"
                                    )}
                                >
                                    {bridgeStatus?.running ? '● Running' : '○ Stopped'}
                                </span>
                            </button>

                            {/* Dual-Track Toggle */}
                            <button
                                type="button"
                                disabled={asarBusy !== null}
                                onClick={handleMiniToggleDualTrack}
                                className={clsx(
                                    "p-2 rounded-xl border text-left flex flex-col justify-between gap-1 transition-all cursor-pointer",
                                    (asarTelemetry?.dual_track_enabled ?? true)
                                        ? "bg-amber-50/80 dark:bg-amber-950/30 border-amber-300/70 dark:border-amber-700/50"
                                        : "bg-black/[0.02] dark:bg-white/[0.04] border-black/[0.05] dark:border-white/[0.08]"
                                )}
                                title="Bật/tắt Dual-Track (Tách làn Claude Lead & Gemini Worker)"
                            >
                                <span className="text-[9.5px] text-gray-500 dark:text-gray-400 truncate">Dual-Track</span>
                                <span
                                    className={clsx(
                                        "text-[10px] font-bold",
                                        (asarTelemetry?.dual_track_enabled ?? true)
                                            ? "text-amber-600 dark:text-amber-400"
                                            : "text-gray-500 dark:text-gray-400"
                                    )}
                                >
                                    {(asarTelemetry?.dual_track_enabled ?? true) ? '● Active' : '○ Off'}
                                </span>
                            </button>
                        </div>

                        {/* Live Active Account & Dual-Track Slots Card */}
                        <div className="bg-gradient-to-br from-emerald-500/[0.06] to-cyan-500/[0.04] dark:from-emerald-500/[0.1] dark:to-cyan-500/[0.05] rounded-xl p-2.5 border border-emerald-500/20 space-y-2">
                            <div className="flex items-center justify-between gap-2">
                                <div className="flex items-center gap-1.5 min-w-0 flex-1">
                                    <span
                                        className={clsx(
                                            "w-2 h-2 rounded-full shrink-0",
                                            (asarTelemetry?.in_flight ?? 0) > 0
                                                ? "bg-emerald-500 animate-ping"
                                                : bridgeStatus?.running
                                                    ? "bg-emerald-500"
                                                    : "bg-gray-400"
                                        )}
                                    />
                                    <span className="text-[11px] font-bold text-gray-800 dark:text-zinc-100 truncate">
                                        ⚡ Đang gánh IDE:{' '}
                                        <span className="text-emerald-600 dark:text-emerald-400 font-mono">
                                            {asarTelemetry?.last_active_email || 'Sẵn sàng'}
                                        </span>
                                    </span>
                                </div>
                                {asarTelemetry?.last_active_model && (
                                    <span className="shrink-0 text-[9.5px] font-mono px-1.5 py-0.5 rounded bg-emerald-500/15 text-emerald-700 dark:text-emerald-300 max-w-[115px] truncate">
                                        {asarTelemetry.last_active_model}
                                    </span>
                                )}
                            </div>

                            {(asarTelemetry?.dual_track_enabled ?? true) && (
                                <div className="pt-1.5 border-t border-emerald-500/15 flex items-center justify-between gap-1.5 text-[10px]">
                                    <div className="flex flex-col gap-0.5 min-w-0 flex-1">
                                        <div className="truncate text-amber-700 dark:text-amber-300">
                                            👑 Claude:{' '}
                                            <span className="font-mono font-semibold">
                                                {asarTelemetry?.claude_slot_email || 'Auto'}
                                            </span>
                                        </div>
                                        <div className="truncate text-cyan-700 dark:text-cyan-300">
                                            ⚡ Gemini:{' '}
                                            <span className="font-mono font-semibold">
                                                {asarTelemetry?.gemini_slot_email || 'Auto'}
                                            </span>
                                        </div>
                                    </div>
                                    <button
                                        type="button"
                                        disabled={asarBusy !== null}
                                        onClick={handleMiniResetSlots}
                                        className="shrink-0 flex items-center gap-1 px-2 py-1 rounded-lg text-[10px] font-semibold bg-white/80 dark:bg-zinc-800/90 hover:bg-white dark:hover:bg-zinc-700 text-gray-700 dark:text-gray-200 border border-black/[0.08] dark:border-white/10 transition-colors cursor-pointer"
                                        title="Reset các slot ghim Dual-Track"
                                    >
                                        <RotateCcw size={10} />
                                        <span>Reset Slots</span>
                                    </button>
                                </div>
                            )}
                        </div>

                        {/* Compact 4-Metric Row */}
                        <div className="grid grid-cols-4 gap-1.5 text-center">
                            <div className="p-2 rounded-xl bg-black/[0.02] dark:bg-white/[0.04] border border-black/[0.05] dark:border-white/[0.08]">
                                <div className="text-[9.5px] text-gray-500 dark:text-gray-400 truncate">Đang chạy</div>
                                <div className="text-xs font-bold font-mono text-emerald-600 dark:text-emerald-400 mt-0.5">
                                    {asarTelemetry?.in_flight ?? 0}
                                </div>
                            </div>
                            <div className="p-2 rounded-xl bg-black/[0.02] dark:bg-white/[0.04] border border-black/[0.05] dark:border-white/[0.08]">
                                <div className="text-[9.5px] text-gray-500 dark:text-gray-400 truncate">Tổng Req</div>
                                <div className="text-xs font-bold font-mono text-gray-800 dark:text-zinc-100 mt-0.5">
                                    {formatCompactNumber(asarTelemetry?.total_requests ?? 0)}
                                </div>
                            </div>
                            <div className="p-2 rounded-xl bg-black/[0.02] dark:bg-white/[0.04] border border-black/[0.05] dark:border-white/[0.08]">
                                <div className="text-[9.5px] text-gray-500 dark:text-gray-400 truncate">Tự xoay</div>
                                <div className="text-xs font-bold font-mono text-amber-600 dark:text-amber-400 mt-0.5">
                                    {asarTelemetry?.failover_count ?? 0}
                                </div>
                            </div>
                            <div className="p-2 rounded-xl bg-black/[0.02] dark:bg-white/[0.04] border border-black/[0.05] dark:border-white/[0.08]" title="Input / Output Tokens">
                                <div className="text-[9.5px] text-gray-500 dark:text-gray-400 truncate">Tokens</div>
                                <div className="text-[10.5px] font-bold font-mono text-blue-600 dark:text-blue-400 mt-0.5 truncate">
                                    {formatCompactNumber(asarTelemetry?.total_input_tokens ?? 0)}/
                                    {formatCompactNumber(asarTelemetry?.total_output_tokens ?? 0)}
                                </div>
                            </div>
                        </div>
                    </div>
                ) : activeTab === 'pool' ? (
                    /* 全池生产力矩阵视图 */
                    accounts.length === 0 ? (
                        <div className="h-full flex flex-col items-center justify-center text-center opacity-50 space-y-2 py-6">
                            <ShieldAlert size={32} />
                            <p className="text-sm">{t('dashboard.no_accounts', '账号池为空')}</p>
                        </div>
                    ) : (
                        <div className="space-y-2.5">
                            {/* 全池模式切换条 */}
                            <div className="flex items-center justify-between pb-1 border-b border-gray-100 dark:border-white/5">
                                <span className="text-xs font-semibold text-gray-700 dark:text-gray-300 flex items-center gap-1.5">
                                    <Users className="w-3.5 h-3.5 text-indigo-500" />
                                    <span>{t('dashboard.account_pool', '全账号池矩阵')}</span>
                                    <span className="text-[10px] font-mono text-gray-400 font-normal">({availableAccounts.length}/{accounts.length})</span>
                                </span>
                                <div className="flex items-center bg-black/[0.04] dark:bg-white/[0.08] rounded-lg p-0.5 text-[10px] font-bold">
                                    <button
                                        onClick={() => setQuotaView('weighted')}
                                        className={clsx(
                                            "px-2 py-0.5 rounded-md transition-all cursor-pointer",
                                            quotaView === 'weighted' ? "bg-indigo-600 text-white shadow-xs" : "text-gray-500 dark:text-gray-400 hover:text-gray-900 dark:hover:text-white"
                                        )}
                                        title={t('dashboard.view_mode_title_weighted', '综合加权')}
                                    >
                                        {t('dashboard.mini_view_weighted_short', '综')}
                                    </button>
                                    <button
                                        onClick={() => setQuotaView('5h')}
                                        className={clsx(
                                            "px-2 py-0.5 rounded-md transition-all cursor-pointer",
                                            quotaView === '5h' ? "bg-emerald-600 text-white shadow-xs" : "text-gray-500 dark:text-gray-400 hover:text-gray-900 dark:hover:text-white"
                                        )}
                                        title={t('dashboard.view_mode_title_5h', '5H 滚动')}
                                    >
                                        {t('dashboard.mini_view_5h_short', '5H')}
                                    </button>
                                    <button
                                        onClick={() => setQuotaView('weekly')}
                                        className={clsx(
                                            "px-2 py-0.5 rounded-md transition-all cursor-pointer",
                                            quotaView === 'weekly' ? "bg-purple-600 text-white shadow-xs" : "text-gray-500 dark:text-gray-400 hover:text-gray-900 dark:hover:text-white"
                                        )}
                                        title={t('dashboard.view_mode_title_weekly', '7天周配额')}
                                    >
                                        {t('dashboard.mini_view_weekly_short', '周')}
                                    </button>
                                </div>
                            </div>

                            <div className="space-y-3 pt-0.5">
                                {renderPoolMetricRow(
                                    t('accounts.quota_group_gemini', 'Gemini 模型'),
                                    poolMetrics.gemini,
                                    'emerald',
                                    <Sparkles className="w-3.5 h-3.5 text-emerald-500 shrink-0" />,
                                    t('dashboard.gemini_group_desc', '包含 Pro / Flash / 绘图')
                                )}
                                {renderPoolMetricRow(
                                    t('accounts.quota_group_claude', 'Claude & GPT'),
                                    poolMetrics.claude,
                                    'cyan',
                                    <Bot className="w-3.5 h-3.5 text-cyan-500 shrink-0" />
                                )}
                                {poolMetrics.argon && poolMetrics.argon.count > 0 && renderPoolMetricRow(
                                    'Gemini 4 Argon',
                                    poolMetrics.argon,
                                    'purple',
                                    <Gemini.Color className="w-3.5 h-3.5 shrink-0" />
                                )}
                            </div>
                        </div>
                    )
                ) : showAccountList ? (
                    /* 专属账号切换面板：不拉伸窗体，优雅列出所有账号 */
                    <div className="h-full flex flex-col space-y-2">
                        <div className="flex items-center justify-between pb-1 border-b border-gray-100 dark:border-white/5">
                            <span className="text-xs font-semibold text-gray-700 dark:text-gray-300 flex items-center gap-1.5">
                                <Shuffle className="w-3.5 h-3.5 text-blue-500" />
                                <span>{t('dashboard.switch_account', '切换当前应用账号')}</span>
                                <span className="text-[10px] text-gray-400 font-mono">({switchableAccounts.length})</span>
                            </span>
                            <button
                                onClick={() => setShowAccountList(false)}
                                className="text-xs font-medium text-gray-500 hover:text-gray-800 dark:text-gray-400 dark:hover:text-white px-2 py-0.5 rounded-md hover:bg-black/5 dark:hover:bg-white/5 transition-colors cursor-pointer"
                            >
                                {t('common.close', '返回')}
                            </button>
                        </div>

                        <div className="flex-1 overflow-y-auto space-y-1 pr-0.5 scrollbar-thin max-h-[200px]">
                            {switchableAccounts.map((acc) => {
                                const isCurrent = acc.id === displayedAccount?.id;
                                const accTier = getAccountTier(acc);
                                const isHealthy = !acc.disabled && !isAccountAbnormal(acc);

                                return (
                                    <button
                                        key={acc.id}
                                        disabled={isSwitching}
                                        onClick={() => handleSwitchAccount(acc.id)}
                                        className={clsx(
                                            "w-full flex items-center justify-between p-2 rounded-xl text-left transition-all cursor-pointer group",
                                            isCurrent
                                                ? "bg-blue-50/80 dark:bg-blue-900/30 border border-blue-200/70 dark:border-blue-700/50 shadow-xs"
                                                : "hover:bg-black/[0.03] dark:hover:bg-white/[0.05] border border-transparent"
                                        )}
                                    >
                                        <div className="flex items-center gap-2 min-w-0 flex-1">
                                            <span className={clsx(
                                                "w-1.5 h-1.5 rounded-full shrink-0",
                                                isHealthy ? "bg-emerald-500" : "bg-rose-500"
                                            )} />
                                            <span className={clsx(
                                                "text-xs truncate",
                                                isCurrent
                                                    ? "font-semibold text-blue-600 dark:text-blue-400"
                                                    : "text-gray-700 dark:text-gray-200"
                                            )}>
                                                {acc.email}
                                            </span>
                                        </div>

                                        <div className="flex items-center gap-1.5 shrink-0 ml-2">
                                            {accTier === 'ultra' && (
                                                <span className="text-[9px] font-bold px-1 py-0.5 rounded bg-violet-100 dark:bg-violet-900/40 text-violet-600 dark:text-violet-400">
                                                    ULTRA
                                                </span>
                                            )}
                                            {accTier === 'pro' && (
                                                <span className="text-[9px] font-bold px-1 py-0.5 rounded bg-blue-100 dark:bg-blue-900/40 text-blue-600 dark:text-blue-400">
                                                    PRO
                                                </span>
                                            )}
                                            {accTier === 'free' && (
                                                <span className="text-[9px] font-bold px-1 py-0.5 rounded bg-gray-100 dark:bg-zinc-800 text-gray-500">
                                                    FREE
                                                </span>
                                            )}
                                            {isCurrent ? (
                                                <Check className="w-3.5 h-3.5 text-blue-500 shrink-0" />
                                            ) : (
                                                <ArrowRight className="w-3 h-3 text-gray-300 dark:text-gray-600 opacity-0 group-hover:opacity-100 transition-opacity shrink-0" />
                                            )}
                                        </div>
                                    </button>
                                );
                            })}
                        </div>
                    </div>
                ) : !displayedAccount ? (
                    /* 该客户端未绑定账号的空态指引 */
                    <div className="h-full flex flex-col items-center justify-center text-center space-y-2 py-4">
                        <div className="w-10 h-10 rounded-2xl bg-gray-100 dark:bg-white/[0.05] flex items-center justify-center text-gray-400 mb-0.5">
                            <HelpCircle className="w-5 h-5 text-gray-400/80" />
                        </div>
                        <div className="text-xs font-semibold text-gray-700 dark:text-gray-300">
                            {t('dashboard.no_client_account', '该应用当前暂无登录或绑定的账号')}
                        </div>
                        <div className="text-[11px] text-gray-400 max-w-[240px]">
                            {tabs.find((t) => t.key === activeTab)?.tooltip}
                        </div>

                        <button
                            onClick={() => setShowAccountList(true)}
                            className="mt-1.5 px-3 py-1 text-xs font-semibold text-blue-600 dark:text-blue-400 bg-blue-50 dark:bg-blue-900/20 hover:bg-blue-100 dark:hover:bg-blue-900/30 rounded-xl transition-colors flex items-center gap-1.5 cursor-pointer"
                        >
                            <span>{t('dashboard.go_switch_account', '为该应用指派账号')}</span>
                            <ArrowRight className="w-3.5 h-3.5" />
                        </button>
                    </div>
                ) : (
                    /* 客户端绑定账号详情与配额面板 (100% 对齐首页) */
                    <div className="space-y-2.5">
                        {/* 账号信息与换号入口卡片 */}
                        <div className="bg-gradient-to-br from-black/[0.02] to-black/[0.04] dark:from-white/[0.04] dark:to-white/[0.02] rounded-xl p-2.5 border border-black/[0.05] dark:border-white/[0.08] space-y-2">
                            {/* 邮箱行与订阅标识 */}
                            <div className="flex items-center justify-between gap-2">
                                <div
                                    onClick={() => handleCopyEmail(displayedAccount.email)}
                                    className="flex items-center gap-1.5 min-w-0 flex-1 cursor-pointer hover:opacity-80 transition-opacity"
                                    title={t('common.copy_to_clipboard', '点击复制邮箱')}
                                >
                                    <Mail className="w-3.5 h-3.5 text-gray-400 shrink-0" />
                                    <span className="text-xs font-semibold text-gray-800 dark:text-zinc-200 truncate">
                                        {displayedAccount.email}
                                    </span>
                                    {copied ? (
                                        <Check className="w-3.5 h-3.5 text-emerald-500 shrink-0" />
                                    ) : (
                                        <Copy className="w-2.5 h-2.5 text-gray-400 opacity-0 group-hover:opacity-100 transition-opacity shrink-0" />
                                    )}
                                </div>

                                <div className="flex items-center gap-1 shrink-0">
                                    {tier === 'ultra' && (
                                        <span className="flex items-center gap-0.5 px-1.5 py-0.5 rounded-full bg-gradient-to-r from-violet-600 to-fuchsia-600 text-white text-[9px] font-bold shadow-xs">
                                            <Gem className="w-2.5 h-2.5 fill-current" />
                                            ULTRA
                                        </span>
                                    )}
                                    {tier === 'pro' && (
                                        <span className="flex items-center gap-0.5 px-1.5 py-0.5 rounded-full bg-gradient-to-r from-blue-600 to-cyan-600 text-white text-[9px] font-bold shadow-xs">
                                            <Diamond className="w-2.5 h-2.5 fill-current" />
                                            PRO
                                        </span>
                                    )}
                                    {tier === 'free' && (
                                        <span className="flex items-center gap-0.5 px-1.5 py-0.5 rounded-full bg-gray-100 dark:bg-white/10 text-gray-500 dark:text-gray-400 text-[9px] font-bold border border-gray-200 dark:border-white/10">
                                            <Circle className="w-2 h-2" />
                                            FREE
                                        </span>
                                    )}

                                    {displayedAccount.custom_label && (
                                        <span className="flex items-center gap-0.5 px-1.5 py-0.5 rounded-full bg-orange-100 dark:bg-orange-900/30 text-orange-600 dark:text-orange-400 text-[9px] font-bold">
                                            <Tag className="w-2.5 h-2.5" />
                                            {displayedAccount.custom_label}
                                        </span>
                                    )}
                                </div>
                            </div>

                            {/* 账号选择与一键切下个操作条 */}
                            <div className="flex items-center gap-1.5 pt-0.5">
                                {/* 切换账号主按钮 */}
                                <button
                                    onClick={() => setShowAccountList(true)}
                                    className="flex-1 flex items-center justify-between px-2.5 py-1 rounded-lg text-[11px] font-medium bg-white dark:bg-zinc-800/80 hover:bg-gray-50 dark:hover:bg-zinc-700/80 text-gray-700 dark:text-zinc-200 border border-gray-200/80 dark:border-white/10 shadow-xs transition-colors cursor-pointer"
                                >
                                    <span className="flex items-center gap-1.5 truncate">
                                        <Shuffle className="w-3 h-3 text-blue-500" />
                                        <span>{t('dashboard.switch_account', '切换账号')}</span>
                                    </span>
                                    <ChevronDown className="w-3 h-3 text-gray-400" />
                                </button>

                                {/* 托盘功能对齐：切换下一个账号 */}
                                <button
                                    onClick={handleQuickNextAccount}
                                    disabled={isSwitching || accounts.length <= 1}
                                    title={t('tray.switch_next', '切换下一个账号')}
                                    className="flex items-center gap-1 px-2.5 py-1 rounded-lg text-[11px] font-semibold bg-blue-50 dark:bg-blue-900/20 hover:bg-blue-100 dark:hover:bg-blue-900/30 text-blue-600 dark:text-blue-400 border border-blue-200/60 dark:border-blue-700/40 transition-colors disabled:opacity-40 cursor-pointer shrink-0"
                                >
                                    <Shuffle className={clsx("w-3 h-3", isSwitching && "animate-spin")} />
                                    <span>{t('accounts.switch_next_short', '切下个')}</span>
                                </button>
                            </div>
                        </div>

                        {/* 配额胶囊条：直接复用首页 GroupQuotaCard 规范 */}
                        <div className="flex flex-col gap-1.5">
                            {groupSummaries
                                .filter((s) => s.key !== 'argon')
                                .map((summary) => (
                                    <GroupQuotaCard
                                        key={summary.key}
                                        summary={summary}
                                        isProtected={isGroupProtected(summary.key)}
                                        liveLimit={
                                            summary.key === 'gemini'
                                                ? getLiveLimitForModel(displayedAccount, 'gemini-pro')
                                                : getLiveLimitForModel(displayedAccount, 'claude')
                                        }
                                    />
                                ))}
                            {(() => {
                                const argon = groupSummaries.find((s) => s.key === 'argon');
                                if (!argon) return null;
                                return (
                                    <GroupQuotaCard
                                        summary={argon}
                                        isProtected={isGroupProtected('argon')}
                                        liveLimit={getLiveLimitForModel(displayedAccount, 'gemini-4-argon')}
                                        fullWidth
                                    />
                                );
                            })()}
                            {groupSummaries.length === 0 && (
                                <div className="text-center py-4 text-xs text-gray-400">
                                    {t('dashboard.no_quota_data', '暂无配额数据')}
                                </div>
                            )}
                        </div>
                    </div>
                )}
            </div>

                {/* 3. 底部托盘综合控制栏：单行紧凑不折行布局 */}
                <div
                    className={clsx(
                        "flex-none h-10 bg-gray-50/95 dark:bg-black/40 flex items-center justify-between gap-1.5 px-2.5 text-[11px] text-gray-500 dark:text-gray-400 border-t border-gray-100 dark:border-white/[0.06] select-none cursor-default",
                        isTauri() ? "rounded-b-[8px]" : "rounded-b-2xl"
                    )}
                    data-tauri-drag-region
                    onMouseDown={(e) => {
                        const target = e.target as HTMLElement;
                        if (target.closest('button, a, input, select, textarea, [data-no-drag="true"]')) {
                            return;
                        }
                        if (isTauri()) {
                            getCurrentWindow().startDragging().catch(() => {});
                        }
                    }}
                >
                    {/* 左侧：托盘轻量模式开关 (退出时释放内存) */}
                    <div
                        onClick={handleToggleLightweight}
                        className="min-w-0 flex items-center gap-1 cursor-pointer hover:opacity-80 transition-opacity whitespace-nowrap"
                        title={t('settings.lightweight_mode_desc', '开启后退出或最小化到托盘将彻底释放 WebView 内存')}
                    >
                        <Feather size={12} className={clsx("shrink-0", config?.lightweight_mode ? "text-emerald-500" : "text-gray-400")} />
                        <span className="font-medium text-[10.5px] text-gray-600 dark:text-gray-300 truncate max-w-[92px]">
                            {t('tray.lightweight_mode', '轻量模式')}
                        </span>
                        {/* 微型开关组件 */}
                        <div className={clsx(
                            "w-6 h-3.5 rounded-full p-0.5 transition-colors relative flex items-center shrink-0",
                            config?.lightweight_mode ? "bg-emerald-500" : "bg-gray-300 dark:bg-zinc-600"
                        )}>
                            <div className={clsx(
                                "w-2.5 h-2.5 rounded-full bg-white shadow-xs transition-transform",
                                config?.lightweight_mode ? "translate-x-2.5" : "translate-x-0"
                            )} />
                        </div>
                    </div>

                    {/* 中间：打开主控制台核心按钮（单行不折行） */}
                    <button
                        onClick={handleMaximize}
                        className="shrink-0 flex items-center gap-1 px-2.5 py-1 rounded-xl text-[11px] font-semibold whitespace-nowrap bg-white dark:bg-zinc-800 hover:bg-gray-100 dark:hover:bg-zinc-700 text-gray-800 dark:text-zinc-100 border border-gray-200/90 dark:border-white/10 shadow-xs transition-all active:scale-95 cursor-pointer"
                        title={t('tray.open_console', '打开主控制台')}
                    >
                        <Maximize2 size={11} className="text-blue-500 shrink-0" />
                        <span>{t('tray.open_console', '打开主控制台')}</span>
                    </button>

                    {/* 右侧：退出应用 */}
                    <button
                        onClick={handleQuitApp}
                        className="shrink-0 p-1.5 rounded-lg text-gray-400 hover:text-rose-500 hover:bg-rose-50 dark:hover:bg-rose-950/30 transition-colors cursor-pointer"
                        title={t('tray.quit', '退出应用')}
                    >
                        <Power size={13} />
                    </button>
                </div>
        </div>
    );
}
