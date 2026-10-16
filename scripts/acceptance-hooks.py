#!/usr/bin/env python3
"""Isolated subprocess acceptance for configured command, script, and webhook hooks.

The fixture uses a temporary bare Git mirror, SQLite ledger, random credentials,
local child processes, and a loopback webhook receiver. Every receipt and
external effect is synthetic; this script never invokes AutoTask or user units.

Run after building the coordinator:
  python3 scripts/acceptance-hooks.py [--binary target/debug/shipbraid]
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


def write_json(path: Path, value: object) -> None:
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, sort_keys=True))
    os.replace(temporary, path)


def read_json(path: Path, fallback: object) -> object:
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return fallback


def key_for(envelope: dict) -> str:
    run = envelope["run"]
    return f"{run['id']}-{run['generation']}"


def fixture_reply(envelope: dict, state_dir: Path, transport: str) -> tuple[dict, str | None]:
    """Return a protocol Reply and optional injected transport failure."""
    run = envelope["run"]
    lane = envelope["lane"]
    operation = envelope["operation"]
    run_id = str(run["id"])
    generation = int(run["generation"])
    target = run["target"]
    revision = target["revision"]
    state_path = state_dir / f"{key_for(envelope)}.json"
    control_path = state_dir / "control.json"
    control = read_json(control_path, {})
    if not isinstance(control, dict):
        control = {}
    failures = control.get("failure_by_revision", {})
    failure = failures.get(revision) if isinstance(failures, dict) else None
    record = read_json(state_path, {})
    if not isinstance(record, dict):
        record = {}

    if operation == "dispatch":
        if failure in ("bad_stdout", "nonzero", "timeout"):
            reply = make_reply(envelope, None, "unknown", f"synthetic {failure} fixture", [])
            return reply, failure
        if not record:
            record = {
                "executor_ref": f"synthetic:{lane['id']}:{run_id}:{generation}",
                "dispatch_count": 1,
                "observe_count": 0,
                "verify_count": 0,
                "log_sequence": 0,
                "status": "running",
            }
            write_json(state_path, record)
        log = append_log(record, state_path, f"synthetic fixture {transport} dispatch accepted")
        status = "started" if transport != "webhook" else "unknown"
        reply = make_reply(envelope, record["executor_ref"], status, "synthetic hook dispatch", [log])
        # Webhook dispatch intentionally returns HTTP 202 with an empty body.
        # The recorded identity becomes usable only through a later observe.
        return reply, "empty_202" if transport == "webhook" else None

    if operation == "observe":
        if not record:
            reply = make_reply(envelope, None, "unknown", "synthetic fixture has no dispatched run", [])
            return reply, None
        record["observe_count"] = int(record.get("observe_count", 0)) + 1
        write_json(state_path, record)
        gate = control.get("hold_observe_for_revision") == revision
        if gate:
            status = "unknown"
            message = "synthetic observation gate is closed"
        else:
            status = "completed"
            message = "synthetic independent observation proves completion"
        log = append_log(record, state_path, f"synthetic fixture {transport} observe: {status}")
        return make_reply(envelope, record["executor_ref"], status, message, [log]), None

    if operation == "verify":
        if not record:
            reply = make_reply(envelope, None, "unknown", "synthetic fixture cannot verify an undispatched run", [])
            return reply, None
        record["verify_count"] = int(record.get("verify_count", 0)) + 1
        write_json(state_path, record)
        gate = control.get("hold_verify_for_revision") == revision
        if gate and not control.get("release_verify", False):
            # The outer acceptance test kills the coordinator while this
            # read-only verification is outstanding, then releases a fresh
            # verification after restart.
            marker = state_dir / "verify-gate-entered"
            marker.touch()
            deadline = time.monotonic() + 45
            while time.monotonic() < deadline:
                current = read_json(control_path, {})
                if isinstance(current, dict) and current.get("release_verify"):
                    break
                time.sleep(0.05)
        receipt = make_receipt(run, lane)
        log = append_log(record, state_path, f"synthetic fixture {transport} independent verification")
        reply = make_reply(envelope, record["executor_ref"], "verified", "synthetic verification evidence", [log])
        reply["receipt"] = receipt
        return reply, None

    return make_reply(envelope, None, "unknown", "unsupported fixture operation", []), None


def append_log(record: dict, state_path: Path, message: str) -> dict:
    record["log_sequence"] = int(record.get("log_sequence", 0)) + 1
    write_json(state_path, record)
    return {"sequence": record["log_sequence"], "level": "info", "message": message}


def make_reply(envelope: dict, executor_ref: str | None, status: str, message: str, logs: list) -> dict:
    run = envelope["run"]
    return {
        "version": 1,
        "run_id": str(run["id"]),
        "generation": int(run["generation"]),
        "executor_ref": executor_ref,
        "status": status,
        "message": message,
        "logs": logs,
    }


def make_receipt(run: dict, lane: dict) -> dict:
    target = run["target"]
    run_id = str(run["id"])
    generation = int(run["generation"])
    namespace = uuid.UUID("f52c5bd9-774c-42b5-966a-d51c7d4cfac1")
    receipt_id = str(uuid.uuid5(namespace, f"{run_id}:{generation}:synthetic-hook-receipt"))
    component_digests = {
        component: hashlib.sha256(
            f"synthetic-hook-fixture:{run_id}:{generation}:{target['revision']}:{component}".encode()
        ).hexdigest()
        for component in target["components"]
    }
    return {
        "id": receipt_id,
        "run_id": run_id,
        "generation": generation,
        "observed_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        "source_revision": target["revision"],
        "component_digests": component_digests,
        "profile_digest": target["profile_digest"],
        "verification_report": (
            "SYNTHETIC hook acceptance receipt; the fixture did not deploy or modify an environment. "
            f"Project={lane['project']} environment={lane['environment']} run={run_id} generation={generation}."
        ),
        "result": "success",
    }


def hook_worker(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--transport", required=True, choices=("command", "script"))
    parser.add_argument("--state-dir", required=True, type=Path)
    args = parser.parse_args(argv)
    envelope = json.load(sys.stdin)
    reply, failure = fixture_reply(envelope, args.state_dir, args.transport)
    if failure == "bad_stdout":
        print("this is deliberately not JSON")
        return 0
    if failure == "nonzero":
        print(json.dumps(reply))
        return 23
    if failure == "timeout":
        time.sleep(5)
        print(json.dumps(reply))
        return 0
    print(json.dumps(reply, separators=(",", ":")))
    return 0


class WebhookFixture(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, handler, state_dirs, token):
        super().__init__(address, handler)
        self.state_dirs = state_dirs
        self.token = token


class WebhookHandler(BaseHTTPRequestHandler):
    server: WebhookFixture

    def log_message(self, _format, *_args):
        pass

    def do_POST(self):
        try:
            authorization = self.headers.get("Authorization", "")
            if not secrets.compare_digest(authorization, f"Bearer {self.server.token}"):
                self.send_error(401)
                return
            length = int(self.headers.get("Content-Length", "0"))
            if length <= 0 or length > 64 * 1024:
                self.send_error(413)
                return
            envelope = json.loads(self.rfile.read(length))
            lane_id = envelope["lane"]["id"]
            state_dir = self.server.state_dirs[lane_id]
            run = envelope["run"]
            control = read_json(state_dir / "control.json", {})
            if not isinstance(control, dict):
                control = {}
            if (
                envelope["operation"] == "dispatch"
                and control.get("failure_by_revision", {}).get(run["target"]["revision"]) == "timeout"
            ):
                time.sleep(2)
                self.send_response(200)
                self.end_headers()
                return

            reply, mode = fixture_reply(envelope, state_dir, "webhook")
            if mode == "empty_202":
                self.send_response(202)
                self.end_headers()
                return
            raw = json.dumps(reply, separators=(",", ":")).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:  # fixture errors should be visible to the acceptance process
            raw = json.dumps({"fixture_error": str(error)}).encode()
            try:
                self.send_response(500)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)
            except (BrokenPipeError, ConnectionResetError):
                pass


def quote(value: str) -> str:
    return json.dumps(value)


def lane_config(lane: dict, state_dir: Path, mirror: Path, hook_token_env: str, webhook_url: str) -> str:
    kind = lane["transport"]
    lines = [
        "[[lanes]]",
        f"id = {quote(lane['id'])}",
        f"project = {quote(lane['project'])}",
        f"environment = {quote(lane['environment'])}",
        'repository_id = "synthetic-fixture"',
        f"repository_path = {quote(str(mirror))}",
        'release_ref = "refs/heads/main"',
        f"profile_digest = {quote('synthetic-hook-profile-' + kind)}",
        'components = ["app"]',
        "allow_descendant = true",
        "allow_preemption = false",
        "enabled = true",
        "[lanes.executor]",
        'kind = "hooks"',
    ]
    for operation in ("dispatch", "observe", "verify"):
        lines.extend((f"[lanes.executor.{operation}]", f"kind = {quote(kind)}"))
        if kind == "command":
            lines.extend(
                (
                    f"program = {quote(sys.executable)}",
                    "args = ["
                    + ", ".join(
                        quote(value)
                        for value in (
                            str(Path(__file__).resolve()),
                            "hook-worker",
                            "--transport",
                            "command",
                            "--state-dir",
                            str(state_dir),
                        )
                    )
                    + "]",
                    f"cwd = {quote(str(state_dir))}",
                    "timeout_ms = 2500",
                )
            )
        elif kind == "script":
            lines.extend(
                (
                    f"path = {quote(str(Path(__file__).resolve()))}",
                    f"interpreter = {quote(sys.executable)}",
                    "args = ["
                    + ", ".join(
                        quote(value)
                        for value in (
                            "hook-worker",
                            "--transport",
                            "script",
                            "--state-dir",
                            str(state_dir),
                        )
                    )
                    + "]",
                    f"cwd = {quote(str(state_dir))}",
                    "timeout_ms = 2500",
                )
            )
        else:
            lines.extend(
                (
                    f"url = {quote(webhook_url)}",
                    f"token_env = {quote(hook_token_env)}",
                    "timeout_ms = 350",
                )
            )
    return "\n".join(lines) + "\n"


class Acceptance:
    def __init__(self, binary: str, base: Path, env: dict, url: str, tokens: dict, lanes: list[dict]):
        self.binary = binary
        self.base = base
        self.env = env
        self.url = url
        self.tokens = tokens
        self.lanes = lanes

    def request(self, path: str, body=None, key=None, token=None, expected=200):
        headers = {"Authorization": f"Bearer {token or self.tokens['admin']}"}
        if key:
            headers["Idempotency-Key"] = key
        data = None
        if body is not None:
            headers["Content-Type"] = "application/json"
            data = json.dumps(body).encode()
        request = urllib.request.Request(self.url + path, data=data, headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=8) as response:
                status, raw = response.status, response.read()
        except urllib.error.HTTPError as error:
            status, raw = error.code, error.read()
        assert status == expected, (path, status, expected, raw.decode(errors="replace"))
        return json.loads(raw) if raw else {}

    def wait_for(self, predicate, timeout=15):
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            try:
                last = predicate()
                if last:
                    return last
            except (OSError, AssertionError, KeyError):
                pass
            time.sleep(0.06)
        raise AssertionError(f"condition timed out; last={last}")

    def submit_http(self, lane: dict, revision: str, token: str, key: str):
        return self.request(
            "/v1/requests",
            {
                "project": lane["project"],
                "environment": lane["environment"],
                "revision": revision,
                "components": ["app"],
                "coverage_mode": "exact",
                "priority": "normal",
                "reason": "synthetic hook acceptance",
            },
            key=key,
            token=token,
            expected=202,
        )

    def get_request(self, request_id: str, token: str):
        return self.request(f"/v1/requests/{request_id}", token=token)

    def wait_for_run_state(self, request_id: str, token: str, state: str, timeout=15):
        def find_run():
            request_value = self.get_request(request_id, token)
            run_id = request_value.get("current_run_id")
            if not run_id:
                return None
            run = self.request(f"/v1/runs/{run_id}", token=token)
            return run if run.get("state") == state else None

        return self.wait_for(find_run, timeout=timeout)

    def run_request(self, request_value: dict, token: str):
        return self.wait_for(
            lambda: self._satisfied(request_value["id"], token),
            timeout=20,
        )

    def _satisfied(self, request_id: str, token: str):
        value = self.get_request(request_id, token)
        return value if value.get("status") == "satisfied" else None

    def cli(self, token: str, *arguments: str, timeout=20):
        result = subprocess.run(
            [self.binary, "--url", self.url, "--token", token, *arguments],
            env=self.env,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        assert result.returncode == 0, (arguments, result.returncode, result.stderr, result.stdout)
        return json.loads(result.stdout)

    def start_service(self, config: Path, log_handle):
        process = subprocess.Popen(
            [self.binary, "serve", "--config", str(config)],
            env=self.env,
            stdout=log_handle,
            stderr=log_handle,
        )
        self.wait_for(lambda: process.poll() is None and self._ready(), timeout=15)
        return process

    def _ready(self):
        try:
            with urllib.request.urlopen(self.url + "/ready", timeout=0.4) as response:
                return response.status == 200
        except OSError:
            return False


def make_git_mirror(base: Path) -> tuple[Path, list[str]]:
    source = base / "source"
    source.mkdir()
    subprocess.run(["git", "init", "-q", "-b", "main", str(source)], check=True)
    revisions = []
    for index in range(6):
        (source / "fixture.txt").write_text(f"synthetic revision {index}\n")
        subprocess.run(["git", "-C", str(source), "add", "fixture.txt"], check=True)
        subprocess.run(
            [
                "git",
                "-C",
                str(source),
                "-c",
                "user.name=Synthetic hook acceptance",
                "-c",
                "user.email=hook-acceptance@example.invalid",
                "commit",
                "-qm",
                f"synthetic fixture revision {index}",
            ],
            check=True,
        )
        revisions.append(
            subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
        )
    mirror = base / "mirror.git"
    subprocess.run(["git", "clone", "--quiet", "--mirror", str(source), str(mirror)], check=True)
    return mirror, revisions


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/shipbraid")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    assert Path(binary).is_file(), f"coordinator binary not found: {binary}"
    report = {"fixture": "synthetic external hooks", "real_deployments": 0, "checks": []}

    with tempfile.TemporaryDirectory(prefix="shipbraid-hooks-acceptance-") as temp:
        base = Path(temp)
        mirror, revisions = make_git_mirror(base)
        lane_specs = [
            ("synthetic-command", "command-dev", "command"),
            ("synthetic-script", "script-test", "script"),
            ("synthetic-webhook", "webhook-qa", "webhook"),
            ("failure-bad-stdout", "failure-command", "command"),
            ("failure-nonzero", "failure-script", "script"),
            ("failure-timeout", "failure-webhook", "webhook"),
        ]
        lanes = [
            {
                "id": lane_id,
                "project": "synthetic-hooks",
                "environment": environment,
                "transport": transport,
                "state_dir": base / "hooks" / lane_id,
            }
            for lane_id, environment, transport in lane_specs
        ]
        for lane in lanes:
            lane["state_dir"].mkdir(parents=True)
            write_json(lane["state_dir"] / "control.json", {})

        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            listen_port = sock.getsockname()[1]
        url = f"http://127.0.0.1:{listen_port}"
        hook_secret = secrets.token_urlsafe(48)
        hook_token_env = "SHIPBRAID_ACCEPTANCE_HOOK_BEARER"
        env_names = {
            "admin": "SHIPBRAID_ACCEPTANCE_ADMIN",
            **{lane["id"]: f"SHIPBRAID_ACCEPTANCE_{index}" for index, lane in enumerate(lanes)},
        }
        tokens = {name: secrets.token_urlsafe(48) for name in env_names}
        env = {
            **os.environ,
            hook_token_env: hook_secret,
            **{env_names[name]: token for name, token in tokens.items()},
        }

        webhook = WebhookFixture(
            ("127.0.0.1", 0),
            WebhookHandler,
            {lane["id"]: lane["state_dir"] for lane in lanes},
            hook_secret,
        )
        webhook_url = f"http://127.0.0.1:{webhook.server_address[1]}/hook"
        webhook_thread = threading.Thread(target=webhook.serve_forever, daemon=True)
        webhook_thread.start()

        config = base / "config.toml"
        config_lines = [
            f"database_path = {quote(str(base / 'ledger.db'))}",
            f"listen = {quote(f'127.0.0.1:{listen_port}')}",
            "max_active_runs = 6",
            "tick_ms = 50",
            "scripted_phase_ms = 50",
            "coalesce_ms = 20",
            "cancel_timeout_seconds = 5",
        ]
        config_lines.extend(
            (
                "[[tokens]]",
                'id = "synthetic-admin"',
                f"token_env = {quote(env_names['admin'])}",
                "admin = true",
                "urgent = true",
            )
        )
        for lane in lanes:
            config_lines.extend(
                (
                    "[[tokens]]",
                    f"id = {quote('scope-' + lane['id'])}",
                    f"token_env = {quote(env_names[lane['id']])}",
                    f"lanes = [{quote(lane['id'])}]",
                )
            )
        for lane in lanes:
            config_lines.append(lane_config(lane, lane["state_dir"], mirror, hook_token_env, webhook_url))
        config.write_text("\n".join(config_lines))

        server_log = open(base / "coordinator.log", "w+")
        acceptance = Acceptance(binary, base, env, url, tokens, lanes)
        process = None
        try:
            process = acceptance.start_service(config, server_log)

            command_lane, script_lane, webhook_lane = lanes[:3]
            command_token = tokens[command_lane["id"]]
            script_token = tokens[script_lane["id"]]
            webhook_token = tokens[webhook_lane["id"]]

            command_request = acceptance.submit_http(command_lane, revisions[0], command_token, "hooks-command-main")
            assert command_request["lane_id"] == command_lane["id"]
            command_done = acceptance.run_request(command_request, command_token)
            command_run_id = command_done["satisfied_by_run"]
            assert command_done["satisfied_by_receipt"] and command_done["receipt"]["result"] == "success"

            script_submit = acceptance.cli(
                script_token,
                "submit",
                "--project",
                script_lane["project"],
                "--environment",
                script_lane["environment"],
                "--revision",
                revisions[0],
                "--components",
                "app",
                "--idempotency-key",
                "hooks-script-main",
                "--reason",
                "synthetic hook acceptance",
            )
            script_request = script_submit
            assert script_request["lane_id"] == script_lane["id"]
            script_done = acceptance.cli(
                script_token,
                "wait",
                script_request["id"],
                "--timeout-seconds",
                "20",
                "--poll-ms",
                "60",
            )
            assert script_done["status"] == "satisfied", script_done
            script_run_id = script_done["satisfied_by_run"]
            assert script_done["receipt"]["result"] == "success"
            assert script_run_id != command_run_id, "same project/revision in another environment must use another run"

            acceptance.request(f"/v1/lanes/{script_lane['id']}", token=command_token, expected=403)
            acceptance.request(
                "/v1/requests",
                {
                    "project": script_lane["project"],
                    "environment": script_lane["environment"],
                    "revision": revisions[1],
                    "components": ["app"],
                    "coverage_mode": "exact",
                    "priority": "normal",
                    "reason": "cross-lane denial fixture",
                },
                key="cross-lane-denied",
                token=command_token,
                expected=403,
            )
            report["checks"].append("HTTP and CLI project/environment selectors resolve separate lanes; lane-scoped token gets 403 across environments")

            # HTTP 202 with an empty body records only fixture-side work. Keep
            # observation gated until the coordinator has persisted unknown;
            # only independent observe evidence may move this run forward.
            write_json(
                webhook_lane["state_dir"] / "control.json",
                {"hold_observe_for_revision": revisions[0]},
            )
            webhook_request = acceptance.submit_http(webhook_lane, revisions[0], webhook_token, "hooks-webhook-main")
            webhook_unknown = acceptance.wait_for_run_state(
                webhook_request["id"], webhook_token, "unknown", timeout=15
            )
            webhook_run_id = webhook_unknown["id"]
            unknown_run = acceptance.request(f"/v1/runs/{webhook_run_id}", token=webhook_token)
            assert unknown_run.get("executor_ref") is None
            assert acceptance.get_request(webhook_request["id"], webhook_token)["status"] != "satisfied"
            webhook_record = read_json(webhook_lane["state_dir"] / f"{webhook_run_id}-{unknown_run['generation']}.json", {})
            assert webhook_record.get("dispatch_count") == 1
            write_json(webhook_lane["state_dir"] / "control.json", {})
            webhook_done = acceptance.run_request(webhook_request, webhook_token)
            assert webhook_done["receipt"]["result"] == "success"
            for lane, done, run_id, token in (
                (command_lane, command_done, command_run_id, command_token),
                (script_lane, script_done, script_run_id, script_token),
                (webhook_lane, webhook_done, webhook_run_id, webhook_token),
            ):
                run = acceptance.request(f"/v1/runs/{run_id}", token=token)
                receipt = done["receipt"]
                assert receipt["run_id"] == run_id and receipt["generation"] == run["generation"]
                assert receipt["source_revision"] == revisions[0]
                assert receipt["profile_digest"] == run["profile_digest"]
                logs = acceptance.request(f"/v1/runs/{run_id}/logs?after=0&limit=200", token=token)
                assert logs["items"] and any("synthetic fixture" in item["message"] for item in logs["items"])
            report["checks"].append("empty HTTP 202 did not establish Started; later webhook observation and independent synthetic receipt completed the run")

            # Restart while a read-only verification is outstanding. A new
            # verification may run after recovery; the dispatch count must
            # remain one because dispatch is never replayed.
            gated_revision = revisions[4]
            write_json(
                webhook_lane["state_dir"] / "control.json",
                {"hold_verify_for_revision": gated_revision},
            )
            gated_request = acceptance.submit_http(webhook_lane, gated_revision, webhook_token, "hooks-webhook-restart-verify")
            marker = webhook_lane["state_dir"] / "verify-gate-entered"
            acceptance.wait_for(lambda: marker.exists(), timeout=15)
            gated_run_id = acceptance.get_request(gated_request["id"], webhook_token)["current_run_id"]
            assert gated_run_id
            gated_generation = acceptance.request(f"/v1/runs/{gated_run_id}", token=webhook_token)["generation"]
            process.kill()
            process.wait(timeout=5)
            process = None
            write_json(webhook_lane["state_dir"] / "control.json", {"release_verify": True})
            process = acceptance.start_service(config, server_log)
            recovered = acceptance.run_request(gated_request, webhook_token)
            assert recovered["satisfied_by_run"] == gated_run_id
            gated_record = read_json(webhook_lane["state_dir"] / f"{gated_run_id}-{gated_generation}.json", {})
            assert gated_record.get("dispatch_count") == 1, gated_record
            assert gated_record.get("verify_count", 0) >= 2, gated_record
            report["checks"].append("restart performed fresh read-only verification without a second dispatch")

            failure_specs = (
                (lanes[3], revisions[1], "bad_stdout"),
                (lanes[4], revisions[2], "nonzero"),
                (lanes[5], revisions[3], "timeout"),
            )
            for index, (lane, revision, failure) in enumerate(failure_specs):
                write_json(
                    lane["state_dir"] / "control.json",
                    {"failure_by_revision": {revision: failure}},
                )
                failed_request = acceptance.submit_http(
                    lane,
                    revision,
                    tokens[lane["id"]],
                    f"hooks-failure-{index}",
                )
                failed_run = acceptance.wait_for_run_state(
                    failed_request["id"], tokens[lane["id"]], "unknown", timeout=12
                )
                request_state = acceptance.get_request(failed_request["id"], tokens[lane["id"]])
                assert request_state["status"] != "satisfied", (failure, request_state)
                assert failed_run.get("executor_ref") is None, (failure, failed_run)
                assert failed_run.get("receipt") is None, (failure, failed_run)
            report["checks"].append("malformed stdout, nonzero command exit, and webhook timeout stay unknown without successful receipts")

            report["runs"] = {"command": command_run_id, "script": script_run_id, "webhook": webhook_run_id}
            report["synthetic_only"] = True
            print(json.dumps(report, indent=2))
        finally:
            if process is not None and process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            webhook.shutdown()
            webhook.server_close()
            webhook_thread.join(timeout=2)
            server_log.close()


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "hook-worker":
        raise SystemExit(hook_worker(sys.argv[2:]))
    main()
