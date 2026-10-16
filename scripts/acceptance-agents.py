#!/usr/bin/env python3
"""Isolated multi-agent HTTP/CLI acceptance; all execution evidence is synthetic.

Run after cargo build. Git, SQLite, random credentials, and server logs live only
in a TemporaryDirectory. No existing service, hooks, or real deployment is used.
"""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import secrets
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def run_acceptance(binary, report):
    with tempfile.TemporaryDirectory(prefix="shipbraid-agents-") as directory:
        base = Path(directory)
        repository = base / "repository"
        subprocess.run(["git", "init", "-q", "-b", "main", str(repository)], check=True)
        revisions = []
        for index in range(2):
            (repository / "revision").write_text(str(index))
            subprocess.run(["git", "-C", str(repository), "add", "revision"], check=True)
            subprocess.run([
                "git", "-C", str(repository), "-c", "user.name=Synthetic acceptance",
                "-c", "user.email=acceptance@example.invalid", "commit", "-qm", f"synthetic {index}",
            ], check=True)
            revisions.append(subprocess.check_output([
                "git", "-C", str(repository), "rev-parse", "HEAD",
            ], text=True).strip())
        mirror = base / "source.git"
        subprocess.run(["git", "clone", "--quiet", "--mirror", str(repository), str(mirror)], check=True)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        url = f"http://127.0.0.1:{port}"
        principals = ["agent-a", "agent-b", "agent-c", "admin", "executor", "outsider"]
        tokens = {principal: secrets.token_urlsafe(40) for principal in principals}
        env_names = {principal: f"SHIPBRAID_TEST_{principal.upper().replace('-', '_')}" for principal in principals}
        env = dict(os.environ)
        env.pop("AUTO_DELIVER_TOKEN_FILE", None)
        env.update({env_names[principal]: token for principal, token in tokens.items()})
        database = base / "ledger.db"
        config = base / "config.toml"
        lines = [
            f"database_path = {json.dumps(str(database))}",
            f'listen = "127.0.0.1:{port}"',
            "max_active_runs = 2", "tick_ms = 40", "scripted_phase_ms = 1500",
            "coalesce_ms = 400", "cancel_timeout_seconds = 60",
        ]
        for principal in principals:
            lines.extend([
                "[[tokens]]", f"id = {json.dumps(principal)}",
                f"token_env = {json.dumps(env_names[principal])}",
                'lanes = ["other-test"]' if principal == "outsider" else 'lanes = ["team-test"]',
            ])
            if principal == "admin":
                lines.append("admin = true")
            if principal == "executor":
                lines.append("executor = true")
        for lane, project in [("team-test", "team"), ("other-test", "other")]:
            lines.extend([
                "[[lanes]]", f"id = {json.dumps(lane)}", f"project = {json.dumps(project)}",
                'environment = "test"', 'repository_id = "synthetic-repo"',
                f"repository_path = {json.dumps(str(mirror))}", 'release_ref = "refs/heads/main"',
                'profile_digest = "synthetic-agent-v1"', 'components = ["app"]',
                "allow_descendant = true", "allow_preemption = true", 'executor = { kind = "scripted" }',
            ])
        config.write_text("\n".join(lines) + "\n")
        process = None

        def http(path, body=None, principal="admin", expected=200, key=None):
            headers = {}
            if principal is not None:
                headers["Authorization"] = f"Bearer {tokens[principal]}"
            if key is not None:
                headers["Idempotency-Key"] = key
            data = None
            if body is not None:
                headers["Content-Type"] = "application/json"
                data = json.dumps(body).encode()
            request = urllib.request.Request(url + path, data=data, headers=headers)
            try:
                with urllib.request.urlopen(request, timeout=10) as response:
                    status, raw = response.status, response.read()
            except urllib.error.HTTPError as error:
                status, raw = error.code, error.read()
            assert status == expected, f"{path.split('?')[0]} returned {status}, expected {expected}"
            return json.loads(raw)

        def cli(principal, *arguments, timeout=20):
            result = subprocess.run(
                [binary, "--json", "--url", url, *arguments],
                env={**env, "AUTO_DELIVER_TOKEN": tokens[principal]},
                capture_output=True, text=True, timeout=timeout,
            )
            assert result.returncode == 0, f"CLI {arguments[0]} failed with exit {result.returncode}"
            return json.loads(result.stdout)

        def wait_for(predicate, timeout=15):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                result = predicate()
                if result:
                    return result
                time.sleep(0.05)
            raise AssertionError("synthetic coordinator condition timed out")

        def get(identifier, principal="admin"):
            return http(f"/v1/requests/{identifier}", principal=principal)

        def active_run(identifier):
            request = get(identifier)
            run_id = request["current_run_id"]
            if not run_id:
                return None
            run = http(f"/v1/runs/{run_id}")
            return run if run["state"] == "running" and run["executor_ref"] else None

        def submit(principal, session, revision=None, task="shared-task", collaborators=(), use_project=False, deadline=None):
            arguments = [
                "submit", "--revision", revision or revisions[0],
                "--components", "app", "--task-id", task, "--session-id", session,
                "--reason", "synthetic multi-agent acceptance",
            ]
            arguments.extend(["--project", "team", "--environment", "test"] if use_project else ["--lane", "team-test"])
            if deadline is not None:
                arguments.extend(["--deadline", deadline])
            for collaborator in collaborators:
                arguments.extend(["--collaborator", collaborator])
            return cli(principal, *arguments)

        with (base / "server.log").open("w+") as server_log:
            def start():
                nonlocal process
                process = subprocess.Popen([
                    binary, "serve", "--config", str(config),
                ], env=env, stdout=server_log, stderr=server_log)
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    assert process.poll() is None, "isolated coordinator exited during startup"
                    try:
                        http("/ready", principal=None)
                        return
                    except (OSError, AssertionError):
                        time.sleep(0.05)
                raise AssertionError("isolated coordinator did not become ready")

            try:
                start()
                system = cli("agent-a", "system")
                assert system["tenant_id"] == "local" and system["principal"] == "agent-a"
                assert system["api_version"] and system["capabilities"]["submit"]
                lanes = cli("agent-a", "list", "lane")["items"]
                assert len(lanes) == 1 and lanes[0]["id"] == "team-test"
                assert lanes[0]["components"] == ["app"] and lanes[0]["executor_kind"] == "scripted"
                http("/v1/lanes/team-test", principal="outsider", expected=403)
                report["checks"].append("CLI system and lane discovery expose local tenant/capabilities and enforce lane scope")

                with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
                    futures = [
                        pool.submit(submit, "agent-a", "session-a1", collaborators=("agent-b",), deadline="2099-01-01T00:00:00Z"),
                        pool.submit(submit, "agent-b", "session-b1"),
                        pool.submit(submit, "agent-c", "session-c1"),
                    ]
                    a, b, c = [future.result() for future in futures]
                assert len({a["id"], b["id"], c["id"]}) == 3
                replay = submit("agent-a", "session-a2", collaborators=("agent-b",), use_project=True, deadline="2099-01-01T08:00:00+08:00")
                assert replay["id"] == a["id"] and replay["session_id"] == "session-a1"
                assert replay["idempotency_key"] == a["idempotency_key"]
                run = wait_for(lambda: active_run(c["id"]))
                run_id = run["id"]
                for request in [a, b, c]:
                    assert get(request["id"])["current_run_id"] == run_id
                assert len(http("/v1/runs?lane_id=team-test")["items"]) == 1
                assert get(a["id"], "agent-c")["allowed_actions"] == []
                for action in ["withdraw", "retry", "priority"]:
                    body = {"priority": "urgent"} if action == "priority" else {}
                    http(f"/v1/requests/{a['id']}/{action}", body, principal="agent-c", expected=403)
                withdrawn = cli("agent-b", "withdraw", a["id"])
                assert withdrawn["status"] == "withdrawn"
                assert get(b["id"])["status"] == get(c["id"])["status"] == "waiting"
                report["checks"].append("three concurrent principals retain separate requests and share one run; equivalent selectors/timezones and a new session replay the same task key and creation session")
                report["checks"].append("unlisted peer cannot withdraw/retry/raise priority; frozen collaborator withdraws only its authorized request")

                # B and C still own independent requests. Kill the coordinator;
                # C finds its original request after restart and never resubmits.
                process.kill()
                process.wait(timeout=5)
                start()
                found = cli("agent-c", "list", "request", "--task-id", "shared-task", "--principal", "agent-c")
                assert [item["id"] for item in found["items"]] == [c["id"]]
                with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                    waits = [pool.submit(cli, principal, "wait", request["id"], "--timeout-seconds", "20", "--poll-ms", "60", timeout=25)
                             for principal, request in [("agent-b", b), ("agent-c", c)]]
                    b_done, c_done = [future.result() for future in waits]
                assert b_done["status"] == c_done["status"] == "satisfied"
                assert b_done["satisfied_by_run"] == c_done["satisfied_by_run"] == run_id
                assert b_done["satisfied_by_receipt"] == c_done["satisfied_by_receipt"]
                assert c_done["receipt"]["result"] == "success"
                assert get(a["id"])["status"] == "withdrawn"
                assert len(http("/v1/runs?lane_id=team-test")["items"]) == 1
                events = http("/v1/events?lane_id=team-test&after=0&limit=200")["items"]
                assert any(event["request_id"] == a["id"] and event["kind"] == "request_withdrawn" and event["actor"] == "agent-b" for event in events)
                report["checks"].append("SIGKILL/restart permits task lookup and CLI wait on the original request; remaining peers share one verified receipt and actor audit survives")

                identifiers, cursor = [], None
                while True:
                    arguments = ["list", "request", "--task-id", "shared-task", "--limit", "1"]
                    if cursor:
                        arguments.extend(["--cursor", cursor])
                    page = cli("agent-c", *arguments)
                    assert len(page["items"]) == 1
                    identifiers.append(page["items"][0]["id"])
                    cursor = page["next_cursor"]
                    if cursor is None:
                        break
                    assert len(identifiers) <= 3, "pagination cursor did not advance"
                assert len(identifiers) == len(set(identifiers)) == 3
                assert set(identifiers) == {a["id"], b["id"], c["id"]}
                filtered = cli("agent-c", "list", "request", "--task-id", "shared-task", "--principal", "agent-b", "--status", "satisfied")
                assert [item["id"] for item in filtered["items"]] == [b["id"]]
                for query in ["cursor=invalid", "status=not_a_status", "limit=0", "limit=201"]:
                    http("/v1/requests?" + query, principal="agent-c", expected=400)
                report["checks"].append("task/principal/status filtering and limit-one cursor traversal return all requests without duplicates; invalid queries are 400")

                body = {"lane_id": "team-test", "revision": revisions[0], "components": ["app"]}
                for field in ["tenant_id", "principal"]:
                    http("/v1/requests", {**body, field: "forged"}, principal="agent-a", key=f"forged-{field}", expected=400)
                for collaborator in ["unknown", "executor", "outsider"]:
                    http("/v1/requests", {**body, "collaborators": [collaborator]}, principal="agent-a", key=f"invalid-{collaborator}", expected=400)
                report["checks"].append("client-selected tenant/principal and unknown, executor, or out-of-scope collaborators are rejected")

                retry_request = submit("agent-a", "retry-session", revisions[1], "urgent-retry-task", ("agent-b",))
                promoted = cli("admin", "priority", retry_request["id"], "urgent", "--reason", "synthetic admin authorization")
                assert promoted["priority"] == "urgent"
                retry_run = wait_for(lambda: active_run(retry_request["id"]))
                assert retry_run["phase"] in ["preparing", "building"], "failure must be injected before deployment"
                http("/v1/executor-events", {
                    "run_id": retry_run["id"], "generation": retry_run["generation"],
                    "executor_ref": retry_run["executor_ref"], "event": "failed",
                    "reason": "synthetic preparation failure", "environment_known_unchanged": True,
                }, principal="executor")
                attention = get(retry_request["id"], "agent-b")
                assert attention["status"] == "needs_attention" and attention["blocked_reason"]
                assert "retry" not in attention["allowed_actions"]
                http(f"/v1/requests/{retry_request['id']}/retry", {}, principal="agent-b", expected=403)
                http(f"/v1/requests/{retry_request['id']}/retry", {}, principal="agent-a", expected=403)
                retried = cli("admin", "retry", retry_request["id"])
                assert retried["manual_retries_used"] == 1
                done = cli("agent-b", "wait", retry_request["id"], "--timeout-seconds", "20", "--poll-ms", "60", timeout=25)
                assert done["status"] == "satisfied" and done["revision"] == revisions[1]
                assert done["satisfied_by_run"] != retry_run["id"] and done["receipt"]["result"] == "success"
                report["checks"].append("admin can promote another owner's request; urgent retry is denied to an ordinary owner/collaborator and succeeds for admin after synthetic failure")

                with sqlite3.connect(database) as connection:
                    for table in ["lanes", "requests", "runs", "receipts", "commands", "events"]:
                        tenants = connection.execute(f"SELECT DISTINCT tenant_id FROM {table}").fetchall()
                        assert tenants == [("local",)], f"{table} contains missing or non-local tenant data"
                    assert connection.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
                report["checks"].append("all six persisted entity tables contain only local tenant rows and SQLite integrity is valid")
                report["synthetic_only"] = True
            finally:
                if process is not None and process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/auto-deliver")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    report = {"executor": "scripted", "checks": [], "real_deployments": 0}
    try:
        run_acceptance(str(Path(args.binary).resolve()), report)
        report["passed"] = True
    except Exception as error:
        report["passed"] = False
        # Fixture credentials are never command-line arguments or report data.
        report["error"] = f"{type(error).__name__}: {error}"
    rendered = json.dumps(report, indent=2) + "\n"
    if args.output is not None:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered)
    print(rendered, end="")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
