"""Safety tests for the generic local adapter; systemd and cgroups are mocked."""

from __future__ import annotations

import datetime
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
from unittest.mock import patch


ADAPTER_PATH = Path(__file__).with_name("adapter.py")
SPEC = importlib.util.spec_from_file_location("local_adapter", ADAPTER_PATH)
adapter = importlib.util.module_from_spec(SPEC)
assert SPEC and SPEC.loader
SPEC.loader.exec_module(adapter)


def utc_stamp() -> str:
    return datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")


class LocalAdapterTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.state_dir = self.root / "state"
        self.cwd = self.root / "project"
        self.cwd.mkdir(mode=0o700)
        self.run_id = str(uuid.uuid4())
        self.unit = adapter.deterministic_unit(self.run_id, 1)
        self.target = {
            "repository_id": "demo",
            "revision": "a" * 40,
            "components": ["app"],
            "profile_digest": "profile-v1",
            "release_line_id": "mainline",
            "config_version": 1,
            "coverage_mode": "exact",
        }
        self.lane = {
            "tenant_id": "local",
            "id": "demo-dev",
            "project": "demo",
            "environment": "dev",
            "enabled": True,
            "repository_id": "demo",
            "release_ref": "refs/heads/main",
            "release_line_id": "mainline",
            "profile_digest": "profile-v1",
            "config_version": 1,
            "descendant_coverage_enabled": False,
            "preemption_enabled": False,
            "allowed_revisions": ["a" * 40],
            "allowed_components": ["app"],
            "active_run_id": self.run_id,
            "generation": 1,
            "blocked_reason": None,
            "failure_paused": False,
        }
        self.envelope = {
            "version": 1,
            "operation": "dispatch",
            "command": {"id": str(uuid.uuid4()), "run_id": self.run_id, "generation": 1,
                        "kind": "dispatch", "delivery_state": "delivered", "payload_digest": "a" * 64,
                        "created_at": utc_stamp()},
            "run": {
                "id": self.run_id,
                "lane_id": "demo-dev",
                "target": self.target,
                "generation": 1,
                "executor_ref": None,
            },
            "lane": self.lane,
        }
        self.environment_file = self.root / "deploy.env"
        self.environment_file.write_text("SYNTHETIC_SECRET=only-for-deploy\n", encoding="utf-8")
        self.environment_file.chmod(0o600)
        self.deploy_script = self.cwd / "deploy.sh"
        self.deploy_script.write_text("#!/bin/bash\nexit 0\n", encoding="utf-8")
        self.deploy_script.chmod(0o700)
        self.verify_script = self.cwd / "verify.py"
        self.verify_script.write_text(
            "import datetime,json,sys\n"
            "json.load(sys.stdin)\n"
            "print(json.dumps({'source_revision':'" + "a" * 40 + "',"
            "'component_digests':{'app':'" + "d" * 64 + "'},"
            "'profile_digest':'profile-v1','observed_at':"
            "datetime.datetime.now(datetime.timezone.utc).isoformat().replace('+00:00','Z'),"
            "'healthy':True,'verification_report':'synthetic files checked'}))\n",
            encoding="utf-8",
        )
        self.verify_script.chmod(0o700)
        self.config_path = self.root / "adapter.json"
        self.config_data = {
            "version": 1,
            "lane_id": "demo-dev",
            "project": "demo",
            "environment": "dev",
            "repository_id": "demo",
            "components": ["app"],
            "state_dir": str(self.state_dir),
            "cwd": str(self.cwd),
            "deploy": ["/bin/bash", str(self.deploy_script)],
            "verify": [sys.executable, str(self.verify_script)],
            "environment_file": str(self.environment_file),
            "verify_timeout_seconds": 3,
        }
        self.write_config()

    def tearDown(self) -> None:
        self.temp.cleanup()

    def write_config(self) -> None:
        self.config_path.write_text(json.dumps(self.config_data), encoding="utf-8")

    def read_config_fences(self) -> tuple[dict, str, str]:
        config = adapter.load_config(self.config_path)
        _, env_digest = adapter.read_environment_file(Path(config["environment_file"]))
        return config, adapter.configuration_digest(config, env_digest), env_digest

    def paths(self) -> tuple[Path, Path, Path, Path]:
        config, _, _ = self.read_config_fences()
        return adapter.run_paths(config, self.run_id, 1)

    def durable_request(self) -> tuple[dict, Path, Path]:
        config, config_digest, env_digest = self.read_config_fences()
        run_dir, request_path, result_path, _ = adapter.run_paths(config, self.run_id, 1)
        frozen = dict(self.envelope)
        record = {
            "version": 1,
            "run_id": self.run_id,
            "generation": 1,
            "target": self.target,
            "unit": self.unit,
            "configuration_digest": config_digest,
            "environment_file_digest": env_digest,
            "created_at": utc_stamp(),
            "envelope": frozen,
        }
        adapter.atomic_json(request_path, record)
        return config, request_path, result_path

    def result(self, code: int = 0, invocation: str = "invocation-1") -> dict:
        _, config_digest, env_digest = self.read_config_fences()
        return {
            "run_id": self.run_id,
            "generation": 1,
            "target": self.target,
            "unit": self.unit,
            "configuration_digest": config_digest,
            "environment_file_digest": env_digest,
            "invocation_id": invocation,
            "returncode": code,
            "finished_at": utc_stamp(),
        }

    def unit_state(self, code: int = 0, invocation: str = "invocation-1") -> dict[str, str]:
        if code == 0:
            return {
                "Id": self.unit,
                "InvocationID": invocation,
                "LoadState": "loaded",
                "ActiveState": "active",
                "SubState": "exited",
                "MainPID": "0",
                "ExecMainCode": "1",
                "ExecMainStatus": "0",
                "ExecMainExitTimestamp": utc_stamp(),
                "Result": "success",
                "ControlGroup": "/user.slice/" + self.unit,
                "ExecStart": "{ path=/usr/bin/python3 ; argv[]=/usr/bin/python3 adapter.py --worker }",
            }
        return {
            "Id": self.unit,
            "InvocationID": invocation,
            "LoadState": "loaded",
            "ActiveState": "failed",
            "SubState": "failed",
            "MainPID": "0",
            "ExecMainCode": "1",
            "ExecMainStatus": str(code),
            "ExecMainExitTimestamp": utc_stamp(),
            "Result": "exit-code",
            "ControlGroup": "/user.slice/" + self.unit,
            "ExecStart": "{ path=/usr/bin/python3 ; argv[]=/usr/bin/python3 adapter.py --worker }",
        }

    def fake_cgroup(self, populated: int = 0, pids: str = "") -> Path:
        root = self.root / "cgroup"
        root.mkdir(exist_ok=True)
        (root / "cgroup.controllers").write_text("cpu memory\n", encoding="utf-8")
        group = root / "user.slice" / self.unit
        group.mkdir(parents=True, exist_ok=True)
        (group / "cgroup.events").write_text(f"populated {populated}\nfrozen 0\n", encoding="utf-8")
        (group / "cgroup.procs").write_text(pids, encoding="utf-8")
        return root

    def test_duplicate_dispatch_attempts_systemd_only_once(self) -> None:
        not_found = {"LoadState": "not-found"}
        running = {"Id": self.unit, "LoadState": "loaded", "ActiveState": "active", "SubState": "running"}
        with patch.object(adapter, "unit_state", side_effect=[not_found, running]), \
                patch.object(adapter, "systemd_run") as start:
            first = adapter.handle_hook(self.envelope, self.config_path)
            second = adapter.handle_hook(self.envelope, self.config_path)
        self.assertEqual(first["status"], "started")
        self.assertEqual(second["status"], "running")
        self.assertEqual(first["executor_ref"], self.unit)
        start.assert_called_once()
        _, request_path, _, _ = self.paths()
        self.assertEqual(request_path.stat().st_mode & 0o777, 0o600)

    def test_uncertain_handoff_is_not_replayed(self) -> None:
        with patch.object(adapter, "unit_state", side_effect=[{"LoadState": "not-found"}, {"LoadState": "not-found"}]), \
                patch.object(adapter, "systemd_run", side_effect=adapter.AdapterError("lost response")) as start:
            first = adapter.handle_hook(self.envelope, self.config_path)
            second = adapter.handle_hook(self.envelope, self.config_path)
        self.assertEqual(first["status"], "unknown")
        self.assertEqual(second["status"], "unknown")
        start.assert_called_once()

    def test_systemd_worker_does_not_apply_a_global_file_size_limit(self) -> None:
        completed = type("Completed", (), {})()
        with patch.object(adapter.subprocess, "run", return_value=completed) as run:
            adapter.systemd_run(self.config_path, self.root / "request.json", self.unit)
        command = run.call_args.args[0]
        self.assertIn("--property=StandardOutput=null", command)
        self.assertIn("--property=StandardError=null", command)
        self.assertFalse(any(value.startswith("--property=LimitFSIZE=") for value in command))

    def test_wrong_target_cannot_create_a_dispatch_intent(self) -> None:
        wrong = json.loads(json.dumps(self.envelope))
        wrong["run"]["target"]["repository_id"] = "other"
        with patch.object(adapter, "systemd_run") as start:
            reply = adapter.handle_hook(wrong, self.config_path)
        self.assertEqual(reply["status"], "unknown")
        self.assertNotIn("receipt", reply)
        start.assert_not_called()

    def test_observation_rejects_mismatched_systemd_invocation_id(self) -> None:
        config, request_path, result_path = self.durable_request()
        adapter.atomic_json(result_path, self.result(invocation="invocation-result"))
        root = self.fake_cgroup()
        observe = dict(self.envelope, operation="observe", command=None)
        state = self.unit_state(invocation="invocation-other")
        state["ControlGroup"] = ""
        with patch.object(adapter, "CGROUP_ROOT", root), patch.object(adapter, "unit_state", return_value=state):
            reply = adapter.handle_hook(observe, self.config_path)
        self.assertEqual(reply["status"], "unknown")
        self.assertNotIn("failure_proof", reply)
        self.assertNotIn("receipt", reply)

    def test_systemd249_empty_control_group_is_destroyed_after_matching_stop_proof(self) -> None:
        _, _, result_path = self.durable_request()
        adapter.atomic_json(result_path, self.result())
        root = self.fake_cgroup()
        observe = dict(self.envelope, operation="observe", command=None)
        # Observed on systemd 249 for Type=exec + RemainAfterExit=yes:
        # active/exited, MainPID=0, ExecMainCode=1, and an empty ControlGroup.
        state = self.unit_state()
        state["ActiveState"] = "active"
        state["SubState"] = "exited"
        state["ControlGroup"] = ""
        with patch.object(adapter, "CGROUP_ROOT", root), patch.object(adapter, "unit_state", return_value=state):
            reply = adapter.handle_hook(observe, self.config_path)
        self.assertEqual(reply["status"], "completed")
        self.assertNotIn("receipt", reply)
        self.assertEqual(adapter.cgroup_snapshot(""), {
            "state": "destroyed", "populated": 0, "groups_scanned": 0,
            "double_scan_stable": True, "path": "",
        })

    def test_literal_cgroup_root_never_proves_stop(self) -> None:
        _, _, result_path = self.durable_request()
        adapter.atomic_json(result_path, self.result())
        root = self.fake_cgroup()
        observe = dict(self.envelope, operation="observe", command=None)
        state = self.unit_state()
        state["ControlGroup"] = "/"
        with patch.object(adapter, "CGROUP_ROOT", root), patch.object(adapter, "unit_state", return_value=state):
            reply = adapter.handle_hook(observe, self.config_path)
        self.assertEqual(reply["status"], "unknown")
        self.assertNotIn("failure_proof", reply)

    def test_missing_nonroot_cgroup_path_is_destroyed_only_after_v2_root_check(self) -> None:
        root = self.root / "cgroup-missing-path"
        root.mkdir(mode=0o700)
        (root / "cgroup.controllers").write_text("cpu memory\n", encoding="utf-8")
        with patch.object(adapter, "CGROUP_ROOT", root):
            evidence = adapter.cgroup_snapshot("/user.slice/removed.service")
            self.assertEqual(evidence["state"], "destroyed")
            self.assertEqual(evidence["path"], "/user.slice/removed.service")
            with self.assertRaises(adapter.AdapterError):
                adapter.cgroup_snapshot("/")
        invalid_root = self.root / "not-cgroup-v2"
        invalid_root.mkdir(mode=0o700)
        with patch.object(adapter, "CGROUP_ROOT", invalid_root):
            with self.assertRaises(adapter.AdapterError):
                adapter.cgroup_snapshot("/user.slice/removed.service")

    def test_missing_nonroot_cgroup_with_matching_terminal_unit_is_observable(self) -> None:
        _, _, result_path = self.durable_request()
        adapter.atomic_json(result_path, self.result())
        root = self.root / "cgroup-removed-unit"
        root.mkdir(mode=0o700)
        (root / "cgroup.controllers").write_text("cpu memory\n", encoding="utf-8")
        state = self.unit_state()
        state["ControlGroup"] = "/user.slice/removed.service"
        observe = dict(self.envelope, operation="observe", command=None)
        with patch.object(adapter, "CGROUP_ROOT", root), patch.object(adapter, "unit_state", return_value=state):
            reply = adapter.handle_hook(observe, self.config_path)
        self.assertEqual(reply["status"], "completed")

    def test_nonempty_cgroup_never_yields_failure_stop_proof(self) -> None:
        _, _, result_path = self.durable_request()
        adapter.atomic_json(result_path, self.result(code=7))
        root = self.fake_cgroup(populated=1, pids="4321\n")
        observe = dict(self.envelope, operation="observe", command=None)
        state = self.unit_state(code=7)
        with patch.object(adapter, "CGROUP_ROOT", root), patch.object(adapter, "unit_state", return_value=state):
            reply = adapter.handle_hook(observe, self.config_path)
        self.assertEqual(reply["status"], "unknown")
        self.assertNotIn("failure_proof", reply)

    def test_stopped_failure_returns_fresh_bounded_proof_without_receipt(self) -> None:
        _, _, result_path = self.durable_request()
        adapter.atomic_json(result_path, self.result(code=7))
        root = self.fake_cgroup()
        observe = dict(self.envelope, operation="observe", command=None)
        state = self.unit_state(code=7)
        started = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(seconds=1)
        with patch.object(adapter, "CGROUP_ROOT", root), patch.object(adapter, "unit_state", return_value=state):
            reply = adapter.handle_hook(observe, self.config_path, call_started=started)
        self.assertEqual(reply["status"], "failed")
        self.assertIs(reply["environment_known_unchanged"], False)
        self.assertGreaterEqual(adapter.parse_timestamp(reply["failure_proof"]["observed_at"]), started)
        self.assertLessEqual(len(adapter.canonical_json(reply["failure_proof"]["evidence"])), 8192)
        self.assertNotIn("receipt", reply)

    def test_fresh_read_only_verification_wraps_matching_receipt(self) -> None:
        config, _, result_path = self.durable_request()
        adapter.atomic_json(result_path, self.result())
        root = self.fake_cgroup()
        verify = dict(self.envelope, operation="verify", command={
            "id": str(uuid.uuid4()), "run_id": self.run_id, "generation": 1,
            "kind": "verify", "delivery_state": "delivered", "payload_digest": "b" * 64,
            "created_at": utc_stamp(),
        })
        # The revision authorization set grows over time; it is checked on dispatch,
        # but must not invalidate observation of this already-fenced run.
        verify["lane"]["allowed_revisions"].append("b" * 40)
        state = self.unit_state()
        with patch.object(adapter, "CGROUP_ROOT", root), patch.object(adapter, "unit_state", return_value=state):
            reply = adapter.handle_hook(verify, self.config_path)
        self.assertEqual(reply["status"], "verified")
        receipt = reply["receipt"]
        self.assertEqual(receipt["source_revision"], self.target["revision"])
        self.assertEqual(receipt["component_digests"], {"app": "d" * 64})
        self.assertEqual(receipt["profile_digest"], self.target["profile_digest"])
        self.assertEqual(receipt["result"], "success")

    def test_verify_command_must_match_run_generation(self) -> None:
        verify = dict(self.envelope, operation="verify", command={
            "id": str(uuid.uuid4()), "run_id": self.run_id, "generation": 2, "kind": "verify",
        })
        reply = adapter.handle_hook(verify, self.config_path)
        self.assertEqual(reply["status"], "unknown")

    def test_timed_out_verifier_holds_lock_until_child_exits(self) -> None:
        script = self.cwd / "slow-verify.py"
        marker = self.root / "verify-started"
        script.write_text(
            "import pathlib,time\n"
            f"pathlib.Path({str(marker)!r}).write_text('started')\n"
            "time.sleep(2)\n",
            encoding="utf-8",
        )
        script.chmod(0o700)
        self.config_data["verify"] = [sys.executable, str(script)]
        self.config_data["verify_timeout_seconds"] = 1
        self.write_config()
        _, _, result_path = self.durable_request()
        config, config_digest, env_digest = self.read_config_fences()
        request = adapter.load_json(self.paths()[1])
        request["configuration_digest"] = config_digest
        request["environment_file_digest"] = env_digest
        adapter.atomic_json(self.paths()[1], request)
        adapter.atomic_json(result_path, self.result())
        root = self.fake_cgroup()
        verify = dict(self.envelope, operation="verify", command=None)
        state = self.unit_state()
        with patch.object(adapter, "CGROUP_ROOT", root), patch.object(adapter, "unit_state", return_value=state):
            first = adapter.handle_hook(verify, self.config_path)
            second = adapter.handle_hook(verify, self.config_path)
        self.assertEqual(first["status"], "unknown")
        self.assertEqual(second["status"], "unknown")
        self.assertTrue(marker.exists())
        lock_path = self.paths()[0] / "verify.lock"
        fd = os.open(lock_path, os.O_RDWR)
        try:
            with self.assertRaises(BlockingIOError):
                adapter.fcntl.flock(fd, adapter.fcntl.LOCK_EX | adapter.fcntl.LOCK_NB)
        finally:
            os.close(fd)
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            fd = os.open(lock_path, os.O_RDWR)
            try:
                try:
                    adapter.fcntl.flock(fd, adapter.fcntl.LOCK_EX | adapter.fcntl.LOCK_NB)
                except BlockingIOError:
                    time.sleep(0.05)
                    continue
                adapter.fcntl.flock(fd, adapter.fcntl.LOCK_UN)
                break
            finally:
                os.close(fd)
        else:
            self.fail("timed-out verifier did not release its inherited lock after exit")

    def test_verifier_output_is_bounded_without_spooling_to_disk(self) -> None:
        script = self.cwd / "large-verify.py"
        script.write_text("import sys\nsys.stdout.write('x' * 1100000)\n", encoding="utf-8")
        script.chmod(0o700)
        self.config_data["verify"] = [sys.executable, str(script)]
        self.write_config()
        _, _, result_path = self.durable_request()
        config, config_digest, env_digest = self.read_config_fences()
        request_path = self.paths()[1]
        request = adapter.load_json(request_path)
        request["configuration_digest"] = config_digest
        request["environment_file_digest"] = env_digest
        adapter.atomic_json(request_path, request)
        adapter.atomic_json(result_path, self.result())
        root = self.fake_cgroup()
        verify = dict(self.envelope, operation="verify", command=None)
        state = self.unit_state()
        with patch.object(adapter, "CGROUP_ROOT", root), patch.object(adapter, "unit_state", return_value=state):
            reply = adapter.handle_hook(verify, self.config_path)
        self.assertEqual(reply["status"], "unknown")
        self.assertFalse(list(self.paths()[0].glob("verify-*.output.json")))

    def test_old_verification_timestamp_does_not_create_receipt(self) -> None:
        _, request_path, result_path = self.durable_request()
        stale = self.cwd / "stale-verify.py"
        stale.write_text(
            "import json,sys\njson.load(sys.stdin)\n"
            "print(json.dumps({'source_revision':'" + "a" * 40 + "','component_digests':{'app':'" + "d" * 64
            + "'},'profile_digest':'profile-v1','observed_at':'2000-01-01T00:00:00Z','healthy':True,'verification_report':'old'}))\n",
            encoding="utf-8",
        )
        stale.chmod(0o700)
        self.config_data["verify"] = [sys.executable, str(stale)]
        self.write_config()
        config, config_digest, env_digest = self.read_config_fences()
        record = adapter.load_json(request_path)
        record["configuration_digest"] = config_digest
        record["environment_file_digest"] = env_digest
        adapter.atomic_json(request_path, record)
        adapter.atomic_json(result_path, self.result())
        verify = dict(self.envelope, operation="verify", command=None)
        root = self.fake_cgroup()
        with patch.object(adapter, "CGROUP_ROOT", root), patch.object(adapter, "unit_state", return_value=self.unit_state()):
            reply = adapter.handle_hook(verify, self.config_path)
        self.assertEqual(reply["status"], "unknown")
        self.assertNotIn("receipt", reply)

    def test_worker_passes_only_allowlisted_environment_to_deploy(self) -> None:
        self.deploy_script.write_text(
            "import json,os\n"
            "from pathlib import Path\n"
            "keys=('SHIPBRAID_COORDINATOR_TOKEN','INVOCATION_ID','SYNTHETIC_SECRET','SHIPBRAID_RUN_ID')\n"
            "Path('captured-env.json').write_text(json.dumps({key:os.environ.get(key) for key in keys}))\n",
            encoding="utf-8",
        )
        self.deploy_script.chmod(0o700)
        self.config_data["deploy"] = [sys.executable, str(self.deploy_script)]
        self.write_config()
        config, config_digest, env_digest = self.read_config_fences()
        run_dir, request_path, _, _ = adapter.run_paths(config, self.run_id, 1)
        adapter.atomic_json(request_path, {
            "version": 1,
            "run_id": self.run_id,
            "generation": 1,
            "target": self.target,
            "unit": self.unit,
            "configuration_digest": config_digest,
            "environment_file_digest": env_digest,
            "created_at": utc_stamp(),
            "envelope": self.envelope,
        })
        state = {"Id": self.unit, "LoadState": "loaded", "InvocationID": "invocation-worker", "ActiveState": "active"}
        with patch.dict(os.environ, {"INVOCATION_ID": "invocation-worker", "SHIPBRAID_COORDINATOR_TOKEN": "must-not-leak"}), \
                patch.object(adapter, "unit_state", return_value=state):
            code = adapter.worker(request_path, self.config_path)
        self.assertEqual(code, 0)
        child_env = json.loads((self.cwd / "captured-env.json").read_text(encoding="utf-8"))
        self.assertIsNone(child_env["SHIPBRAID_COORDINATOR_TOKEN"])
        self.assertIsNone(child_env["INVOCATION_ID"])
        self.assertEqual(child_env["SYNTHETIC_SECRET"], "only-for-deploy")
        self.assertEqual(child_env["SHIPBRAID_RUN_ID"], self.run_id)

    def test_large_deploy_artifact_survives_capped_and_drained_logs(self) -> None:
        noisy = self.cwd / "noisy-deploy.py"
        noisy.write_text(
            "import json,pathlib,sys\n"
            "sys.stdout.write('LOG_API_SENTINEL' * 20000)\n"
            "sys.stdout.flush()\n"
            "json.load(sys.stdin)\n"
            "sys.stderr.write('stderr-output' * 20000)\n"
            "sys.stderr.flush()\n"
            "pathlib.Path('large-artifact.bin').write_bytes(b'a' * 4096)\n",
            encoding="utf-8",
        )
        noisy.chmod(0o700)
        self.config_data["deploy"] = [sys.executable, str(noisy)]
        self.write_config()
        config, config_digest, env_digest = self.read_config_fences()
        run_dir, request_path, result_path, _ = adapter.run_paths(config, self.run_id, 1)
        adapter.atomic_json(request_path, {
            "version": 1, "run_id": self.run_id, "generation": 1, "target": self.target,
            "unit": self.unit, "configuration_digest": config_digest,
            "environment_file_digest": env_digest, "created_at": utc_stamp(),
            "envelope": self.envelope,
        })
        worker_state = {"Id": self.unit, "LoadState": "loaded", "InvocationID": "invocation-noisy",
                        "ActiveState": "active"}
        with patch.dict(os.environ, {"INVOCATION_ID": "invocation-noisy"}), \
                patch.object(adapter, "unit_state", return_value=worker_state), \
                patch.object(adapter, "DEPLOY_LOG_MAX_BYTES", 128):
            code = adapter.worker(request_path, self.config_path)
        self.assertEqual(code, 0)
        artifact = self.cwd / "large-artifact.bin"
        log_path = run_dir / "deploy.log"
        self.assertEqual(artifact.stat().st_size, 4096)
        self.assertEqual(log_path.stat().st_size, 128)
        result = adapter.load_json(result_path)
        self.assertEqual(result["returncode"], 0)

        root = self.fake_cgroup()
        state = self.unit_state(invocation="invocation-noisy")
        observe = dict(self.envelope, operation="observe", command=None)
        with patch.object(adapter, "CGROUP_ROOT", root), patch.object(adapter, "unit_state", return_value=state):
            reply = adapter.handle_hook(observe, self.config_path)
        self.assertEqual(reply["status"], "completed")
        self.assertNotIn("LOG_API_SENTINEL", json.dumps(reply))

    def test_synthetic_project_bundle_reads_and_verifies_fake_environment(self) -> None:
        demo_root = self.root / "temporary-demo-environment"
        self.environment_file.write_text(
            f"SHIPBRAID_DEMO_ROOT={demo_root}\nSHIPBRAID_DEMO_DELAY_SECONDS=0\n", encoding="utf-8")
        self.environment_file.chmod(0o600)
        config = adapter.load_config(self.config_path)
        _, environment_digest = adapter.read_environment_file(self.environment_file)
        env = adapter.child_environment(config, environment_digest, self.envelope)
        deploy = subprocess.run(["/bin/bash", str(ADAPTER_PATH.parents[2] / "examples/standalone/project/deploy.sh")],
                                cwd=config["cwd"], env=env, input=adapter.canonical_json(self.envelope),
                                capture_output=True, check=False)
        self.assertEqual(deploy.returncode, 0, deploy.stderr.decode("utf-8", errors="replace"))
        verify_envelope = dict(self.envelope, operation="verify", command=None)
        verified = subprocess.run([sys.executable, str(ADAPTER_PATH.parents[2] / "examples/standalone/project/verify.py")],
                                  cwd=config["cwd"],
                                  env=adapter.child_environment(config, environment_digest, verify_envelope),
                                  input=adapter.canonical_json(verify_envelope), capture_output=True, check=False)
        self.assertEqual(verified.returncode, 0, verified.stderr.decode("utf-8", errors="replace"))
        proof = json.loads(verified.stdout)
        self.assertTrue(proof["healthy"])
        self.assertEqual(proof["source_revision"], self.target["revision"])
        self.assertEqual(len((demo_root / "deployment-starts").read_text().splitlines()), 1)
        self.assertTrue((demo_root / "app-artifact.json").is_file())


if __name__ == "__main__":
    unittest.main()
