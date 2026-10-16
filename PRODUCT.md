# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack

React, TypeScript, and Tailwind CSS, as specified for the dashboard.

## Users

Developers, agents, and release owners who submit or oversee requests to publish a specific revision to a configured lane. They need to understand which execution owns a request, whether the request has been verified, and what happened during each execution.

## Product Purpose

Shipbraid coordinates persistent deployment requests and their execution records. Success means each request remains independently traceable through coverage, queueing, replacement, and verified completion, including when work is retried or superseded.

## Positioning

Requests and runs are separate records. A request becomes satisfied only through verified evidence; replacing a run does not imply that its request failed or succeeded.

## Operating Context

Shipbraid runs as a local, single-instance Rust service with SQLite. Each project/environment lane selects its own ScriptedExecutor or configured Hooks executor, which can use a fixed command, script, or webhook transport. The dashboard reads the service over its HTTP API and is used to inspect lanes, requests, runs, event history, and bounded executor logs. Development and production are separate lanes; production remains explicitly disabled unless an administrator enables it.

## Capabilities and Constraints

- The dashboard is read-only and displays records returned by the API. Project and environment filters operate on the returned lane list.
- Executor identity is shown for the selected lane. A mixed service uses that lane's executor label instead of implying one executor handles every environment.
- ScriptedExecutor output and synthetic receipts are labeled as simulated; hook receipts show the evidence returned by the configured verifier.
- Disabled lanes remain visible with an explicit disabled state and cannot be selected to read execution records.
- Request links show current run ownership from `current_run_id`, or historical satisfaction through `satisfied_by_run` when no current run is linked. Run-to-request history is limited to returned request records and read audit events.
- Runs can be planned, dispatched, running, cancel-requested, superseded, canceled, unknown, succeeded, or failed. Unknown execution state retains ownership until evidence resolves it.
- A superseded run is an execution fact and does not by itself determine the outcome of requests attached to it.
- Logs are bounded, sanitized output from the selected lane's executor. A cursor represents the portion currently loaded.
- API access uses an explicitly entered bearer token held in memory for the current page session. The dashboard polls while visible.
- Production remains disabled for this integration and is not authorized by the presence of a configuration example.

## Evidence on Hand

The live API is the only source of dashboard records. No production deployment receipts or customer evidence are supplied; none should be fabricated. ScriptedExecutor data and synthetic receipts must be identified as simulated execution data.

## Product Principles

- Preserve the distinction between a caller's request and an executor run.
- Show ownership, replacement, and verification evidence with their actual source records.
- Treat unknown ownership as an active operational condition, not a terminal result.
- Keep the dashboard read-only and truthful about the selected lane's executor and evidence source.

## Accessibility & Inclusion

Use semantic controls, visible keyboard focus, readable contrast, responsive layouts, and respect reduced-motion preferences.
