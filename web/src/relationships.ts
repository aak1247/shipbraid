import type { ReleaseRequest } from "./types";

export function requestRunId(request: Pick<ReleaseRequest, "current_run_id"> & Partial<Pick<ReleaseRequest, "satisfied_by_run">>): string | null {
  return request.current_run_id ?? request.satisfied_by_run ?? null;
}

// Transferred requests still belong to the original run's audit history.
export function requestRelatedToRun(
  request: Pick<import("./types").ReleaseRequest, "id" | "current_run_id" | "satisfied_by_run">,
  runId: string,
  events: Pick<import("./types").AuditEvent, "run_id" | "request_id">[],
): boolean {
  return requestRunId(request) === runId || events.some((event) => event.run_id === runId && event.request_id === request.id);
}
