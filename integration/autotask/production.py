#!/usr/bin/env python3
"""Production release hook for the reviewed AutoTask release pipeline.

The coordinator sends a versioned JSON envelope on stdin. Dispatch first
records that envelope and then starts one durable systemd user unit. The unit
uses the administrator-pinned release tooling, while release.sh checks out the
requested source SHA into its own clean worktree. Observe and verify only read
the live cluster, worker supervisor, and website; they never deploy.
"""
import argparse
import datetime
import fcntl
import hashlib
import http.client
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import tarfile
import uuid

try:
    import bridge
except ImportError:  # Imported by tests or an installed package.
    _bridge_spec = importlib.util.spec_from_file_location(
        'autotask_bridge', Path(__file__).with_name('bridge.py'))
    if _bridge_spec is None or _bridge_spec.loader is None:
        raise
    bridge = importlib.util.module_from_spec(_bridge_spec)
    _bridge_spec.loader.exec_module(bridge)


REPOSITORY_DEFAULT = Path('/home/yahui/repos/autotask')
MASTER_HOST = 'root@172.168.1.231'
WORKER_HOST = 'liuyahui@172.168.1.166'
WORKER_UNIT = 'autotask-worker-prod.service'
JOB_UNIT_PREFIX = 'shipbraid-autotask-prod-'
APP_DEPLOYMENT = 'autotask'
WEBSITE_DEPLOYMENT = 'autotask-website'
APP_CONTAINER = 'autotask'
WEBSITE_CONTAINER = 'website'
APP_IMAGE_REPOSITORY = 'harbor.hive-intel.com/prod/autotask'
WEBSITE_IMAGE_REPOSITORY = 'harbor.hive-intel.com/prod/autotask-website'
EXPECTED_CONTEXT = 'kubernetes-admin@kubernetes'
EXPECTED_API_SERVER = 'https://lb.kubesphere.local:6443'
MAX_RESULT_BYTES = 64 * 1024
SHA_RE = re.compile(r'^[0-9a-f]{40}$')
IMAGE_DIGEST_RE = re.compile(r'^sha256:[0-9a-f]{64}$')
K8S_NAME_RE = re.compile(r'^[a-z0-9](?:[-a-z0-9.]*[a-z0-9])?$')
_ACTIVE_TOOL_PATH = None

# Executed by release.sh or its direct helpers. The source SHA may change
# application code, but it must not replace the reviewed production pipeline.
PIPELINE_FILES = (
    'scripts/release.sh',
    'scripts/resolve-prod-static-asset-mode.py',
    'scripts/verify-dist-inventory.py',
    'scripts/build-base-image.sh',
    'scripts/verify-prod-image-labels.py',
    'scripts/deploy-prod-k8s.sh',
    'scripts/prod-deploy-source.sh',
    'scripts/extract-prod-app-deployment.py',
    'scripts/preserve-prod-auto-delegation-off.py',
    'scripts/publish-prod-static-assets.sh',
    'scripts/build-prod-deployment-jsonpatch.py',
    'scripts/prove-prod-retained-assets.sh',
    'scripts/deploy-website.sh',
    'scripts/check-plugin-public-release.mjs',
    'deploy/k8s/prod/40-app.yaml',
    'deploy/k8s/prod/50-website.yaml',
)


REMOTE_WORKER_PROBE = r'''import hashlib, json, os, re, subprocess
unit = "autotask-worker-prod.service"
props = ("Id", "InvocationID", "ActiveState", "SubState", "MainPID", "ExecStart")
def systemd():
    raw = subprocess.check_output(["sudo", "-n", "systemctl", "show", unit,
        *[item for key in props for item in ("-p", key)]], text=True,
        stderr=subprocess.DEVNULL, timeout=10)
    return dict(line.split("=", 1) for line in raw.splitlines() if "=" in line)
def identity(state):
    if (state.get("Id") != unit or state.get("ActiveState") != "active"
            or state.get("SubState") != "running" or not state.get("InvocationID")):
        raise RuntimeError("worker supervisor is not actively running")
    pid_text = state.get("MainPID", "")
    if not re.fullmatch(r"[1-9][0-9]*", pid_text):
        raise RuntimeError("worker supervisor has no main process")
    pid = int(pid_text)
    exec_start = state.get("ExecStart", "")
    match = re.search(r"(?:^|[;{]\s*)path=([^ ;]+)", exec_start)
    if not match:
        raise RuntimeError("worker supervisor executable is unavailable")
    expected_path = match.group(1)
    proc = "/proc/" + str(pid)
    executable = os.readlink(proc + "/exe")
    if executable != expected_path:
        raise RuntimeError("worker process executable differs from its unit")
    stat = open(proc + "/stat", encoding="ascii").read()
    start_ticks = stat[stat.rfind(")") + 2:].split()[19]
    digest = hashlib.sha256()
    with open(proc + "/exe", "rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return {"invocation_id": state["InvocationID"], "main_pid": pid,
        "start_ticks": start_ticks, "executable_sha256": digest.hexdigest()}
first = systemd()
result = identity(first)
second = systemd()
again = identity(second)
if (result != again or any(first.get(key) != second.get(key)
        for key in ("Id", "InvocationID", "ActiveState", "SubState", "MainPID", "ExecStart"))):
    raise RuntimeError("worker supervisor changed during inspection")
print(json.dumps(result, sort_keys=True, separators=(",", ":")))
'''


def _run(argv, *, input_bytes=None, env=None, timeout=30, cwd=None):
    """Run a fixed administrative command without echoing output or arguments."""
    if env is None:
        env = _runtime_environment()
    try:
        completed = subprocess.run(
            argv, input=input_bytes, capture_output=True, env=env, cwd=cwd,
            timeout=timeout, check=False)
    except (OSError, subprocess.SubprocessError) as error:
        raise RuntimeError('a required production command could not be completed') from error
    if completed.returncode != 0:
        raise RuntimeError('a required production command returned an error')
    return completed.stdout


def _validate_tool_path(value):
    if not isinstance(value, str) or not value or any(char in value for char in '\x00\r\n'):
        raise RuntimeError('administrator-pinned production tool path is invalid')
    entries = value.split(':')
    if any(not entry for entry in entries):
        raise RuntimeError('administrator-pinned production tool path is invalid')
    canonical = []
    for entry in entries:
        path = Path(entry)
        if not path.is_absolute() or '..' in path.parts:
            raise RuntimeError('administrator-pinned production tool path must use absolute directories')
        try:
            resolved = path.resolve(strict=True)
        except OSError as error:
            raise RuntimeError('administrator-pinned production tool directory is unavailable') from error
        if not resolved.is_dir():
            raise RuntimeError('administrator-pinned production tool path contains a non-directory')
        canonical.append(str(resolved))
    return ':'.join(canonical)


