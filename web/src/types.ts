export type CoverageMode = "exact" | "descendant";
export type Priority = "normal" | "urgent";
export type RequestStatus =
  | "accepted"
  | "waiting"
  | "satisfied"
  | "needs_attention"
  | "withdrawn"
  | "expired";
export type RunState =
  | "planned"
  | "dispatching"
  | "running"
  | "cancel_requested"
  | "superseded"
  | "canceled"
  | "unknown"
  | "succeeded"
  | "failed";
export type RunPhase = "preparing" | "building" | "deploying" | "verifying";
export type ExecutorKind = "scripted" | "hooks";
export type HookTransport = "command" | "script" | "webhook";
export type ServiceExecutor = ExecutorKind | "mixed";

export interface Lane {
  id: string;
  project: string;
  environment: string;
  enabled: boolean;
  executor_kind: ExecutorKind;
  hook_transport?: HookTransport | null;
  profile_digest: string;
  active_run_id: string | null;
  generation: number;
  blocked_reason: string | null;
  [key: string]: unknown;
}

export interface ReleaseRequest {
  id: string;
  lane_id: string;
  revision: string;
  components: string[];
  coverage_mode: CoverageMode;
  priority: Priority;
  status: RequestStatus;
  current_run_id: string | null;
  satisfied_by_run?: string | null;
  created_at: string;
  deadline_at: string | null;
  reason: string;
  satisfied_by_receipt: string | null;
  [key: string]: unknown;
}

export interface DeliveryRun {
  id: string;
  lane_id: string;
  revision: string;
  components: string[];
  profile_digest: string;
  effective_priority: Priority;
  state: RunState;
  phase: RunPhase;
  generation: number;
  replacement_run_id: string | null;
  created_at: string;
  updated_at: string;
  [key: string]: unknown;
}

export interface AuditEvent {
  sequence: number;
  lane_id: string;
  run_id: string | null;
  request_id: string | null;
  timestamp: string;
  kind: string;
  message: string;
  [key: string]: unknown;
}

export interface ExecutorLog {
  sequence: number;
  timestamp: string;
  level: string;
  message: string;
}

export interface SystemInfo {
  executor: ServiceExecutor;
  draining: boolean;
  version: string;
}

export interface ReceiptSummary {
  id: string;
  run_id: string;
  generation: number;
  observed_at: string;
  source_revision: string;
  component_digests: Record<string, unknown>;
  profile_digest: string;
  verification_report: unknown;
  result: "success" | "failure" | string;
  synthetic?: boolean;
  [key: string]: unknown;
}

export type RequestDetail = ReleaseRequest & {
  receipt?: ReceiptSummary | null;
  coverage_reason?: string | null;
};

export type RunDetail = DeliveryRun & {
  receipt?: ReceiptSummary | null;
  coverage_reason?: string | null;
};
