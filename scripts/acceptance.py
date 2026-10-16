#!/usr/bin/env python3
"""Local, synthetic subprocess acceptance. No real workflows or deployments.

Run after cargo build: python3 scripts/acceptance.py [--binary target/debug/shipbraid]
All credentials, Git fixtures and SQLite databases live in a TemporaryDirectory.
"""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/shipbraid")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    report = {"executor": "scripted", "checks": [], "real_deployments": 0}
    with tempfile.TemporaryDirectory(prefix="shipbraid-acceptance-") as directory:
        base = Path(directory)
        repository = base / "repository"
        repository.mkdir()
        subprocess.run(["git", "init", "-q", "-b", "main", str(repository)], check=True)
        revisions = []
        for index in range(4):
            (repository / "revision").write_text(str(index))
            subprocess.run(["git", "-C", str(repository), "add", "revision"], check=True)
            subprocess.run(["git", "-C", str(repository), "-c", "user.name=Synthetic acceptance", "-c", "user.email=acceptance@example.invalid", "commit", "-qm", f"synthetic {index}"], check=True)
            revisions.append(subprocess.check_output(["git", "-C", str(repository), "rev-parse", "HEAD"], text=True).strip())
        mirror = base / "source.git"
        subprocess.run(["git", "clone", "--quiet", "--mirror", str(repository), str(mirror)], check=True)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        url = f"http://127.0.0.1:{port}"
        operator = secrets.token_urlsafe(40)
        scoped = secrets.token_urlsafe(40)
        executor = secrets.token_urlsafe(40)
        env = {**os.environ, "SHIPBRAID_ACCEPTANCE_OPERATOR": operator, "SHIPBRAID_ACCEPTANCE_SCOPED": scoped, "SHIPBRAID_ACCEPTANCE_EXECUTOR": executor}
        config = base / "config.toml"
        config.write_text(f'''database_path = "{base / 'ledger.db'}"
listen = "127.0.0.1:{port}"
max_active_runs = 2
tick_ms = 50
scripted_phase_ms = 1200
coalesce_ms = 1200
cancel_timeout_seconds = 60
[[tokens]]
id = "operator"
token_env = "SHIPBRAID_ACCEPTANCE_OPERATOR"
admin = true
urgent = true
[[tokens]]
id = "scoped"
token_env = "SHIPBRAID_ACCEPTANCE_SCOPED"
lanes = ["synthetic-test"]
[[tokens]]
id = "executor"
token_env = "SHIPBRAID_ACCEPTANCE_EXECUTOR"
lanes = ["synthetic-test"]
executor = true
[[lanes]]
id = "synthetic-test"
project = "acceptance"
environment = "test"
repository_id = "fixture"
repository_path = "{mirror}"
release_ref = "refs/heads/main"
profile_digest = "synthetic-v1"
components = ["app"]
allow_descendant = true
allow_preemption = true
[[lanes]]
id = "other-test"
project = "other"
environment = "test"
repository_id = "fixture"
repository_path = "{mirror}"
release_ref = "refs/heads/main"
profile_digest = "synthetic-v1"
components = ["app"]
''')
        log = open(base / "server.log", "w+")
        process = None

        def request(path, body=None, key=None, token=operator, expected=200):
            headers = {"Authorization": f"Bearer {token}"} if token else {}
            if key:
                headers["Idempotency-Key"] = key
            data = None
            if body is not None:
                headers["Content-Type"] = "application/json"
                data = json.dumps(body).encode()
            req = urllib.request.Request(url + path, data=data, headers=headers)
            try:
                with urllib.request.urlopen(req, timeout=20) as response:
                    status, raw = response.status, response.read()
            except urllib.error.HTTPError as error:
                status, raw = error.code, error.read()
            assert status == expected, (path, status, expected, raw.decode())
            return json.loads(raw)

        def wait_for(predicate, timeout=20):
            deadline = time.monotonic() + timeout
            last = None
            while time.monotonic() < deadline:
                last = predicate()
                if last:
                    return last
                time.sleep(0.08)
            raise AssertionError(f"condition timed out; last={last}")

        def start():
            nonlocal process
            process = subprocess.Popen([binary, "serve", "--config", str(config)], env=env, stdout=log, stderr=log)
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    log.seek(0)
                    raise AssertionError(f"service startup failed: {log.read()}")
                try:
                    request("/ready", token=None)
                    return
                except (OSError, AssertionError):
                    time.sleep(0.1)
            raise AssertionError("service did not become ready")

        def submit(revision, priority="normal", coverage="exact", key=None, token=operator):
            return request("/v1/requests", {"lane_id": "synthetic-test", "revision": revision, "components": ["app"], "coverage_mode": coverage, "priority": priority, "reason": "synthetic acceptance"}, key=key or secrets.token_hex(12), token=token, expected=202)

        def get_request(identifier):
            return request(f"/v1/requests/{identifier}")

        def satisfied(identifier):
            value = get_request(identifier)
            return value if value["status"] == "satisfied" else None

        try:
            start()
            request("/v1/lanes", token=None, expected=401)
            request("/v1/lanes/other-test", token=scoped, expected=403)
            request("/v1/requests", {"lane_id": "synthetic-test", "revision": revisions[0], "components": ["app"], "coverage_mode": "exact", "priority": "urgent", "reason": "unauthorized"}, key="denied", token=scoped, expected=403)
            report["checks"].append("unauthenticated reads, cross-lane reads and unauthorized urgent denied")
            second = subprocess.run([binary, "serve", "--config", str(config)], env=env, capture_output=True, text=True, timeout=8)
            assert second.returncode != 0 and ("lock" in second.stderr.lower() or "owns" in second.stderr.lower()), second.stderr
            report["checks"].append("second process cannot acquire database ownership")
            started = time.monotonic()
            latencies = []
            def concurrent_submit(index):
                before = time.monotonic()
                result = submit(revisions[0], key=f"parallel-{index}")
                latencies.append((time.monotonic() - before) * 1000)
                return result
            with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
                requests = list(pool.map(concurrent_submit, range(100)))
            report["concurrent_100_submit_p95_ms"] = round(sorted(latencies)[94], 3)
            ids = [item["id"] for item in requests]
            assert len(set(ids)) == 100
            first = satisfied(ids[0]) or wait_for(lambda: satisfied(ids[0]))
            for identifier in ids:
                result = wait_for(lambda identifier=identifier: satisfied(identifier))
                assert result["satisfied_by_receipt"] == first["satisfied_by_receipt"]
            runs = request("/v1/runs?lane_id=synthetic-test")["items"]
            assert len(runs) == 1, runs
            reused = submit(revisions[0], key="parallel-0")
            assert reused["id"] == ids[0]
            request("/v1/requests", {"lane_id": "synthetic-test", "revision": revisions[1], "components": ["app"], "coverage_mode": "exact", "priority": "normal", "reason": "synthetic acceptance"}, key="parallel-0", expected=409)
            report["checks"].append("100 concurrent callers share one verified run; idempotency replay/conflict correct")
            report["concurrent_100_seconds"] = round(time.monotonic() - started, 3)
            normal = submit(revisions[1], coverage="descendant")
            active = wait_for(lambda: get_request(normal["id"]).get("current_run_id"))
            wait_for(lambda: request(f"/v1/runs/{active}")["state"] == "running")
            urgent = submit(revisions[2], "urgent", "descendant")
            wait_for(lambda: request(f"/v1/runs/{active}")["state"] == "superseded")
            old_result = wait_for(lambda: satisfied(normal["id"]))
            new_result = wait_for(lambda: satisfied(urgent["id"]))
            assert old_result["satisfied_by_receipt"] == new_result["satisfied_by_receipt"]
            assert old_result["satisfied_by_run"] != active
            report["checks"].append("urgent descendant supersedes stoppable normal run; both requests settle from successor receipt")
            pending = submit(revisions[3], coverage="descendant")
            running = wait_for(lambda: get_request(pending["id"]).get("current_run_id"))
            wait_for(lambda: request(f"/v1/runs/{running}")["state"] == "running")
            covered = submit(revisions[3], priority="urgent")
            assert get_request(covered["id"])["current_run_id"] == running
            assert request(f"/v1/runs/{running}")["effective_priority"] == "urgent"
            run_before_unknown = request(f"/v1/runs/{running}")
            request("/v1/executor-events", {"run_id":running,"generation":run_before_unknown["generation"],"event":"unknown","executor_ref":run_before_unknown["executor_ref"],"reason":"synthetic observation uncertainty"}, token=executor)
            assert request("/v1/lanes/synthetic-test")["active_run_id"] == running
            request("/v1/lanes/synthetic-test/resolve-unknown", {"action":"confirm_stopped","evidence":"human assertion only","reason":"must not unlock"}, expected=409)
            assert request(f"/v1/runs/{running}")["state"] == "unknown"
            request("/v1/lanes/synthetic-test/resolve-unknown", {"action":"adopt_running","evidence":"re-observe durable ScriptedExecutor identity","reason":"synthetic recovery acceptance"})
            assert request(f"/v1/runs/{running}")["state"] == "running"
            report["checks"].append("unknown retains ownership; human stop assertion rejected; independent scripted observation restores running")
            timeout = subprocess.run([binary, "--url", url, "wait", pending["id"], "--timeout-seconds", "0"], env={**env, "SHIPBRAID_TOKEN":operator}, capture_output=True, text=True, timeout=10)
            assert timeout.returncode == 124, timeout.stderr
            assert get_request(pending["id"])["status"] == "waiting"
            report["checks"].append("covered urgent promotes current run; CLI local timeout does not withdraw request")
            # SIGKILL deliberately bypasses graceful drain. Recovery must observe persisted executor identity.
            process.kill()
            process.wait(timeout=5)
            start()
            resumed = wait_for(lambda: satisfied(pending["id"]))
            assert resumed["satisfied_by_run"] == running
            assert wait_for(lambda: satisfied(covered["id"]))["satisfied_by_receipt"] == resumed["satisfied_by_receipt"]
            report["checks"].append("SIGKILL/restart observes durable scripted execution without creating replacement run")
            logs = request(f"/v1/runs/{running}/logs?after=0&limit=200")
            assert logs["items"] and logs["next_cursor"] > 0
            events = request("/v1/events?lane_id=synthetic-test&after=0&limit=200")["items"]
            assert events and [item["sequence"] for item in events] == sorted(item["sequence"] for item in events)
            request("/v1/drain", {})
            assert request("/v1/system")["draining"] is True
            request("/v1/resume", {})
            assert request("/v1/system")["draining"] is False
            report["checks"].append("real run logs and monotonic audit cursors readable; explicit drain/resume works")
            # Use SQLite's online backup API, including committed WAL contents.
            request("/v1/drain", {})
            backup = base / "backup.db"
            with sqlite3.connect(base / "ledger.db") as source, sqlite3.connect(backup) as destination:
                source.backup(destination)
                assert destination.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
                report["backed_up_requests"] = destination.execute("SELECT COUNT(*) FROM requests").fetchone()[0]
            process.kill()
            process.wait(timeout=5)
            config.write_text(config.read_text().replace(str(base / "ledger.db"), str(backup)))
            start()
            assert request("/v1/system")["draining"] is True
            restored = get_request(pending["id"])
            assert restored["status"] == "satisfied" and restored["satisfied_by_run"] == running
            assert submit(revisions[0], key="parallel-0")["id"] == ids[0]
            report["checks"].append("online SQLite backup/restore retains receipts, idempotency and persistent drain")
            request("/v1/resume", {})
            failing = submit(revisions[1])
            failed_run = wait_for(lambda: get_request(failing["id"]).get("current_run_id"))
            wait_for(lambda: request(f"/v1/runs/{failed_run}")["state"] == "running")
            run_record = request(f"/v1/runs/{failed_run}")
            failure_event = {"run_id": failed_run, "generation": run_record["generation"], "event": "failed", "executor_ref": run_record["executor_ref"], "reason": "synthetic failure injection", "environment_known_unchanged": True}
            request("/v1/executor-events", failure_event, expected=403)
            request("/v1/executor-events", failure_event, token=executor)
            assert get_request(failing["id"])["status"] == "needs_attention"
            assert request("/v1/lanes/synthetic-test")["active_run_id"] is None
            waiting = submit(revisions[1])
            time.sleep(0.2)
            assert get_request(waiting["id"])["current_run_id"] is None
            assert request("/v1/lanes/synthetic-test")["blocked_reason"] is not None
            request(f"/v1/requests/{failing['id']}/retry", {})
            retried = wait_for(lambda: satisfied(failing["id"]))
            assert retried["manual_retries_used"] == 1 and retried["revision"] == revisions[1]
            assert wait_for(lambda: satisfied(waiting["id"]))["satisfied_by_receipt"] == retried["satisfied_by_receipt"]
            report["checks"].append("executor role enforced; confirmed failure pauses lane; new requests do not unpause; explicit retry preserves target")
            print(json.dumps(report, indent=2))
        except Exception:
            log.flush()
            log.seek(0)
            print(log.read())
            raise
        finally:
            if process and process.poll() is None:
                process.send_signal(signal.SIGTERM)
                try:
                    process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            log.close()


if __name__ == "__main__":
    main()