def _activate_tool_path(value):
    """Install the administrator-owned PATH for this short-lived hook process."""
    global _ACTIVE_TOOL_PATH
    _ACTIVE_TOOL_PATH = _validate_tool_path(value)
    # bridge.unit_state inherits the process environment. Replacing PATH here
    # keeps its systemctl lookup on the same pinned toolchain as our subprocesses.
    os.environ['PATH'] = _ACTIVE_TOOL_PATH
    return _ACTIVE_TOOL_PATH


def _run_text(argv, *, input_bytes=None, env=None, timeout=30, cwd=None):
    return _run(argv, input_bytes=input_bytes, env=env, timeout=timeout, cwd=cwd).decode(
        'utf-8', errors='strict').strip()


def _git(repository, *args):
    return _run_text(['git', '-C', str(repository), *args], timeout=60)


def _sha256_bytes(value):
    return hashlib.sha256(value).hexdigest()


def _canonical_digest(value):
    raw = json.dumps(value, sort_keys=True, separators=(',', ':')).encode()
    return _sha256_bytes(raw)


def _validate_full_sha(value):
    if not isinstance(value, str) or not SHA_RE.fullmatch(value):
        raise RuntimeError('production release requires a full lowercase commit SHA')
    return value


def _database_backup_policy(target):
    parameters = target.get('parameters') if isinstance(target, dict) else None
    if (not isinstance(parameters, dict)
            or set(parameters) != {'database_backup'}
            or parameters.get('database_backup') not in ('include', 'skip')):
        raise RuntimeError('production target must explicitly choose database_backup include or skip')
    return parameters['database_backup']


def _database_backup_flag(target):
    policy = _database_backup_policy(target)
    return ['--include-db-backup'] if policy == 'include' else ['--skip-db-backup']


def _unit_name(identifier, generation):
    return f'{JOB_UNIT_PREFIX}{identifier}-{generation}.service'


def context(envelope, args):
    if not isinstance(envelope, dict) or envelope.get('version') != 1:
        raise RuntimeError('unsupported production hook protocol')
    run, lane = envelope.get('run'), envelope.get('lane')
    if not isinstance(run, dict) or not isinstance(lane, dict):
        raise RuntimeError('production hook envelope is incomplete')
    identifier = str(uuid.UUID(str(run.get('id'))))
    generation = run.get('generation')
    if isinstance(generation, bool) or not isinstance(generation, int) or generation < 1:
        raise RuntimeError('production run generation is invalid')
    if (lane.get('id') != 'autotask-prod' or lane.get('project') != 'autotask'
            or lane.get('environment') != 'prod' or run.get('lane_id') != lane.get('id')):
        raise RuntimeError('production hook is restricted to the configured AutoTask production lane')
    target = run.get('target')
    if not isinstance(target, dict):
        raise RuntimeError('production target is missing')
    _validate_full_sha(target.get('revision'))
    _database_backup_policy(target)
    if target.get('components') != ['app', 'website', 'worker']:
        raise RuntimeError('production target must contain app, website, and worker in canonical order')
    if not isinstance(target.get('profile_digest'), str) or not target['profile_digest']:
        raise RuntimeError('production executor profile fingerprint is missing')
    lane_profile = lane.get('profile_digest')
    if lane_profile is not None and lane_profile != target['profile_digest']:
        raise RuntimeError('production executor profile fingerprint changed')
    executor_ref = run.get('executor_ref')
    unit = _unit_name(identifier, generation)
    if executor_ref not in (None, unit):
        raise RuntimeError('production supervisor reference does not match the durable run fence')
    repository = Path(args.repository).resolve()
    state_dir = Path(args.state_dir).resolve()
    directory = state_dir / identifier
    return run, target, directory, unit, repository


def _read_json(path):
    raw = path.read_bytes()
    if len(raw) > MAX_RESULT_BYTES:
        raise RuntimeError('durable production state exceeds the size limit')
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise RuntimeError('durable production state is malformed')
    return value


def _safe_log_entries(directory):
    # The shared bridge reader removes terminal controls and bearer credentials.
    entries = bridge.logs(directory / 'deploy.log')
    filtered = []
    for entry in entries:
        message = entry.get('message', '')
        if not isinstance(message, str):
            continue
        if re.search(r'(?i)(?:password|token|secret|authorization|envfrom|environment variables)', message):
            continue
        if message.lstrip().startswith(('{', '[')) and len(message) > 160:
            continue
        filtered.append({'sequence': int(entry.get('sequence', 0)),
            'level': entry.get('level', 'info'), 'message': message[:1000]})
    return filtered[-30:]


def _validate_recorded(envelope, recorded):
    run, lane = envelope['run'], envelope['lane']
    if (recorded.get('run', {}).get('id') != run.get('id')
            or recorded.get('run', {}).get('generation') != run.get('generation')
            or recorded.get('run', {}).get('target') != run.get('target')
            or recorded.get('lane', {}).get('id') != lane.get('id')
            or recorded.get('lane', {}).get('project') != lane.get('project')
            or recorded.get('lane', {}).get('environment') != lane.get('environment')
            or recorded.get('lane', {}).get('profile_digest') != lane.get('profile_digest')):
        raise RuntimeError('durable production dispatch does not match the active run and profile')


