import { loadAdminOverview, mergeStatusOverview, requestJson } from "./admin-api.js";

import { Badge } from "./admin-badge.js";

import { errorMessage, quotaTone } from "./admin-formatters.js";

import { getAdminStatusSnapshot, publishAdminStatus } from "./admin-status-store";

import { AdminStatus } from "./admin-types.js";

import { profileAccountLabel, profileBillingLabel, profilePlanLabel, profileRuntimeLabel, profileTitle } from "./auth-profile-display";

import { profileQuotaItems, ProfileQuotaMetrics, profileQuotaSummary } from "./profile-quota-summary.js";

import React, { useMemo, useState } from "react";

export function ProfilesPanel({ status, message, setMessage, onAdd }: { readonly status: AdminStatus; readonly message: string | null; readonly setMessage: (message: string | null) => void; readonly onAdd: () => void }): React.JSX.Element {
  const profiles = [...(status.profiles?.items || [])].sort((left, right) => String(left.profile_id || "").localeCompare(String(right.profile_id || "")));

  async function deleteProfile(profileId: string): Promise<void> {
    if (!window.confirm(`删除 Profile ${profileId}？`)) return;
    setMessage("正在删除 Profile...");
    try {
      await requestJson(`/admin/api/profiles/${encodeURIComponent(profileId)}`, {
        method: "DELETE",
      });
      const overview = await loadAdminOverview();
      publishAdminStatus(mergeStatusOverview(getAdminStatusSnapshot().status, overview));
      setMessage("Profile 已删除");
    } catch (error) {
      setMessage(errorMessage(error));
    }
  }

  return (
    <section className="panel ops-panel">
      <div className="panel-head">
        <div className="panel-title">Profiles</div>
        <button type="button" onClick={onAdd}>
          添加
        </button>
      </div>
      <div className="panel-body maintenance-grid">
        {profiles.length ? (
          profiles.map((profile: Record<string, any>) => {
            const quota = profileQuotaSummary(profile);
            const plan = profilePlanLabel(profile);
            const issue = profile.account?.error || profile.rateLimits?.error || "";
            const cardTone = profile.account?.ok === false || quota.ok === false ? "danger" : quota.tone;
            return (
              <div className={"profile-card " + cardTone} key={String(profile.profile_id)} title={profileTitle(profile)}>
                <div className="profile-card-head">
                  <div className="profile-identity">
                    <div className="profile-account-row">
                      <span className="profile-account">{profileAccountLabel(profile)}</span>
                      <span className="profile-plan-badge">{profileBillingLabel(profile)}</span>
                      {plan ? <span className="profile-plan-badge">{plan}</span> : null}
                      <Badge label={profile.auth_configured ? "认证已配置" : "缺少认证"} tone={profile.auth_configured ? "good" : "danger"} />
                    </div>
                    <div className="profile-card-subtitle">{profileRuntimeLabel(profile)}</div>
                    <ProfileModels models={profile.models} />
                    {issue ? <div className="profile-card-subtitle">{issue}</div> : null}
                  </div>
                  <button
                    className="profile-delete-button danger"
                    type="button"
                    onClick={() => {
                      void deleteProfile(String(profile.profile_id || ""));
                    }}
                  >
                    删除
                  </button>
                </div>
                <ProfileQuotaMetrics quota={quota} />
              </div>
            );
          })
        ) : (
          <div className="empty-state">暂无 Profile</div>
        )}
      </div>
      {message ? (
        <div className="summary-detail" style={{ padding: "0 8px 8px" }}>
          {message}
        </div>
      ) : null}
    </section>
  );
}

