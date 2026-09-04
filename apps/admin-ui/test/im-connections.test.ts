import React from "react";
import { renderToStaticMarkup } from "react-dom/server";

import { describe, expect, it } from "vite-plus/test";

import { ConnectionSidebarList, ConnectionsView, type ImConnection, type ImProvider } from "../im-connections.js";

describe("IM connection navigation", () => {
  it("renders two accounts of the same provider as separate connections with independent modes", () => {
    const connections: ImConnection[] = [
      {
        id: "work",
        name: "工作 Slack",
        provider: "slack",
        mode: "normal",
        enabled: true,
        configured: true,
        fieldValues: { apiBaseUrl: "https://slack.com/api" },
        secretFieldsSet: { appToken: true, botToken: true },
        runtime: { state: "connected" },
      },
      {
        id: "community",
        name: "社区 Slack",
        provider: "slack",
        mode: "proactive",
        enabled: true,
        configured: true,
        fieldValues: { apiBaseUrl: "https://slack.com/api" },
        secretFieldsSet: { appToken: true, botToken: true },
        runtime: { state: "connecting" },
      },
    ];

    const html = renderToStaticMarkup(
      React.createElement(ConnectionSidebarList, {
        connections,
        selectedId: "community",
        onSelect: () => undefined,
      }),
    );

    expect(html).toContain("工作 Slack");
    expect(html).toContain("社区 Slack");
    expect(html).toContain("Slack · 普通");
    expect(html).toContain("Slack · 主动");
    expect(html).toContain("sidebar-connection active");
  });

  it("uses provider metadata for the connection editor instead of Slack-specific fields", () => {
    const providers: ImProvider[] = [
      {
        id: "matrix",
        name: "Matrix",
        modes: ["normal", "proactive"],
        fields: [
          { id: "homeserver", label: "Homeserver" },
          { id: "accessToken", label: "Access Token", secret: true },
        ],
      },
    ];
    const connections: ImConnection[] = [
      {
        id: "matrix-work",
        name: "工作 Matrix",
        provider: "matrix",
        mode: "normal",
        enabled: true,
        configured: true,
        fieldValues: { homeserver: "https://matrix.example" },
        secretFieldsSet: { accessToken: true },
        runtime: { state: "connected" },
      },
    ];

    const html = renderToStaticMarkup(
      React.createElement(ConnectionsView, {
        connections,
        providers,
        selectedId: "matrix-work",
        onSelect: () => undefined,
        onReload: async () => undefined,
        onOpenCreate: () => undefined,
      }),
    );

    expect(html).toContain("Matrix 接入");
    expect(html).toContain("Homeserver");
    expect(html).toContain("https://matrix.example");
    expect(html).toContain("Access Token");
    expect(html).toContain("已保存，留空则不改");
    expect(html).not.toContain("App Token");
    expect(html).not.toContain("Bot Token");
  });
});
