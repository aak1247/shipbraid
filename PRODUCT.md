# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack

React, TypeScript, and Tailwind CSS, as specified for the dashboard.

## Users

Developers, agents, and release owners who submit or oversee requests to publish a specific revision to a configured lane. They need to understand which execution owns a request, whether the request has been verified, and what happened during each execution.

## Product Purpose

Auto-deliver coordinates persistent deployment requests and their execution records. Success means each request remains independently traceable through coverage, queueing, replacement, and verified completion, including when work is retried or superseded.

## Positioning

Requests and runs are separate records. A request becomes satisfied only through verified evidence; replacing a run does not imply that its request failed or succeeded.

## Operating Context

M1 runs as a local, single-instance Rust service with SQLite and a ScriptedExecutor. The dashboard reads the service over its HTTP API. It is used by developers and release owners to inspect lanes, requests, runs, event history, and bounded executor logs.

The connected dashboard labels the executor as `模拟执行 · ScriptedExecutor`. Its ledger states `最近 200 条 · 筛选当前列表`, so filters apply to the records currently returned to the page. These labels describe the M1 view and do not claim a production deployment or a complete historical archive.

## Capabilities and Constraints

- The dashboard is read-only in M1 and displays records returned by the API.
- M1 uses only the explicitly identified ScriptedExecutor; it does not perform real builds or deployments.
- Synthetic ScriptedExecutor evidence is labeled as simulated in receipt headings and results; a simulated receipt is not production environment verification.
- Request links show current run ownership from `current_run_id`, or historical satisfaction through `satisfied_by_run` when no current run is linked. Run-to-request history is limited to returned request records and read audit events.
- Runs can be planned, dispatched, running, cancel-requested, superseded, canceled, unknown, succeeded, or failed. Unknown execution state retains ownership until evidence resolves it.
- A superseded run is an execution fact and does not by itself determine the outcome of requests attached to it.
- Logs are bounded, sanitized ScriptedExecutor output. A cursor represents the portion currently loaded.
- API access uses an explicitly entered bearer token held in memory for the current page session. The dashboard polls while visible.
- Production workflows, deployment evidence, and real execution are outside M1.

## Evidence on Hand

The live API is the only source of dashboard records. No production measurements, production deployment receipts, or customer evidence are supplied for the M1 interface; none should be fabricated. ScriptedExecutor data must be identified as simulated execution data.

## Product Principles

- Preserve the distinction between a caller's request and an executor run.
- Show ownership, replacement, and verification evidence with their actual source records.
- Treat unknown ownership as an active operational condition, not a terminal result.
- Keep the M1 surface read-only and truthful about its simulated executor.

## Accessibility & Inclusion

Use semantic controls, visible keyboard focus, readable contrast, responsive layouts, and respect reduced-motion preferences.
