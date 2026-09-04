import { AdminStatus } from "./admin-types.js";

import { DeployPanel } from "./deployment-panel.js";

import { GitHubAccountBindDialog, GitHubAccountsPanel } from "./github-accounts-panel.js";

import { LogsPanel, OperationRecords, ServicePanel } from "./operations-status-panels.js";

import { AddProfileDialog, ProfilesPanel } from "./profiles-panel.js";

import React, { useState } from "react";

export function OperationsView({ status }: { readonly status: AdminStatus }): React.JSX.Element {
  const [addProfileOpen, setAddProfileOpen] = useState(false);
  const [githubBindAccount, setGitHubBindAccount] = useState<Record<string, any> | null>(null);
  const [deployStatus, setDeployStatus] = useState<string | null>(null);
  const [profileStatus, setProfileStatus] = useState<string | null>(null);
  const [githubStatus, setGitHubStatus] = useState<string | null>(null);

  return (
    <div className="ops-page">
      <header className="page-heading ops-heading">
        <div>
          <div className="eyebrow">Control</div>
          <h1>设置与运行</h1>
          <p>管理模型 Profile、部署和 Gateway 运行状态。IM 账号在“IM 接入”中单独管理。</p>
        </div>
      </header>
      <div className="view-grid ops-grid">
        <DeployPanel status={status} message={deployStatus} setMessage={setDeployStatus} />
        <OperationRecords status={status} />
      </div>

      <div className="view-grid ops-grid">
        <ProfilesPanel status={status} message={profileStatus} setMessage={setProfileStatus} onAdd={() => setAddProfileOpen(true)} />
        <GitHubAccountsPanel status={status} message={githubStatus} setMessage={setGitHubStatus} onBind={setGitHubBindAccount} />
      </div>

      <div className="view-grid ops-grid">
        <LogsPanel logs={status.state?.recentBrokerLogs || []} />
        <ServicePanel service={status.service || {}} />
      </div>

      {addProfileOpen ? <AddProfileDialog onClose={() => setAddProfileOpen(false)} onStatus={setProfileStatus} /> : null}
      {githubBindAccount ? <GitHubAccountBindDialog account={githubBindAccount} onClose={() => setGitHubBindAccount(null)} onStatus={setGitHubStatus} /> : null}
    </div>
  );
}
