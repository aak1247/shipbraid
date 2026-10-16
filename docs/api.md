# Shipbraid HTTP / CLI contract

All `/v1` endpoints require a configured bearer token. The server fixes `tenant_id` to `local`; clients cannot select a tenant. Tokens identify the actor; a body cannot choose another principal. Read responses are scoped by the token's project/lane grants. `admin`, `urgent`, and `executor` are separate capabilities. `/live` and `/ready` disclose health only.

Lane/run/event lists return `{ "items": [...] }`. Request lists return `{ "items": [...], "next_cursor": null | "opaque-cursor" }`, default 100 and maximum 200 entries per page; callers must follow cursors. Details return entity fields at the top level. Dates are UTC RFC3339 strings; request/run IDs are UUIDs, lane IDs are configured slugs. Errors have `{ "code", "message", "retryable" }`. JSON body limit is 16 KiB, reason limit 2 KiB. Unknown fields in commands are rejected.

| Method | Route | Purpose |
| --- | --- | --- |
| GET | `/v1/system` | Executor identity, version, persistent drain flag |
| GET | `/v1/lanes` | Authorized lanes |
| GET | `/v1/lanes/{id}` | Lane ownership/generation/block reason |
| GET | `/v1/requests?lane_id=...` | Requests, newest first |
| GET | `/v1/requests/{id}` | Request and satisfaction receipt details |
| POST | `/v1/requests` | Persist an immutable request before returning 202 |
| POST | `/v1/requests/{id}/withdraw` | Withdraw one request, preserving shared execution |
| POST | `/v1/requests/{id}/retry` | Explicit retry with original target/deadline |
| POST | `/v1/requests/{id}/priority` | Change priority with capability checks |
| GET | `/v1/runs?lane_id=...` | Actual executions, newest first |
| GET | `/v1/runs/{id}` | Frozen target, phase, generation and replacement |
| GET | `/v1/runs/{id}/logs?after=0&limit=200` | Bounded ScriptedExecutor audit-derived log page |
| GET | `/v1/events?lane_id=...&after=0&limit=200` | Ascending per-lane audit sequence |
| POST | `/v1/drain` | Admin: stop new dispatch, continue observing active runs |
| POST | `/v1/resume` | Admin: explicitly resume dispatch |
| POST | `/v1/runs/{id}/phase-grants` | Executor: fenced stage transition |
| POST | `/v1/executor-events` | Executor: report a fenced execution event |
| POST | `/v1/lanes/{id}/resolve-unknown` | Admin: reconcile independently observed evidence |

Cursor sequences belong to a lane. Events require `lane_id`, including the first page; logs belong to a single run. When a request follows a successor, start that run's log cursor at zero.

## Agent discovery, context, and collaboration

`GET /v1/system` adds `api_version: "1"`, `tenant_id: "local"`, authenticated `principal`, and `capabilities` (submit/withdraw/retry/raise_priority/admin). These are principal capabilities, not permission to mutate every request. Lane views include `tenant_id`, `repository_id`, allowed `components` and `coverage_modes`; filesystem paths and credentials are omitted.

Submission optionally accepts `task_id`, `session_id` (nonempty, at most 256 bytes, no control characters), and `collaborators` (at most 16 configured, lane-authorized, non-executor token ids). Task and sorted/deduplicated collaborators participate in the request hash only when nonempty; absent/null/empty context and session-only submissions preserve the old hash. Session is creation trace only and never changes on replay. Collaborators are immutable in this version.

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

## Executor and unknown resolution

The executor token can submit a phase grant with `{ "generation":1, "next_phase":"building", "executor_ref":"scripted:..." }`. The current run ID is part of the route. The coordinator's transaction decides whether cancellation or phase admission won; stale generation callbacks cannot release ownership.

`/v1/executor-events` takes run_id, generation, event (`started`, `stopped`, `failed`, `unknown`), executor_ref, reason and environment_known_unchanged. Public callbacks cannot submit a fabricated successful receipt: only the configured ScriptedExecutor verifier generates M1 receipts.

Unknown resolution takes `{ "action":"confirm_stopped", "evidence":"<observation reference>", "reason":"investigation" }` or `adopt_running`. Administrator text alone is insufficient: the configured executor must independently observe the matching condition. When it cannot, leave the lane blocked.

M1 receipts are synthetic and explicitly identify ScriptedExecutor. An HTTP 202, successful cancel call or CI exit code does not mean a request is satisfied.
