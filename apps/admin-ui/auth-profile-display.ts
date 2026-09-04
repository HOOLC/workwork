import { formatAuthQuotaDisplay } from "./auth-profile-quota";

type AuthProfileRecord = Record<string, any>;
interface QuotaLabelOptions {
  readonly now?: Date | number | string | undefined;
}

export function profileAccountLabel(profile: AuthProfileRecord): string {
  const accountStatus = profile.account || {};
  if (accountStatus.ok === false) {
    return "账号状态读取失败";
  }

  const account = accountStatus.account || {};
  return readString(account.email) || readString(account.name) || readString(account.id) || (profile.billing === "usage" ? "API Key" : "未知账号");
}

export function profilePlanLabel(profile: AuthProfileRecord): string {
  const accountStatus = profile.account || {};
  if (accountStatus.ok === false) {
    return "";
  }

  const account = accountStatus.account || {};
  const plan = readString(account.planType) || readString(account.type);
  if (!plan) {
    return "";
  }

  if (plan === "prolite") return "Pro Lite";
  if (plan === "pro") return "Pro";
  if (plan === "chatgpt") return "ChatGPT";
  return plan;
}

export function profileDisplayLabel(profile: AuthProfileRecord): string {
  return [profileAccountLabel(profile), profilePlanLabel(profile)].filter(Boolean).join(" · ");
}

export function profileBillingLabel(profile: AuthProfileRecord): string {
  return profile.billing === "usage" ? "按量" : "订阅";
}

export function profileRuntimeLabel(profile: AuthProfileRecord): string {
  const provider = readString(profile.provider) || "xai";
  return [readString(profile.profile_id), provider].filter(Boolean).join(" · ");
}

export function profileOptionLabel(profile: AuthProfileRecord, options: QuotaLabelOptions = {}): string {
  return [profileDisplayLabel(profile), profileQuotaLabel(profile, options)].filter(Boolean).join(" · ");
}

export function profileSessionActionLabel(profile: AuthProfileRecord, options: QuotaLabelOptions = {}): string {
  return profileOptionLabel(profile, options);
}

export function profileTitle(profile: AuthProfileRecord, options: QuotaLabelOptions = {}): string {
  const profileId = readString(profile.profile_id);
  return [profileOptionLabel(profile, options), profileId ? `Profile ${profileId}` : ""].filter(Boolean).join(" · ");
}

export function profileIsSelectable(profile: AuthProfileRecord): boolean {
  return profile.account?.ok !== false && profile.rateLimits?.ok !== false;
}

export function profileQuotaLabel(profile: AuthProfileRecord, options: QuotaLabelOptions = {}): string {
  if (profile.billing === "usage") {
    return formatUsageQuotaLabel(profile);
  }

  const rateLimits = profile.rateLimits || {};
  if (rateLimits.ok === false) {
    return "额度状态读取失败";
  }

  const limits = rateLimits.rateLimits || {};
  const label = formatAuthQuotaDisplay({
    primary: limits.primary,
    secondary: limits.secondary,
    now: options.now,
  });
  return label ?? "额度未知";
}

export function profileWeeklyQuotaLabel(profile: AuthProfileRecord, options: QuotaLabelOptions = {}): string {
  if (profile.billing === "usage") {
    return formatUsageQuotaLabel(profile);
  }

  const rateLimits = profile.rateLimits || {};
  if (rateLimits.ok === false) {
    return "不可用";
  }

  const limits = rateLimits.rateLimits || {};
  return (
    formatAuthQuotaDisplay({
      primary: limits.primary,
      secondary: limits.secondary,
      now: options.now,
    }) ?? "额度未知"
  );
}

function formatUsageQuotaLabel(profile: AuthProfileRecord): string {
  const remaining = usageRemainingOf(profile);
  if (remaining === undefined) {
    return "按量 · 未提供额度信息";
  }
  if (remaining === Number.POSITIVE_INFINITY) {
    return "按量 无限";
  }
  return `按量 $${formatUsageBalance(remaining)}`;
}

export function usageRemainingOf(profile: AuthProfileRecord): number | undefined {
  const direct = profile.usageQuota?.remaining;
  if (typeof direct === "number" && (Number.isFinite(direct) || direct === Number.POSITIVE_INFINITY)) {
    return direct;
  }

  const credits = profile.rateLimits?.ok === false ? null : profile.rateLimits?.rateLimits?.credits;
  if (!credits) {
    return undefined;
  }
  if (credits.unlimited) {
    return Number.POSITIVE_INFINITY;
  }
  const parsed = Number(credits.balance);
  if (Number.isFinite(parsed)) {
    return Math.max(0, parsed);
  }
  if (credits.hasCredits === false) {
    return 0;
  }
  return undefined;
}

export function formatUsageBalance(remaining: number): string {
  if (remaining >= 100) {
    return remaining.toFixed(0);
  }
  return remaining.toFixed(2).replace(/\.00$/, "");
}

function readString(value: unknown): string | null {
  if (typeof value !== "string") {
    return null;
  }

  const trimmed = value.trim();
  return trimmed ? trimmed : null;
}
