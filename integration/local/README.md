# Generic local hooks adapter

`adapter.py` implements hooks v1 for a fixed, administrator-configured command
bundle on Linux with Python 3.11+, a systemd user manager, and cgroup v2. It is
independent of downstream applications. The adapter accepts a JSON envelope on stdin and emits
one JSON reply on stdout. The standalone installer creates its configuration.
The unpacked bundle contains the adapter at
`$SHIPBRAID_BUNDLE/adapters/local/adapter.py`; the installed hook points to its
immutable copy at `.../releases/<release>/adapters/local/adapter.py`. The
configuration interface is:

```json
{
  "version": 1,
  "lane_id": "demo-dev",
  "project": "demo",
  "environment": "dev",
  "repository_id": "demo",
  "components": ["app"],
  "state_dir": "/home/user/.local/share/shipbraid/var/runs",
  "cwd": "/home/user/.local/share/shipbraid/var/work",
  "deploy": ["/bin/bash", "/home/user/.local/share/shipbraid/releases/<release>/project/deploy.sh"],
  "verify": ["/usr/bin/python3", "/home/user/.local/share/shipbraid/releases/<release>/project/verify.py"],
  "environment_file": "/home/user/.local/share/shipbraid/etc/deploy.env",
  "verify_timeout_seconds": 10
}
```

`version` defaults to 1, `environment_file` may be null or empty, and
`verify_timeout_seconds` defaults to 10 and accepts 1–10 seconds. Paths are
absolute, commands are non-empty argv arrays, and production environments are
rejected. The dedicated environment file must be a user-owned regular file with
mode 0600 or stricter. It contains literal `KEY=value` lines (no shell
expansion); comments and blank lines are ignored.

Configure the hooks executor to use the immutable adapter and JSON config with
fixed arguments, for example:

```toml
[lanes.executor]
kind = "hooks"
[lanes.executor.dispatch]
kind = "command"
program = "/usr/bin/python3"
args = ["/immutable/release/adapters/local/adapter.py", "--config", "/private/shipbraid/adapter.json"]
cwd = "/"
timeout_ms = 20000
[lanes.executor.observe]
kind = "command"
program = "/usr/bin/python3"
args = ["/immutable/release/adapters/local/adapter.py", "--config", "/private/shipbraid/adapter.json"]
cwd = "/"
timeout_ms = 20000
[lanes.executor.verify]
kind = "command"
program = "/usr/bin/python3"
args = ["/immutable/release/adapters/local/adapter.py", "--config", "/private/shipbraid/adapter.json"]
cwd = "/"
timeout_ms = 20000
```

The fixed command receives the operation envelope on stdin; it does not accept
deployment commands from a request. Alternatively, run the adapter as an
HTTP hook endpoint on loopback:

```bash
SHIPBRAID_LOCAL_HOOK_TOKEN='read-from-a-private-secret-store' \
  /usr/bin/python3 "$SHIPBRAID_BUNDLE/adapters/local/adapter.py" \
  --config /private/shipbraid/adapter.json \
  --listen 127.0.0.1:9788 --token-env SHIPBRAID_LOCAL_HOOK_TOKEN
```

The endpoint is only available on `127.0.0.1`, requires an exact Bearer token,
and limits JSON bodies to 256 KiB. Do not expose it directly to the internet.

Before handing off, the adapter durably records the complete run target,
configuration digest, and deterministic unit identity. For a given run UUID
and generation it calls `systemd-run` at most once; after an uncertain response
it only observes and never retries deployment. The independent systemd worker
does not depend on the coordinator process. Its child receives a small base
environment plus values from the dedicated environment file and target
metadata; it does not inherit coordinator tokens or the systemd invocation ID.
The worker writes deploy output to a private log capped at 64 MiB. The adapter
never returns that log through the hook API.

An observation is completed only when the durable result matches the frozen
target, configuration and systemd `InvocationID`, the main process is stopped,
and stable recursive cgroup-v2 snapshots prove that the unit tree has no
processes. Missing or conflicting evidence remains `unknown`; a non-empty
sub-cgroup is never treated as stopped. A normally failed task produces a
bounded, fresh `failure_proof` only after the same stop checks.

Verification runs only after successful stop proof. The fixed read-only script
receives the same verify envelope and must return fresh `source_revision`,
`component_digests` for every requested component, `profile_digest`,
`observed_at`, `healthy: true`, and a non-empty `verification_report`. Exit code
zero alone is insufficient. Output is captured through a bounded pipe (1 MiB);
stderr is discarded. A per-run inherited `flock` prevents overlapping verifier
processes after an adapter/coordinator timeout. A timed-out verifier is left
alone and the reply remains `unknown` until it exits; timeout is not a stop
proof.

The companion [`examples/standalone/project`](../../examples/standalone/project)
is synthetic. It writes only under the private directory named by
`SHIPBRAID_DEMO_ROOT`, appends the run UUID to `deployment-starts` before any
optional delay, and verifies only files in that fake environment. It is for
package end-to-end checks, not a production deployer or project verifier.

Run the adapter's local unit tests with:

```bash
python3 -B -m unittest integration.local.test_adapter -v
```

The tests mock systemd and cgroup v2. An actual temporary user unit and package
end-to-end run are separate platform checks; unit-test success alone does not
claim a real deployment environment has been validated.