def _production_failure_proof(recorded, result, run, target, unit, state, request_path):
    # A worker killed after writing its durable running guard has no final
    # result to report. Accept only that exact guard form and bind the stop
    # proof to the terminal status of the same systemd invocation.
    if result.get('phase') == 'running':
        if result.get('returncode') != 255:
            raise RuntimeError('durable running guard has an unexpected exit status')
        invocation_id = result.get('invocation_id')
        if (result.get('run_id') != run.get('id')
                or result.get('generation') != run.get('generation')
                or result.get('target') != target or result.get('unit') != unit
                or not isinstance(invocation_id, str) or not invocation_id
                or state.get('Id') != unit or state.get('InvocationID') != invocation_id):
            raise RuntimeError('durable running guard does not match the terminal supervisor')
        try:
            terminal_status = int(state.get('ExecMainStatus', ''))
        except (TypeError, ValueError) as error:
            raise RuntimeError('systemd has no valid terminal exit status') from error
        if terminal_status <= 0:
            raise RuntimeError('systemd has no nonzero terminal exit status')
        proof_result = dict(result)
        proof_result['returncode'] = terminal_status
        proof = bridge.failure_proof(
            recorded, proof_result, run, target, unit, state, request_path)
        evidence = proof.get('evidence')
        if not isinstance(evidence, dict):
            raise RuntimeError('systemd stop proof has no structured evidence')
        evidence['terminal_status_source'] = 'systemd'
        evidence['durable_running_guard'] = result
        evidence['durable_running_guard_sha256'] = _canonical_digest(result)
        if len(json.dumps(evidence, sort_keys=True, separators=(',', ':')).encode()) > 8192:
            raise RuntimeError('systemd stop proof exceeds the protocol size limit')
        return proof
    return bridge.failure_proof(recorded, result, run, target, unit, state, request_path)


def hook(envelope, args):
    tool_path = _activate_tool_path(args.tool_path)
    run, target, directory, unit, repository = context(envelope, args)
    operation = envelope.get('operation')
    if operation not in ('dispatch', 'observe', 'verify'):
        raise RuntimeError('unsupported production hook operation')
    base = {'version': 1, 'run_id': run['id'], 'generation': run['generation'],
        'executor_ref': unit}
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        directory.chmod(0o700)
    except OSError:
        pass
    request_path = directory / 'request.json'
    result_path = directory / 'result.json'
    log_path = directory / 'deploy.log'
    if operation == 'dispatch':
        with (directory / 'dispatch.lock').open('a') as lock:
            os.chmod(directory / 'dispatch.lock', 0o600)
            fcntl.flock(lock, fcntl.LOCK_EX)
            fresh = not request_path.exists()
            if not fresh:
                recorded = _read_json(request_path)
                _validate_recorded(envelope, recorded)
            try:
                _preflight_user_bus(tool_path)
            except Exception:
                if fresh:
                    return {**base, 'executor_ref': None, 'status': 'failed',
                        'environment_known_unchanged': True,
                        'message': 'User service manager is unavailable; no production intent was created',
                        'logs': _safe_log_entries(directory)}
                return {**base, 'status': 'unknown',
                    'message': 'User service manager is unavailable; the existing production intent will not be replayed',
                    'logs': _safe_log_entries(directory)}
            if fresh:
                # The read-only bus preflight above must succeed before this
                # durable intent is written. A later systemd-run timeout remains
                # unknown and keeps this intent so it can never be replayed.
                bridge.atomic_json(request_path, envelope)
            try:
                state = bridge.unit_state(unit)
            except Exception:
                return {**base, 'status': 'unknown',
                    'message': 'Production supervisor identity cannot be observed',
                    'logs': _safe_log_entries(directory)}
            if state.get('LoadState') != 'not-found':
                if state.get('Id') != unit or not state.get('InvocationID'):
                    return {**base, 'status': 'unknown',
                        'message': 'Production supervisor identity conflicts with this run',
                        'logs': _safe_log_entries(directory)}
                if state.get('ActiveState') == 'active' and state.get('SubState') in (
                        'running', 'start', 'start-pre', 'start-post'):
                    return {**base, 'status': 'started',
                        'message': 'Production release is running under its durable supervisor',
                        'logs': _safe_log_entries(directory)}
                return {**base, 'status': 'unknown',
                    'message': 'A production supervisor already exists; the release will not be replayed',
                    'logs': _safe_log_entries(directory)}
            if not fresh:
                return {**base, 'status': 'unknown',
                    'message': 'A durable production intent exists without an observable supervisor; it will not be replayed',
                    'logs': _safe_log_entries(directory)}
            script = Path(__file__).resolve()
            worker_environment = _runtime_environment(tool_path)
            handoff = [
                'systemd-run', '--user', '--quiet', '--unit', unit,
                '--setenv=PATH=' + tool_path,
            ]
            for key in ('XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS'):
                if key in worker_environment:
                    handoff.append('--setenv=' + key + '=' + worker_environment[key])
            handoff.extend([
                '--property=Type=exec', '--property=RemainAfterExit=yes',
                '--property=KillMode=control-group', '--property=UMask=0077',
                '--property=StandardOutput=append:' + str(log_path),
                '--property=StandardError=append:' + str(log_path),
                sys.executable, str(script), '--state-dir', str(args.state_dir),
                '--repository', str(repository), '--reviewed-release-commit',
                str(args.reviewed_release_commit), '--tool-path', tool_path,
                '--worker', str(request_path),
            ])
            try:
                _run(handoff, env=worker_environment, timeout=20)
            except Exception:
                return {**base, 'status': 'unknown',
                    'message': 'Production handoff outcome is uncertain; its durable intent will not be replayed',
                    'logs': _safe_log_entries(directory)}
        return {**base, 'status': 'started',
            'message': 'Production release handed to its durable supervisor',
            'logs': _safe_log_entries(directory)}

    if not request_path.exists():
        return {**base, 'status': 'unknown',
            'message': 'No durable production release intent is available',
            'logs': _safe_log_entries(directory)}
    recorded = _read_json(request_path)
    _validate_recorded(envelope, recorded)
    output = {**base, 'logs': _safe_log_entries(directory)}
    try:
        state = bridge.unit_state(unit)
    except Exception:
        return {**output, 'status': 'unknown',
            'message': 'Production supervisor identity cannot be observed'}
    if (state.get('LoadState') != 'loaded' or state.get('Id') != unit
            or not state.get('InvocationID')):
        return {**output, 'status': 'unknown',
            'message': 'Production supervisor identity cannot be observed'}
    if state.get('SubState') in ('running', 'start', 'start-pre', 'start-post'):
        return {**output, 'status': 'running',
            'message': 'Production release is still running under its durable supervisor'}
    if not result_path.exists():
        return {**output, 'status': 'unknown',
            'message': 'Production task stopped without a durable result'}
    try:
        result = _read_json(result_path)
    except Exception:
        return {**output, 'status': 'unknown',
            'message': 'Production task result cannot be read safely'}
    if (result.get('run_id') != run['id']
            or result.get('generation') != run['generation']
            or result.get('target') != target or result.get('unit') != unit):
        return {**output, 'status': 'unknown',
            'message': 'Production task result does not match the active run fence'}
    result_code = result.get('returncode')
    if isinstance(result_code, bool) or not isinstance(result_code, int):
        return {**output, 'status': 'unknown',
            'message': 'Production task result has no valid exit status'}
    if result_code != 0:
        try:
            proof = _production_failure_proof(
                recorded, result, run, target, unit, state, request_path)
        except Exception:
            return {**output, 'status': 'unknown',
                'message': 'Production release failed, but its supervisor has not proven it stopped'}
        return {**output, 'status': 'failed', 'environment_known_unchanged': False,
            'failure_proof': proof,
            'message': 'Production release failed and the supervisor proved the release job stopped'}
    if (state.get('ActiveState') != 'active' or state.get('SubState') != 'exited'
            or state.get('Result') != 'success' or state.get('ExecMainStatus') != '0'
            or state.get('InvocationID') != result.get('invocation_id')):
        return {**output, 'status': 'unknown',
            'message': 'Production release completion is not confirmed by its supervisor'}
    if (result.get('reviewed_release_commit') != args.reviewed_release_commit
            or not isinstance(result.get('pipeline_blobs'), dict)):
        return {**output, 'status': 'unknown',
            'message': 'Production result is not bound to the configured reviewed pipeline'}
    try:
        current_blobs = validate_pipeline_blobs(
            repository, target['revision'], args.reviewed_release_commit)
    except Exception:
        return {**output, 'status': 'unknown',
            'message': 'Reviewed production pipeline fingerprint cannot be revalidated'}
    if current_blobs != result['pipeline_blobs']:
        return {**output, 'status': 'unknown',
            'message': 'Production pipeline fingerprint changed after release'}
    saved_proof = result.get('proof')
    if not isinstance(saved_proof, dict) or not isinstance(saved_proof.get('core'), dict):
        return {**output, 'status': 'unknown',
            'message': 'Production release has no independent three-component proof'}
    selected_policy = _database_backup_policy(target)
    if (saved_proof['core'].get('database_backup_policy') != selected_policy
            or saved_proof['core'].get('target_parameters') != target['parameters']):
        return {**output, 'status': 'unknown',
            'message': 'Production proof does not match the frozen database backup policy'}
    if operation == 'observe':
        return {**output, 'status': 'completed',
            'message': 'Production release completed; independent verification is available'}
    if operation != 'verify':
        return {**output, 'status': 'unknown', 'message': 'Unsupported production verification operation'}
    try:
        before = saved_proof['core']['components']['worker']['before']
        fresh_proof = capture_proof(repository, target, before)
    except Exception:
        return {**output, 'status': 'unknown',
            'message': 'A fresh independent production verification could not be completed'}
    if fresh_proof['core'] != saved_proof['core']:
        return {**output, 'status': 'unknown',
            'message': 'One or more production components changed after the recorded release proof'}
    receipt = {
        'id': str(uuid.uuid4()),
        'run_id': run['id'],
        'generation': run['generation'],
        'observed_at': bridge.now(),
        'source_revision': target['revision'],
        'component_digests': fresh_proof['component_digests'],
        'profile_digest': target['profile_digest'],
        'verification_report': json.dumps(fresh_proof['core'], sort_keys=True, separators=(',', ':')),
        'result': 'success',
    }
    return {**output, 'status': 'verified', 'receipt': receipt,
        'message': 'App, standalone worker, and public website independently match the frozen source SHA'}


