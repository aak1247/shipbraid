# Optional GitHub push receiver

`receiver.py` is a Python 3.11+ standard-library service that accepts signed GitHub `ping` and `push` webhook deliveries and turns an authorized branch push into a normal, exact Shipbraid request. It is optional and runs separately from the coordinator. The service listens only on a loopback address and does not fetch Git repositories, run payload commands, accept payload URLs, or grant permissions.

Start with a private copy of [the example configuration](../../examples/standalone/github.json). Replace the sample numeric `repository_id`, exact `refs/heads/...` value, lane, component names, and environment variable names with administrator-approved values. A route is an exact `(repository_id, ref)` pair; a repository name supplied by the payload is never used for authorization. Route `id` is a stable identity used to scope the idempotency key. Keep it unchanged for retries of the same configured route.

The top-level `secret_env` names the environment variable containing the GitHub webhook secret. The value must contain at least 32 UTF-8 bytes. Each route's `token_env` names a separate environment variable containing a Shipbraid token authorized to submit only to its lane; it must also contain at least 32 UTF-8 bytes. Store both values outside the repository, such as in a private environment file readable only by the service account. The JSON file contains names only. Do not pass either secret as a command-line argument or put values in logs.

Run the receiver with the environment populated by the service manager or a private environment file:

```sh
python3 "$SHIPBRAID_BUNDLE/adapters/github/receiver.py" --config /etc/shipbraid/github.json
```

The default example targets a Shipbraid API on `127.0.0.1:8787`; the receiver rejects non-loopback API URLs, URL credentials, query strings, fragments, and non-root URL paths. It disables ambient outbound proxy settings and refuses redirects. Its single-threaded HTTP server handles one delivery at a time, caps request bodies at 256 KiB, and applies the configured timeout (1–60 seconds) both as an idle-read timeout and as a total deadline for the complete request body. It reads the body in chunks of at most 64 KiB so a client cannot extend that deadline by periodically sending another byte. Coordinator calls use the same bounded timeout. The receiver does not log request lines, bodies, signatures, tokens, or upstream response bodies.

## Public GitHub endpoint

For GitHub's public webhook delivery, place a controlled HTTPS reverse proxy in front of the loopback listener. The proxy owns the public certificate and forwards `POST /github` to `127.0.0.1:8788/github`. Keep the receiver bound to loopback. Configure the proxy to preserve the exact raw request body and these headers: `Content-Type`, `Content-Length`, `X-GitHub-Event`, `X-GitHub-Delivery`, and `X-Hub-Signature-256`. Do not decode and re-encode JSON, rewrite the signature header, or accept another event path. Apply the same or a lower body-size limit, use a bounded upstream timeout, and disable request-body and credential logging. In GitHub, configure JSON payload delivery, set the same high-entropy secret referenced by `secret_env`, and subscribe only to `ping` and `push` events.

GitHub signs the raw body with HMAC-SHA256 and sends the hex digest in `X-Hub-Signature-256`. This receiver verifies that signature before parsing or routing the payload, using constant-time comparison. See GitHub's [validation guidance](https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries) and [webhook event headers](https://docs.github.com/en/webhooks/webhook-events-and-payloads).

## Delivery and retry behavior

A valid `ping` returns `200`; a deleted push for an authorized repository/ref returns `202` without submission. A non-deleted push must contain a positive numeric repository ID, an exact `refs/heads/...` branch, a boolean `deleted` field, a valid full 40- or 64-character commit ID in `after`, and a GitHub delivery UUID. The receiver submits the configured lane and components with `coverage_mode: exact`, `priority: normal`, and no deadline. The payload cannot choose a lane, components, priority, URL, command, or token.

The coordinator receives `Idempotency-Key: github-<sha256>`, derived from the stable route ID and normalized delivery UUID. The deterministic request reason includes the delivery UUID and SHA-256 digest of the signed raw body, but not the payload itself. A redelivery with the same body therefore reuses the same key and request hash. Reuse of a delivery ID with different signed content changes the request hash, so Shipbraid returns `409`; the receiver returns that conflict without exposing the upstream response. The receiver has no separate replay database: Shipbraid owns idempotency and conflict detection.

Coordinator transport failures/timeouts and 5xx/429 responses return a retryable non-2xx response. Other upstream rejection responses also return non-2xx without exposing their body. If the coordinator refuses a commit because the local mirror has not been synchronized, first sync the trusted mirror through its normal administrator-controlled process, then use GitHub's redelivery action. This receiver never performs `git fetch` or any other repository synchronization itself. A lost response is safe to redeliver with the same GitHub delivery because the request key and body are deterministic.

## Local signed-fixture tests

The tests start temporary loopback HTTP servers and send correctly signed fixture requests through the receiver. They do not contact GitHub or an external endpoint:

```sh
python3 -m unittest discover -s integration/github -p 'test_*.py'
```
