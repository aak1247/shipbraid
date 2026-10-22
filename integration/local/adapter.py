#!/usr/bin/env python3
"""Generic local Linux hook adapter. Python 3.11+, systemd --user, cgroup v2."""

from __future__ import annotations

import argparse
import datetime as dt
import fcntl
import hashlib
import hmac
import http.server
import json
import os
from pathlib import Path
import pwd
import re
import stat
import subprocess
import sys
import tempfile
import threading
import uuid
from typing import Any


HOOK_MAX_BYTES = 256 * 1024
HOOK_MAX_REPLY_BYTES = 1024 * 1024
FAILURE_EVIDENCE_MAX_BYTES = 8192
DEPLOY_LOG_MAX_BYTES = 64 * 1024 * 1024
CGROUP_ROOT = Path("/sys/fs/cgroup")
UNIT_FENCE_FIELDS = (
    "Id",
    "InvocationID",
    "LoadState",
    "ActiveState",
    "SubState",
    "MainPID",
    "ExecMainCode",
    "ExecMainStatus",
    "ExecMainExitTimestamp",
    "Result",
    "ControlGroup",
    "ExecStart",
)
SYSTEMD_SHOW_FIELDS = ("Id", "InvocationID", "LoadState", "ActiveState", "SubState", "MainPID",
                       "ExecMainCode", "ExecMainStatus", "ExecMainExitTimestamp", "Result",
                       "ControlGroup", "ExecStart")
CONFIG_FIELDS = {
    "version", "lane_id", "project", "environment", "repository_id", "components",
    "state_dir", "cwd", "deploy", "verify", "environment_file", "verify_timeout_seconds",
    "forward_env_prefixes",
}
ENV_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*\Z")
COMPONENT_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}\Z")
REVISION = re.compile(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})\Z")


class AdapterError(Exception):
    """An expected boundary or evidence failure, safe to reduce to an unknown reply."""


def utc_now() -> dt.datetime:
    return dt.datetime.now(dt.timezone.utc)


def timestamp(value: dt.datetime | None = None) -> str:
    return (value or utc_now()).isoformat(timespec="microseconds").replace("+00:00", "Z")


def parse_timestamp(value: Any) -> dt.datetime:
    if not isinstance(value, str) or not value:
        raise AdapterError("timestamp is missing")
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as exc:
        raise AdapterError("timestamp is invalid") from exc
    if parsed.tzinfo is None or parsed.utcoffset() != dt.timedelta(0):
        raise AdapterError("timestamp must be UTC")
    return parsed.astimezone(dt.timezone.utc)