def _fetch_published_ref(repository, environment):
    _run(['git', '-C', str(repository), 'fetch', 'origin',
        '+refs/heads/vibe:refs/remotes/origin/vibe'], env=environment, timeout=180)


def validate_published_revision(repository, revision, reviewed_commit, environment=None):
    revision = _validate_full_sha(revision)
    reviewed_commit = _validate_full_sha(reviewed_commit)
    if environment is not None:
        _fetch_published_ref(repository, environment)
    for candidate in (revision, reviewed_commit):
        resolved = _git(repository, 'rev-parse', '--verify', candidate + '^{commit}')
        if resolved != candidate:
            raise RuntimeError('a release commit is not present as the exact full SHA')
        if subprocess.run(['git', '-C', str(repository), 'merge-base', '--is-ancestor',
                           candidate, 'refs/remotes/origin/vibe'],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                          env=environment, timeout=60, check=False).returncode != 0:
            raise RuntimeError('a release commit is not reachable from published origin/vibe')


def validate_pipeline_blobs(repository, revision, reviewed_commit):
    mismatches = []
    for path in PIPELINE_FILES:
        try:
            reviewed_blob = _git(repository, 'rev-parse', '--verify', f'{reviewed_commit}:{path}')
            target_blob = _git(repository, 'rev-parse', '--verify', f'{revision}:{path}')
        except Exception:
            mismatches.append(path)
            continue
        if reviewed_blob != target_blob:
            mismatches.append(path)
    if mismatches:
        # Path names are safe metadata; no script contents or credentials are emitted.
        raise RuntimeError('target release tooling differs from the administrator-reviewed pipeline')
    return {path: _git(repository, 'rev-parse', '--verify', f'{revision}:{path}')
        for path in PIPELINE_FILES}


def _deployment_image(deployment, container_name, repository):
    containers = deployment.get('spec', {}).get('template', {}).get('spec', {}).get('containers', [])
    matches = [container for container in containers if container.get('name') == container_name]
    if len(matches) != 1:
        raise RuntimeError('production deployment does not have the expected container')
    image = matches[0].get('image')
    prefix = repository + '@'
    if not isinstance(image, str) or not image.startswith(prefix):
        raise RuntimeError('production deployment does not use an immutable image reference')
    digest = image[len(prefix):]
    if not IMAGE_DIGEST_RE.fullmatch(digest):
        raise RuntimeError('production deployment image digest is invalid')
    return image, digest


def _pod_container(pod, name):
    spec = [item for item in pod.get('spec', {}).get('containers', []) if item.get('name') == name]
    status = [item for item in pod.get('status', {}).get('containerStatuses', []) if item.get('name') == name]
    if len(spec) != 1 or len(status) != 1:
        raise RuntimeError('production pod container identity is incomplete')
    return spec[0], status[0]


