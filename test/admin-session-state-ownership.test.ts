import { describe, expect, it } from "vite-plus/test";

import { sessionOperationalState } from "../apps/admin-ui/session-row-display.js";
import { summarizeSessionLead } from "../apps/admin-ui/session-selection.js";
import { sessionFilters } from "../apps/admin-ui/session-types.js";
import { defaultUiState, normalizeUiState } from "../apps/admin-ui/session-view-state.js";

describe("Admin session state ownership", () => {
  it("does not infer Agent execution state from Gateway records", () => {
    expect(sessionOperationalState({}).label).toBe("Agent 状态未知");
    expect(sessionOperationalState({ lastUserMessage: { text: "accepted" } })).toMatchObject({
      label: "已投递",
      detail: "已有 Agent mailbox receipt",
    });
    expect(summarizeSessionLead({})).toBe("暂无输入记录");
  });

  it("defaults to all routing sessions instead of a synthetic ongoing view", () => {
    expect(sessionFilters).toEqual(["all", "jobs", "issues"]);
    expect(defaultUiState().sessionFilter).toBe("all");
    expect(normalizeUiState({ sessionFilter: "ongoing" }).sessionFilter).toBe("all");
  });
});
