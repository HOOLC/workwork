type SessionRecord = Record<string, any>;

export interface SessionMetaPill {
  readonly key: string;
  readonly label: string;
  readonly tone: string;
  readonly title?: string;
}

export interface SessionOperationalState {
  readonly label: string;
  readonly tone: string;
  readonly rank: number;
  readonly detail: string;
}

export interface SessionDeliveryIssueIndicator {
  readonly value: string;
  readonly detail: string;
  readonly tone: string;
  readonly title: string;
}

export function shouldShowSessionState(state: { readonly rank: number }): boolean {
  return state.rank > 10;
}

export function buildChannelLabelById(sessions: readonly SessionRecord[]): ReadonlyMap<string, string> {
  const labels = new Map<string, string>();
  for (const session of sessions) {
    const channelId = String(session.channelId || "");
    const label = sessionHumanChannelLabel(session);
    if (channelId && label) {
      labels.set(channelId, label);
    }
  }
  return labels;
}

export function resolveSessionChannelLabel(session: SessionRecord, channelLabelById?: ReadonlyMap<string, string>): string {
  const channelId = String(session.channelId || "");
  return sessionHumanChannelLabel(session) || (channelId ? channelLabelById?.get(channelId) : undefined) || channelId || "未知频道";
}

export function renderSessionMeta(session: SessionRecord, channelLabelById?: ReadonlyMap<string, string>): SessionMetaPill[] {
  const deliveryIssue = sessionDeliveryIssueIndicator(session);
  const activeJobCount = activeBackgroundJobCount(session);
  const selectionBlocked = sessionSelectionBlocked(session);
  return [
    platformPill(session),
    connectionPill(session),
    modePill(session),
    {
      key: "channel",
      label: resolveSessionChannelLabel(session, channelLabelById),
      tone: "info",
      title: stringOrUndefined(session.channelId || session.key),
    },
    selectionBlocked
      ? {
          key: "selection-blocked",
          label: "模型选择不可用",
          tone: "danger",
          title: stringOrUndefined(session.selectionBlockReason),
        }
      : null,
    session.profileId
      ? {
          key: "profile",
          label: [session.profileId, session.model, session.thinking].filter(Boolean).join(" · "),
          tone: "info",
          title: `Profile ${String(session.profileId)}`,
        }
      : null,
    deliveryIssue
      ? {
          key: "delivery-blocked",
          label: "投递失败 " + deliveryIssue.value,
          tone: deliveryIssue.tone,
          title: deliveryIssue.title,
        }
      : null,
    activeJobCount > 0 ? { key: "jobs", label: "Jobs " + activeJobCount, tone: "good" } : null,
  ].filter((item): item is SessionMetaPill => Boolean(item));
}

function connectionPill(session: SessionRecord): SessionMetaPill | null {
  const connectionId = String(session.connectionId || "").trim();
  const connectionName = String(session.connectionName || connectionId).trim();
  if (!connectionName) return null;
  return {
    key: "connection",
    label: connectionName,
    tone: "purple",
    title: connectionId ? `IM 接入 ${connectionId}` : "IM 接入",
  };
}

function modePill(session: SessionRecord): SessionMetaPill {
  const proactive = String(session.mode || "normal") === "proactive";
  return {
    key: "mode",
    label: proactive ? "主动" : "普通",
    tone: proactive ? "warn" : "",
    title: proactive ? "该接入的所有消息进入同一个 Agent Session" : "每个远端对话使用独立 Agent Session",
  };
}

export function sessionOperationalState(session: SessionRecord): SessionOperationalState {
  if (sessionSelectionBlocked(session)) {
    return {
      label: "模型选择不可用",
      tone: "danger",
      rank: 70,
      detail: String(session.selectionBlockReason || "没有可用的 Profile/模型组合"),
    };
  }
  if (Number(session.blockedInboundCount || 0) > 0) {
    return {
      label: "投递失败",
      tone: "danger",
      rank: 60,
      detail: session.blockedInboundCount + " 条消息未进入 Agent mailbox",
    };
  }
  const activeJobCount = activeBackgroundJobCount(session);
  if (activeJobCount > 0) {
    return { label: "后台任务", tone: "good", rank: 20, detail: activeJobCount + " 个运行任务" };
  }
  if (session.lastUserMessage) {
    return {
      label: "已投递",
      tone: "info",
      rank: 10,
      detail: "已有 Agent mailbox receipt",
    };
  }
  return {
    label: "Agent 状态未知",
    tone: "",
    rank: 0,
    detail: "Gateway 不持有 Agent 执行状态",
  };
}