def validate_replicas(deployment, pods, container_name, repository, expected_tag_digest):
    metadata = deployment.get('metadata', {})
    generation = metadata.get('generation')
    replicas = deployment.get('spec', {}).get('replicas')
    status = deployment.get('status', {})
    if (isinstance(replicas, bool) or not isinstance(replicas, int) or replicas < 1
            or status.get('observedGeneration') != generation
            or status.get('updatedReplicas') != replicas
            or status.get('readyReplicas') != replicas
            or status.get('availableReplicas') != replicas
            or status.get('unavailableReplicas', 0) != 0 or len(pods) != replicas):
        raise RuntimeError('production deployment does not have all replicas updated and Ready')
    image_ref, image_digest = _deployment_image(deployment, container_name, repository)
    if image_digest != expected_tag_digest:
        raise RuntimeError('production deployment digest differs from the source-tag digest')
    selector = deployment.get('spec', {}).get('selector', {}).get('matchLabels', {})
    if not selector:
        raise RuntimeError('production deployment has no stable pod selector')
    proof_pods = []
    for pod in pods:
        pod_metadata = pod.get('metadata', {})
        labels = pod_metadata.get('labels', {})
        if (not isinstance(pod_metadata.get('name'), str)
                or not K8S_NAME_RE.fullmatch(pod_metadata['name'])
                or not isinstance(pod_metadata.get('uid'), str)
                or not pod_metadata['uid']
                or pod_metadata.get('deletionTimestamp') is not None
                or any(labels.get(key) != value for key, value in selector.items())
                or pod.get('status', {}).get('phase') != 'Running'
                or not any(condition.get('type') == 'Ready' and condition.get('status') == 'True'
                           for condition in pod.get('status', {}).get('conditions', []))):
            raise RuntimeError('a production pod is not a current Ready replica')
        spec, container_status = _pod_container(pod, container_name)
        image_id = container_status.get('imageID', '')
        found = re.search(r'@?(sha256:[0-9a-f]{64})$', image_id)
        if (spec.get('image') != image_ref or container_status.get('ready') is not True
                or not found or found.group(1) != image_digest):
            raise RuntimeError('a production pod is not running the source-tag image digest')
        proof_pods.append({'name': pod_metadata['name'], 'uid': pod_metadata['uid'],
            'image_digest': image_digest, 'ready': True})
    return image_ref, image_digest, sorted(proof_pods, key=lambda value: value['name'])


def _rollout_snapshot(deployment, image_ref, image_digest, replicas):
    return {
        'generation': deployment.get('metadata', {}).get('generation'),
        'replica_count': deployment.get('spec', {}).get('replicas'),
        'observed_generation': deployment.get('status', {}).get('observedGeneration'),
        'image_ref': image_ref,
        'image_digest': image_digest,
        'replicas': replicas,
    }


def _assert_rollout_unchanged(name, container, repository, image_tag, initial):
    tag_digest = _docker_tag_digest(image_tag)
    if tag_digest != initial['image_digest']:
        raise RuntimeError('production source image tag changed during independent verification')
    deployment, pods = _get_workload(name)
    image_ref, image_digest, replicas = validate_replicas(
        deployment, pods, container, repository, initial['image_digest'])
    current = _rollout_snapshot(deployment, image_ref, image_digest, replicas)
    if current != initial:
        raise RuntimeError('production deployment or replicas changed during independent verification')


def _assert_observation_unchanged(app_snapshot, website_snapshot, app_tag, website_tag,
                                  worker_observation):
    _assert_rollout_unchanged(APP_DEPLOYMENT, APP_CONTAINER, APP_IMAGE_REPOSITORY,
        app_tag, app_snapshot)
    _assert_rollout_unchanged(WEBSITE_DEPLOYMENT, WEBSITE_CONTAINER, WEBSITE_IMAGE_REPOSITORY,
        website_tag, website_snapshot)
    current_worker = _worker_observation()
    if current_worker != worker_observation:
        raise RuntimeError('standalone worker supervisor changed during independent verification')


def _ssh_kubectl(argv):
    if any(not isinstance(item, str) or '\x00' in item for item in argv):
        raise RuntimeError('invalid fixed Kubernetes query')
    script = 'set -euo pipefail\nexport KUBECONFIG=/etc/kubernetes/admin.conf\nexec kubectl ' + shlex.join(argv) + '\n'
    return _run(['ssh', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes',
        MASTER_HOST, 'bash', '-s'], input_bytes=script.encode(), timeout=90)


def _kubectl_json(*argv):
    try:
        value = json.loads(_ssh_kubectl(list(argv)))
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise RuntimeError('Kubernetes returned invalid deployment evidence') from error
    if not isinstance(value, dict):
        raise RuntimeError('Kubernetes returned an unexpected resource shape')
    return value


def _assert_cluster_identity():
    context_name = _ssh_kubectl(['config', 'current-context']).decode().strip()
    api_server = _ssh_kubectl(['config', 'view', '--minify', '-o',
        'jsonpath={.clusters[0].cluster.server}']).decode().strip()
    if context_name != EXPECTED_CONTEXT or api_server != EXPECTED_API_SERVER:
        raise RuntimeError('production Kubernetes context does not match the pinned cluster')


def _get_workload(name):
    if not K8S_NAME_RE.fullmatch(name):
        raise RuntimeError('invalid production workload name')
    deployment = _kubectl_json('get', 'deployment', name, '-n', 'prod', '-o', 'json')
    pods = _kubectl_json('get', 'pods', '-n', 'prod', '-l', 'app=' + name, '-o', 'json')
    items = pods.get('items')
    if not isinstance(items, list) or any(not isinstance(item, dict) for item in items):
        raise RuntimeError('Kubernetes returned an unexpected pod list')
    return deployment, items


def _pod_exec(pod_name, container_name, command):
    if not K8S_NAME_RE.fullmatch(pod_name):
        raise RuntimeError('invalid production pod identity')
    return _ssh_kubectl(['exec', '-n', 'prod', pod_name, '-c', container_name,
        '--', *command])


def _docker_tag_digest(tag):
    output = _run_text(['docker', 'buildx', 'imagetools', 'inspect', '--format',
        '{{.Manifest.Digest}}', tag], timeout=120)
    if not IMAGE_DIGEST_RE.fullmatch(output):
        raise RuntimeError('production source image tag did not resolve to one immutable digest')
    return output


def _docker_labels(image_ref):
    _run(['docker', 'pull', image_ref], timeout=300)
    raw = _run_text(['docker', 'image', 'inspect', '--format',
        '{{json .Config.Labels}}', image_ref], timeout=60)
    try:
        labels = json.loads(raw)
    except json.JSONDecodeError as error:
        raise RuntimeError('production image provenance labels are invalid') from error
    if not isinstance(labels, dict):
        raise RuntimeError('production image provenance labels are missing')
    return labels


