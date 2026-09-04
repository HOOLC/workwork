import React, { useEffect, useState } from "react";

import { requestJson } from "./session-api.js";
import type { SessionRecord } from "./session-types.js";

type ContextConfig = { strategy: "compaction" | "handoff"; keep_recent_tokens: number };

export function SessionContextPanel({ session }: { readonly session: SessionRecord }): React.JSX.Element {
  return <ContextEditor key={String(session.id || session.key)} session={session} />;
}

function ContextEditor({ session }: { readonly session: SessionRecord }): React.JSX.Element {
  const [config, setConfig] = useState<ContextConfig | null>(null);
  const [recent, setRecent] = useState("");
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const path = `/admin/api/sessions/${encodeURIComponent(String(session.key))}/context`;

  useEffect(() => {
    let active = true;
    if (session.id) {
      void requestJson(path).then(
        (value) => {
          if (!active) return;
          const loaded = value as ContextConfig;
          setConfig(loaded);
          setRecent(String(loaded.keep_recent_tokens));
        },
        (error: unknown) => {
          if (active) setMessage(error instanceof Error ? error.message : String(error));
        },
      );
    }
    return () => {
      active = false;
    };
  }, [path, session.id]);

  const count = Number(recent);
  const valid = recent.trim() !== "" && Number.isInteger(count) && count >= 0 && count <= 4_294_967_295;

  async function save(): Promise<void> {
    if (!config || !valid) return;
    setBusy(true);
    setMessage("");
    try {
      const saved = (await requestJson(path, {
        method: "PUT",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ ...config, keep_recent_tokens: count }),
      })) as ContextConfig;
      setConfig(saved);
      setRecent(String(saved.keep_recent_tokens));
      setMessage("已保存，下次整理上下文时生效");
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="auth-profile-panel">
      <div className="mini-title">上下文</div>
      {config ? (
        <>
          <div className="session-selection-fields">
            <label className="session-selection-field">
              <span>整理方式</span>
              <select value={config.strategy} disabled={busy} onChange={(event) => setConfig({ ...config, strategy: event.target.value as ContextConfig["strategy"] })}>
                <option value="compaction">摘要并保留最近原文</option>
                <option value="handoff">交接文档</option>
              </select>
            </label>
            <label className="session-selection-field">
              <span>最近原文目标（tokens）</span>
              <input type="number" min="0" max="4294967295" step="1" value={recent} disabled={busy || config.strategy === "handoff"} onChange={(event) => setRecent(event.target.value)} />
            </label>
          </div>
          <div className="summary-detail">摘要模式会保留最近原文，实际范围受模型输入预算限制；交接模式只保留交接文档。</div>
          <button type="button" className="link-button session-selection-apply" disabled={busy || !valid} onClick={() => void save()}>
            {busy ? "保存中…" : "保存上下文设置"}
          </button>
        </>
      ) : (
        <div className="summary-detail">{session.id ? "正在读取上下文设置…" : "Agent 会话创建后可调整"}</div>
      )}
      {message ? (
        <div className="summary-detail" role="status">
          {message}
        </div>
      ) : null}
    </div>
  );
}
