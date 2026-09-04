import { getAdminStatusSnapshot, getTimelineSnapshot, subscribeAdminStatus, subscribeTimeline } from "./admin-status-store";

import { adminSessionPath, requestJson, slackThreadUrlApiPath } from "./session-api.js";

import { Badge } from "./session-badge.js";
import { SessionContextPanel } from "./session-context.js";

import { classSafeValue, fmtDateTime, fmtRelativeTime } from "./session-formatters.js";

import { GitHubIdentityPanel } from "./session-github-binding.js";

import { activeBackgroundJobCount, activeBackgroundJobs, buildChannelLabelById, resolveSessionChannelLabel, sessionActivityAt, sessionDeliveryIssueIndicator, sessionOperationalState, SessionOperationalState, shouldShowSessionState } from "./session-row-display";

import { SessionDebugPanel, SessionResetButton, SessionRuntimePanel, SessionTraceStats } from "./session-runtime.js";

import { sessionFirstText, sessionPrimaryText } from "./session-selection.js";

import { JobsTable, SessionSelectionPanel, SessionTimeline } from "./session-timeline.js";

import { mergeSessionRecords, SessionRecord, TimelinePayload, timelinePayloadSession } from "./session-types.js";

import type { SessionMetaItem } from "./session-metadata.js";

import React, { useState, useSyncExternalStore } from "react";

export function SessionDetail({ session: providedSession, isPermalink = false }: { readonly session: SessionRecord; readonly isPermalink?: boolean }): React.JSX.Element {
  const sessionKey = String(providedSession.key || "");
  const timelineSnapshot = useSyncExternalStore(
    (listener) => subscribeTimeline(sessionKey, listener),
    () => getTimelineSnapshot(sessionKey),
    () => getTimelineSnapshot(sessionKey),
  );
  const session = mergeSessionRecords(providedSession, timelinePayloadSession(timelineSnapshot.payload as TimelinePayload | null)) || providedSession;
  const snapshot = useSyncExternalStore(subscribeAdminStatus, getAdminStatusSnapshot, getAdminStatusSnapshot);
  const sessions = (((snapshot.status || {}) as Record<string, any>).state?.sessions || []) as SessionRecord[];
  const channelLabelById = buildChannelLabelById([...sessions, session]);
  const channelLabel = resolveSessionChannelLabel(session, channelLabelById);
  const state = sessionOperationalState(session);
  const activityAt = sessionActivityAt(session);
  const primary = sessionPrimaryText(session);
  const first = sessionFirstText(session);
  const blockedInbound = Number(session.blockedInboundCount || 0);
  const currentJobs = activeBackgroundJobs(session);
  const runningJobs = activeBackgroundJobCount(session);
  const totalJobs = Number(session.backgroundJobCount || (Array.isArray(session.backgroundJobs) ? session.backgroundJobs.length : 0));
  const hasJobs = runningJobs > 0;
  return (
    <>
      <AgentSessionHero title={primary} request={first} state={state} channelLabel={channelLabel} activityAt={activityAt} blockedInbound={blockedInbound} runningJobs={runningJobs} totalJobs={totalJobs} />
      <div className="session-body">
        <div className="session-inspector">
          <div className="mini-panel trace-panel session-timeline-panel">
            <div className="mini-title">工作时间线</div>
            <div className="mini-body">
              <SessionTimeline session={session} />
            </div>
          </div>
          <div className="session-side-column">
            <div className="mini-panel">
              <div className="mini-title">接管 / 链接</div>
              <div className="mini-body">
                <SessionActions session={session} isPermalink={isPermalink} />
              </div>
            </div>
            <SessionRuntimePanel session={session} state={state} blockedInbound={blockedInbound} totalJobs={totalJobs} runningJobs={runningJobs} />
            {hasJobs ? (
              <div className="mini-panel">
                <div className="mini-title">后台任务</div>
                <div className="mini-body">{runningJobs > 0 ? <JobsTable session={session} jobs={currentJobs} expectedCount={runningJobs} /> : null}</div>
              </div>
            ) : null}
            <div className="mini-panel">
              <div className="mini-title">时间线统计</div>
              <div className="mini-body">
                <SessionTraceStats sessionKey={String(session.key || "")} />
              </div>
            </div>
            <div className="mini-panel">
              <div className="mini-title">技术上下文</div>
              <div className="mini-body">
                <SessionDebugPanel session={session} channelLabel={channelLabel} channelTitle={String(session.channelId || "")} activityAt={activityAt} />
              </div>
            </div>
          </div>
        </div>
      </div>
    </>
  );
}

