import { describe, expect, it } from "vitest";
import type { Lane } from "./types";
import {
  laneEnvironments,
  laneExecutorLabel,
  laneProjects,
  lanesInScope,
  preferredEnabledLane,
  receiptIsSynthetic,
  serviceExecutorLabel,
} from "./laneFilters";

const lanes: Lane[] = [
  {
    id: "dev-hooks",
    project: "AutoTask",
    environment: "development",
    enabled: true,
    executor_kind: "hooks",
    hook_transport: "script",
    profile_digest: "profile-dev",
    active_run_id: null,
    generation: 2,
    blocked_reason: null,
  },
  {
    id: "prod-disabled",
    project: "AutoTask",
    environment: "production",
    enabled: false,
    executor_kind: "hooks",
    hook_transport: "webhook",
    profile_digest: "profile-prod",
    active_run_id: null,
    generation: 0,
    blocked_reason: null,
  },
  {
    id: "docs-scripted",
    project: "Docs",
    environment: "development",
    enabled: true,
    executor_kind: "scripted",
    profile_digest: "profile-docs",
    active_run_id: null,
    generation: 1,
    blocked_reason: null,
  },
];

describe("lane filters and executor disclosures", () => {
  it("builds project/environment scopes from the API lane list, including disabled lanes", () => {
    expect(laneProjects(lanes)).toEqual(["AutoTask", "Docs"]);
    expect(laneEnvironments(lanes, "AutoTask")).toEqual(["development", "production"]);
    expect(lanesInScope(lanes, { project: "AutoTask", environment: "production" }).map((lane) => lane.id))
      .toEqual(["prod-disabled"]);
  });

  it("keeps disabled lanes visible but never selects them as active", () => {
    const productionScope = lanesInScope(lanes, { project: "AutoTask", environment: "production" });
    expect(preferredEnabledLane(productionScope, null)).toBeNull();
    expect(preferredEnabledLane(lanes, "prod-disabled")?.id).toBe("dev-hooks");
    expect(laneExecutorLabel(lanes[1]!)).toBe("Hooks · Webhook");
  });

  it("identifies simulated lanes and synthetic receipt evidence without labeling hooks as simulated", () => {
    expect(laneExecutorLabel(lanes[2]!)).toBe("模拟执行 · ScriptedExecutor");
    expect(serviceExecutorLabel("mixed")).toBe("多执行器");
    expect(receiptIsSynthetic({ verification_report: { executor: "SYNTHETIC" } }, "hooks")).toBe(true);
    expect(receiptIsSynthetic({ verification_report: { checks: ["healthy"] } }, "hooks")).toBe(false);
    expect(receiptIsSynthetic({ verification_report: {} }, "scripted")).toBe(true);
  });
});
