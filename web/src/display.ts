import type { RequestStatus, RunPhase, RunState } from "./types";

const requestLabels: Record<RequestStatus, string> = {
  accepted: "已接收",
  waiting: "等待执行",
  satisfied: "已满足",
  needs_attention: "需处理",
  withdrawn: "已撤回",
  expired: "已过期",
};

const runLabels: Record<RunState, string> = {
  planned: "已计划",
  dispatching: "派发中",
  running: "执行中",
  cancel_requested: "正在请求取消",
  superseded: "已被替代",
  canceled: "已取消",
  unknown: "状态未知",
  succeeded: "执行成功",
  failed: "执行失败",
};

const phaseLabels: Record<RunPhase, string> = {
  preparing: "准备",
  building: "构建",
  deploying: "部署",
  verifying: "验收",
};

export function requestLabel(status: RequestStatus): string {
  return requestLabels[status] ?? status;
}

export function runLabel(state: RunState): string {
  return runLabels[state] ?? state;
}

export function phaseLabel(phase: RunPhase): string {
  return phaseLabels[phase] ?? phase;
}

export function requestTone(status: RequestStatus): string {
  if (status === "satisfied") return "success";
  if (status === "needs_attention") return "attention";
  if (status === "accepted" || status === "waiting") return "active";
  return "quiet";
}

export function runTone(state: RunState): string {
  if (state === "succeeded") return "success";
  if (state === "failed") return "failure";
  if (state === "unknown" || state === "cancel_requested") return "attention";
  if (state === "running" || state === "dispatching" || state === "planned") return "active";
  return "quiet";
}

export function priorityLabel(priority: "normal" | "urgent"): string {
  return priority === "urgent" ? "紧急" : "普通";
}

export function formatDate(value: string | null | undefined): string {
  if (!value) return "—";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return new Intl.DateTimeFormat("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  }).format(date);
}

export function shortId(value: string): string {
  return value.length > 14 ? `${value.slice(0, 8)}…${value.slice(-4)}` : value;
}

export function revisionLabel(value: string): string {
  return value.length > 15 ? value.slice(0, 12) : value;
}

export function eventLabel(kind: string): string {
  const known: Record<string, string> = {
    request_created: "请求已创建",
    request_accepted: "请求已接收",
    request_bound_to_run: "请求分配执行",
    request_transferred_to_run: "请求转移到后继",
    run_dispatch_requested: "执行等待派发",
    replacement_dispatch_requested: "后继等待派发",
    phase_granted: "阶段已准入",
    command_delivered: "命令已送达",
    command_acknowledged: "命令已确认",
    receipt_verified_success: "验收通过",
    receipt_verified_failure: "验收失败",
    "executor.scripted.info": "模拟执行日志",
    "executor.scripted.warn": "模拟执行提示",
    request_attached: "请求已关联",
    run_planned: "执行已计划",
    run_dispatched: "执行已派发",
    run_started: "执行已启动",
    phase_changed: "阶段已更新",
    cancel_requested: "已请求取消",
    run_superseded: "执行已被替代",
    run_stopped: "执行已确认停止",
    run_unknown: "执行状态未知",
    receipt_verified: "环境验收通过",
    request_satisfied: "请求已满足",
    run_failed: "执行失败",
    request_needs_attention: "请求需处理",
  };
  return known[kind] ?? kind.replaceAll("_", " ");
}