export function AgentSessionHero({
  title,
  request,
  state,
  channelLabel,
  activityAt,
  blockedInbound,
  runningJobs,
  totalJobs,
}: {
  readonly title: string;
  readonly request: string;
  readonly state: SessionOperationalState;
  readonly channelLabel: string;
  readonly activityAt: unknown;
  readonly blockedInbound: number;
  readonly runningJobs: number;
  readonly totalJobs: number;
}): React.JSX.Element {
  const deliveryIssue = sessionDeliveryIssueIndicator({ blockedInboundCount: blockedInbound });
  const visibleState = shouldShowSessionState(state) ? { label: state.label, tone: state.tone, title: state.detail } : { label: "Agent 状态未知", tone: "", title: "Gateway 不持有 Agent 执行状态" };
  const stats: Array<SessionMetaItem | null> = [
    { label: "频道", value: channelLabel, title: channelLabel },
    {
      label: "最近",
      value: fmtRelativeTime(activityAt),
      detail: fmtDateTime(activityAt),
      title: fmtDateTime(activityAt),
    },
    runningJobs > 0
      ? {
          label: "任务",
          value: runningJobs + " 运行",
          detail: totalJobs > runningJobs ? "历史共 " + totalJobs : undefined,
          tone: "good",
        }
      : null,
    deliveryIssue
      ? {
          label: "投递失败",
          value: deliveryIssue.value,
          title: deliveryIssue.title,
          tone: deliveryIssue.tone,
        }
      : null,
  ];

  return (
    <div className={"agent-session-hero " + classSafeValue(visibleState.tone, "neutral")}>
      <div className="agent-session-copy">
        <div className="agent-session-kicker">
          <span>Agent Session</span>
          <Badge label={visibleState.label} tone={visibleState.tone} title={visibleState.title} />
        </div>
        <h1 className="agent-session-title" title={title}>
          {title}
        </h1>
        <div className="agent-session-request" title={request}>
          {request}
        </div>
      </div>
      <div className="agent-session-stat-grid">
        {stats
          .filter((item): item is SessionMetaItem => item !== null)
          .map((item) => (
            <div key={item.label} className={"agent-session-stat " + classSafeValue(item.tone, "")} title={item.title || item.detail || item.value}>
              <span>{item.label}</span>
              <strong>{item.value}</strong>
              {item.detail ? <em>{item.detail}</em> : null}
            </div>
          ))}
      </div>
    </div>
  );
}

export function SessionActions({ session, isPermalink }: { readonly session: SessionRecord; readonly isPermalink: boolean }): React.JSX.Element {
  const sessionKey = String(session.key || "");
  const hasSlackThread = String(session.platform || "slack") === "slack" && String(session.mode || "normal") === "normal" && Boolean(session.channelId && session.rootMessageId);
  const [threadBusy, setThreadBusy] = useState(false);
  const [threadError, setThreadError] = useState<string | null>(null);

  async function openSlackThread(): Promise<void> {
    if (!sessionKey || threadBusy) {
      return;
    }
    const opened = window.open("", "_blank");
    setThreadBusy(true);
    setThreadError(null);
    try {
      const payload = (await requestJson(slackThreadUrlApiPath(sessionKey))) as Record<string, any>;
      const url = typeof payload.url === "string" ? payload.url : "";
      if (!url) {
        throw new Error("Slack permalink missing");
      }
      if (opened) {
        try {
          opened.opener = null;
        } catch {}
        opened.location.href = url;
      } else {
        window.open(url, "_blank", "noopener,noreferrer");
      }
    } catch (error) {
      if (opened) {
        opened.close();
      }
      setThreadError("Slack Thread 跳转失败：" + (error instanceof Error ? error.message : String(error)));
    } finally {
      setThreadBusy(false);
    }
  }

  return (
    <div className="side-action-stack">
      <SessionSelectionPanel session={session} />
      <SessionContextPanel session={session} />
      <GitHubIdentityPanel session={session} />
      <SessionResetButton session={session} />
      <div className="side-link-grid">
        {!isPermalink ? (
          <a className="link-button" href={adminSessionPath(String(session.key || ""))}>
            打开独立视图
          </a>
        ) : (
          <a className="link-button" href="/admin">
            返回会话列表
          </a>
        )}
        {hasSlackThread ? (
          <button
            type="button"
            className="link-button"
            disabled={threadBusy || !sessionKey}
            onClick={() => {
              void openSlackThread();
            }}
          >
            {threadBusy ? "正在打开 Slack 线程" : "打开 Slack 线程"}
          </button>
        ) : null}
      </div>
      {threadError ? <div className="summary-detail">{threadError}</div> : null}
    </div>
  );
}
