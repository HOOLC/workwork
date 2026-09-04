import React from "react";
import { renderToStaticMarkup } from "react-dom/server";

import { describe, expect, it } from "vite-plus/test";

import { ProfilesPanel } from "../profiles-panel.js";

function renderProfile(profile: Record<string, unknown>): string {
  return renderToStaticMarkup(
    React.createElement(ProfilesPanel, {
      status: { profiles: { items: [profile] } },
      message: null,
      setMessage: () => undefined,
      onAdd: () => undefined,
    }),
  );
}

function occurrences(value: string, search: string): number {
  return value.split(search).length - 1;
}

describe("Profile card display", () => {
  it("shows every configured usage model once without treating unreported quota as an error", () => {
    const html = renderProfile({
      profile_id: "qwen-27b",
      provider: "openai-compatible",
      billing: "usage",
      auth_configured: true,
      account: {
        ok: true,
        account: { type: "openai-compatible", planType: "Custom API" },
      },
      rateLimits: { ok: true, reported: false },
      models: [
        {
          id: "qwen-primary",
          thinking: ["xhigh"],
          default_thinking: "xhigh",
          capabilities: { input: ["text"] },
          default: true,
        },
        {
          id: "qwen-secondary",
          thinking: ["low", "high"],
          default_thinking: "high",
          capabilities: { input: ["text", "image"] },
          default: false,
        },
      ],
    });

    expect(html).toContain("API Key");
    expect(html).toContain("可用模型");
    expect(html).toContain("思考深度：xhigh");
    expect(html).toContain("输入：文本 / 图片");
    expect(html).toContain("未提供额度信息");
    expect(occurrences(html, "qwen-primary")).toBe(1);
    expect(occurrences(html, "qwen-secondary")).toBe(1);
    expect(html).not.toContain("profile-card danger");
    expect(html).not.toContain("profile-quota-error");
    expect(html).not.toContain("not_reported_by_provider");
  });

  it("keeps a real authentication failure in the error state", () => {
    const html = renderProfile({
      profile_id: "broken",
      provider: "openai-compatible",
      billing: "usage",
      auth_configured: true,
      account: { ok: false, error: "invalid API key" },
      rateLimits: { ok: false, error: "invalid API key" },
      models: [],
    });

    expect(html).toContain("profile-card danger");
    expect(html).toContain("invalid API key");
  });
});
