import { formatResetTime, quotaTone } from "./admin-formatters.js";

import { Tone } from "./admin-types.js";

import { formatUsageBalance, profileQuotaLabel, profileTitle, usageRemainingOf } from "./auth-profile-display";

import { daysUntilReset, formatAuthQuotaDisplayParts, formatWeightedWeeklyQuotaScore, remainingPercent, weightedWeeklyQuotaScore } from "./auth-profile-quota";

import React from "react";

export function ProfileQuotaMetrics({ quota }: { readonly quota: ProfileQuotaSummary }): React.JSX.Element {
  if (quota.ok === false) {
    return <div className="profile-quota-error">{quota.error}</div>;
  }
  if (quota.reported === false) {
    return <div className="profile-quota-unreported">{quota.label}</div>;
  }
  return (
    <div className="profile-quota-block" title={quota.fullLabel}>
      <div className="profile-quota-metrics">
        <div className={"profile-quota-metric " + quota.tone}>
          <span>{quota.remainingCaption}</span>
          <strong>{quota.remainingLabel}</strong>
        </div>
        <div className={"profile-quota-metric " + quota.tone}>
          <span>{quota.scoreCaption}</span>
          <strong>{quota.scoreLabel}</strong>
        </div>
        <div className="profile-quota-metric">
          <span>重置</span>
          <strong>{quota.resetLabel}</strong>
        </div>
      </div>
      {quota.shortLabel ? (
        <div className="profile-short-window">
          <span>短窗</span>
          <strong>{quota.shortLabel}</strong>
        </div>
      ) : null}
    </div>
  );
}

export type ProfileQuotaSummary =
  | {
      readonly ok: true;
      readonly reported: true;
      readonly fullLabel: string;
      readonly remainingCaption: string;
      readonly remainingLabel: string;
      readonly scoreCaption: string;
      readonly scoreLabel: string;
      readonly resetLabel: string;
      readonly shortLabel: string | null;
      readonly tone: Tone;
    }
  | {
      readonly ok: true;
      readonly reported: false;
      readonly label: string;
      readonly tone: Tone;
    }
  | {
      readonly ok: false;
      readonly error: string;
      readonly tone: Tone;
    };

export function profileQuotaSummary(profile: any): ProfileQuotaSummary {
  const rateLimits = profile?.rateLimits ?? profile;
  if (rateLimits?.ok === false) {
    return {
      ok: false,
      error: rateLimits?.error || "额度不可用",
      tone: "danger",
    };
  }

  if (profile?.billing === "usage") {
    const remaining = usageRemainingOf(profile);
    if (remaining === undefined) {
      return {
        ok: true,
        reported: false,
        label: "未提供额度信息",
        tone: "",
      };
    }
    const remainingLabel = remaining === Number.POSITIVE_INFINITY ? "无限" : `$${formatUsageBalance(remaining)}`;
    return {
      ok: true,
      reported: true,
      fullLabel: profileQuotaLabel(profile),
      remainingCaption: "余额",
      remainingLabel,
      scoreCaption: "加权",
      scoreLabel: "—",
      resetLabel: "按量",
      shortLabel: null,
      tone: quotaTone(remaining && remaining > 0 ? 100 : 0),
    };
  }

  const snapshot = rateLimits.rateLimits || {};
  const display = formatAuthQuotaDisplayParts({
    primary: snapshot.primary,
    secondary: snapshot.secondary,
  });
  const weekly = display.windows.weekly;
  const remaining = remainingPercent(weekly?.usedPercent);
  const score = weightedWeeklyQuotaScore(remaining, daysUntilReset(weekly?.resetsAt));
  return {
    ok: true,
    reported: true,
    fullLabel: display.fullLabel || "额度未知",
    remainingCaption: "7d 剩余",
    remainingLabel: remaining === undefined ? "--" : `${Math.round(remaining)}%`,
    scoreCaption: "加权",
    scoreLabel: formatWeightedWeeklyQuotaScore(score),
    resetLabel: formatResetTime(weekly?.resetsAt),
    shortLabel: display.shortLabel,
    tone: quotaTone(remaining ?? 100) || (display.weeklyLabel ? "" : "warn"),
  };
}

export function profileQuotaItems(profiles: readonly Record<string, any>[]): Array<{
  readonly label: string;
  readonly title: string;
  readonly billing: "subscription" | "usage";
  readonly score: number;
  readonly remaining: number;
}> {
  return profiles
    .map((profile) => {
      const billing = profile.billing === "usage" ? ("usage" as const) : ("subscription" as const);
      const rateLimits = profile.rateLimits || {};
      if (rateLimits.ok === false) return null;
      if (billing === "usage") {
        const remaining = usageRemainingOf(profile);
        if (remaining === undefined) return null;
        return {
          label: profileQuotaLabel(profile),
          title: profileTitle(profile),
          billing,
          score: remaining === Number.POSITIVE_INFINITY ? Number.POSITIVE_INFINITY : remaining,
          remaining: remaining > 0 ? 100 : 0,
        };
      }
      const limits = rateLimits.rateLimits || {};
      const display = formatAuthQuotaDisplayParts({
        primary: limits.primary,
        secondary: limits.secondary,
      });
      if (!display.fullLabel) return null;
      const weekly = display.windows.weekly;
      const remaining = remainingPercent(weekly?.usedPercent);
      const score = weightedWeeklyQuotaScore(remaining, daysUntilReset(weekly?.resetsAt));
      return {
        label: display.fullLabel,
        title: profileTitle(profile),
        billing,
        score: score ?? -1,
        remaining: remaining ?? 0,
      };
    })
    .filter(
      (
        item,
      ): item is {
        readonly label: string;
        readonly title: string;
        readonly billing: "subscription" | "usage";
        readonly score: number;
        readonly remaining: number;
      } => Boolean(item),
    )
    .sort((left, right) => {
      if (left.billing !== right.billing) {
        return left.billing === "subscription" ? -1 : 1;
      }
      return right.score - left.score || right.remaining - left.remaining || left.title.localeCompare(right.title);
    });
}