def _image_worker_digest(image_ref, state_dir):
    container = _run_text(['docker', 'create', image_ref], timeout=60)
    if not re.fullmatch(r'[a-f0-9]{12,64}', container):
        raise RuntimeError('Docker returned an invalid temporary container identity')
    try:
        with tempfile.TemporaryDirectory(prefix='prod-worker-image-', dir=state_dir) as temp:
            binary = Path(temp) / 'autotask-worker'
            _run(['docker', 'cp', container + ':/app/autotask-worker', str(binary)], timeout=60)
            if not binary.is_file() or binary.is_symlink():
                raise RuntimeError('production app image has no regular standalone worker binary')
            return bridge.digest(binary)
    finally:
        _run(['docker', 'rm', container], timeout=30)


def _worker_observation():
    command = 'sudo -n python3 -c ' + shlex.quote(REMOTE_WORKER_PROBE)
    raw = _run(['ssh', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes',
        WORKER_HOST, command], timeout=45)
    try:
        value = json.loads(raw)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise RuntimeError('standalone worker returned invalid supervisor evidence') from error
    required = ('invocation_id', 'main_pid', 'start_ticks', 'executable_sha256')
    if (not isinstance(value, dict) or any(key not in value for key in required)
            or not isinstance(value['invocation_id'], str) or not value['invocation_id']
            or isinstance(value['main_pid'], bool) or not isinstance(value['main_pid'], int)
            or value['main_pid'] <= 0
            or not re.fullmatch(r'[0-9]+', str(value['start_ticks']))
            or not re.fullmatch(r'[0-9a-f]{64}', str(value['executable_sha256']))):
        raise RuntimeError('standalone worker supervisor evidence is incomplete')
    return value


def _assert_worker_restarted(before, after):
    if (not isinstance(before, dict) or before.get('invocation_id') == after.get('invocation_id')
            or (before.get('main_pid') == after.get('main_pid')
                and str(before.get('start_ticks')) == str(after.get('start_ticks')))):
        raise RuntimeError('standalone worker supervisor identity did not change after release')


def _read_app_container(pod):
    pod_name = pod['metadata']['name']
    for endpoint in ('/live', '/ready'):
        body = _pod_exec(pod_name, APP_CONTAINER,
            ['wget', '-q', '-O', '-', 'http://127.0.0.1:8011' + endpoint])
        if endpoint == '/ready':
            try:
                ready = json.loads(body)
            except json.JSONDecodeError as error:
                raise RuntimeError('production app readiness endpoint returned invalid JSON') from error
            if not isinstance(ready, dict) or ready.get('ready') is not True:
                raise RuntimeError('production app readiness endpoint is not ready')
    stamp_raw = _pod_exec(pod_name, APP_CONTAINER, ['cat', '/app/dist/build-info.json'])
    try:
        stamp = json.loads(stamp_raw)
    except json.JSONDecodeError as error:
        raise RuntimeError('production app build-info is invalid') from error
    return stamp, _sha256_bytes(stamp_raw)


def _public_website_hash():
    connection = http.client.HTTPConnection('172.168.1.231', 80, timeout=20)
    try:
        connection.request('GET', '/', headers={'Host': 'www.autotask.run', 'Cache-Control': 'no-cache'})
        response = connection.getresponse()
        if response.status != 200:
            raise RuntimeError('public website ingress did not return HTTP 200')
        body = response.read(4 * 1024 * 1024 + 1)
        if len(body) > 4 * 1024 * 1024:
            raise RuntimeError('public website entry exceeds the verification limit')
        return _sha256_bytes(body)
    except (OSError, http.client.HTTPException) as error:
        raise RuntimeError('public website ingress could not be verified') from error
    finally:
        connection.close()


def _target_file(repository, revision, path):
    return _run(['git', '-C', str(repository), 'show', f'{revision}:{path}'], timeout=60)


def _target_website_manifest(repository, revision):
    archive = _run(['git', '-C', str(repository), 'archive', '--format=tar',
        revision, 'website'], timeout=90)
    manifest = {}
    try:
        with tarfile.open(fileobj=io.BytesIO(archive), mode='r:') as bundle:
            for member in bundle.getmembers():
                name = member.name
                if member.isdir():
                    continue
                if (not member.isfile() or not name.startswith('website/')
                        or name.startswith('website/../') or '/..' in name):
                    raise RuntimeError('website source archive contains an unsafe entry')
                relative = name[len('website/'):]
                stream = bundle.extractfile(member)
                if stream is None:
                    raise RuntimeError('website source archive has an unreadable file')
                digest = _sha256_bytes(stream.read())
                if relative in manifest:
                    raise RuntimeError('website source archive contains duplicate paths')
                manifest[relative] = {'mode': member.mode & 0o777, 'sha256': digest}
    except tarfile.TarError as error:
        raise RuntimeError('website source archive is invalid') from error
    if 'index.html' not in manifest or not manifest:
        raise RuntimeError('target source has no website entry page')
    return dict(sorted(manifest.items()))


def _verify_website_image_tree(image_root, manifest):
    root = Path(image_root).resolve(strict=True)
    if not root.is_dir():
        raise RuntimeError('website image content is not a directory')
    for relative, expected in manifest.items():
        parts = Path(relative).parts
        if not parts or any(part in ('', '.', '..') for part in parts):
            raise RuntimeError('target website manifest contains an unsafe path')
        candidate = root
        for part in parts:
            candidate = candidate / part
            if candidate.is_symlink():
                raise RuntimeError('website image contains a symlink in a tracked source path')
        resolved = candidate.resolve(strict=True)
        if not resolved.is_relative_to(root) or not resolved.is_file():
            raise RuntimeError('website image is missing a tracked source file')
        if bridge.digest(resolved) != expected['sha256']:
            raise RuntimeError('website image content differs from the frozen source tree')
    return _canonical_digest(manifest)


def _website_image_tree_digest(image_ref, repository, revision):
    manifest = _target_website_manifest(repository, revision)
    _run(['docker', 'pull', image_ref], timeout=300)
    container = _run_text(['docker', 'create', image_ref], timeout=60)
    if not re.fullmatch(r'[a-f0-9]{12,64}', container):
        raise RuntimeError('Docker returned an invalid temporary website container identity')
    try:
        with tempfile.TemporaryDirectory(prefix='prod-website-image-') as temp:
            image_root = Path(temp) / 'site'
            image_root.mkdir(mode=0o700)
            _run(['docker', 'cp', container + ':/usr/share/nginx/html/.', str(image_root)], timeout=90)
            return _verify_website_image_tree(image_root, manifest), manifest
    finally:
        _run(['docker', 'rm', container], timeout=30)


