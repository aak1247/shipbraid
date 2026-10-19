# Shipbraid HTTP / CLI contract

All `/v1` endpoints require a configured bearer token. The server fixes `tenant_id` to `local`; clients cannot select a tenant. Tokens identify the actor; a body cannot choose another principal. Read responses are scoped by the token's project/lane grants. `admin`, `urgent`, and `executor` are separate capabilities. `/live` and `/ready` disclose health only.

Lane/run/event lists return `{ "items": [...] }`. Request lists return `{ "items": [...], "next_cursor": null | "opaque-cursor" }`, default 100 and maximum 200 entries per page; callers must follow cursors. Details return entity fields at the top level. Dates are UTC RFC3339 strings; request/run IDs are UUIDs, lane IDs are configured slugs. Errors have `{ "code", "message", "retryable" }`. JSON body limit is 16 KiB, reason limit 2 KiB. Unknown fields in commands are rejected.

| Method | Route | Purpose |
| --- | --- | --- |
| GET | `/v1/system` | Authenticated principal, executor mix, version and persistent drain flag |
| GET | `/v1/lanes` | Authorized lanes |
| GET | `/v1/lanes/{id}` | Lane ownership/generation/block reason |
| GET | `/v1/requests?lane_id=...` | Requests, newest first |
| GET | `/v1/requests/{id}` | Request and satisfaction receipt details |
| GET | `/v1/requests/{id}/logs?cursor=...&limit=200` | Current run's logs with a request/run-bound cursor |
| POST | `/v1/requests` | Persist an immutable request before returning 202 |
| POST | `/v1/requests/{id}/withdraw` | Withdraw one request, preserving shared execution |
| POST | `/v1/requests/{id}/retry` | Explicit retry with original target/deadline |
| POST | `/v1/requests/{id}/priority` | Change priority with capability checks |
| GET | `/v1/runs?lane_id=...` | Actual executions, newest first |
| GET | `/v1/runs/{id}` | Frozen target, phase, generation and replacement |
| GET | `/v1/runs/{id}/logs?after=0&limit=200` | Bounded executor audit-derived log page |
| GET | `/v1/events?lane_id=...&after=0&limit=200` | Ascending per-lane audit sequence |
| POST | `/v1/drain` | Admin: stop new dispatch, continue observing active runs |
| POST | `/v1/resume` | Admin: explicitly resume dispatch |
| POST | `/v1/runs/{id}/phase-grants` | Executor: fenced stage transition |
| POST | `/v1/executor-events` | Executor: report a fenced execution event |
| POST | `/v1/lanes/{id}/unpause` | Admin: clear lane failure pause |
| POST | `/v1/lanes/{id}/resolve-unknown` | Admin: reconcile independently observed evidence |

Cursor sequences belong to a lane. Events require `lane_id`, including the first page; logs belong to a single run. When a request follows a successor, start that run's log cursor at zero.

## Log queries

This section documents the current source version `0.3.2-dev`. Published `0.3.1` supports run logs through `shipbraid logs RUN_ID [--after N]`; it does not include the request-log endpoint or the newer `logs run` / `logs request` CLI forms. Those changes require a later release. API version `1` alone does not indicate support for this additive endpoint.

`shipbraid logs RUN_ID [--after SEQUENCE]` remains supported for v0.3.1 compatibility. Current source also provides `shipbraid logs run RUN_ID [--after SEQUENCE]` for a fixed run. The run-log API returns `{run_id, items, next_cursor}`, where `next_cursor` is a numeric sequence for the next `after` query. Existing items/cursor fields remain compatible. Each item contains `sequence`, `timestamp`, `level`, and `message`; sequences need not be contiguous. Pages contain at most 200 items and include persisted ScriptedExecutor and hook-reported logs, not necessarily full process stdout/stderr.

`shipbraid logs request REQUEST_ID [--cursor CURSOR]` calls the request-log endpoint. It prefers `current_run_id`, then `satisfied_by_run`, then the last run associated with the request in lane audit events. This keeps logs readable after satisfaction, failure, withdrawal, or expiry while preferring any newly assigned run. Resolution and log reads use one snapshot, returning `{request_id, run_id, items, next_cursor}`. Omit `cursor` initially; pass the returned opaque string on subsequent queries. The cursor binds the request, run, and sequence: the same run resumes after the saved sequence, while a changed run starts at zero and returns only the selected run's logs. A cursor for another request is rejected with `400 invalid_cursor`; malformed query parameters return a structured 400. The endpoint accepts `cursor` and optional `limit`, not numeric `after`. Authentication and lane authorization are checked on each query; a cursor grants no access.

Without any associated run, request logs return `{request_id, run_id:null, items:[], next_cursor:null}`. Poll again without a cursor. With an associated run, empty pages retain the cursor. Historical runs remain accessible through run-log queries. Both new CLI modes return one page per call and have no `--follow`; the legacy `logs RUN_ID` form remains valid.

## Agent discovery, context, and collaboration

