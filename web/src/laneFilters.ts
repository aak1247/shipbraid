import type { Lane, ServiceExecutor } from "./types";

export type LaneScope = {
  project: string;
  environment: string;
};

export function lanesInScope(lanes: Lane[], scope: LaneScope): Lane[] {
  return lanes.filter((lane) =>
    (!scope.project || lane.project === scope.project)
    && (!scope.environment || lane.environment === scope.environment),
  );
}

export function laneProjects(lanes: Lane[]): string[] {
  return [...new Set(lanes.map((lane) => lane.project))].sort((a, b) => a.localeCompare(b, "zh-CN"));
}

export function laneEnvironments(lanes: Lane[], project: string): string[] {
  const scoped = project ? lanes.filter((lane) => lane.project === project) : lanes;
  return [...new Set(scoped.map((lane) => lane.environment))].sort((a, b) => a.localeCompare(b, "zh-CN"));
}

export function isLaneEnabled(lane: Lane): boolean {
  return lane.enabled !== false;
}

export function preferredEnabledLane(lanes: Lane[], currentId: string | null): Lane | null {
  const enabled = lanes.filter(isLaneEnabled);
  return enabled.find((lane) => lane.id === currentId) ?? enabled[0] ?? null;
}

export function laneExecutorLabel(lane: Lane, fallback: ServiceExecutor | null = null): string {
  const kind = lane.executor_kind as string | undefined;
  if (kind === "scripted") return "模拟执行 · ScriptedExecutor";
  if (kind !== "hooks") return fallback ? serviceExecutorLabel(fallback) : "执行器未知";
  const transports: Record<string, string> = {
    command: "命令",
    script: "脚本",
    webhook: "Webhook",
  };
  const transport = lane.hook_transport ? transports[lane.hook_transport] ?? lane.hook_transport : null;
  return transport ? `Hooks · ${transport}` : "Hooks 执行器";
}

export function serviceExecutorLabel(executor: ServiceExecutor): string {
  if (executor === "scripted") return "模拟执行 · ScriptedExecutor";
  if (executor === "hooks") return "Hooks 执行器";
  if (executor === "mixed") return "多执行器";
  return executor;
}

export function receiptIsSynthetic(
  receipt: { synthetic?: boolean; verification_report: unknown },
  laneExecutorKind: Lane["executor_kind"] | null,
): boolean {
  if (receipt.synthetic === true || laneExecutorKind === "scripted") return true;
  try {
    return /scriptedexecutor|synthetic/i.test(JSON.stringify(receipt.verification_report));
  } catch {
    return false;
  }
}