def canonical_json(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def digest_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def load_json(path: Path, max_bytes: int = HOOK_MAX_REPLY_BYTES) -> Any:
    try:
        with path.open("rb") as handle:
            raw = handle.read(max_bytes + 1)
    except OSError as exc:
        raise AdapterError("durable adapter record cannot be read") from exc
    if len(raw) > max_bytes:
        raise AdapterError("durable adapter record exceeds its size limit")
    try:
        return json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise AdapterError("durable adapter record is invalid") from exc


def atomic_json(path: Path, value: Any) -> None:
    """Durably replace one private JSON file and its containing directory entry."""
    raw = canonical_json(value)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(prefix="." + path.name + ".", dir=path.parent)
    temporary = Path(temporary_name)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as handle:
            handle.write(raw)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
        directory_fd = os.open(path.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    except BaseException:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass
        raise


def ensure_private_dir(path: Path) -> Path:
    missing: list[Path] = []
    cursor = path
    while not cursor.exists():
        missing.append(cursor)
        parent = cursor.parent
        if parent == cursor:
            break
        cursor = parent
    try:
        path.mkdir(mode=0o700, parents=True, exist_ok=True)
        info = path.lstat()
    except OSError as exc:
        raise AdapterError("private adapter state directory is unavailable") from exc
    if not stat.S_ISDIR(info.st_mode) or stat.S_ISLNK(info.st_mode):
        raise AdapterError("adapter state directory is not a real directory")
    if info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise AdapterError("adapter state directory must be user-owned and private")
    for created in reversed(missing):
        try:
            parent_fd = os.open(created.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
            try:
                os.fsync(parent_fd)
            finally:
                os.close(parent_fd)
        except OSError as exc:
            raise AdapterError("adapter state directory could not be made durable") from exc
    return path


def absolute_path(value: Any, label: str) -> Path:
    if not isinstance(value, str) or not value or "\x00" in value:
        raise AdapterError(f"{label} is invalid")
    path = Path(value)
    if not path.is_absolute():
        raise AdapterError(f"{label} must be absolute")
    return path


def validate_argv(value: Any, label: str) -> list[str]:
    argv = value
    if (not isinstance(argv, list) or not argv or len(argv) > 128
            or any(not isinstance(arg, str) or not arg or "\x00" in arg or len(arg) > 4096 for arg in argv)):
        raise AdapterError(f"{label}.argv is invalid")
    if not Path(argv[0]).is_absolute():
        raise AdapterError(f"{label}.argv[0] must be an absolute executable path")
    executable = Path(argv[0])
    if not executable.is_file() or not os.access(executable, os.X_OK):
        raise AdapterError(f"{label}.argv[0] is not an executable file")
    return list(argv)


def read_environment_file(path: Path | None) -> tuple[dict[str, str], str]:
    if path is None:
        return {}, sha256_bytes(b"")
    try:
        info = path.lstat()
        raw = path.read_bytes()
    except OSError as exc:
        raise AdapterError("dedicated environment file is unavailable") from exc
    if (not stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode)
            or info.st_uid != os.getuid() or info.st_mode & 0o077):
        raise AdapterError("dedicated environment file must be a private user-owned regular file")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise AdapterError("dedicated environment file must use UTF-8") from exc
    values: dict[str, str] = {}
    reserved = {"PATH", "HOME", "LANG", "LC_ALL", "INVOCATION_ID", "LISTEN_PID", "LISTEN_FDS",
                "PYTHONPATH", "PYTHONHOME", "BASH_ENV", "ENV", "SHELLOPTS", "LD_PRELOAD", "LD_LIBRARY_PATH"}
    allowed_demo_names = {"SHIPBRAID_DEMO_ROOT", "SHIPBRAID_DEMO_DELAY_SECONDS"}
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        name, separator, value = line.partition("=")
        if (not separator or not ENV_NAME.fullmatch(name) or name in reserved
                or (name.startswith("SHIPBRAID_") and name not in allowed_demo_names)):
            raise AdapterError("dedicated environment file contains an invalid or reserved name")
        if "\x00" in value or "\n" in value or "\r" in value:
            raise AdapterError("dedicated environment file contains an invalid value")
        if name in values:
            raise AdapterError("dedicated environment file contains a duplicate name")
        values[name] = value
    return values, sha256_bytes(raw)


def load_config(path: Path) -> dict[str, Any]:
    if not path.is_absolute():
        raise AdapterError("--config must be an absolute path")
    config = load_json(path, 64 * 1024)
    if not isinstance(config, dict) or set(config) - CONFIG_FIELDS:
        raise AdapterError("adapter configuration has unsupported fields")
    required = {"lane_id", "project", "environment", "repository_id", "components",
                "state_dir", "cwd", "deploy", "verify"}
    if not required.issubset(config) or config.get("version", 1) != 1:
        raise AdapterError("adapter configuration is incomplete or unsupported")
    for name in ("lane_id", "project", "repository_id"):
        value = config.get(name)
        if not isinstance(value, str) or not value or len(value) > 256 or "\x00" in value:
            raise AdapterError(f"adapter configuration {name} is invalid")
    if config.get("environment") not in ("dev", "test"):
        raise AdapterError("local adapter supports only dev and test environments")
    components = config.get("components")
    if (not isinstance(components, list) or not components
            or any(not isinstance(item, str) or not COMPONENT_NAME.fullmatch(item) for item in components)
            or len(set(components)) != len(components)):
        raise AdapterError("configured component allowlist is invalid")
    state_dir = absolute_path(config.get("state_dir"), "state_dir")
    cwd = absolute_path(config.get("cwd"), "cwd")
    try:
        cwd = cwd.resolve(strict=True)
    except OSError as exc:
        raise AdapterError("configured working directory does not exist") from exc
    if not cwd.is_dir():
        raise AdapterError("configured working directory is not a directory")
    environment_file_value = config.get("environment_file")
    environment_file = None if environment_file_value in (None, "") else absolute_path(environment_file_value, "environment_file")
    forward_env_prefixes_value = config.get("forward_env_prefixes", [])
    if (not isinstance(forward_env_prefixes_value, list)
            or any(not isinstance(prefix, str) or not prefix or "\x00" in prefix for prefix in forward_env_prefixes_value)):
        raise AdapterError("configured forward_env_prefixes is invalid")
    verify_timeout = config.get("verify_timeout_seconds", 10)
    if isinstance(verify_timeout, bool) or not isinstance(verify_timeout, int) or not 1 <= verify_timeout <= 10:
        raise AdapterError("verify_timeout_seconds must be between 1 and 10")
    deploy_argv = validate_argv(config.get("deploy"), "deploy")
    verify_argv = validate_argv(config.get("verify"), "verify")
    normalized = {
        "version": 1,
        "lane_id": config["lane_id"],
        "project": config["project"],
        "environment": config["environment"],
        "repository_id": config["repository_id"],
        "components": list(config["components"]),
        "state_dir": str(state_dir),
        "cwd": str(cwd),
        "deploy": deploy_argv,
        "verify": verify_argv,
        "environment_file": str(environment_file) if environment_file else None,
        "verify_timeout_seconds": verify_timeout,
        "forward_env_prefixes": list(forward_env_prefixes_value),
    }
    return normalized


def collect_forwarded_environment(prefixes: list[str]) -> dict[str, str]:
    if not prefixes:
        return {}
    forwarded: dict[str, str] = {}
    reserved = {"PATH", "HOME", "LANG", "LC_ALL", "INVOCATION_ID", "LISTEN_PID", "LISTEN_FDS",
                "PYTHONPATH", "PYTHONHOME", "BASH_ENV", "ENV", "SHELLOPTS", "LD_PRELOAD", "LD_LIBRARY_PATH"}
    allowed_demo_names = {"SHIPBRAID_DEMO_ROOT", "SHIPBRAID_DEMO_DELAY_SECONDS"}
    for name in sorted(os.environ):
        if not any(name.startswith(prefix) for prefix in prefixes):
            continue
        value = os.environ[name]
        if (not ENV_NAME.fullmatch(name) or len(name) > 64 or name in reserved
                or (name.startswith("SHIPBRAID_") and name not in allowed_demo_names)):
            raise AdapterError(f"forwarded environment contains an invalid or reserved name: {name}")
        if not value or len(value) > 8192 or "\x00" in value or "\n" in value or "\r" in value:
            raise AdapterError(f"forwarded environment contains an invalid value for {name}")
        forwarded[name] = value
    return forwarded


def configuration_digest(config: dict[str, Any], environment_digest: str) -> str:
    script_files: dict[str, str] = {}
    for name in ("deploy", "verify"):
        argv = config[name]
        for index, argument in enumerate(argv[1:], start=1):
            candidate = Path(argument)
            if not candidate.is_absolute():
                continue
            try:
                resolved = candidate.resolve(strict=True)
            except OSError:
                continue
            if resolved.is_file():
                script_files[f"{name}:{index}"] = digest_file(resolved)
    material = {"configuration": config, "environment_file_sha256": environment_digest,
                "project_files": script_files}
    return sha256_bytes(canonical_json(material))


def run_paths(config: dict[str, Any], run_id: str, generation: int) -> tuple[Path, Path, Path, Path]:
    root = ensure_private_dir(Path(config["state_dir"]))
    run_dir = ensure_private_dir(root / run_id)
    run_dir = ensure_private_dir(run_dir / str(generation))
    return run_dir, run_dir / "request.json", run_dir / "result.json", run_dir / "dispatch.lock"


def deterministic_unit(run_id: str, generation: int) -> str:
    try:
        parsed = uuid.UUID(run_id)
    except (ValueError, AttributeError, TypeError) as exc:
        raise AdapterError("run id is invalid") from exc
    if isinstance(generation, bool) or not isinstance(generation, int) or generation < 1 or generation > 2**64 - 1:
        raise AdapterError("run generation is invalid")
    return f"shipbraid-local-{parsed.hex}-g{generation}.service"


def validate_envelope(envelope: Any, config: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any], str]:
    if not isinstance(envelope, dict) or envelope.get("version") != 1:
        raise AdapterError("unsupported hook envelope")
    operation = envelope.get("operation")
    if operation not in ("dispatch", "observe", "verify"):
        raise AdapterError("unsupported hook operation")
    run = envelope.get("run")
    lane = envelope.get("lane")
    target = run.get("target") if isinstance(run, dict) else None
    if not isinstance(run, dict) or not isinstance(lane, dict) or not isinstance(target, dict):
        raise AdapterError("hook envelope is missing its run, target, or lane")
    run_id = run.get("id")
    generation = run.get("generation")
    unit = deterministic_unit(run_id, generation)
    if run.get("lane_id") != config["lane_id"] or lane.get("id") != config["lane_id"]:
        raise AdapterError("hook run is outside the configured lane")
    if (lane.get("project") != config["project"] or lane.get("environment") != config["environment"]
            or lane.get("repository_id") != config["repository_id"]):
        raise AdapterError("hook lane does not match the configured project environment")
    if config["environment"] not in ("dev", "test"):
        raise AdapterError("production dispatch is disabled")
    if target.get("repository_id") != config["repository_id"]:
        raise AdapterError("target repository does not match the configured repository")
    revision = target.get("revision")
    if not isinstance(revision, str) or not REVISION.fullmatch(revision):
        raise AdapterError("target revision must be a full Git object id")
    components = target.get("components")
    if (not isinstance(components, list) or not components or any(not isinstance(value, str) for value in components)
            or len(set(components)) != len(components)):
        raise AdapterError("target component set is invalid")
    configured = set(config["components"])
    lane_components = lane.get("allowed_components")
    if not isinstance(lane_components, list):
        raise AdapterError("lane component authorization is missing")
    if not set(components).issubset(configured) or not set(components).issubset(set(lane_components)):
        raise AdapterError("target includes an unauthorized component")
    for target_key, lane_key in (("release_line_id", "release_line_id"),
                                 ("profile_digest", "profile_digest"),
                                 ("config_version", "config_version")):
        if target.get(target_key) != lane.get(lane_key):
            raise AdapterError("target does not match the lane configuration snapshot")
    if operation == "dispatch":
        if lane.get("enabled") is not True or lane.get("preemption_enabled") is not False:
            raise AdapterError("lane is disabled or permits preemption")
        allowed_revisions = lane.get("allowed_revisions")
        if not isinstance(allowed_revisions, list) or (allowed_revisions and revision not in allowed_revisions):
            raise AdapterError("target revision is not authorized by the lane")
        validate_command(envelope.get("command"), run, "dispatch")
    elif operation == "observe":
        if envelope.get("command") is not None:
            raise AdapterError("observe envelopes must not contain a command")
    elif envelope.get("command") is not None:
        validate_command(envelope.get("command"), run, "verify")
    recorded_ref = run.get("executor_ref")
    if recorded_ref not in (None, unit):
        raise AdapterError("executor reference does not match the run and generation")
    return run, target, unit


def validate_command(command: Any, run: dict[str, Any], expected_kind: str) -> None:
    if not isinstance(command, dict) or command.get("kind") != expected_kind:
        raise AdapterError(f"{expected_kind} envelope has no matching durable command")
    try:
        uuid.UUID(command.get("id", ""))
    except (ValueError, AttributeError, TypeError) as exc:
        raise AdapterError("hook command id is invalid") from exc
    if command.get("run_id") != run.get("id") or command.get("generation") != run.get("generation"):
        raise AdapterError("hook command does not match the run and generation")


def lane_fence(lane: dict[str, Any]) -> dict[str, Any]:
    fields = ("id", "project", "environment", "repository_id", "release_line_id", "profile_digest",
              "config_version", "allowed_components")
    return {field: lane.get(field) for field in fields}


def record_matches(record: Any, envelope: dict[str, Any], target: dict[str, Any], unit: str,
                   config_digest: str, environment_digest: str) -> bool:
    if not isinstance(record, dict) or not isinstance(record.get("envelope"), dict):
        return False
    recorded_envelope = record["envelope"]
    recorded_run = recorded_envelope.get("run", {})
    recorded_lane = recorded_envelope.get("lane", {})
    run = envelope.get("run", {})
    lane = envelope.get("lane", {})
    return (
        record.get("version") == 1
        and record.get("unit") == unit
        and record.get("run_id") == run.get("id")
        and record.get("generation") == run.get("generation")
        and recorded_run.get("id") == run.get("id")
        and recorded_run.get("generation") == run.get("generation")
        and recorded_run.get("lane_id") == run.get("lane_id")
        and record.get("target") == target
        and recorded_run.get("target") == target
        and lane_fence(recorded_lane) == lane_fence(lane)
        and record.get("configuration_digest") == config_digest
        and record.get("environment_file_digest") == environment_digest
    )


def unit_state(unit: str) -> dict[str, str]:
    command = ["systemctl", "--user", "show", "--no-pager", unit]
    command.extend("--property=" + field for field in SYSTEMD_SHOW_FIELDS)
    try:
        completed = subprocess.run(command, check=True, capture_output=True, text=True, timeout=8)
    except (OSError, subprocess.SubprocessError) as exc:
        raise AdapterError("systemd unit state is unavailable") from exc
    values: dict[str, str] = {}
    for line in completed.stdout.splitlines():
        key, separator, value = line.partition("=")
        if separator:
            values[key] = value
    return values


def cgroup_snapshot(control_group: str) -> dict[str, Any]:
    """Read two full recursive cgroup-v2 snapshots and require populated=0 throughout."""
    if not isinstance(control_group, str):
        raise AdapterError("systemd returned no valid cgroup path")
    try:
        root_info = CGROUP_ROOT.lstat()
        root = CGROUP_ROOT.resolve(strict=True)
    except OSError as exc:
        raise AdapterError("cgroup v2 root is unavailable") from exc
    controllers = root / "cgroup.controllers"
    if (stat.S_ISLNK(root_info.st_mode) or not stat.S_ISDIR(root_info.st_mode)
            or controllers.is_symlink() or not controllers.is_file()):
        raise AdapterError("cgroup filesystem is not v2")
    if control_group == "":
        # systemd may clear ControlGroup after a transient RemainAfterExit unit
        # completes. stopped_observation separately fences this with the loaded
        # unit identity, durable result, InvocationID, MainPID=0, and a second
        # matching systemd snapshot. A literal '/' is never treated as destroyed.
        return {"state": "destroyed", "populated": 0, "groups_scanned": 0,
                "double_scan_stable": True, "path": ""}
    if not control_group.startswith("/") or control_group == "/":
        raise AdapterError("systemd returned an invalid cgroup path")
    raw_parts = control_group.split("/")[1:]
    if not raw_parts or any(part in ("", ".", "..") for part in raw_parts):
        raise AdapterError("systemd returned an invalid cgroup path")

    def locate_group() -> Path | None:
        current = root
        for part in raw_parts:
            current = current / part
            try:
                info = current.lstat()
            except FileNotFoundError:
                return None
            except OSError as exc:
                raise AdapterError("systemd cgroup path cannot be inspected") from exc
            if stat.S_ISLNK(info.st_mode):
                raise AdapterError("systemd cgroup path contains a symlink")
            if not stat.S_ISDIR(info.st_mode):
                raise AdapterError("systemd cgroup path contains a non-directory")
        try:
            group = current.resolve(strict=True)
        except OSError as exc:
            raise AdapterError("systemd cgroup changed during inspection") from exc
        if not group.is_relative_to(root) or not group.is_dir():
            raise AdapterError("systemd cgroup path is outside cgroup v2")
        return group

    first_group = locate_group()
    second_group = locate_group()
    if first_group is None or second_group is None:
        if first_group is not None or second_group is not None:
            raise AdapterError("systemd cgroup path changed during destroyed-state inspection")
        return {"state": "destroyed", "populated": 0, "groups_scanned": 0,
                "double_scan_stable": True, "path": control_group}
    if first_group != second_group:
        raise AdapterError("systemd cgroup path changed during inspection")
    group = first_group

    def read_populated(directory: Path) -> str:
        events = directory / "cgroup.events"
        if events.is_symlink():
            raise AdapterError("cgroup event file is a symlink")
        try:
            values = [line.split()[1] for line in events.read_text().splitlines()
                      if line.split() and line.split()[0] == "populated" and len(line.split()) == 2]
        except OSError as exc:
            raise AdapterError("cgroup population state is unavailable") from exc
        if len(values) != 1 or values[0] not in ("0", "1"):
            raise AdapterError("cgroup population state is invalid")
        if values[0] != "0":
            raise AdapterError("unit cgroup still has processes")
        return values[0]

    def read_pids(directory: Path) -> tuple[str, ...]:
        path = directory / "cgroup.procs"
        if path.is_symlink():
            raise AdapterError("cgroup process list is a symlink")
        try:
            pids = tuple(path.read_text().split())
        except OSError as exc:
            raise AdapterError("cgroup process list is unavailable") from exc
        if any(not pid.isdecimal() or int(pid) <= 0 for pid in pids):
            raise AdapterError("cgroup process list is invalid")
        if pids:
            raise AdapterError("unit cgroup still has processes")
        return pids

    def children(directory: Path) -> tuple[str, ...]:
        names = []
        try:
            with os.scandir(directory) as entries:
                for entry in entries:
                    if entry.is_symlink():
                        raise AdapterError("cgroup subtree contains a symlink")
                    if entry.is_dir(follow_symlinks=False):
                        names.append(entry.name)
        except OSError as exc:
            raise AdapterError("cgroup subtree cannot be inspected") from exc
        return tuple(sorted(names))

    def scan() -> tuple[tuple[Any, ...], ...]:
        snapshot: list[tuple[Any, ...]] = []

        def visit(directory: Path) -> None:
            resolved = directory.resolve(strict=True)
            if directory.is_symlink() or not resolved.is_relative_to(root) or not resolved.is_dir():
                raise AdapterError("cgroup subtree changed during inspection")
            before_children = children(directory)
            pids = read_pids(directory)
            populated = read_populated(directory)
            relative = directory.relative_to(group).as_posix()
            snapshot.append((relative, pids, populated, before_children))
            for child in before_children:
                visit(directory / child)
            if read_pids(directory) != pids or read_populated(directory) != populated or children(directory) != before_children:
                raise AdapterError("cgroup subtree changed during inspection")

        visit(group)
        return tuple(sorted(snapshot))

    first_root = read_populated(group)
    first = scan()
    middle_root = read_populated(group)
    second = scan()
    final_root = read_populated(group)
    if first_root != "0" or middle_root != "0" or final_root != "0" or first != second:
        raise AdapterError("cgroup subtree did not remain empty across stable snapshots")
    return {"state": "empty", "populated": 0, "groups_scanned": len(first), "double_scan_stable": True}


def validate_recorded_result(result: Any, record: dict[str, Any], run: dict[str, Any], target: dict[str, Any],
                             unit: str, state: dict[str, str]) -> tuple[int, str]:
    if not isinstance(result, dict):
        raise AdapterError("durable task result is invalid")
    code = result.get("returncode")
    invocation_id = result.get("invocation_id")
    if isinstance(code, bool) or not isinstance(code, int) or code < 0 or code > 255:
        raise AdapterError("durable task result has no valid exit code")
    if (result.get("run_id") != run.get("id") or result.get("generation") != run.get("generation")
            or result.get("target") != target or result.get("unit") != unit
            or result.get("configuration_digest") != record.get("configuration_digest")
            or result.get("environment_file_digest") != record.get("environment_file_digest")):
        raise AdapterError("durable task result is not bound to this run, target, or configuration")
    if not isinstance(invocation_id, str) or not invocation_id or state.get("InvocationID") != invocation_id:
        raise AdapterError("systemd invocation does not match the durable task result")
    if state.get("Id") != unit or state.get("LoadState") != "loaded":
        raise AdapterError("systemd unit identity is not loaded")
    if state.get("MainPID") != "0" or state.get("ActiveState") not in ("active", "inactive", "failed"):
        raise AdapterError("systemd has not confirmed that the task main process stopped")
    if not state.get("ExecMainExitTimestamp") or state.get("ExecMainCode") not in ("1", "exited"):
        raise AdapterError("systemd has no stable normal process exit evidence")
    try:
        systemd_code = int(state.get("ExecMainStatus", ""))
    except ValueError as exc:
        raise AdapterError("systemd exit status is invalid") from exc
    if systemd_code != code:
        raise AdapterError("systemd and durable task exit statuses differ")
    finished = parse_timestamp(result.get("finished_at"))
    exit_stamp = state.get("ExecMainExitTimestamp", "")
    # InvocationID fences the systemd instance; the timestamp provides an extra stale-result check.
    try:
        exit_time = dt.datetime.fromisoformat(exit_stamp.replace("Z", "+00:00"))
    except ValueError:
        exit_time = None
    if exit_time and exit_time.tzinfo is None:
        exit_time = None
    if exit_time and abs((finished - exit_time.astimezone(dt.timezone.utc)).total_seconds()) > 10:
        raise AdapterError("durable task finish time does not match systemd")
    return code, invocation_id


def stopped_observation(envelope: dict[str, Any], config: dict[str, Any], target: dict[str, Any],
                        unit: str, request_path: Path, result_path: Path,
                        config_digest: str, environment_digest: str) -> tuple[dict[str, Any], dict[str, str], dict[str, Any], int, str]:
    record = load_json(request_path)
    if not record_matches(record, envelope, target, unit, config_digest, environment_digest):
        raise AdapterError("durable dispatch intent does not match the hook envelope")
    if record.get("configuration_digest") != config_digest:
        raise AdapterError("adapter configuration changed since dispatch")
    try:
        first_state = unit_state(unit)
    except AdapterError:
        raise
    if first_state.get("ActiveState") == "active" and first_state.get("SubState") in (
            "running", "start", "start-pre", "start-post"):
        raise AdapterError("task is still running")
    result = load_json(result_path)
    code, invocation_id = validate_recorded_result(result, record, envelope["run"], target, unit, first_state)
    process_check = cgroup_snapshot(first_state.get("ControlGroup", ""))
    second_state = unit_state(unit)
    if any(first_state.get(key) != second_state.get(key) for key in UNIT_FENCE_FIELDS):
        raise AdapterError("systemd unit identity or state changed during stop observation")
    # Re-read the atomic result after cgroup inspection so result replacement cannot race the proof.
    second_result = load_json(result_path)
    if second_result != result:
        raise AdapterError("durable task result changed during stop observation")
    if code == 0:
        if (first_state.get("ActiveState") != "active" or first_state.get("SubState") != "exited"
                or first_state.get("Result") != "success" or first_state.get("ExecMainStatus") != "0"):
            raise AdapterError("systemd has not confirmed successful task completion")
    else:
        if first_state.get("ActiveState") not in ("inactive", "failed"):
            raise AdapterError("systemd has not confirmed failed task completion")
        if first_state.get("Result") not in ("exit-code", "success"):
            raise AdapterError("systemd failure state is not a normal nonzero exit")
    return record, first_state, process_check, code, invocation_id


def failure_reply(base: dict[str, Any], envelope: dict[str, Any], target: dict[str, Any], unit: str,
                  record: dict[str, Any], state: dict[str, str], process_check: dict[str, Any],
                  code: int, invocation_id: str) -> dict[str, Any]:
    evidence = {
        "run_id": envelope["run"]["id"],
        "generation": envelope["run"]["generation"],
        "target": target,
        "configuration_digest": record["configuration_digest"],
        "unit": unit,
        "result": {
            "returncode": code,
            "run_id": envelope["run"]["id"],
            "generation": envelope["run"]["generation"],
            "target": target,
            "unit": unit,
            "invocation_id": invocation_id,
            "finished_at": record.get("result_finished_at"),
        },
        "supervisor": {key: state.get(key, "") for key in UNIT_FENCE_FIELDS if key != "ExecStart"},
        "cgroup": process_check,
    }
    raw = canonical_json(evidence)
    if not evidence or len(raw) > FAILURE_EVIDENCE_MAX_BYTES:
        raise AdapterError("fresh stop proof exceeds the protocol evidence limit")
    return {**base, "status": "failed", "environment_known_unchanged": False,
            "failure_proof": {"observed_at": timestamp(), "evidence": evidence},
            "message": "Task failed and systemd plus cgroup v2 confirmed it stopped; inspect the private deployment log"}


def systemd_run(config_path: Path, request_path: Path, unit: str,
                forward_env: dict[str, str] | None = None) -> None:
    command = [
        "systemd-run", "--user", "--quiet", "--no-block", "--unit", unit,
        "--property=Type=exec", "--property=RemainAfterExit=yes", "--property=Restart=no",
        "--property=KillMode=control-group", "--property=TimeoutStopSec=infinity",
        "--property=SendSIGKILL=no", "--property=UMask=0077",
        "--property=StandardOutput=null", "--property=StandardError=null",
    ]
    if forward_env:
        for key, value in forward_env.items():
            command.append(f"--setenv={key}={value}")
    command.extend([
        sys.executable, str(Path(__file__).resolve()), "--config", str(config_path.resolve()),
        "--worker", str(request_path.resolve()),
    ])
    try:
        subprocess.run(command, check=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=12)
    except (OSError, subprocess.SubprocessError) as exc:
        raise AdapterError("systemd handoff response is uncertain") from exc


def dispatch(envelope: dict[str, Any], config: dict[str, Any], config_path: Path,
             run: dict[str, Any], target: dict[str, Any], unit: str,
             config_digest: str, environment_digest: str) -> dict[str, Any]:
    run_dir, request_path, result_path, lock_path = run_paths(config, run["id"], run["generation"])
    base = {"version": 1, "run_id": run["id"], "generation": run["generation"], "executor_ref": unit}
    with lock_path.open("a+b") as lock:
        os.chmod(lock_path, 0o600)
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        fresh = not request_path.exists()
        if fresh:
            record = {
                "version": 1,
                "run_id": run["id"],
                "generation": run["generation"],
                "target": target,
                "unit": unit,
                "configuration_digest": config_digest,
                "environment_file_digest": environment_digest,
                "created_at": timestamp(),
                "envelope": envelope,
            }
            # This is the irrevocable dispatch intent. A failure after this write is never replayed.
            atomic_json(request_path, record)
        else:
            record = load_json(request_path)
            if not record_matches(record, envelope, target, unit, config_digest, environment_digest):
                raise AdapterError("a conflicting durable dispatch intent already exists")

        try:
            state = unit_state(unit)
        except AdapterError:
            if not fresh:
                return {**base, "status": "unknown", "message": "Dispatch intent exists but its systemd unit cannot be observed; no replay was attempted"}
            # The intent is durable, so inability to query systemd is an uncertain handoff.
            return {**base, "status": "unknown", "message": "Dispatch intent is durable but systemd handoff state is uncertain"}
        if fresh:
            if state.get("LoadState") != "not-found":
                return {**base, "status": "unknown", "message": "A unit already exists for this new dispatch intent; no command was sent"}
            try:
                prefixes = config.get("forward_env_prefixes", [])
                forwarded = collect_forwarded_environment(prefixes) if prefixes else None
                systemd_run(config_path, request_path, unit, forwarded)
            except AdapterError:
                return {**base, "status": "unknown", "message": "Dispatch intent is durable but systemd handoff response is uncertain; no replay will occur"}
            return {**base, "status": "started", "message": "Task was handed to an independent systemd user unit"}
        if state.get("LoadState") != "loaded" or state.get("Id") != unit:
            return {**base, "status": "unknown", "message": "Previous dispatch intent has no observable systemd task; no replay was attempted"}
        if state.get("ActiveState") == "active" and state.get("SubState") in (
                "running", "start", "start-pre", "start-post"):
            return {**base, "status": "running", "message": "The existing systemd task is still running"}
        return {**base, "status": "started", "message": "The existing systemd task is retained; dispatch was not repeated"}


def child_environment(config: dict[str, Any], environment_digest: str,
                      envelope: dict[str, Any]) -> dict[str, str]:
    env_values, current_digest = read_environment_file(
        Path(config["environment_file"]) if config.get("environment_file") else None)
    if current_digest != environment_digest:
        raise AdapterError("dedicated environment file changed after dispatch")
    run = envelope["run"]
    target = run["target"]
    try:
        home = pwd.getpwuid(os.getuid()).pw_dir
    except KeyError:
        home = "/"
    env = {"PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
           "HOME": home, "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8"}
    env.update(env_values)
    prefixes = config.get("forward_env_prefixes", [])
    if prefixes:
        env.update(collect_forwarded_environment(prefixes))
    env.update({
        "SHIPBRAID_RUN_ID": str(run["id"]),
        "SHIPBRAID_GENERATION": str(run["generation"]),
        "SHIPBRAID_LANE_ID": str(run["lane_id"]),
        "SHIPBRAID_PROJECT": str(envelope["lane"]["project"]),
        "SHIPBRAID_ENVIRONMENT": str(envelope["lane"]["environment"]),
        "SHIPBRAID_REPOSITORY_ID": str(target["repository_id"]),
        "SHIPBRAID_TARGET_REVISION": str(target["revision"]),
        "SHIPBRAID_PROFILE_DIGEST": str(target["profile_digest"]),
        "SHIPBRAID_COMPONENTS": json.dumps(target["components"], separators=(",", ":")),
    })
    return env


def read_invocation_id(unit: str) -> str:
    invocation_id = os.environ.get("INVOCATION_ID", "")
    state = unit_state(unit)
    if (not invocation_id or state.get("Id") != unit or state.get("LoadState") != "loaded"
            or state.get("InvocationID") != invocation_id or state.get("ActiveState") != "active"):
        raise AdapterError("worker cannot establish its systemd invocation identity")
    return invocation_id


def run_deploy(argv: list[str], cwd: str, env: dict[str, str], payload: bytes,
               log_path: Path) -> int:
    """Run deploy without a timeout while draining output into a capped private log."""
    log_fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL |
                     getattr(os, "O_NOFOLLOW", 0), 0o600)
    try:
        process = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                   close_fds=True, bufsize=0)
    except OSError:
        os.close(log_fd)
        raise
    assert process.stdout is not None
    assert process.stdin is not None
    logged = 0
    log_writable = True

    def drain_output() -> None:
        nonlocal logged, log_writable
        try:
            while True:
                chunk = process.stdout.read(64 * 1024)
                if not chunk:
                    break
                remaining = DEPLOY_LOG_MAX_BYTES - logged
                if not log_writable or remaining <= 0:
                    continue
                view = memoryview(chunk)[:remaining]
                while view:
                    try:
                        written = os.write(log_fd, view)
                    except OSError:
                        # A full/unavailable log must not back-pressure the deployer.
                        log_writable = False
                        break
                    if written <= 0:
                        log_writable = False
                        break
                    logged += written
                    view = view[written:]
        except OSError:
            # Keep the child output pipe drained if the log stream itself fails.
            log_writable = False
            try:
                while process.stdout.read(64 * 1024):
                    pass
            except OSError:
                pass
        finally:
            process.stdout.close()

    reader = threading.Thread(target=drain_output, name="shipbraid-deploy-log", daemon=True)
    reader.start()
    input_error = False
    try:
        try:
            remaining_input = memoryview(payload)
            while remaining_input:
                written = process.stdin.write(remaining_input)
                if written is None or written <= 0:
                    input_error = True
                    break
                remaining_input = remaining_input[written:]
            if not input_error:
                process.stdin.flush()
        except (BrokenPipeError, OSError):
            input_error = True
        finally:
            try:
                process.stdin.close()
            except OSError:
                input_error = True
        returncode = process.wait()
        reader.join()
    finally:
        if not process.stdin.closed:
            try:
                process.stdin.close()
            except OSError:
                pass
        os.close(log_fd)
    if input_error and returncode == 0:
        return 74
    return returncode if 0 <= returncode <= 255 else 1


