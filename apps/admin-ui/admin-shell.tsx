import React, { useEffect, useState, useSyncExternalStore } from "react";

import { loadAdminLogs, loadAdminOverview, loadAdminSessionsStatus, mergeStatusLogs, mergeStatusOverview } from "./admin-api.js";
import { errorMessage } from "./admin-formatters.js";
import { connectAdminRealtime, getAdminStatusSnapshot, publishAdminStatus, subscribeAdminStatus } from "./admin-status-store";
import type { AdminStatus, AdminView } from "./admin-types.js";
import { loadAdminView, persistAdminView } from "./admin-view-state.js";
import { AddConnectionDialog, ConnectionSidebarList, ConnectionsView, useImConnections } from "./im-connections";
import { OperationsView } from "./operations-view.js";
import { TopbarProfiles } from "./profiles-panel.js";
import { AdminSessionsView } from "./session-view";

export function AdminShell({ serviceName }: { readonly serviceName: string }): React.JSX.Element {
  const snapshot = useSyncExternalStore(subscribeAdminStatus, getAdminStatusSnapshot, getAdminStatusSnapshot);
  const status = (snapshot.status || {}) as AdminStatus;
  const [adminView, setAdminView] = useState<AdminView>(loadAdminView);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [selectedConnectionId, setSelectedConnectionId] = useState<string | null>(null);
  const [addConnectionOpen, setAddConnectionOpen] = useState(false);
  const im = useImConnections();

  useEffect(() => {
    let cancelled = false;
    let disconnectRealtime: (() => void) | undefined;
    async function load(): Promise<void> {
      try {
        const nextStatus = await loadAdminSessionsStatus();
        if (!cancelled) {
          publishAdminStatus(nextStatus);
          disconnectRealtime = connectAdminRealtime();
          setLoadError(null);
          void loadAdminOverview()
            .then((overview) => {
              if (!cancelled) publishAdminStatus(mergeStatusOverview(getAdminStatusSnapshot().status, overview));
            })
            .catch((error) => {
              if (!cancelled) setLoadError(errorMessage(error));
            });
          void loadAdminLogs()
            .then((logsStatus) => {
              if (!cancelled) publishAdminStatus(mergeStatusLogs(getAdminStatusSnapshot().status, logsStatus.logs));
            })
            .catch(() => undefined);
        }
      } catch (error) {
        if (!cancelled) setLoadError(error instanceof Error ? error.message : String(error));
      }
    }
    void load();
    return () => {
      cancelled = true;
      disconnectRealtime?.();
    };
  }, []);

  function switchView(nextView: AdminView): void {
    setAdminView(nextView);
    persistAdminView(nextView);
  }

  function openConnection(id: string): void {
    setSelectedConnectionId(id);
    switchView("connections");
  }

  return (
    <div className="shell control-shell" data-service-name={serviceName}>
      <aside className="control-sidebar">
        <div className="control-brand">
          <span className="control-brand-mark">Z</span>
          <span>Zork</span>
        </div>

        <nav className="sidebar-nav" aria-label="Control 模块">
          <button className={adminView === "sessions" ? "active" : ""} type="button" onClick={() => switchView("sessions")}>
            <NavIcon kind="sessions" />
            <span>会话</span>
          </button>
          <button className={adminView === "connections" ? "active" : ""} type="button" onClick={() => switchView("connections")}>
            <NavIcon kind="connections" />
            <span>IM 接入</span>
          </button>
          <button className={adminView === "ops" ? "active" : ""} type="button" onClick={() => switchView("ops")}>
            <NavIcon kind="operations" />
            <span>设置与运行</span>
          </button>
        </nav>

        <section className="sidebar-section">
          <div className="sidebar-section-heading">
            <span>IM 接入</span>
            <button className="sidebar-add" type="button" aria-label="添加 IM 接入" onClick={() => setAddConnectionOpen(true)}>
              +
            </button>
          </div>
          <ConnectionSidebarList connections={im.connections} providers={im.providers} selectedId={adminView === "connections" ? selectedConnectionId : null} onSelect={openConnection} />
          {im.error ? <div className="sidebar-error">{im.error}</div> : null}
        </section>

        <div className="sidebar-spacer" />
        <div className="sidebar-profile-summary">
          <TopbarProfiles profiles={status.profiles?.items || []} />
        </div>
        <div className="sidebar-footer">
          <span className="connection-dot connected" />
          <span>Gateway 运行中</span>
        </div>
      </aside>

      <div className="control-main">
        {loadError ? <div className="global-error">{loadError}</div> : null}
        <section className={"admin-view" + (adminView === "sessions" ? " active" : "")} data-admin-view="sessions">
          <AdminSessionsView connections={im.connections} />
        </section>
        <section className={"admin-view" + (adminView === "connections" ? " active" : "")} data-admin-view="connections">
          <ConnectionsView connections={im.connections} providers={im.providers} selectedId={selectedConnectionId} onSelect={setSelectedConnectionId} onReload={im.reload} onOpenCreate={() => setAddConnectionOpen(true)} />
        </section>
        <section className={"admin-view" + (adminView === "ops" ? " active" : "")} data-admin-view="ops">
          <OperationsView status={status} />
        </section>
      </div>

      {addConnectionOpen ? (
        <AddConnectionDialog
          providers={im.providers}
          onClose={() => setAddConnectionOpen(false)}
          onCreated={(connection) => {
            setAddConnectionOpen(false);
            setSelectedConnectionId(connection.id);
            switchView("connections");
            void im.reload();
          }}
        />
      ) : null}
    </div>
  );
}

function NavIcon({ kind }: { readonly kind: "sessions" | "connections" | "operations" }): React.JSX.Element {
  const paths = {
    sessions: "M4 5.5h16v11H8l-4 3v-14Zm4 4h8M8 13h5",
    connections: "M8 7V5a3 3 0 0 1 6 0v2m-8 0h10a2 2 0 0 1 2 2v8H4V9a2 2 0 0 1 2-2Zm3 10v2m6-2v2",
    operations: "M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8Zm0-5v2m0 14v2M3 12h2m14 0h2M5.6 5.6 7 7m10 10 1.4 1.4m0-12.8L17 7M7 17l-1.4 1.4",
  };
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <path d={paths[kind]} />
    </svg>
  );
}
