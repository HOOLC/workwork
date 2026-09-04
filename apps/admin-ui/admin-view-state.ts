import { AdminView } from "./admin-types.js";

export function loadAdminView(): AdminView {
  try {
    const raw = window.localStorage.getItem(uiStateStorageKey());
    const parsed = raw ? (JSON.parse(raw) as Record<string, unknown>) : {};
    return parsed.adminView === "ops" || parsed.adminView === "connections" ? parsed.adminView : "sessions";
  } catch {
    return "sessions";
  }
}

export function persistAdminView(adminView: AdminView): void {
  try {
    const raw = window.localStorage.getItem(uiStateStorageKey());
    const previous = raw ? (JSON.parse(raw) as Record<string, unknown>) : {};
    window.localStorage.setItem(uiStateStorageKey(), JSON.stringify({ ...previous, adminView }));
  } catch {}
}

export function uiStateStorageKey(): string {
  return "admin-ui-state:" + window.location.pathname;
}
