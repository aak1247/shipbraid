import { describe, expect, it, vi } from "vitest";
import { ApiError, createApiClient } from "./api";
import { transitionLogTarget } from "./logFeed";
import { requestRelatedToRun, requestRunId } from "./relationships";

function jsonResponse(value: unknown, status = 200): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

describe("dashboard API client", () => {
  it("keeps the bearer token in the authorization header and encodes the selected lane", async () => {
    const fetcher: typeof fetch = vi.fn(async () => jsonResponse({ items: [] }));
    const client = createApiClient("local-secret", fetcher);

    await client.getRequests("prod/east & blue");

    const [input, init] = vi.mocked(fetcher).mock.calls[0]!;
    expect(String(input)).toBe("/v1/requests?lane_id=prod%2Feast+%26+blue");
    expect(new Headers(init?.headers).get("Authorization")).toBe("Bearer local-secret");
    expect(String(input)).not.toContain("local-secret");
  });

  it("preserves structured API errors without leaking the token", async () => {
    const fetcher: typeof fetch = vi.fn(async () => jsonResponse({
      code: "invalid_token",
      message: "token rejected",
      retryable: false,
    }, 401));
    const client = createApiClient("private-value", fetcher);

    let failure: unknown;
    try {
      await client.getSystem();
    } catch (error) {
      failure = error;
    }
    expect(failure).toBeInstanceOf(ApiError);
    expect(failure).toMatchObject({
      name: "ApiError",
      status: 401,
      code: "invalid_token",
      message: "token rejected",
      retryable: false,
    });
    expect(String(vi.mocked(fetcher).mock.calls[0]?.[0])).not.toContain("private-value");
  });

  it("forwards abort signals and uses the bounded ascending log cursor", async () => {
    const fetcher: typeof fetch = vi.fn(async () => jsonResponse({ items: [], next_cursor: 42 }));
    const client = createApiClient("local-secret", fetcher);
    const controller = new AbortController();

    await client.getLogs("run/east-2", 42, controller.signal);

    const [input, init] = vi.mocked(fetcher).mock.calls[0]!;
    expect(String(input)).toBe("/v1/runs/run%2Feast-2/logs?after=42&limit=100");
    expect(init?.signal).toBe(controller.signal);
  });

  it("rejects list and log responses that do not match their API envelopes", async () => {
    const client = createApiClient("local-secret", vi.fn(async () => jsonResponse({ records: [] })));
    await expect(client.getLanes()).rejects.toMatchObject({
      name: "ApiError",
      status: 200,
      code: "invalid_response",
    });

    const logClient = createApiClient("local-secret", vi.fn(async () => jsonResponse({ items: [], next_cursor: "4" })));
    await expect(logClient.getLogs("run-1", 0)).rejects.toMatchObject({
      name: "ApiError",
      status: 200,
      code: "invalid_response",
    });
  });
});

describe("log feed target changes", () => {
  it("drops old run entries and resets its cursor when a request moves to a successor", () => {
    const oldFeed = { cursor: 240, items: [{ sequence: 240, message: "old executor" }] };
    const nextFeed = transitionLogTarget("run-old", "run-next", oldFeed);

    expect(nextFeed).toEqual({ cursor: 0, items: [] });
    expect(transitionLogTarget("run-next", "run-next", oldFeed)).toBe(oldFeed);
  });

  it("keeps a satisfied request linked to the run that produced its receipt", () => {
    expect(requestRunId({ current_run_id: null, satisfied_by_run: "run-completed" })).toBe("run-completed");
    expect(requestRunId({ current_run_id: "run-current", satisfied_by_run: "run-old" })).toBe("run-current");
    expect(requestRunId({ current_run_id: null, satisfied_by_run: null })).toBeNull();
  });
});

it("keeps the original run association after a request follows its successor", () => {
  const request = {id: "request-a", current_run_id: null, satisfied_by_run: "successor"};
  const history = [{request_id: "request-a", run_id: "original"}];
  expect(requestRelatedToRun(request, "original", history)).toBe(true);
  expect(requestRelatedToRun(request, "successor", history)).toBe(true);
  expect(requestRelatedToRun(request, "unrelated", history)).toBe(false);
});
