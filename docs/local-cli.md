# Single-project foreground CLI

`run` executes one project's coordinator in the foreground, using the local
SQLite ledger. It does not start an HTTP server, require an API/client token,
launch a coordinator service, or provide a Dashboard. Use `serve` with a
server-mode configuration when several projects need a shared coordinator or
people need the Dashboard.

This workflow is in source version **0.3.2-dev** and has not shipped in a
GitHub Release. The published v0.3.1 binary supports the legacy server/client
workflow, but has no `run` command, no `mode` configuration, and no installer
`--mode` option. Use a binary from a later release containing this feature;
the examples below describe that source behavior and are not v0.3.1 commands.

## Install a local project

The installer defaults to local mode. Run the binary and installer from the
verified bundle; no Cargo command is needed by an operator. The installer
creates a private prefix (default `~/.local/share/shipbraid`, mode `0700`),
copies the reviewed project scripts and local adapter into a versioned release,
and writes `etc/config.toml`, `etc/deploy.env`, and the adapter configuration.
It does not start the CLI, accept a release request, or create API tokens,
`tokens.env`, a Dashboard configuration, or a `shipbraid.service` unit.

```bash
python3 "$SHIPBRAID_BUNDLE/install.py" \
  --prefix "$HOME/.local/share/shipbraid" \
  --project myapp --environment dev --components app \
  --repository /srv/git/myapp.git --release-ref refs/heads/main \
  --project-bundle /srv/myapp/shipbraid-scripts-v1
```

The installer initializes only `dev` and `test`, requires a trusted local bare
Git mirror with the configured release ref, and refuses an existing prefix.
Review the copied `deploy.sh` and `verify.py` and put deployment-only values in
the generated `etc/deploy.env` (mode `0600`). Keep the prefix, ledger, scripts,
and credentials private and outside the project Git checkout. This is a
same-user trust boundary, not isolation from scripts running as that user.
The `installation.json` summary includes `mode` and paths available in this
installation (binary, config, adapter config, deploy environment file, ledger,
run-state directory, and work directory). Local summaries omit server-only
`unit` and token-file fields.

The local TOML must say `mode = "local"`, describe lanes for only one project,
and omit API tokens and `dashboard_dir`. Several environments for that same
project are allowed. Older configs without `mode` continue to mean `server`;
they are not silently converted. `run` refuses a server config, and `serve`
refuses a local config. Local mode also rejects a ledger containing requests
from another project or a principal other than `local-cli`; it will not take
over a shared server history.

The local CLI uses the same private ledger as any other process using the same
config. An exclusive lock prevents two coordinators from using that ledger at
once. Do not run parallel `run` processes against one ledger. Separate
databases do not coordinate ownership of the same real deployment target, so
operators must ensure another coordinator does not manage that target. If
multiple Agents/CI jobs need concurrent submissions, shared queuing, or a
background observer, use a server even if the server handles only this one
project.

## Submit a request

Use a full commit SHA that already exists in the configured mirror. If the
config has one enabled lane, `--lane` can be omitted; with multiple enabled
lanes, choose one explicitly. Multiple lanes are useful for dev and test
environments of the same project.

```bash
shipbraid run --config "$HOME/.local/share/shipbraid/etc/config.toml" \
  --revision <full-commit-sha> --components app \
  --coverage exact --priority normal \
  --task-id issue-42 --session-id agent-session-7 \
  --timeout-seconds 300
```

`--lane` is optional only when exactly one enabled lane exists. `--components`
is comma-separated; it must contain 1–64 unique component names allowed by the
lane. Coverage defaults to `exact`; `descendant` requires lane opt-in and does
not prove that code has not been reverted. Priority defaults to `normal`.
`--deadline` accepts RFC3339 with a timezone, and `--reason` records up to 2
KiB of request reason. Repeat `--param KEY=VALUE` for up to 16 frozen,
non-secret project parameters; keys must be unique and the lane must declare
the permitted keys and values. Task/session ids are each limited to 256 bytes;
an idempotency key is limited to 128 bytes. Never put a token, password, or
other secret in a parameter, reason, task id, or session id. There is no generic
`--context` flag: use `--task-id` and `--session-id` for those references.