def capture_proof(repository, target, worker_before):
    revision = _validate_full_sha(target.get('revision'))
    tree = _git(repository, 'rev-parse', '--verify', revision + '^{tree}')
    short = _git(repository, 'rev-parse', '--short', revision)
    website_manifest = _target_website_manifest(repository, revision)
    expected_website = website_manifest['index.html']['sha256']
    app_tag = APP_IMAGE_REPOSITORY + ':git-' + short
    website_tag = WEBSITE_IMAGE_REPOSITORY + ':git-' + revision[:12]

    _assert_cluster_identity()
    app_deployment, app_pods = _get_workload(APP_DEPLOYMENT)
    website_deployment, website_pods = _get_workload(WEBSITE_DEPLOYMENT)
    app_tag_digest = _docker_tag_digest(app_tag)
    website_tag_digest = _docker_tag_digest(website_tag)
    app_image_ref, app_digest, app_replicas = validate_replicas(
        app_deployment, app_pods, APP_CONTAINER, APP_IMAGE_REPOSITORY, app_tag_digest)
    website_image_ref, website_digest, website_replicas = validate_replicas(
        website_deployment, website_pods, WEBSITE_CONTAINER, WEBSITE_IMAGE_REPOSITORY,
        website_tag_digest)
    app_snapshot = _rollout_snapshot(app_deployment, app_image_ref, app_digest, app_replicas)
    website_snapshot = _rollout_snapshot(
        website_deployment, website_image_ref, website_digest, website_replicas)

    labels = _docker_labels(app_image_ref)
    if (labels.get('org.opencontainers.image.revision') != revision
            or labels.get('com.autotask.source-tree') != tree
            or labels.get('org.opencontainers.image.version') != 'git-' + short):
        raise RuntimeError('production app OCI provenance does not match the frozen source SHA')
    app_stamps = []
    for pod in app_pods:
        stamp, stamp_digest = _read_app_container(pod)
        if stamp.get('commit') != revision or stamp.get('dirty') is not False:
            raise RuntimeError('a production app replica has a different build-info source SHA')
        app_stamps.append({'pod': pod['metadata']['name'], 'build_info_sha256': stamp_digest})
    app_stamps.sort(key=lambda value: value['pod'])

    worker_digest = _image_worker_digest(app_image_ref, Path(tempfile.gettempdir()))
    worker_after = _worker_observation()
    if worker_after.get('executable_sha256') != worker_digest:
        raise RuntimeError('standalone worker executable differs from the released app image')
    if worker_before is not None:
        _assert_worker_restarted(worker_before, worker_after)

    website_entries = []
    for pod in website_pods:
        body = _pod_exec(pod['metadata']['name'], WEBSITE_CONTAINER,
            ['cat', '/usr/share/nginx/html/index.html'])
        body_digest = _sha256_bytes(body)
        if body_digest != expected_website:
            raise RuntimeError('a production website replica serves different index.html bytes')
        website_entries.append({'pod': pod['metadata']['name'], 'index_html_sha256': body_digest})
    website_entries.sort(key=lambda value: value['pod'])
    ingress_digest = _public_website_hash()
    if ingress_digest != expected_website:
        raise RuntimeError('the public website ingress serves different index.html bytes')
    website_tree_digest, _ = _website_image_tree_digest(website_image_ref, repository, revision)

    # These checks run after image, pod-content, ingress, and worker binary
    # inspection. Reject a proof assembled across two different rollouts.
    _assert_observation_unchanged(
        app_snapshot, website_snapshot, app_tag, website_tag, worker_after)

    before = None
    if worker_before is not None:
        before = {
            'invocation_id': worker_before.get('invocation_id'),
            'main_pid': worker_before.get('main_pid'),
            'start_ticks': str(worker_before.get('start_ticks')),
            'executable_sha256': worker_before.get('executable_sha256'),
        }
    core = {
        'source_revision': revision,
        'source_tree': tree,
        'target_parameters': dict(target['parameters']),
        'database_backup_policy': _database_backup_policy(target),
        'components': {
            'app': {
                'image_tag': app_tag,
                'image_ref': app_image_ref,
                'image_digest': app_digest,
                'oci_source_revision': labels['org.opencontainers.image.revision'],
                'oci_source_tree': labels['com.autotask.source-tree'],
                'build_info': app_stamps,
                'replicas': app_replicas,
            },
            'website': {
                'image_tag': website_tag,
                'image_ref': website_image_ref,
                'image_digest': website_digest,
                'tracked_source_tree_sha256': website_tree_digest,
                'tracked_source_file_count': len(website_manifest),
                'index_html_sha256': expected_website,
                'replicas': website_replicas,
                'entries': website_entries,
                'public_ingress_sha256': ingress_digest,
            },
            'worker': {
                'unit': WORKER_UNIT,
                'invocation_id': worker_after['invocation_id'],
                'main_pid': worker_after['main_pid'],
                'start_ticks': str(worker_after['start_ticks']),
                'executable_sha256': worker_after['executable_sha256'],
                'before': before,
            },
        },
    }
    component_digests = {
        'app': app_digest,
        'website': website_digest,
        'worker': 'sha256:' + worker_digest,
    }
    return {
        'observed_at': bridge.now(),
        'core': core,
        'component_digests': component_digests,
        'proof_sha256': _canonical_digest(core),
    }


def _runtime_environment(tool_path=None):
    # Keep only the local toolchain identity and the SSH agent socket. Deployment
    # skips, host/build overrides, tokens, proxies, and arbitrary command hooks
    # are intentionally absent. Trusted user-bus coordinates are retained so
    # systemctl --user can reach its manager. Production destinations are pinned.
    if tool_path is None:
        tool_path = _ACTIVE_TOOL_PATH
    if tool_path is None:
        raise RuntimeError('administrator-pinned production tool path is required')
    tool_path = _validate_tool_path(tool_path)
    allowed = (
        'HOME', 'TMPDIR', 'LANG', 'LC_ALL', 'SSH_AUTH_SOCK',
        'XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS',
    )
    env = {key: os.environ[key] for key in allowed if key in os.environ}
    env['PATH'] = tool_path
    env.update({
        'K8S_MASTER': MASTER_HOST,
        'WORKER_HOST': WORKER_HOST,
        'STAGING_WORKER_HOST': WORKER_HOST,
        'WORKER_BUILD_IMAGE': 'rust:1.90-bullseye',
    })
    return env


