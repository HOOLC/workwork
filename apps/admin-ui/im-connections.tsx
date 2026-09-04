import React, { useCallback, useEffect, useMemo, useState } from "react";

import { requestJson } from "./admin-api.js";
import { errorMessage } from "./admin-formatters.js";

export type ImMode = "normal" | "proactive";

export type ImConnection = {
  readonly id: string;
  readonly name: string;
  readonly provider: string;
  readonly mode: ImMode;
  readonly enabled: boolean;
  readonly configured: boolean;
  readonly fieldValues: Readonly<Record<string, string>>;
  readonly secretFieldsSet: Readonly<Record<string, boolean>>;
  readonly runtime?: {
    readonly state?: string;
    readonly identity?: Record<string, any> | null;
    readonly error?: string | null;
    readonly connectedAt?: string | null;
  };
};

export type ImProvider = {
  readonly id: string;
  readonly name: string;
  readonly modes: readonly ImMode[];
  readonly fields?: readonly {
    readonly id: string;
    readonly label: string;
    readonly secret?: boolean;
    readonly optional?: boolean;
    readonly placeholder?: string;
  }[];
};

export function useImConnections(): {
  readonly connections: readonly ImConnection[];
  readonly providers: readonly ImProvider[];
  readonly loading: boolean;
  readonly error: string | null;
  readonly reload: () => Promise<void>;
} {
  const [connections, setConnections] = useState<readonly ImConnection[]>([]);
  const [providers, setProviders] = useState<readonly ImProvider[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(async (): Promise<void> => {
    try {
      const [connectionPayload, providerPayload] = await Promise.all([requestJson("/admin/api/im/connections"), requestJson("/admin/api/im/providers")]);
      setConnections(Array.isArray(connectionPayload.connections) ? (connectionPayload.connections as ImConnection[]) : []);
      setProviders(Array.isArray(providerPayload.providers) ? (providerPayload.providers as ImProvider[]) : []);
      setError(null);
    } catch (nextError) {
      setError(errorMessage(nextError));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    let active = true;
    const load = async (): Promise<void> => {
      if (active) await reload();
    };
    void load();
    const timer = window.setInterval(() => void load(), 5_000);
    return () => {
      active = false;
      window.clearInterval(timer);
    };
  }, [reload]);

  return { connections, providers, loading, error, reload };
}

export function ConnectionSidebarList({ connections, providers = [], selectedId, onSelect }: { readonly connections: readonly ImConnection[]; readonly providers?: readonly ImProvider[]; readonly selectedId: string | null; readonly onSelect: (id: string) => void }): React.JSX.Element {
  return (
    <div className="sidebar-connection-list">
      {connections.map((connection) => (
        <button className={"sidebar-connection" + (selectedId === connection.id ? " active" : "")} type="button" key={connection.id} onClick={() => onSelect(connection.id)}>
          <span className={"connection-dot " + connectionState(connection)} aria-hidden="true" />
          <span className="sidebar-connection-copy">
            <span className="sidebar-connection-name">{connection.name}</span>
            <span className="sidebar-connection-meta">
              {providers.find((provider) => provider.id === connection.provider)?.name || providerLabel(connection.provider)} · {modeLabel(connection.mode)}
            </span>
          </span>
        </button>
      ))}
      {!connections.length ? <div className="sidebar-empty">还没有 IM 接入</div> : null}
    </div>
  );
}

export function ConnectionsView({
  connections,
  providers,
  selectedId,
  onSelect,
  onReload,
  onOpenCreate,
}: {
  readonly connections: readonly ImConnection[];
  readonly providers: readonly ImProvider[];
  readonly selectedId: string | null;
  readonly onSelect: (id: string | null) => void;
  readonly onReload: () => Promise<void>;
  readonly onOpenCreate: () => void;
}): React.JSX.Element {
  const selected = useMemo(() => connections.find((connection) => connection.id === selectedId) ?? connections[0] ?? null, [connections, selectedId]);

  useEffect(() => {
    if (selected && selected.id !== selectedId) onSelect(selected.id);
  }, [onSelect, selected, selectedId]);

  if (!selected) {
    return (
      <main className="control-page connections-page empty-connections-page">
        <div className="empty-hero">
          <div className="provider-mark">Z</div>
          <h1>连接一个 IM</h1>
          <p>每个接入都有独立账号、连接状态和消息模式。同一种 IM 可以添加多次。</p>
          <button className="primary control-button" type="button" onClick={onOpenCreate}>
            添加接入
          </button>
        </div>
      </main>
    );
  }

  const provider = providers.find((candidate) => candidate.id === selected.provider);
  return <ConnectionDetail key={selected.id} connection={selected} provider={provider} onReload={onReload} onDeleted={() => onSelect(null)} />;
}

function ConnectionDetail({ connection, provider, onReload, onDeleted }: { readonly connection: ImConnection; readonly provider: ImProvider | undefined; readonly onReload: () => Promise<void>; readonly onDeleted: () => void }): React.JSX.Element {
  const [name, setName] = useState(connection.name);
  const [mode, setMode] = useState<ImMode>(connection.mode);
  const [enabled, setEnabled] = useState(connection.enabled);
  const [providerFields, setProviderFields] = useState<Record<string, string>>(() => Object.fromEntries((provider?.fields || []).map((field) => [field.id, field.secret ? "" : connection.fieldValues?.[field.id] || ""])));
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const identity = connection.runtime?.identity || {};

  async function save(): Promise<void> {
    setBusy(true);
    setMessage(null);
    try {
      const changedProviderFields = Object.fromEntries((provider?.fields || []).filter((field) => !field.secret || (providerFields[field.id] || "").trim()).map((field) => [field.id, (providerFields[field.id] || "").trim()]));
      await requestJson(`/admin/api/im/connections/${encodeURIComponent(connection.id)}`, {
        method: "PATCH",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          name: name.trim(),
          mode,
          enabled,
          ...changedProviderFields,
        }),
      });
      setProviderFields((current) => Object.fromEntries(Object.entries(current).map(([fieldId, value]) => [fieldId, provider?.fields?.find((field) => field.id === fieldId)?.secret ? "" : value])));
      setMessage("已保存，Gateway 正在应用这个接入的配置。");
      await onReload();
    } catch (error) {
      setMessage(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  async function remove(): Promise<void> {
    if (!window.confirm(`删除 IM 接入“${connection.name}”？历史 Session 会保留。`)) return;
    setBusy(true);
    try {
      const response = await fetch(`/admin/api/im/connections/${encodeURIComponent(connection.id)}`, { method: "DELETE" });
      if (!response.ok) throw new Error((await response.json().catch(() => ({}))).error || "删除失败");
      onDeleted();
      await onReload();
    } catch (error) {
      setMessage(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="control-page connections-page">
      <header className="page-heading connection-heading">
        <div>
          <div className="eyebrow">{provider?.name || providerLabel(connection.provider)} 接入</div>
          <h1>{connection.name}</h1>
          <div className="connection-heading-meta">
            <span className={"runtime-state " + connectionState(connection)}>
              <span className="connection-dot" /> {connectionStateLabel(connection)}
            </span>
            <span>{modeLabel(connection.mode)}模式</span>
            {identity.userId ? <span>{String(identity.username || identity.userId)}</span> : null}
          </div>
        </div>
        <label className="enable-switch">
          <input type="checkbox" checked={enabled} onChange={(event) => setEnabled(event.target.checked)} />
          <span>{enabled ? "已启用" : "已停用"}</span>
        </label>
      </header>

      <div className="settings-stack">
        <section className="settings-section">
          <div className="settings-copy">
            <h2>消息模式</h2>
            <p>模式只决定消息如何绑定 Agent Session；所有消息仍通过 mailbox 进入 Agent。</p>
          </div>
          <div className="mode-picker" role="radiogroup" aria-label="消息模式">
            {(provider?.modes || [connection.mode]).map((candidate) => (
              <button className={mode === candidate ? "active" : ""} type="button" role="radio" aria-checked={mode === candidate} key={candidate} onClick={() => setMode(candidate)}>
                <strong>{modeLabel(candidate)}</strong>
                <span>{candidate === "proactive" ? "这个接入的消息进入同一个 Session，由 Agent 判断是否回复" : "每个对话或 thread 使用独立 Session"}</span>
              </button>
            ))}
          </div>
        </section>

        <section className="settings-section">
          <div className="settings-copy">
            <h2>接入信息</h2>
            <p>凭证只保存在 Gateway 配置中，Control 不会读取或返回原文。</p>
          </div>
          <div className="settings-form">
            <label>
              名称
              <input value={name} onChange={(event) => setName(event.target.value)} />
            </label>
            {(provider?.fields || []).map((field) => (
              <label key={field.id}>
                <span>
                  {field.label} {field.optional ? <span className="optional-label">可选</span> : null}
                </span>
                <input
                  type={field.secret ? "password" : "text"}
                  autoComplete="off"
                  value={providerFields[field.id] || ""}
                  placeholder={field.secret && connection.secretFieldsSet?.[field.id] ? "已保存，留空则不改" : field.placeholder || ""}
                  onChange={(event) => setProviderFields((current) => ({ ...current, [field.id]: event.target.value }))}
                />
              </label>
            ))}
          </div>
        </section>

        <section className="settings-section runtime-section">
          <div className="settings-copy">
            <h2>当前连接</h2>
            <p>这是 transport 连接状态，不是 Agent Session 状态。</p>
          </div>
          <dl className="runtime-details">
            <div>
              <dt>状态</dt>
              <dd>{connectionStateLabel(connection)}</dd>
            </div>
            <div>
              <dt>Bot</dt>
              <dd>{String(identity.username || identity.userId || "尚未识别")}</dd>
            </div>
            <div>
              <dt>连接时间</dt>
              <dd>{connection.runtime?.connectedAt ? new Date(connection.runtime.connectedAt).toLocaleString("zh-CN") : "—"}</dd>
            </div>
            {connection.runtime?.error ? (
              <div className="runtime-error">
                <dt>错误</dt>
                <dd>{connection.runtime.error}</dd>
              </div>
            ) : null}
          </dl>
        </section>
      </div>

      <footer className="settings-actions">
        <button className="danger ghost-button" type="button" disabled={busy} onClick={() => void remove()}>
          删除接入
        </button>
        <div>
          {message ? <span className="form-message">{message}</span> : null}
          <button className="primary control-button" type="button" disabled={busy || !name.trim()} onClick={() => void save()}>
            {busy ? "保存中…" : "保存更改"}
          </button>
        </div>
      </footer>
    </main>
  );
}

export function AddConnectionDialog({ providers, onClose, onCreated }: { readonly providers: readonly ImProvider[]; readonly onClose: () => void; readonly onCreated: (connection: ImConnection) => void }): React.JSX.Element {
  const [providerId, setProviderId] = useState(providers[0]?.id || "");
  const provider = providers.find((candidate) => candidate.id === providerId) || providers[0];
  const [name, setName] = useState("");
  const [mode, setMode] = useState<ImMode>("normal");
  const [providerFields, setProviderFields] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  async function create(): Promise<void> {
    if (!provider) return;
    setBusy(true);
    setMessage(null);
    try {
      const payload = await requestJson("/admin/api/im/connections", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          name: name.trim(),
          provider: provider.id,
          mode,
          enabled: true,
          ...Object.fromEntries(Object.entries(providerFields).map(([key, value]) => [key, value.trim()])),
        }),
      });
      onCreated(payload.connection as ImConnection);
    } catch (error) {
      setMessage(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={(event) => event.target === event.currentTarget && onClose()}>
      <section className="control-dialog" role="dialog" aria-modal="true" aria-labelledby="add-connection-title">
        <header>
          <div>
            <div className="eyebrow">新的 IM 接入</div>
            <h2 id="add-connection-title">{provider ? `连接 ${provider.name}` : "添加 IM 接入"}</h2>
          </div>
          <button className="icon-button" type="button" aria-label="关闭" onClick={onClose}>
            ×
          </button>
        </header>
        <div className="dialog-form">
          {provider ? (
            <>
              <label>
                名称
                <input autoFocus value={name} placeholder="例如：团队 Slack" onChange={(event) => setName(event.target.value)} />
              </label>
              <label>
                IM 类型
                <select
                  value={provider.id}
                  onChange={(event) => {
                    const nextProvider = providers.find((candidate) => candidate.id === event.target.value);
                    setProviderId(event.target.value);
                    setProviderFields({});
                    if (nextProvider && !nextProvider.modes.includes(mode)) setMode(nextProvider.modes[0] || "normal");
                  }}
                >
                  {providers.map((candidate) => (
                    <option value={candidate.id} key={candidate.id}>
                      {candidate.name}
                    </option>
                  ))}
                </select>
              </label>
            </>
          ) : null}
          <div>
            <span className="field-label">模式</span>
            <div className="compact-mode-picker">
              {(provider?.modes || []).map((candidate) => (
                <button className={mode === candidate ? "active" : ""} type="button" key={candidate} onClick={() => setMode(candidate)}>
                  {modeLabel(candidate)}
                </button>
              ))}
            </div>
          </div>
          {(provider?.fields || []).map((field) => (
            <label key={field.id}>
              <span>
                {field.label} {field.optional ? <span className="optional-label">可选</span> : null}
              </span>
              <input type={field.secret ? "password" : "text"} autoComplete="off" value={providerFields[field.id] || ""} placeholder={field.placeholder || ""} onChange={(event) => setProviderFields((current) => ({ ...current, [field.id]: event.target.value }))} />
            </label>
          ))}
          {message ? <div className="dialog-error">{message}</div> : null}
        </div>
        <footer>
          <button className="ghost-button" type="button" onClick={onClose}>
            取消
          </button>
          <button className="primary control-button" type="button" disabled={busy || !provider || !name.trim() || (provider.fields || []).some((field) => !field.optional && !(providerFields[field.id] || "").trim())} onClick={() => void create()}>
            {busy ? "连接中…" : "添加接入"}
          </button>
        </footer>
      </section>
    </div>
  );
}

function connectionState(connection: ImConnection): string {
  if (!connection.enabled) return "disabled";
  return String(connection.runtime?.state || (connection.configured ? "connecting" : "error"));
}

function connectionStateLabel(connection: ImConnection): string {
  const labels: Record<string, string> = {
    connected: "已连接",
    connecting: "连接中",
    error: "连接失败",
    disabled: "已停用",
  };
  return labels[connectionState(connection)] || connectionState(connection);
}

function modeLabel(mode: ImMode): string {
  return mode === "proactive" ? "主动" : "普通";
}

function providerLabel(provider: string): string {
  return provider === "slack" ? "Slack" : provider;
}
