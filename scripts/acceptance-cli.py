#!/usr/bin/env python3
"""Isolated CLI machine-contract fixtures; no coordinator or real deployments."""
import argparse
import http.server
import json
import os
from pathlib import Path
import secrets
import subprocess
import threading
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/shipbraid")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    secret = secrets.token_urlsafe(40)
    state = {"mode": "forbidden", "calls": 0}
    guard = threading.Lock()
    request_id = "00000000-0000-4000-8000-000000000001"

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def respond(self, status, payload, content_type="application/json", retry_after=None):
            body = json.dumps(payload).encode() if isinstance(payload, dict) else payload.encode()
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            if retry_after is not None:
                self.send_header("Retry-After", str(retry_after))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            with guard:
                state["calls"] += 1
                call = state["calls"]
                mode = state["mode"]
            if mode == "rate_limit":
                self.respond(429, {"code": "queue_limit", "message": "back off", "retryable": True}, retry_after=3)
            elif mode == "html_forbidden":
                self.respond(403, "private upstream body " + secret, "text/html", retry_after=2)
            elif mode == "unreadable":
                self.respond(200, "private response " + secret, "text/plain")
            elif mode == "transient" and call <= 2:
                self.respond(503, {"code": "not_ready", "message": "starting", "retryable": True}, retry_after=0)
            elif mode == "transient":
                self.respond(200, {"id": request_id, "status": "satisfied"})
            elif mode == "pending":
                self.respond(200, {"id": request_id, "status": "waiting"})
            else:
                self.respond(403, {"code": "forbidden", "message": "credential " + secret + " rejected", "retryable": False})

        def do_POST(self):
            with guard:
                state["calls"] += 1
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
            with guard:
                state["body"] = body
                current_mode = state["mode"]
            if current_mode == "parameters":
                self.respond(202, {"id": request_id, "status": "waiting", "parameters": body.get("parameters")})
                return
            # Ambiguous handoff: server consumed the request and lost its response.
            self.close_connection = True

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    env = {key: value for key, value in os.environ.items() if not key.startswith("SHIPBRAID_")}
    env.update(SHIPBRAID_TOKEN=secret, SHIPBRAID_URL=f"http://127.0.0.1:{server.server_port}")
    report = {"executor": "http-fixture", "real_deployments": 0, "checks": []}

    def mode(value):
        with guard:
            state.update(mode=value, calls=0)

    def run(*arguments, expected=1):
        result = subprocess.run([binary, "--json", *arguments], env=env, text=True, capture_output=True, timeout=12)
        assert result.returncode == expected, (arguments[0], result.returncode, expected)
        assert secret not in result.stdout + result.stderr, "credential escaped machine error contract"
        return json.loads(result.stdout)

    try:
        value = run("system")
        assert value["error"]["code"] == "forbidden" and value["error"]["http_status"] == 403
        assert not value["error"]["retryable"]
        report["checks"].append("HTTP errors preserve public code/status and redact credentials")

        mode("rate_limit")
        value = run("system")
        assert value["error"]["retry_after_seconds"] == 3 and value["error"]["retryable"]
        report["checks"].append("Retry-After and retryability are structured")

        mode("html_forbidden")
        value = run("wait", request_id, "--timeout-seconds", "5", "--poll-ms", "10")
        assert value["error"]["code"] == "invalid_response" and not value["error"]["retryable"]
        assert value["error"]["http_status"] == 403 and value["error"]["retry_after_seconds"] == 2
        assert state["calls"] == 1
        report["checks"].append("permanent non-JSON HTTP errors fail without wait retries or body leakage")

        mode("transient")
        value = run("wait", request_id, "--timeout-seconds", "5", "--poll-ms", "10", expected=0)
        assert value["status"] == "satisfied" and state["calls"] == 3
        report["checks"].append("wait resumes after transient read errors without submitting work")

        mode("unreadable")
        started = time.monotonic()
        value = run("wait", request_id, "--timeout-seconds", "1", "--poll-ms", "10", expected=124)
        assert value["local_wait_timeout"] and value["observation_available"] is False
        assert time.monotonic() - started < 3
        report["checks"].append("unobserved wait ends within local budget with explicit timeout")

        mode("pending")
        value = run("wait", request_id, "--timeout-seconds", "0", expected=124)
        assert value["status"] == "waiting" and value["local_wait_timeout"]
        report["checks"].append("zero-second wait preserves one observation and does not cancel")

        value = run("--token", secret, "--not-a-command", expected=2)
        assert value["error"]["code"] == "invalid_arguments"
        value = run("--url", f"https://user:{secret}@example.invalid", "system")
        assert value["error"]["code"] == "local_error"
        local_env = env.copy()
        local_env.pop("SHIPBRAID_TOKEN")
        result = subprocess.run([binary, "--json", "--token-file", "/nonexistent/private-credential-path", "system"], env=local_env, text=True, capture_output=True, timeout=5)
        assert result.returncode == 1 and "private-credential-path" not in result.stdout
        assert json.loads(result.stdout)["error"]["code"] == "local_error"
        report["checks"].append("parameter/local errors stay structured and omit private inputs")

        mode("parameters")
        value = run("submit", "--lane", "fixture", "--revision", "a" * 40, "--components", "app",
                    "--param", "database_backup=skip", "--param", "release_note=approved", expected=0)
        assert value["parameters"] == {"database_backup": "skip", "release_note": "approved"}
        assert state["body"]["parameters"] == value["parameters"]
        mode("parameters")
        value = run("submit", "--lane", "fixture", "--revision", "a" * 40, "--components", "app",
                    "--param", "database_backup=skip", "--param", "database_backup=include")
        assert value["error"]["code"] == "invalid_parameter" and state["calls"] == 0
        report["checks"].append("generic parameters cross HTTP unchanged; duplicate keys fail before submission")

        mode("ambiguous_post")
        value = run("submit", "--lane", "fixture", "--revision", "a" * 40, "--components", "app", "--idempotency-key", "recover-the-same-intent")
        assert value["error"]["code"] == "transport_error"
        assert value["error"]["idempotency_key"] == "recover-the-same-intent" and state["calls"] == 1
        report["checks"].append("ambiguous submit retains original key and never automatically reposts")

        for option in ("--help", "--version"):
            result = subprocess.run([binary, "--json", option], env=env, text=True, capture_output=True, timeout=5)
            assert result.returncode == 0 and result.stdout.strip()
        report["checks"].append("help/version retain standard successful text output")
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=5)
    report["status"] = "passed"
    if args.output:
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