def worker(request_path: Path, config_path: Path) -> int:
    record = load_json(request_path)
    if not isinstance(record, dict) or not isinstance(record.get("envelope"), dict):
        return 78
    envelope = record["envelope"]
    try:
        config = load_config(config_path)
        run, target, unit = validate_envelope(envelope, config)
        _, environment_digest = read_environment_file(
            Path(config["environment_file"]) if config.get("environment_file") else None)
        config_digest = configuration_digest(config, environment_digest)
        if (record.get("unit") != unit or record.get("target") != target
                or record.get("configuration_digest") != config_digest
                or record.get("environment_file_digest") != environment_digest):
            raise AdapterError("frozen dispatch configuration changed before worker start")
        invocation_id = read_invocation_id(unit)
        env = child_environment(config, environment_digest, envelope)
    except Exception:
        # A durable, normal failed result can still yield a stop proof if unit identity is known.
        try:
            unit = record.get("unit")
            run = envelope.get("run", {})
            target = record.get("target")
            invocation_id = read_invocation_id(unit) if isinstance(unit, str) else ""
            if invocation_id:
                atomic_json(request_path.parent / "result.json", {
                    "run_id": run.get("id"), "generation": run.get("generation"), "target": target,
                    "unit": unit, "configuration_digest": record.get("configuration_digest"),
                    "environment_file_digest": record.get("environment_file_digest"),
                    "invocation_id": invocation_id, "returncode": 78, "finished_at": timestamp(),
                })
        except Exception:
            pass
        return 78

    payload = canonical_json(envelope)
    try:
        code = run_deploy(config["deploy"], config["cwd"], env, payload,
                          request_path.parent / "deploy.log")
    except OSError:
        code = 127
    try:
        atomic_json(request_path.parent / "result.json", {
            "run_id": run["id"], "generation": run["generation"], "target": target,
            "unit": unit, "configuration_digest": config_digest,
            "environment_file_digest": environment_digest,
            "invocation_id": invocation_id, "returncode": code, "finished_at": timestamp(),
        })
    except Exception:
        return 74
    return code


