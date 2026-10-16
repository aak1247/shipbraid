import type {
  AuditEvent,
  DeliveryRun,
  ExecutorLog,
  Lane,
  ReleaseRequest,
  RequestDetail,
  RunDetail,
  SystemInfo,
} from "./types";

type ListEnvelope<T> = { items: T[] };
type Fetcher = (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>;

export class ApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly retryable: boolean;

  constructor(status: number, code: string, message: string, retryable = false) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
    this.retryable = retryable;
  }
}

async function responseJson<T>(response: Response): Promise<T> {
  let payload: unknown;
  try {
    payload = await response.json();
  } catch {
    if (!response.ok) {
      throw new ApiError(response.status, "http_error", `服务返回 HTTP ${response.status}`);
    }
    throw new ApiError(response.status, "invalid_response", "服务返回了无法读取的数据", true);
  }

  if (!response.ok) {
    const error = payload as Partial<{ code: string; message: string; retryable: boolean }>;
    throw new ApiError(
      response.status,
      typeof error.code === "string" ? error.code : "http_error",
      typeof error.message === "string" ? error.message : `服务返回 HTTP ${response.status}`,
      error.retryable === true,
    );
  }
  return payload as T;
}

function listItems<T>(payload: ListEnvelope<T>, label: string): T[] {
  if (!payload || !Array.isArray(payload.items)) {
    throw new ApiError(200, "invalid_response", `${label}响应缺少 items 列表`, true);
  }
  return payload.items;
}

export function createApiClient(token: string, fetcher: Fetcher = fetch) {
  async function get<T>(path: string, signal?: AbortSignal): Promise<T> {
    const response = await fetcher(path, {
      method: "GET",
      headers: {
        Accept: "application/json",
        Authorization: `Bearer ${token}`,
      },
      cache: "no-store",
      signal,
    });
    return responseJson<T>(response);
  }

  return {
    getSystem: (signal?: AbortSignal) => get<SystemInfo>("/v1/system", signal),
    async getLanes(signal?: AbortSignal) {
      const payload = await get<ListEnvelope<Lane>>("/v1/lanes", signal);
      return listItems(payload, "通道");
    },
    async getLane(id: string, signal?: AbortSignal) {
      return get<Lane>(`/v1/lanes/${encodeURIComponent(id)}`, signal);
    },
    async getRequests(laneId: string, signal?: AbortSignal) {
      const query = new URLSearchParams({ lane_id: laneId });
      const payload = await get<ListEnvelope<ReleaseRequest>>(`/v1/requests?${query}`, signal);
      return listItems(payload, "请求");
    },
    async getRuns(laneId: string, signal?: AbortSignal) {
      const query = new URLSearchParams({ lane_id: laneId });
      const payload = await get<ListEnvelope<DeliveryRun>>(`/v1/runs?${query}`, signal);
      return listItems(payload, "执行");
    },
    getRequest: (id: string, signal?: AbortSignal) =>
      get<RequestDetail>(`/v1/requests/${encodeURIComponent(id)}`, signal),
    getRun: (id: string, signal?: AbortSignal) =>
      get<RunDetail>(`/v1/runs/${encodeURIComponent(id)}`, signal),
    async getEvents(laneId: string, after: number, signal?: AbortSignal) {
      const query = new URLSearchParams({ lane_id: laneId, after: String(after), limit: "200" });
      const payload = await get<ListEnvelope<AuditEvent>>(`/v1/events?${query}`, signal);
      return listItems(payload, "事件");
    },
    async getLogs(runId: string, after: number, signal?: AbortSignal) {
      const query = new URLSearchParams({ after: String(after), limit: "100" });
      const payload = await get<{ items: ExecutorLog[]; next_cursor: number }>(
        `/v1/runs/${encodeURIComponent(runId)}/logs?${query}`,
        signal,
      );
      if (!payload || !Array.isArray(payload.items) || !Number.isFinite(payload.next_cursor)) {
        throw new ApiError(200, "invalid_response", "执行日志响应格式无效", true);
      }
      return payload;
    },
  };
}

export type ApiClient = ReturnType<typeof createApiClient>;

export function describeApiError(error: unknown): string {
  if (error instanceof ApiError) {
    if (error.status === 401 || error.status === 403) return "访问令牌无效或权限不足，请检查后重新连接。";
    if (error.status === 404) return "记录已不存在或当前令牌无权查看。刷新列表后重试。";
    return error.message;
  }
  if (error instanceof TypeError) return "无法连接协调器，请确认服务已启动后重试。";
  if (error instanceof Error) return error.message;
  return "读取协调器数据失败，请重试。";
}