`GET /v1/system` adds `api_version: "1"`, `tenant_id: "local"`, authenticated `principal`, and `capabilities` (submit/withdraw/retry/raise_priority/admin/unpause). These are principal capabilities, not permission to mutate every request. Lane views include `tenant_id`, `repository_id`, allowed `components` and `coverage_modes`; filesystem paths and credentials are omitted.

Submission optionally accepts the top-level fields `task_id`, `session_id` (nonempty, at most 256 bytes, no control characters), and `collaborators` (at most 16 configured, lane-authorized, non-executor token ids). Only nonempty `task_id` and sorted/deduplicated collaborators participate in the request hash; absent, null, or empty optional values and session-only submissions preserve the old hash. Session is creation trace only and never changes on replay. Collaborators are immutable in this version. See [Agent guide](agents.md#提交并等待) for the matching CLI flags; the CLI has no generic `--context` option.

Each request view adds these flat context fields, `tenant_id`, `waiting_reason`, `blocked_reason`, and `allowed_actions` (`withdraw`, `retry`, `raise_priority`). Owner, explicit collaborator or admin may mutate a request, subject to lane scope and current state. Urgent submission/priority promotion and retry of an existing urgent request also require urgent/admin capability. Withdrawing urgent does not require that extra capability. Actual mutation rechecks within the serialized write transaction; action hints are snapshots, not authorization grants. New request-operation events expose their authenticated `actor`; historical/system events may have null actor.

`GET /v1/requests` supports `lane_id` or project/environment, plus `task_id`, `principal`, `status`, `limit` (1..200; default 100), and opaque `cursor`. Unknown statuses, invalid limits, and invalid cursors return structured 400 errors. Preserve filters between pages. Ordering is descending immutable `(created_at, lane_id, creation_sequence, id)`; keyset pagination avoids offset drift but is not a cross-page snapshot. Dynamic filters or clock changes may alter later results. Recovery should restart from the first page filtered by task. Tenant/lane scope is checked on every page.

A task reference does not deduplicate identities: different principals retain separate requests. A successor agent should query the original task and wait on its existing request. Only the original principal replaying the same intent/key deduplicates submission.

See [Agent guide](agents.md) for CLI machine errors, deterministic task keys, bounded wait retries and examples. HTTP API version remains v1 with additive entity fields; request-list default pagination is an intentional behavior change.

## Submit

Send an `Idempotency-Key` and body:

```json
{
  "lane_id": "demo-test",
  "revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "components": ["app"],
  "coverage_mode": "exact",
  "priority": "normal",
  "deadline_at": null,
  "reason": "manual local verification"
}
```

The revision must be a complete immutable commit, present in the configured local bare mirror and reachable from the configured release ref. No client-supplied URLs or shell commands are accepted. `descendant` additionally requires the lane's explicit opt-in and proven ancestry. Identical requests with different keys remain separately attributable; one active execution may satisfy both. Reusing a key with different content returns 409.

Withdrawal/retry/drain/resume take `{}`. Priority takes `{ "priority":"urgent", "reason":"incident" }`.

## Project request parameters

Submission may include an optional top-level `parameters` object mapping names to string values. The lane's `request_parameters` declaration in `GET /v1/lanes` defines allowed keys, required values, and any allowed-value list. Unknown keys, missing required values, empty values, and values outside the declared list are rejected before the request is accepted. At most 16 parameters are allowed; names are limited to 64 bytes and values to 1024 bytes. A parameter is required by default when the lane declares it, unless its schema sets `required: false`. Each `allowed_values` list accepts at most 32 unique nonempty values; an empty list allows any bounded nonempty value.

Parameters are frozen into the request and run target, appear as `parameters` in request/run views, and are passed to hooks under `run.target.parameters`. Nonempty parameters participate in the idempotency hash; changing them with the same key returns a conflict. They are part of the audit record, so do not send secrets. See [project parameter guidance](request-parameters.md) for configuration and CLI examples (`--param KEY=VALUE`).

```json
{
  "lane_id": "demo-test",
  "revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "components": ["app"],
  "parameters": {"change_window": "scheduled"}
}
```

## Executor and unknown resolution

The executor token can submit a phase grant with `{ "generation":1, "next_phase":"building", "executor_ref":"scripted:..." }`. The current run ID is part of the route. The coordinator's transaction decides whether cancellation or phase admission won; stale generation callbacks cannot release ownership.

`/v1/executor-events` takes run_id, generation, event (`started`, `stopped`, `failed`, `unknown`), executor_ref, reason and environment_known_unchanged. Public callbacks cannot submit a fabricated successful receipt: only the configured ScriptedExecutor verifier generates M1 receipts.

Unknown resolution takes `{ "action":"confirm_stopped", "evidence":"<observation reference>", "reason":"investigation" }` or `adopt_running`. Administrator text alone is insufficient: the configured executor must independently observe the matching condition. When it cannot, leave the lane blocked.

M1 receipts are synthetic and explicitly identify ScriptedExecutor. An HTTP 202, successful cancel call or CI exit code does not mean a request is satisfied.