def _preflight_user_bus(tool_path=None):
    """Verify the trusted user manager is reachable before creating intent."""
    env = _runtime_environment(tool_path)
    if not env.get('XDG_RUNTIME_DIR'):
        raise RuntimeError('user systemd runtime directory is unavailable')
    _run(['systemctl', '--user', 'show-environment'], env=env, timeout=5)
    return env


def _prepare_runner(repository, directory, reviewed_commit, env):
    runner = directory / 'reviewed-runner'
    if runner.exists() or runner.is_symlink():
        raise RuntimeError('per-run reviewed tooling worktree already exists')
    _run(['git', '-C', str(repository), 'worktree', 'add', '--detach',
        str(runner), reviewed_commit], env=env, timeout=180)
    return runner


def _cleanup_runner(repository, runner, env):
    if runner is None:
        return
    if runner.exists() or runner.is_symlink():
        _run(['git', '-C', str(repository), 'worktree', 'remove', '--force', str(runner)],
            env=env, timeout=120)


def worker(request_path, args):
    tool_path = _activate_tool_path(args.tool_path)
    request_path = Path(request_path)
    recorded = _read_json(request_path)
    run = recorded.get('run', {})
    target = run.get('target', {})
    run, target, directory, unit, repository = context(recorded, args)
    reviewed_commit = _validate_full_sha(args.reviewed_release_commit)
    invocation_id = None
    exit_code = 1
    runner = None
    worker_before = None
    env = _runtime_environment(tool_path)
    result_path = directory / 'result.json'
    try:
        active_job = bridge.unit_state(unit)
        if (active_job.get('Id') != unit or active_job.get('LoadState') != 'loaded'
                or not active_job.get('InvocationID')):
            raise RuntimeError('durable production supervisor identity is unavailable')
        invocation_id = active_job['InvocationID']
        # Persist a non-success guard before Git or production inspection. If the
        # worker is killed, an incomplete result cannot be mistaken for success.
        bridge.atomic_json(result_path, {
            'returncode': 255, 'finished_at': bridge.now(), 'run_id': run['id'],
            'generation': run['generation'], 'target': target, 'unit': unit,
            'invocation_id': invocation_id, 'phase': 'running',
        })
        worker_before = _worker_observation()
        validate_published_revision(repository, target['revision'], reviewed_commit, env)
        pipeline_blobs = validate_pipeline_blobs(repository, target['revision'], reviewed_commit)
        runner = _prepare_runner(repository, directory, reviewed_commit, env)
        release_script = runner / 'scripts' / 'release.sh'
        if not release_script.is_file() or release_script.is_symlink():
            raise RuntimeError('reviewed production release script is unavailable')
        log_path = directory / 'deploy.log'
        log_path.touch(mode=0o600, exist_ok=True)
        os.chmod(log_path, 0o600)
        with log_path.open('ab', buffering=0) as output:
            completed = subprocess.run([
                'bash', str(release_script), 'prod', 'all', '--from-git',
                '--ref', target['revision'], *_database_backup_flag(target),
            ], cwd=runner, env=env, stdin=subprocess.DEVNULL, stdout=output,
                stderr=subprocess.STDOUT, check=False)
        exit_code = completed.returncode
        if exit_code != 0:
            raise RuntimeError('reviewed production release command failed')
        # Release helpers and manifests are checked again after the pipeline so
        # a concurrent source-ref change cannot silently alter its evidence.
        final_blobs = validate_pipeline_blobs(repository, target['revision'], reviewed_commit)
        if final_blobs != pipeline_blobs:
            raise RuntimeError('reviewed production pipeline fingerprint changed during release')
        _cleanup_runner(repository, runner, env)
        runner = None
        proof = capture_proof(repository, target, worker_before)
        result = {
            'returncode': 0, 'finished_at': bridge.now(), 'run_id': run['id'],
            'generation': run['generation'], 'target': target, 'unit': unit,
            'invocation_id': invocation_id, 'reviewed_release_commit': reviewed_commit,
            'pipeline_blobs': pipeline_blobs, 'proof': proof,
        }
        bridge.atomic_json(result_path, result)
        print('Production release completed; all components independently verified', flush=True)
        return 0
    except Exception as error:
        if runner is not None:
            try:
                _cleanup_runner(repository, runner, env)
            except Exception:
                # A leaked private worktree is operationally visible in the
                # result's safe error type; no shared checkout is modified.
                pass
        failure_code = exit_code if exit_code != 0 else 1
        bridge.atomic_json(result_path, {
            'returncode': failure_code, 'finished_at': bridge.now(), 'run_id': run['id'],
            'generation': run['generation'], 'target': target, 'unit': unit,
            'invocation_id': invocation_id, 'error': type(error).__name__,
        })
        print('Production release did not produce a verified result: ' + type(error).__name__,
            file=sys.stderr, flush=True)
        return failure_code


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state-dir', type=Path, required=True)
    parser.add_argument('--repository', type=Path, default=REPOSITORY_DEFAULT)
    parser.add_argument('--reviewed-release-commit', required=True,
        help='administrator-pinned full SHA containing the reviewed production release pipeline')
    parser.add_argument('--tool-path', required=True,
        help='administrator-pinned absolute PATH used by the production runner')
    parser.add_argument('--worker', type=Path)
    args = parser.parse_args()
    if args.worker:
        return worker(args.worker, args)
    try:
        envelope = json.load(sys.stdin)
        reply = hook(envelope, args)
    except Exception as error:
        envelope = locals().get('envelope', {})
        run = envelope.get('run', {}) if isinstance(envelope, dict) else {}
        try:
            identifier = str(uuid.UUID(str(run.get('id'))))
        except Exception:
            identifier = str(uuid.uuid4())
        generation = run.get('generation', 0)
        if isinstance(generation, bool) or not isinstance(generation, int):
            generation = 0
        reply = {'version': 1, 'run_id': identifier, 'generation': generation,
            'executor_ref': run.get('executor_ref'), 'status': 'unknown',
            'message': 'Production hook could not establish safe evidence: ' + type(error).__name__,
            'logs': []}
    print(json.dumps(reply, sort_keys=True, separators=(',', ':')))
    return 0


if __name__ == '__main__':
    sys.exit(main())
