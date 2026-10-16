# M1 implementation contract

Implementation started 2026-10-16, authorized with React + TypeScript + Tailwind dashboard.

## Ownership
- Domain agent: src/domain.rs, src/engine.rs, src/lineage.rs and their tests.
- Backend agent: src/store.rs, src/api.rs, src/service.rs, src/executor.rs, migrations/, backend tests.
- Dashboard agent: web/ and frontend product/design records.
- Integrator: Cargo.toml, src/lib.rs, src/main.rs, config examples, integration checks, project documentation.

## Shared boundaries
Single Rust package `auto-deliver`, library `auto_deliver`. Axum 0.8, Tokio 1, serde 1, serde_json 1, chrono 0.4 (serde), uuid 1 (v4/serde), thiserror 2, sqlx 0.8 (runtime-tokio/sqlite/migrate), clap 4 (derive/env), tracing, tracing-subscriber, fs2, toml 0.8. Agents coordinate compiler-visible signatures before implementing callers; they can amend signatures by messaging peers. Do not edit files owned by another agent.

Domain exposes serializable `State` containing lanes, requests, runs, receipts, commands, events; an `Engine` operates on this state with explicit `now` timestamps. All transitions are synchronous and deterministic. Git proofs are computed before mutation and passed into domain transitions. UUID identifiers and RFC3339 UTC timestamps on HTTP. Backend serializes all mutation paths through a single async writer; SQLite transaction durably saves relational rows and events before exposing state or invoking external commands. Domain state may be cloned for transaction rollback; do not make a single JSON blob the sole persistent ledger.

For this first implementation domain + backend agents agree exact internal signatures directly. Required domain operations: submit/idempotent attach; reconcile/dispatch; urgent preemption; confirm stopped; phase grant; executor result/verified receipt; withdraw; retry; expire; unknown/recovery/drain. Preserve all design safety invariants. Tests must cover races/orderings, successor coverage, unknown ownership, idempotency conflicts and receipt settlement.

## Public HTTP contract (dashboard)
Bearer token on all `/v1` routes. `/live` and `/ready` unauthenticated (no secrets). JSON objects, errors `{ "code": "...", "message": "...", "retryable": false }`. Lists `{ "items": [...] }`. `GET /v1/lanes`, `/v1/requests?lane_id=...`, `/v1/runs?lane_id=...`, `/v1/events?lane_id=...&after=0&limit=200`; `GET /v1/lanes/{id}`, `/v1/requests/{id}`, `/v1/runs/{id}`; `GET /v1/runs/{id}/logs?after=0&limit=200` returns `{items:[{sequence:number,timestamp:string,level:string,message:string}],next_cursor:number}`. Logs bounded and sanitized; ScriptedExecutor log messages only. Lists newest first, events/log cursors ascending.

Lane: `{id,project,environment,profile_digest,active_run_id:string|null,generation:number,blocked_reason:string|null}`.
Request: `{id,lane_id,revision,components:string[],coverage_mode:"exact"|"descendant",priority:"normal"|"urgent",status:"accepted"|"waiting"|"satisfied"|"needs_attention"|"withdrawn"|"expired",current_run_id:string|null,created_at,deadline_at:string|null,reason:string,satisfied_by_receipt:string|null}`; additional fields allowed.
Run: `{id,lane_id,revision,components:string[],profile_digest,effective_priority:"normal"|"urgent",state:"planned"|"dispatching"|"running"|"cancel_requested"|"superseded"|"canceled"|"unknown"|"succeeded"|"failed",phase:"preparing"|"building"|"deploying"|"verifying",generation:number,replacement_run_id:string|null,created_at,updated_at}`; additional fields allowed.
Event: `{sequence:number,lane_id,run_id:string|null,request_id:string|null,timestamp,kind,message}`.
Detail endpoints may add `receipt`/`coverage_reason`; retain core entity top-level fields. `GET /v1/system` => `{executor:"scripted",draining:boolean,version:string}`.
Dashboard is read-only in M1; token kept in memory only, user explicitly connects. Poll visible page every 2 seconds; display errors, freshness, empty/loading states and lane filter, run/request selection, status timeline, logs. No invented data in normal mode. Development demo fixture must be explicitly labeled synthetic and opt-in.

POST /v1/requests with Idempotency-Key, body lane_id/revision/components/coverage_mode/priority/deadline_at/reason. HTTP 202 after persistence. Mutation auth derives principal from token, never trusts body. POST /v1/requests/{id}/withdraw, /retry, /priority. POST /v1/runs/{id}/phase-grants, /v1/executor-events privileged executor, /v1/lanes/{id}/resolve-unknown admin. POST /v1/drain admin. Backend agent publishes CLI-facing body examples early.

## Scope and validation
M1 is fully runnable local ScriptedExecutor coordinator + API + CLI + dashboard, no real workflow dispatch or deployment. Trusted local Git repository proves ancestry, exact mode still checks immutable allowed commits. Tokens configured from environment/file; no hardcoded production credentials. Default loopback, no CORS wildcard. Safe shutdown/drain, startup recovers unknown before dispatch. One OS lock per canonical SQLite path. Tests: cargo fmt/clippy/test, frontend typecheck/build and real browser desktop/mobile against backend. Document gaps with unchecked plan items, never mark unsupported safety features complete.
