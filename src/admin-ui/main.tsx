import { createRoot } from "react-dom/client";

import "./admin.css";
import { AdminShell } from "./admin-shell";

interface AdminConfig {
  readonly serviceName?: string;
}

function readAdminConfig(): AdminConfig {
  const element = document.getElementById("admin-config");
  if (!element?.textContent) {
    return {};
  }

  try {
    return JSON.parse(element.textContent) as AdminConfig;
  } catch {
    return {};
  }
}

function isSessionPermalinkPath(): boolean {
  return /^\/admin\/sessions\/[^/]+/.test(window.location.pathname);
}

const config = readAdminConfig();
const rootElement = document.getElementById("admin-root");
const sessionPermalinkPage = isSessionPermalinkPath();

if (!rootElement) {
  throw new Error("missing admin root");
}

document.body.classList.toggle("session-permalink-page", sessionPermalinkPage);

createRoot(rootElement).render(<AdminShell serviceName={config.serviceName || "workwork"} />);
