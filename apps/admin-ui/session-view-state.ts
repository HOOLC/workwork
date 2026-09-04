import { sessionFilters, UiState } from "./session-types.js";

export function readGitHubBindSessionKey(): string | null {
  const match = window.location.pathname.match(/^\/admin\/sessions\/([^/]+)\/github\/bind\/?$/);
  if (!match?.[1]) {
    return null;
  }
  return decodePathSegment(match[1]);
}

export function readPermalinkSessionKey(): string | null {
  if (readGitHubBindSessionKey()) {
    return null;
  }
  const prefix = "/admin/sessions/";
  if (!window.location.pathname.startsWith(prefix)) {
    return null;
  }
  const encoded = window.location.pathname.slice(prefix.length).split("/")[0] || "";
  if (!encoded) {
    return null;
  }
  return decodePathSegment(encoded);
}

export function decodePathSegment(encoded: string): string {
  try {
    return decodeURIComponent(encoded);
  } catch {
    return encoded;
  }
}

export function loadUiState(): UiState {
  try {
    const raw = window.localStorage.getItem(uiStateStorageKey());
    return raw ? normalizeUiState(JSON.parse(raw)) : defaultUiState();
  } catch {
    return defaultUiState();
  }
}

export function persistUiState(next: UiState): void {
  try {
    window.localStorage.setItem(uiStateStorageKey(), JSON.stringify(next));
  } catch {}
}

export function uiStateStorageKey(): string {
  return "admin-ui-state:" + window.location.pathname;
}

export function defaultUiState(): UiState {
  return { adminView: "sessions", sessionFilter: "all", selectedSessionKey: null };
}

export function normalizeUiState(value: unknown): UiState {
  const next = value && typeof value === "object" ? (value as Record<string, unknown>) : {};
  const adminView = ["sessions", "connections", "ops"].includes(String(next.adminView || "")) ? String(next.adminView) : "sessions";
  const sessionFilter = sessionFilters.includes(String(next.sessionFilter || "")) ? String(next.sessionFilter) : "all";
  const selectedSessionKey = typeof next.selectedSessionKey === "string" && next.selectedSessionKey ? next.selectedSessionKey : null;
  return { adminView, sessionFilter, selectedSessionKey };
}