export function sessionDeliveryIssueIndicator(session: SessionRecord): SessionDeliveryIssueIndicator | null {
  const blocked = Math.max(0, Number(session.blockedInboundCount || 0));
  if (blocked === 0) {
    return null;
  }
  return {
    value: blocked + " 条",
    detail: "消息尚未被 Agent mailbox 接受",
    tone: "danger",
    title: "Gateway 只记录 mailbox append 失败；成功投递不代表 Agent 执行完成。",
  };
}

export function sessionSelectionBlocked(session: SessionRecord): boolean {
  return Boolean(session.selectionBlockedAt);
}

export function sessionActivityAt(session: SessionRecord): unknown {
  const candidates = [session.lastActivityAt, session.lastSlackReplyAt, session.updatedAt, ...(session.backgroundJobs || []).flatMap(jobActivityTimestamps), ...(session.failedBackgroundJobs || []).flatMap(jobActivityTimestamps)];
  const latestMs = newestTimestamp(candidates);
  return candidates.find((value) => timestampMs(value) === latestMs) || session.createdAt || session.updatedAt;
}

export function sessionActivityMs(session: SessionRecord): number {
  return timestampMs(sessionActivityAt(session));
}

export function activeBackgroundJobs(session: SessionRecord): Record<string, any>[] {
  const jobs = Array.isArray(session.backgroundJobs) ? session.backgroundJobs : null;
  return jobs ? jobs.filter(isActiveBackgroundJob) : [];
}

export function activeBackgroundJobCount(session: SessionRecord): number {
  if (Array.isArray(session.backgroundJobs)) {
    return activeBackgroundJobs(session).length;
  }
  return Math.max(0, Number(session.runningBackgroundJobCount || 0));
}

export function isActiveBackgroundJob(job: Record<string, any>): boolean {
  const status = String(job.status || "").toLowerCase();
  return status === "registered" || status === "running";
}

function sessionHumanChannelLabel(session: SessionRecord): string | undefined {
  const channelId = String(session.channelId || "");
  const channelName = String(session.channelName || "").trim();
  if (channelName) {
    return formatSlackChannelName(channelName);
  }

  const channelLabel = String(session.channelLabel || "").trim();
  if (channelLabel && channelLabel !== channelId && !looksLikeSlackChannelId(channelLabel)) {
    return channelLabel;
  }

  if (session.channelType === "im") return "私信";
  if (session.channelType === "mpim") return "群聊";
  return undefined;
}

function platformPill(session: SessionRecord): SessionMetaPill | null {
  const platform = String(session.platform || "").trim();
  if (platform === "slack") {
    return {
      key: "platform",
      label: "Slack",
      tone: "info",
      title: "Slack session",
    };
  }
  return null;
}

function formatSlackChannelName(channelName: string): string {
  return channelName.startsWith("#") ? channelName : "#" + channelName;
}

function looksLikeSlackChannelId(value: string): boolean {
  return /^[CDG][A-Z0-9]{8,}$/.test(value);
}

function stringOrUndefined(value: unknown): string | undefined {
  const text = String(value || "");
  return text || undefined;
}

function jobActivityTimestamps(job: Record<string, any>): unknown[] {
  return [job.lastEventAt, job.status === "running" ? null : job.updatedAt, job.createdAt];
}

function timestampMs(value: unknown): number {
  const parsed = typeof value === "string" ? Date.parse(value) : NaN;
  return Number.isFinite(parsed) ? parsed : 0;
}

function newestTimestamp(values: readonly unknown[]): number {
  return values.reduce<number>((latest, value) => Math.max(latest, timestampMs(value)), 0);
}