Add components or parameters only after declaring them in that lane's config.
For example, `--components app,worker` requires both components to be allowed;
`--param change_window=scheduled` requires a matching
`request_parameters.change_window` specification. The fresh installer config
allows only `app` and declares no request parameters, so the basic command
above works unchanged with that generated config.

You may also supply `--idempotency-key`. Without one, a task id gives the same
normalized intent a stable key; without either a task id or explicit key, each
new submission gets a random key. Session id does not affect idempotency. The
CLI prints the request id and key to stderr as soon as the request is accepted,
then writes the final JSON report to stdout (success output is always JSON;
`--json` also makes errors structured). Repeating the same key and frozen
intent reuses the request; reusing it for different intent is an error. Keep
the request id and use `--resume` if the local wait ends before a terminal
result. The report includes `schema_version`, `mode`, `request_id`,
`idempotency_key`, `reused`, the request and its current run/receipt,
`local_wait_timeout`, `interrupted`, and a `scope` note explaining that local
scheduling has stopped while external execution may continue.

The API-client `submit`, `wait`, `events`, and `logs` commands do not read the
local ledger; use `run --resume` for local observation. The local report is a
snapshot, not a paginated event or log stream. The local adapter keeps its
execution logs under its private task state as described in the
[adapter guide](../integration/local/README.md).

The local request is created under principal `local-cli`; there is no API
authentication boundary between users who can read the prefix and run the
binary. Keep the config, local database, adapter scripts, and `deploy.env`
readable only by the intended operator. The installer generates the config
with one project and owns the prefix with private permissions.

## Resume, timeout, and interruption

Resume the same request using its UUID:

```bash
shipbraid run --config "$HOME/.local/share/shipbraid/etc/config.toml" \
  --resume REQUEST_ID --timeout-seconds 300
```

`--retry` is valid only with `--resume` and explicitly retries a request whose
failure is already proven:

```bash
shipbraid run --config "$HOME/.local/share/shipbraid/etc/config.toml" \
  --resume REQUEST_ID --retry --timeout-seconds 300
```

It does not unlock an `unknown` execution or establish that an external worker
stopped. Without `--retry`, resume continues observing and reconciling the
existing request. `--timeout-seconds` defaults to `300`. Setting it to `0`
creates or restores the request and returns its current snapshot without a
scheduler tick. An ordinary pending request exits `124`; an already known
`unknown` execution or `needs_attention` request exits `2`. A terminal request
returns its terminal result code instead.

On timeout (`124`) or interrupt (`130`), the CLI stops local scheduling and
does not withdraw the request or claim that deployment stopped. An independent
systemd worker or other supervisor can continue. There is no background
observer after the CLI exits; resume the same request later to continue local
observation. In-flight hook handoff may take bounded time to settle, so the
process can exit after the requested wait interval.

| Exit code | Meaning |
| --- | --- |
| `0` | The request is satisfied by matching accepted evidence. |
| `1` | Configuration, lock, storage, or request submission error. |
| `2` | Usage error, `needs_attention`, or execution remains `unknown`; inspect the report. |
| `3` | The request expired. |
| `4` | The request was withdrawn. |
| `124` | Local wait timed out; scheduling stopped, request was not withdrawn. |
| `130` | Interrupted; local scheduling stopped, request was not withdrawn. |

Exit code `2` has two meanings, so use the JSON report to distinguish malformed
arguments from a request that needs attention. A dispatch acknowledgement,
script exit status, or old observation is not sufficient for success; only the
request's `satisfied` result with matching receipt is.

## Dependencies and synthetic example

The local CLI needs Git to verify the configured bare mirror and revision; it
does not need Cargo, an HTTP service, a client token, Python, or a Dashboard.
The bundled project adapter additionally uses Python 3.11+, a systemd 249+
user manager, and cgroup v2; project scripts need their own dependencies.
Local mode is not a zero-dependency deployment runtime.

Try the [synthetic standalone example](../examples/standalone/README.md) from a
temporary directory. Its `ScriptedExecutor` produces modeled synthetic
evidence and does not deploy an application. Do not commit its temporary
ledger, mirror, or task files to a project repository.

For shared coordination, cross-project lanes, or a browser Dashboard, install
with `--mode server` and start `shipbraid serve` using that server configuration.
The server installer mode and existing client commands are described in the
[distribution guide](distribution.md). The published v0.3.1 bundle predates
`--mode` and `run`; do not pass these new options to it.