function ProfileModels({ models }: { readonly models: unknown }): React.JSX.Element {
  const items = Array.isArray(models) ? models : [];
  return (
    <div className="profile-models">
      <div className="profile-models-title">可用模型</div>
      {items.length ? (
        items.map((model: Record<string, any>, index: number) => {
          const thinking = stringItems(model.thinking);
          const inputs = stringItems(model.capabilities?.input).map(inputCapabilityLabel);
          return (
            <div className="profile-model" key={`${String(model.id || "model")}:${index}`}>
              <div className="profile-model-name">
                <strong>{String(model.id || "未命名模型")}</strong>
                {model.default ? <span className="profile-model-default">默认</span> : null}
              </div>
              <div className="profile-model-meta">
                <span>思考深度：{thinking.length ? thinking.join(" / ") : "未配置"}</span>
                <span>输入：{inputs.length ? inputs.join(" / ") : "未配置"}</span>
              </div>
            </div>
          );
        })
      ) : (
        <div className="profile-model-empty">未配置模型</div>
      )}
    </div>
  );
}

function stringItems(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  return value.map((item) => String(item).trim()).filter(Boolean);
}

function inputCapabilityLabel(value: string): string {
  const labels: Record<string, string> = {
    text: "文本",
    image: "图片",
    audio: "音频",
    video: "视频",
  };
  return labels[value] || value;
}

export function AddProfileDialog({ onClose, onStatus }: { readonly onClose: () => void; readonly onStatus: (message: string | null) => void }): React.JSX.Element {
  const [profileId, setProfileId] = useState("");
  const [text, setText] = useState(`{
  "provider": "openai",
  "billing": "usage",
  "auth": { "type": "api_key", "key": "" },
  "models": [
    {
      "id": "",
      "api": "openai-completions",
      "streaming": true,
      "thinking": ["off"],
      "default_thinking": "off",
      "capabilities": { "input": ["text"] },
      "default": true
    }
  ]
}`);
  const [file, setFile] = useState<File | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  async function saveProfile(): Promise<void> {
    setBusy(true);
    setMessage("正在保存 Profile...");
    try {
      const id = profileId.trim();
      if (!id) throw new Error("必须填写 Profile ID");
      const content = file ? await file.text() : text.trim();
      if (!content) throw new Error("必须提供 Profile JSON");
      const document = JSON.parse(content) as unknown;
      if (!document || typeof document !== "object" || Array.isArray(document)) {
        throw new Error("Profile JSON 必须是对象");
      }
      await requestJson(`/admin/api/profiles/${encodeURIComponent(id)}`, {
        method: "PUT",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(document),
      });
      const overview = await loadAdminOverview();
      publishAdminStatus(mergeStatusOverview(getAdminStatusSnapshot().status, overview));
      onStatus("Profile 已保存");
      onClose();
    } catch (error) {
      setMessage(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <dialog open>
      <div className="modal-content add-profile-modal">
        <div className="modal-heading">
          <div className="panel-title">添加 Profile</div>
          <div className="summary-detail">Profile 定义 provider、认证、模型、思考深度和输入能力。</div>
        </div>
        <label className="summary-detail" style={{ display: "grid", gap: 4 }}>
          <span>Profile ID</span>
          <input aria-label="Profile ID" value={profileId} onChange={(event) => setProfileId(event.target.value)} />
        </label>
        <input type="file" accept="application/json,.json" onChange={(event) => setFile(event.currentTarget.files?.[0] || null)} />
        <textarea aria-label="Profile JSON" value={text} disabled={file !== null} onChange={(event) => setText(event.target.value)} />
        <div className="modal-actions">
          <button className="secondary" type="button" onClick={onClose}>
            取消
          </button>
          <button
            className="primary"
            type="button"
            disabled={busy}
            onClick={() => {
              void saveProfile();
            }}
          >
            保存 Profile
          </button>
        </div>
        {message ? <div className="summary-detail">{message}</div> : null}
      </div>
    </dialog>
  );
}

export function TopbarProfiles({ profiles }: { readonly profiles: readonly Record<string, any>[] }): React.JSX.Element {
  const quotaItems = useMemo(() => profileQuotaItems(profiles), [profiles]);
  return (
    <div className="topbar-center">
      {quotaItems.length ? (
        quotaItems.map((item) => (
          <span className={"quota-pill " + quotaTone(item.remaining)} title={item.title} key={item.title}>
            <strong>{item.label}</strong>
          </span>
        ))
      ) : (
        <span className="quota-meta">Profile 额度未知</span>
      )}
    </div>
  );
}
