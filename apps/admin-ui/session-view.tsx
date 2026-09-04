import React, { useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";

import { getAdminStatusSnapshot, subscribeAdminStatus } from "./admin-status-store";
import type { ImConnection } from "./im-connections";
import { SessionDetail } from "./session-detail.js";
import { SessionListRow } from "./session-list.js";
import { stableSessionOrder } from "./session-order";
import { GitHubBindPage, SessionPermalinkView } from "./session-pages.js";
import { compareSessionsForMode, resolveSelectedSession, sessionMatchesFilter } from "./session-selection.js";
import { buildChannelLabelById } from "./session-row-display";
import type { SessionRecord, UiState } from "./session-types.js";
import { loadUiState, normalizeUiState, persistUiState, readGitHubBindSessionKey, readPermalinkSessionKey } from "./session-view-state.js";

export function AdminSessionsView({ connections = [] }: { readonly connections?: readonly ImConnection[] }): React.JSX.Element {
  const githubBindSessionKey = readGitHubBindSessionKey();
  if (githubBindSessionKey) {
    return <GitHubBindPage sessionKey={githubBindSessionKey} />;
  }

  const permalinkSessionKey = readPermalinkSessionKey();
  if (permalinkSessionKey) {
    return <SessionPermalinkView sessionKey={permalinkSessionKey} />;
  }

  const snapshot = useSyncExternalStore(subscribeAdminStatus, getAdminStatusSnapshot, getAdminStatusSnapshot);
  const status = (snapshot.status || {}) as Record<string, any>;
  const connectionNames = new Map(connections.map((connection) => [connection.id, connection.name]));
  const sessions = ((status.state?.sessions || []) as SessionRecord[]).map<SessionRecord>((session) => ({
    ...session,
    connectionName: connectionNames.get(String(session.connectionId || "")) || session.connectionName || session.connectionId,
  }));
  const state = status.state || {};
  const channelLabelById = useMemo(() => buildChannelLabelById(sessions), [sessions]);
  const [uiState, setUiState] = useState(loadUiState);
  const mode = uiState.sessionFilter;
  const orderRef = useRef<{ viewKey: string; keys: readonly string[] }>({ viewKey: "", keys: [] });

  const filtered = useMemo(() => {
    return sessions.filter((session) => sessionMatchesFilter(session, mode)).sort((left, right) => compareSessionsForMode(mode, left, right));
  }, [mode, sessions]);

  const filteredKeys = filtered.map((session) => String(session.key)).join("\u001f");
  const viewKey = mode;
  const filteredByKey = new Map(filtered.map((session) => [String(session.key), session]));

  orderRef.current = stableSessionOrder(
    orderRef.current,
    viewKey,
    filtered.map((session) => String(session.key)),
  );

  const orderedSessions = orderRef.current.keys.map((key) => filteredByKey.get(key)).filter((session): session is SessionRecord => Boolean(session));

  const selectedSession = resolveSelectedSession(orderedSessions, uiState.selectedSessionKey);

  useEffect(() => {
    if (selectedSession?.key && selectedSession.key !== uiState.selectedSessionKey) {
      updateSessionUiState({ selectedSessionKey: selectedSession.key });
    }
  }, [filteredKeys, selectedSession?.key, uiState.selectedSessionKey]);

  function updateSessionUiState(patch: Partial<UiState>): void {
    setUiState((previous) => {
      const next = normalizeUiState({ ...loadUiState(), ...previous, ...patch });
      persistUiState(next);
      return next;
    });
  }

  return (
    <div className="session-master-detail">
      <section className="panel session-index-panel">
        <div className="panel-head">
          <div className="panel-title">Agent 会话</div>
          <span className="summary-detail">
            {orderedSessions.length} / {sessions.length} · 投递失败 <span id="session-blocked-count">{state.blockedInboundCount || 0}</span>
          </span>
        </div>
        <div className="toolbar session-filter-bar">
          <span className="session-filter-label">视图</span>
          <select id="session-filter" value={mode} onChange={(event) => updateSessionUiState({ sessionFilter: event.target.value })}>
            <option value="all">全部</option>
            <option value="jobs">有运行任务</option>
            <option value="issues">有问题</option>
          </select>
        </div>
        <div id="sessions-panel" className="session-list">
          {orderedSessions.length ? (
            orderedSessions.map((session) => <SessionListRow key={session.key} session={session} selected={selectedSession?.key === session.key} channelLabelById={channelLabelById} onSelect={() => updateSessionUiState({ selectedSessionKey: session.key })} />)
          ) : (
            <div className="empty-state">没有符合当前筛选的会话</div>
          )}
        </div>
      </section>

      <section className="panel session-detail-panel">
        <div id="session-detail-panel" className="panel-body">
          {selectedSession ? <SessionDetail key={selectedSession.key} session={selectedSession} /> : <div className="empty-state">没有可检查的 session</div>}
        </div>
      </section>
    </div>
  );
}
