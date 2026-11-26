import type { Account, ModelQuota } from '../types/account';
import { isTauri } from './env';
import { isSupportedDisplayModel } from './quotaDisplay';

/** 验证锁是否仍在有效窗口内；0 / 缺失表示无明确截止时间。 */
export function isValidationBlockActive(
    account: Account,
    nowSeconds: number = Math.floor(Date.now() / 1000)
): boolean {
    if (!account.validation_blocked) return false;
    const until = account.validation_blocked_until;
    return until === undefined || until === null || until === 0 || until > nowSeconds;
}

/**
 * 账号是否需要手机/账号验证（VALIDATION_REQUIRED / validation_blocked），排除永久封禁账号。
 */
export function isAccountNeedsVerification(account: Account): boolean {
    if (account.banned) return false;
    if (isValidationBlockActive(account)) return true;

    const reasons = [
        account.validation_blocked_reason,
        account.proxy_disabled_reason,
        account.quota?.forbidden_reason,
    ];
    return reasons.some((raw) => {
        const lower = (raw ?? '').toLowerCase();
        return (
            lower.includes('validation_required') ||
            lower.includes('verify your account') ||
            lower.includes('validation_url') ||
            lower.includes('further action is required') ||
            lower.includes('account_verification') ||
            lower.includes('phone_verification')
        );
    });
}

/**
 * 从账号验证原因或字段中提取 Google 验证链接 (https://...)。
 */