def run_verify(config: dict[str, Any], envelope: dict[str, Any], environment_digest: str,
               call_started: dt.datetime, run_dir: Path) -> dict[str, Any]:
    env = child_environment(config, environment_digest, envelope)
    lock_path = run_dir / "verify.lock"
    lock_fd = os.open(lock_path, os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0), 0o600)
    input_handle = None
    try:
        lock_info = os.fstat(lock_fd)
        if (not stat.S_ISREG(lock_info.st_mode) or lock_info.st_uid != os.getuid()
                or lock_info.st_mode & 0o077):
            raise AdapterError("verification lock is not a private user-owned regular file")
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise AdapterError("a previous read-only verification is still running") from exc

        raw_input = canonical_json(envelope)
        if len(raw_input) > HOOK_MAX_BYTES:
            raise AdapterError("verification envelope exceeds the hook request size limit")
        input_handle = tempfile.TemporaryFile(mode="w+b")
        input_handle.write(raw_input)
        input_handle.flush()
        input_handle.seek(0)
        try:
            process = subprocess.Popen(config["verify"], cwd=config["cwd"], env=env,
                                       stdin=input_handle, stdout=subprocess.PIPE,
                                       stderr=subprocess.DEVNULL, close_fds=True,
                                       pass_fds=(lock_fd,))
        except OSError as exc:
            raise AdapterError("read-only verification script could not start") from exc
        input_handle.close()
        input_handle = None

        captured = bytearray()
        overflow = threading.Event()

        def drain_stdout() -> None:
            assert process.stdout is not None
            try:
                while True:
                    chunk = process.stdout.read(64 * 1024)
                    if not chunk:
                        break
                    room = HOOK_MAX_REPLY_BYTES - len(captured)
                    if room > 0:
                        captured.extend(chunk[:room])
                    if len(chunk) > room:
                        overflow.set()
            except OSError:
                overflow.set()
            finally:
                process.stdout.close()

        reader = threading.Thread(target=drain_stdout, name="shipbraid-verify-output", daemon=True)
        reader.start()
        try:
            code = process.wait(timeout=config["verify_timeout_seconds"])
        except subprocess.TimeoutExpired as exc:
            # The verifier keeps this flock open if the coordinator is killed or times out.
            threading.Thread(target=process.wait, name="shipbraid-verify-reaper", daemon=True).start()
            raise AdapterError("read-only verification timed out; no receipt was created") from exc
        reader.join(timeout=1)
        if reader.is_alive():
            raise AdapterError("read-only verification left stdout open after exit")
        if overflow.is_set():
            raise AdapterError("read-only verification output exceeded its size limit")
        if code != 0:
            raise AdapterError("read-only verification returned a nonzero status")
        raw = bytes(captured)
        if not raw:
            raise AdapterError("read-only verification returned no evidence")
    except AdapterError:
        raise
    except OSError as exc:
        raise AdapterError("read-only verification input or lock is unavailable") from exc
    finally:
        if input_handle is not None:
            input_handle.close()
        # After a timeout the verifier inherited the same open file description and
        # retains this lock even after this coordinator-side descriptor is closed.
        os.close(lock_fd)
    try:
        proof = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise AdapterError("read-only verification response is invalid JSON") from exc
    if not isinstance(proof, dict):
        raise AdapterError("read-only verification evidence must be an object")
    target = envelope["run"]["target"]
    if proof.get("healthy") is not True:
        raise AdapterError("read-only verification did not report a healthy target")
    if proof.get("source_revision") != target["revision"] or proof.get("profile_digest") != target["profile_digest"]:
        raise AdapterError("read-only verification observed a different revision or profile")
    digests = proof.get("component_digests")
    if not isinstance(digests, dict) or set(digests) != set(target["components"]):
        raise AdapterError("read-only verification did not observe every requested component")
    if any(not isinstance(value, str) or not value or len(value) > 256 for value in digests.values()):
        raise AdapterError("read-only verification returned an invalid component digest")
    report = proof.get("verification_report")
    if not isinstance(report, str) or not report.strip() or len(report.encode("utf-8")) > 64 * 1024:
        raise AdapterError("read-only verification report is missing or too large")
    observed = parse_timestamp(proof.get("observed_at"))
    if observed < call_started or observed > utc_now():
        raise AdapterError("read-only verification observation is outside this call window")
    return {"observed_at": timestamp(observed), "source_revision": proof["source_revision"],
            "component_digests": digests, "profile_digest": proof["profile_digest"],
            "verification_report": report}


