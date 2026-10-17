# Synthetic local CLI example

`local.toml` is an isolated, synthetic fixture for the foreground `run` command.
It requires a binary containing the 0.3.2-dev local CLI feature or a later
release; the published v0.3.1 binary does not support it.
It uses the built-in scripted executor, one `demo` project lane, and no API
tokens, HTTP listener, or Dashboard. Its simulated receipt proves only what the
fixture modeled; this is not a deployment or verification setup for a real
application.

Run it from a temporary directory outside any project Git checkout. The config
resolves `data/shipbraid.db` and `data/source.git` relative to its own file, so
the ledger and mirror stay with the temporary fixture. Do not place fixture
databases, task data, or logs in a project repository or commit them.

```bash
DEMO_DIR=$(mktemp -d)
mkdir -p "$DEMO_DIR/data"
cp "$SHIPBRAID_BUNDLE/examples/standalone/local.toml" "$DEMO_DIR/local.toml"
git init --quiet --initial-branch=main "$DEMO_DIR/source"
git -C "$DEMO_DIR/source" -c user.name=Demo -c user.email=demo@example.invalid \
  commit --quiet --allow-empty -m "Synthetic demo source"
git clone --quiet --mirror "$DEMO_DIR/source" "$DEMO_DIR/data/source.git"
REVISION=$(git --git-dir="$DEMO_DIR/data/source.git" rev-parse refs/heads/main)
shipbraid --json run --config "$DEMO_DIR/local.toml" \
  --revision "$REVISION" --components app --timeout-seconds 30
```

The ledger is created under the temporary directory with private permissions.
The command runs in the foreground, does not start a server, and needs no
client token. Keep the returned request id if you want to inspect or resume it
later. See the [local CLI guide](../../docs/local-cli.md) for real project
configuration, recovery, timeouts, and exit codes.