export function extractAccountVerificationUrl(account: Account): string | null {
    const candidates = [
        (account as unknown as { validation_url?: string | null }).validation_url,
        account.validation_blocked_reason,
        account.quota?.forbidden_reason,
        account.proxy_disabled_reason,
    ];

    const cleanUrl = (rawUrl: string): string => rawUrl.replace(/[.,;)\]}>"']+$/, '');

    for (const raw of candidates) {
        if (!raw) continue;
        const taggedMatch = raw.match(/validation_url\s*[:=]\s*(https?:\/\/[^\s"'<>|]+)/i);
        if (taggedMatch?.[1]) {
            return cleanUrl(taggedMatch[1]);
        }
        const anyUrlMatch = raw.match(/https?:\/\/[^\s"'<>|]+/i);
        if (anyUrlMatch?.[0]) {
            return cleanUrl(anyUrlMatch[0]);
        }
    }
    return null;
}

/**
 * 在外部浏览器中打开账号验证链接。
 */
export async function openVerificationUrl(url: string): Promise<void> {
    if (!url) return;
    if (isTauri()) {
        try {
            const { openUrl } = await import('@tauri-apps/plugin-opener');
            await openUrl(url);
            return;
        } catch (err) {
            console.warn('Failed to open verification URL via plugin-opener, falling back:', err);
        }
    }
    window.open(url, '_blank', 'noopener,noreferrer');
}

/**
 * 账号是否拥有 Full 金丝雀模型 (Carbon / Neon / Argon / 5.5 系列)，用于区分 Full 与 Basic (Hydrogen / 4.6) 账号。
 */
export function hasFullCanaryModels(account: Account): boolean {
    const models = account.quota?.models;
    if (!models || models.length === 0) return false;
    return models.some((m) => {
        const name = (m.name ?? '').toLowerCase();
        if (!name || name.includes('disabled')) return false;
        return (
            name.includes('carbon') ||
            name.includes('neon') ||
            name.includes('argon') ||
            name.includes('-5-5')
        );
    });
}

/**
 * 账号是否处于异常状态。
 *
 * 统一账号列表筛选与仪表盘统计口径，避免两处各自实现导致计数漂移。
 * 403 代理锁可能尚未写入 quota.is_forbidden（例如手动禁用或旧数据），
 * 因此同时检查 proxy_disabled_reason。
 */
export function isAccountAbnormal(account: Account): boolean {
    const reason = (account.proxy_disabled_reason ?? '').toLowerCase();
    return Boolean(
        account.banned ||
            isValidationBlockActive(account) ||
            account.quota?.is_forbidden ||
            (account.proxy_disabled &&
                (reason.startsWith('forbidden (403)') ||
                    reason.includes('403 forbidden') ||
                    reason.startsWith('banned:')))
    );
}

/**
 * 账号是否处于健康可用状态（非异常、未停用）。
 */
export function isAccountHealthy(account: Account): boolean {
    return !account.disabled && !isAccountAbnormal(account);
}

/** 账号是否有模型级 403 冻结 */
export function hasForbiddenModels(account: Account): boolean {
    return Boolean(account.forbidden_models && account.forbidden_models.length > 0);
}

/**
 * 判定文本是否属于上游地区不支持（假 403 / 代理出口 IP 地区限制错误），
 * 与后端 `models::account::is_location_unsupported_error` 保持一致。
 */
export function isLocationUnsupportedReason(reason?: string | null): boolean {
    const lower = (reason ?? '').toLowerCase();
    return (
        lower.includes('user location is not supported') ||
        lower.includes('location not supported') ||
        lower.includes('location is not supported') ||
        lower.includes('location not allowed') ||
        lower.includes('unsupported_location')
    );
}

/**
 * 判定 `proxy_disabled_reason` 是否由系统 403 / 验证 / 地区限制锁定引起
 * （与后端 `classify_lock_reason` 的 HardForbidden / Validation 及地区限制一致），
 * 排除用户手动停代、批量禁用、配额保护与非 403 隔离原因。
 */
export function isClearableProxyLockReason(reason?: string | null): boolean {
    const lower = (reason ?? '').trim().toLowerCase();
    if (!lower) return false;
    if (lower.startsWith('banned:') || lower === 'quota_protection') {
        return false;
    }
    return (
        lower.startsWith('forbidden (403)') ||
        lower.includes('403 forbidden') ||
        lower.includes('validation_required') ||
        lower.includes('verify your account') ||
        lower.includes('validation_url') ||
        lower.includes('further action is required') ||
        isLocationUnsupportedReason(lower)
    );
}

/**
 * 判定账号是否允许执行「一键清除 403 标记并重新测活」。
 *
 * 严格排除：
 * - 上游确认永久封禁 (`banned`)
 * - 凭据/OAuth 停用 (`disabled`，如 `invalid_grant`，清 403 无法恢复)
 * - 纯手动停代 (`manual_proxy_disabled`) 或非 403/验证原因的 `proxy_disabled`
 */
export function canClearForbiddenLock(account: Account): boolean {
    if (account.banned || account.disabled) {
        return false;
    }

    if (
        account.quota?.is_forbidden ||
        isValidationBlockActive(account) ||
        Boolean(account.validation_blocked) ||
        hasForbiddenModels(account)
    ) {
        return true;
    }

    const rawReason =
        account.validation_blocked_reason ||
        account.quota?.forbidden_reason ||
        account.proxy_disabled_reason ||
        '';
    if (isLocationUnsupportedReason(rawReason)) {
        return true;
    }

    return Boolean(
        account.proxy_disabled &&
            !account.manual_proxy_disabled &&
            isClearableProxyLockReason(account.proxy_disabled_reason)
    );
}

/**
 * 提取测活/清理失败的真实错误摘要用于前端 Toast 展示，避免吞掉错误原因后一律提示检查代理。
 */
export function formatWarmUpErrorDetail(error: unknown, maxLength: number = 180): string {
    let raw = '';
    if (typeof error === 'string') {
        raw = error;
    } else if (error instanceof Error) {
        raw = error.message;
    } else if (error && typeof error === 'object') {
        const record = error as Record<string, unknown>;
        if (typeof record.error === 'string') {
            raw = record.error;
        } else if (typeof record.message === 'string') {
            raw = record.message;
        } else {
            try {
                raw = JSON.stringify(error);
            } catch {
                raw = String(error);
            }
        }
    } else if (error !== null && error !== undefined) {
        raw = String(error);
    }

    const trimmed = raw.trim();
    if (!trimmed) return '';

    // 若为 JSON 包装错误，优先提取内部 message
    let extracted = trimmed;
    const jsonStart = trimmed.indexOf('{');
    const jsonEnd = trimmed.lastIndexOf('}');
    if (jsonStart >= 0 && jsonEnd > jsonStart) {
        try {
            const parsed = JSON.parse(trimmed.slice(jsonStart, jsonEnd + 1));
            const candidate =
                parsed?.error?.message ||
                (typeof parsed?.error === 'string' ? parsed.error : null) ||
                parsed?.message;
            if (typeof candidate === 'string' && candidate.trim()) {
                extracted = candidate.trim();
            }
        } catch {
            // 保留原始文本
        }
    }

    const compact = extracted.replace(/\s+/g, ' ').trim();
    if (compact.length <= maxLength) {
        return compact;
    }
    return `${compact.slice(0, Math.max(0, maxLength - 3)).trimEnd()}...`;
}


// ============================================================================
// Pool-dynamic capability tier (Full vs Basic)
// ============================================================================

/** 账号当前可展示的有效模型（排除 disabled / 不受支持的内部模型）。 */
export function getAccountDisplayModels(account: Account): ModelQuota[] {
    const models = account.quota?.models ?? [];
    return models.filter(
        (m) => isSupportedDisplayModel(m.name || '') && !(m as { disabled?: boolean }).disabled
    );
}

/** 将模型 ID 归一化为模型家族（去掉 low/medium/high/xhigh/tiered/thinking 后缀）。 */
export function getModelFamilyKey(name: string): string {
    let key = (name ?? '').toLowerCase().trim();
    for (let i = 0; i < 4; i++) {
        const next = key.replace(/[-_](xhigh|high|medium|low|tiered|thinking)$/, '');
        if (next === key) break;
        key = next;
    }
    return key;
}

/** 家族 key -> 人类可读名称，如 gemini-4-carbon -> Gemini 4 Carbon。 */
export function humanizeModelFamily(key: string): string {
    return key
        .split(/[-_]/)
        .filter(Boolean)
        .map((part) => (/^\d+$/.test(part) ? part : part.charAt(0).toUpperCase() + part.slice(1)))
        .join(' ')
        .replace(/(\d) (\d)/g, '$1.$2');
}

function getAccountFamilies(account: Account): Set<string> {
    return new Set(getAccountDisplayModels(account).map((m) => getModelFamilyKey(m.name)));
}

function getHealthyPeers(allAccounts: Account[]): Account[] {
    return allAccounts.filter(
        (a) => !a.banned && !a.disabled && getAccountDisplayModels(a).length > 0
    );
}

function getFullThreshold(peers: Account[]): { max: number; min: number; threshold: number } {
    const counts = peers.map((a) => getAccountDisplayModels(a).length);
    const max = Math.max(...counts);
    const min = Math.min(...counts);
    return { max, min, threshold: Math.ceil(max * 0.85) };
}

/**
 * 池动态判定 ✨ Full / 🔹 Basic：
 * 与池中健康账号的最大模型数比较，达到顶级梯队即为 Full；
 * 池内仅 1 个账号时回退到 carbon/neon/argon/-5-5 检测。
 */
export function isFullCapabilityAccount(account: Account, allAccounts: Account[]): boolean {
    const own = getAccountDisplayModels(account);
    if (own.length === 0) return false;
    const peers = getHealthyPeers(allAccounts);
    if (peers.length <= 1) return hasFullCanaryModels(account);
    const { max, min, threshold } = getFullThreshold(peers);
    if (max === min) return true;
    return own.length >= threshold;
}

/** 返回 Full 同伴账号拥有、但本账号缺失的模型家族（人类可读）。 */
export function getMissingPoolModels(account: Account, allAccounts: Account[]): string[] {
    const peers = getHealthyPeers(allAccounts);
    if (peers.length <= 1) return [];
    const { max, min, threshold } = getFullThreshold(peers);
    if (max === min) return [];
    const ownFamilies = getAccountFamilies(account);
    const missing = new Set<string>();
    for (const peer of peers) {
        if (peer.id === account.id) continue;
        if (getAccountDisplayModels(peer).length < threshold) continue;
        for (const fam of getAccountFamilies(peer)) {
            if (!ownFamilies.has(fam)) missing.add(fam);
        }
    }
    return Array.from(missing).map(humanizeModelFamily);
}