def unknown_reply(base: dict[str, Any], message: str) -> dict[str, Any]:
    return {**base, "status": "unknown", "message": message}


def handle_hook(envelope: Any, config_path: Path, call_started: dt.datetime | None = None) -> dict[str, Any]:
    call_started = call_started or utc_now()
    run = envelope.get("run", {}) if isinstance(envelope, dict) else {}
    run_id = run.get("id") if isinstance(run, dict) else None
    generation = run.get("generation") if isinstance(run, dict) else None
    base: dict[str, Any] = {"version": 1, "run_id": run_id, "generation": generation}
    try:
        config = load_config(config_path)
        _, environment_digest = read_environment_file(
            Path(config["environment_file"]) if config.get("environment_file") else None)
        config_digest = configuration_digest(config, environment_digest)
        parsed_run, target, unit = validate_envelope(envelope, config)
        base = {**base, "run_id": parsed_run["id"], "generation": parsed_run["generation"], "executor_ref": unit}
        operation = envelope["operation"]
        run_dir, request_path, result_path, _ = run_paths(config, parsed_run["id"], parsed_run["generation"])
        if operation == "dispatch":
            return dispatch(envelope, config, config_path, parsed_run, target, unit,
                            config_digest, environment_digest)
        if not request_path.exists():
            return unknown_reply(base, "No durable dispatch intent exists for this run")
        record = load_json(request_path)
        if not record_matches(record, envelope, target, unit, config_digest, environment_digest):
            return unknown_reply(base, "Durable dispatch identity or configuration does not match this run")
        try:
            stopped = stopped_observation(envelope, config, target, unit, request_path, result_path,
                                          config_digest, environment_digest)
        except AdapterError as exc:
            # An active unit is a useful running observation; other missing evidence stays unknown.
            try:
                state = unit_state(unit)
            except AdapterError:
                state = {}
            if state.get("Id") == unit and state.get("LoadState") == "loaded" and state.get("ActiveState") == "active" and state.get("SubState") in (
                    "running", "start", "start-pre", "start-post"):
                return {**base, "status": "running", "message": "The independent systemd task is still running"}
            return unknown_reply(base, "The adapter cannot prove a stable stopped task and matching durable result")
        record, state, process_check, code, invocation_id = stopped
        if code != 0:
            # failure_reply uses the durable finish timestamp from result; retain it in memory only.
            durable_result = load_json(result_path)
            record["result_finished_at"] = durable_result.get("finished_at")
            return failure_reply(base, envelope, target, unit, record, state, process_check, code, invocation_id)
        if operation == "observe":
            return {**base, "status": "completed", "message": "Task stopped successfully; independent environment verification is required"}
        try:
            proof = run_verify(config, envelope, environment_digest, call_started, run_dir)
            # Fence verification with a second full stop observation after the user script returns.
            after = stopped_observation(envelope, config, target, unit, request_path, result_path,
                                        config_digest, environment_digest)
            if (after[3] != 0 or any(state.get(key) != after[1].get(key) for key in UNIT_FENCE_FIELDS)
                    or after[0].get("target") != target):
                raise AdapterError("stopped task identity changed during verification")
            received_at = utc_now()
            observed_at = parse_timestamp(proof["observed_at"])
            if observed_at < call_started or observed_at > received_at:
                raise AdapterError("verification receipt is outside this call window")
            receipt = {"id": str(uuid.uuid4()), "run_id": parsed_run["id"],
                       "generation": parsed_run["generation"], "observed_at": timestamp(observed_at),
                       "source_revision": proof["source_revision"],
                       "component_digests": proof["component_digests"],
                       "profile_digest": proof["profile_digest"],
                       "verification_report": proof["verification_report"], "result": "success"}
            return {**base, "status": "verified", "receipt": receipt,
                    "message": "A fresh read-only verification matched the frozen target"}
        except Exception:
            return unknown_reply(base, "Read-only verification did not establish fresh evidence for the frozen target")
    except Exception:
        if "executor_ref" not in base and isinstance(run_id, str):
            try:
                base["executor_ref"] = deterministic_unit(run_id, generation)
            except Exception:
                pass
        return unknown_reply(base, "The local adapter could not establish the required task evidence")


class HookHandler(http.server.BaseHTTPRequestHandler):
    server_version = "shipbraid-local/1"
    sys_version = ""

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
        if self.path != "/hook":
            self.send_error(404)
            return
        token_env = self.server.token_env  # type: ignore[attr-defined]
        expected = os.environ.get(token_env, "").encode("utf-8")
        supplied = self.headers.get("Authorization", "")
        prefix = "Bearer "
        actual = supplied[len(prefix):].encode("utf-8") if supplied.startswith(prefix) else b""
        if not expected or not hmac.compare_digest(expected, actual):
            self.send_error(401)
            return
        try:
            length = int(self.headers.get("Content-Length", "-1"))
        except ValueError:
            self.send_error(400)
            return
        if length < 0 or length > HOOK_MAX_BYTES:
            self.send_error(413)
            return
        raw = self.rfile.read(length)
        if len(raw) != length:
            self.send_error(400)
            return
        try:
            envelope = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            self.send_error(400)
            return
        reply = handle_hook(envelope, self.server.config_path)  # type: ignore[attr-defined]
        body = canonical_json(reply)
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format: str, *_args: Any) -> None:
        # Do not log bearer credentials or request bodies.
        return


class HookServer(http.server.ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = False

    def __init__(self, address: tuple[str, int], config_path: Path, token_env: str):
        super().__init__(address, HookHandler)
        self.config_path = config_path
        self.token_env = token_env


def parse_listen(value: str) -> tuple[str, int]:
    host, separator, port_text = value.rpartition(":")
    if not separator or host != "127.0.0.1":
        raise AdapterError("HTTP hook listener must bind to 127.0.0.1")
    try:
        port = int(port_text)
    except ValueError as exc:
        raise AdapterError("HTTP hook port is invalid") from exc
    if not 1 <= port <= 65535:
        raise AdapterError("HTTP hook port is invalid")
    return host, port


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path, help="absolute adapter JSON configuration")
    parser.add_argument("--worker", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--listen", help="optional loopback HTTP endpoint, e.g. 127.0.0.1:9788")
    parser.add_argument("--token-env", help="environment variable containing the HTTP bearer token")
    args = parser.parse_args(argv)
    try:
        if args.config.is_absolute() is False:
            raise AdapterError("--config must be an absolute path")
        if args.worker is not None:
            if args.listen or args.token_env:
                raise AdapterError("internal worker cannot serve HTTP")
            return worker(args.worker, args.config)
        if bool(args.listen) != bool(args.token_env):
            raise AdapterError("--listen and --token-env must be specified together")
        if args.listen:
            config = load_config(args.config)
            token_env = args.token_env
            if not isinstance(token_env, str) or not ENV_NAME.fullmatch(token_env) or not os.environ.get(token_env):
                raise AdapterError("HTTP bearer token environment variable is missing")
            server = HookServer(parse_listen(args.listen), args.config, token_env)
            try:
                server.serve_forever(poll_interval=0.5)
            finally:
                server.server_close()
            return 0
        raw = sys.stdin.buffer.read(HOOK_MAX_BYTES + 1)
        if len(raw) > HOOK_MAX_BYTES:
            raise AdapterError("hook envelope exceeds 256 KiB")
        envelope = json.loads(raw)
        reply = handle_hook(envelope, args.config)
        output = canonical_json(reply)
        if len(output) > HOOK_MAX_REPLY_BYTES:
            reply = unknown_reply({"version": 1, "run_id": None, "generation": None},
                                 "Adapter response exceeded the hook protocol size limit")
            output = canonical_json(reply)
        sys.stdout.buffer.write(output + b"\n")
        return 0
    except (AdapterError, OSError, json.JSONDecodeError) as exc:
        print(f"local adapter: {type(exc).__name__}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
