#!/usr/bin/env python3
"""Trusted local-development hook bridge. JSON stdin/stdout; jobs outlive the coordinator."""
import argparse
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import urllib.request
import uuid

MAX_LOG_BYTES = 12_000
CGROUP_ROOT = Path('/sys/fs/cgroup')
UNIT_FENCE_FIELDS = (
    'Id', 'InvocationID', 'LoadState', 'ActiveState', 'SubState', 'MainPID',
    'ExecMainCode', 'ExecMainStatus', 'ExecMainExitTimestamp', 'Result',
    'ControlGroup', 'ExecStart',
)
UNIT_EVIDENCE_FIELDS = tuple(key for key in UNIT_FENCE_FIELDS if key != 'ExecStart')


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat().replace('+00:00', 'Z')


def atomic_json(path, value):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile('w', dir=path.parent, delete=False) as handle:
        os.chmod(handle.name, 0o600)
        json.dump(value, handle, sort_keys=True)
        handle.flush()
        os.fsync(handle.fileno())
        temporary = handle.name
    os.replace(temporary, path)


def digest(path):
    h = hashlib.sha256()
    with open(path, 'rb') as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def git(repo, *args):
    return subprocess.check_output(['git', '-C', str(repo), *args], text=True, stderr=subprocess.DEVNULL).strip()


def unit_state(unit):
    raw = subprocess.check_output(['systemctl', '--user', 'show', unit,
        '-p', 'Id', '-p', 'InvocationID', '-p', 'LoadState', '-p', 'ActiveState',
        '-p', 'SubState', '-p', 'MainPID', '-p', 'ExecMainCode', '-p',
        'ExecMainStatus', '-p', 'ExecMainExitTimestamp', '-p', 'Result',
        '-p', 'ControlGroup', '-p', 'ExecStart'], text=True)
    return dict(line.split('=', 1) for line in raw.splitlines() if '=' in line)


def cgroup_process_check(control_group):
    """Prove a systemd cgroup and its complete descendant tree are empty."""
    if not control_group:
        return {'control_group': '', 'state': 'destroyed', 'pids': [], 'groups_scanned': 0}
    parts = Path(control_group).parts
    if parts and parts[0] == os.sep:
        parts = parts[1:]
    if not parts or any(part in ('.', '..') for part in parts):
        raise RuntimeError('systemd returned an invalid control group path')
    root = CGROUP_ROOT.resolve(strict=True)
    group = root.joinpath(*parts)
    cursor = root
    for part in parts:
        cursor = cursor / part
        if cursor.is_symlink():
            raise RuntimeError('systemd cgroup path contains a symlink')
    candidate = group
    removed = False
    while True:
        try:
            resolved = candidate.resolve(strict=True)
            break
        except FileNotFoundError:
            if candidate.is_symlink():
                raise RuntimeError('systemd cgroup path contains a broken symlink')
            if candidate == root:
                raise RuntimeError('systemd cgroup root cannot be resolved')
            candidate = candidate.parent
            removed = True
    if not resolved.is_relative_to(root) or not resolved.is_dir():
        raise RuntimeError('systemd cgroup path is outside the cgroup filesystem')
    if removed:
        return {'control_group': control_group, 'state': 'destroyed', 'pids': [], 'groups_scanned': 0}

    def populated(directory):
        events = directory / 'cgroup.events'
        if events.is_symlink():
            raise RuntimeError('systemd cgroup event file is a symlink')
        values = []
        for line in events.read_text().splitlines():
            fields = line.split()
            if fields and fields[0] == 'populated':
                if len(fields) != 2 or fields[1] not in ('0', '1'):
                    raise RuntimeError('systemd cgroup returned invalid population state')
                values.append(fields[1])
        if len(values) != 1:
            raise RuntimeError('systemd cgroup has no unique population state')
        return values[0]

    def process_ids(directory):
        procs = directory / 'cgroup.procs'
        if procs.is_symlink():
            raise RuntimeError('systemd cgroup process list is a symlink')
        pids = tuple(procs.read_text().split())
        if any(not re.fullmatch(r'[0-9]+', pid) or int(pid) <= 0 for pid in pids):
            raise RuntimeError('systemd cgroup process list is invalid')
        return pids

    def child_groups(directory):
        if directory.is_symlink():
            raise RuntimeError('systemd cgroup subtree contains a symlink')
        names = []
        with os.scandir(directory) as entries:
            for entry in entries:
                if entry.is_symlink():
                    raise RuntimeError('systemd cgroup subtree contains a symlink')
                if entry.is_dir(follow_symlinks=False):
                    names.append(entry.name)
        return tuple(sorted(names))

    def scan_tree(directory):
        snapshot = []

        def visit(current):
            resolved_current = current.resolve(strict=True)
            if (current.is_symlink() or not resolved_current.is_relative_to(root)
                    or not resolved_current.is_dir()):
                raise RuntimeError('systemd cgroup subtree is not a stable directory tree')
            before_children = child_groups(current)
            before_pids = process_ids(current)
            before_populated = populated(current)
            if before_pids or before_populated != '0':
                raise RuntimeError('systemd cgroup subtree still contains processes')
            relative = current.relative_to(resolved).as_posix()
            snapshot.append((relative, before_pids, before_populated, before_children))
            for name in before_children:
                visit(current / name)
            after_pids = process_ids(current)
            after_populated = populated(current)
            after_children = child_groups(current)
            if (after_pids != before_pids or after_populated != before_populated
                    or after_children != before_children):
                raise RuntimeError('systemd cgroup subtree changed during process inspection')

        visit(directory)
        return tuple(sorted(snapshot))

    # populated is an aggregate over descendants on cgroup v2. Check it
    # around two complete recursive scans so neither a child process nor a
    # changing/hidden child cgroup can be mistaken for an empty parent.
    first_root_population = populated(resolved)
    first_scan = scan_tree(resolved)
    between_population = populated(resolved)
    second_scan = scan_tree(resolved)
    final_population = populated(resolved)
    if (first_root_population != '0' or between_population != '0'
            or final_population != '0' or first_scan != second_scan):
        raise RuntimeError('systemd cgroup subtree changed during process inspection')
    return {
        'control_group': control_group,
        'state': 'empty',
        'pids': [],
        'groups_scanned': len(first_scan),
        'populated': 0,
        'double_scan_stable': True,
    }


def legacy_execstart_matches(exec_start, request_path):
    """Match the supervisor's exact worker request argument."""
    argv_properties = re.findall(r'argv\[\]=(.*?)(?=\s;\s|[}]|$)', exec_start or '')
    expected_request = str(request_path.resolve())
    for value in argv_properties:
        try:
            argv = shlex.split(value)
        except ValueError:
            continue
        if (argv.count('--worker') == 1
                and argv[argv.index('--worker') + 1:argv.index('--worker') + 2] == [expected_request]):
            return True
    return False


def legacy_exit_matches(result, state):
    """Require the supervisor's recorded exit time to match the durable result."""
    try:
        finished = datetime.datetime.fromisoformat(
            result['finished_at'].replace('Z', '+00:00'))
        if finished.tzinfo is None:
            return False
        exit_text = state['ExecMainExitTimestamp'].strip()
        parsed_exit = re.fullmatch(
            r'(?:[A-Za-z]{3} )?(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})(?: ([A-Za-z0-9_+:-]+))?',
            exit_text,
        )
        if parsed_exit:
            exit_time = datetime.datetime.strptime(parsed_exit.group(1), '%Y-%m-%d %H:%M:%S')
            zone = parsed_exit.group(2)
            if zone in ('UTC', 'GMT', 'Z'):
                exit_time = exit_time.replace(tzinfo=datetime.timezone.utc)
            elif zone and zone == datetime.datetime.now().astimezone().tzname():
                exit_time = exit_time.replace(tzinfo=datetime.datetime.now().astimezone().tzinfo)
            else:
                return False
        else:
            # Tests and newer systemd builds may expose the timestamp as ISO.
            parsed = datetime.datetime.fromisoformat(exit_text.replace('Z', '+00:00'))
            if parsed.tzinfo is None:
                return False
            exit_time = parsed
        delta = abs((finished.astimezone(datetime.timezone.utc)
                     - exit_time.astimezone(datetime.timezone.utc)).total_seconds())
        return delta <= 10
    except (KeyError, TypeError, ValueError, OverflowError):
        return False


def failure_proof(recorded, result, run, target, unit, first_state, request_path):
    """Return fresh stop proof only when request, result, systemd, and cgroup agree."""
    recorded_run = recorded.get('run', {})
    if (recorded_run.get('id') != run.get('id')
            or recorded_run.get('generation') != run.get('generation')
            or recorded_run.get('target') != target
            or recorded.get('lane', {}).get('id') != run.get('lane_id')):
        raise RuntimeError('durable dispatch request does not match the observed run')
    result_code = result.get('returncode')
    if (isinstance(result_code, bool) or not isinstance(result_code, int)
            or result_code == 0
            or result.get('run_id') != run.get('id')
            or result.get('generation') != run.get('generation')):
        raise RuntimeError('durable failure result does not match the observed run')
    legacy = not any(key in result for key in ('target', 'unit', 'invocation_id'))
    if legacy:
        if (not legacy_execstart_matches(first_state.get('ExecStart', ''),
                                         request_path)
                or not legacy_exit_matches(result, first_state)):
            raise RuntimeError('legacy result is not bound to this supervisor invocation')
        invocation_id = first_state.get('InvocationID')
    else:
        if result.get('target') != target or result.get('unit') != unit:
            raise RuntimeError('durable failure target or unit does not match the observed run')
        invocation_id = result.get('invocation_id')
        if not isinstance(invocation_id, str) or not invocation_id:
            raise RuntimeError('durable failure result has no stable supervisor invocation')
    if not isinstance(invocation_id, str) or not invocation_id:
        raise RuntimeError('systemd has no stable invocation identity')

    def validate_supervisor(state):
        if (state.get('Id') != unit
                or state.get('InvocationID') != invocation_id
                or state.get('LoadState') != 'loaded'
                or state.get('ActiveState') not in ('inactive', 'failed')
                or state.get('MainPID') != '0'):
            raise RuntimeError('systemd has not proven this failed job is stopped')
        try:
            status = int(state.get('ExecMainStatus', ''))
        except ValueError as error:
            raise RuntimeError('systemd returned an invalid exit status') from error
        if status == 0 or status != result_code:
            raise RuntimeError('systemd exit status does not match the durable failure')

    validate_supervisor(first_state)
    process_check = cgroup_process_check(first_state.get('ControlGroup', ''))
    second_state = unit_state(unit)
    validate_supervisor(second_state)
    if any(first_state.get(key) != second_state.get(key) for key in UNIT_FENCE_FIELDS):
        raise RuntimeError('systemd unit identity or state changed during stop observation')
    supervisor = {key: first_state.get(key, '') for key in UNIT_EVIDENCE_FIELDS}
    proof = {
        'observed_at': now(),
        'evidence': {
            'unit': unit,
            'run_id': run['id'],
            'generation': run['generation'],
            'target': target,
            'result': {
                'returncode': result_code,
                'run_id': result['run_id'],
                'generation': result['generation'],
                'target': result.get('target', target),
                'unit': result.get('unit', unit),
                'invocation_id': invocation_id,
                'finished_at': result.get('finished_at'),
            },
            'legacy_result_read_only': legacy,
            'supervisor': supervisor,
            'process_check': process_check,
        },
    }
    if len(json.dumps(proof['evidence'], sort_keys=True, separators=(',', ':')).encode()) > 8192:
        raise RuntimeError('failure proof exceeds the protocol size limit')
    return proof


def process_proof(unit, root, artifact=None):
    state = unit_state(unit)
    if state.get('ActiveState') != 'active' or state.get('SubState') != 'running':
        raise RuntimeError('target service is not running: ' + unit)
    pid = int(state['MainPID'])
    proc = Path('/proc') / str(pid)
    stat = (proc / 'stat').read_text()
    start_ticks = stat[stat.rfind(')') + 2:].split()[19]
    cwd = (proc / 'cwd').resolve(strict=True)
    if not cwd.is_relative_to(root):
        raise RuntimeError('target service does not use the configured checkout')
    result = {'unit': unit, 'pid': pid, 'start_ticks': start_ticks, 'cwd': str(cwd)}
    if artifact:
        expected = root / artifact
        if (proc / 'exe').resolve(strict=True) != expected.resolve(strict=True):
            raise RuntimeError('target service executable path mismatch: ' + unit)
        result['sha256'] = digest(proc / 'exe')
        if result['sha256'] != digest(expected):
            raise RuntimeError('running executable differs from the built artifact: ' + unit)
    # Re-read identity so a service restart during hashing cannot pass unnoticed.
    if unit_state(unit).get('MainPID') != str(pid) or (proc / 'stat').read_text()[stat.rfind(')') + 2:].split()[19] != start_ticks:
        raise RuntimeError('target service identity changed while observing')
    return result


def environment_proof(checkout, revision, deploy_script):
    checkout = checkout.resolve(strict=True)
    if git(checkout, 'rev-parse', 'HEAD') != revision:
        raise RuntimeError('deployed source does not match the frozen revision')
    if git(checkout, 'status', '--porcelain', '--untracked-files=all'):
        raise RuntimeError('deployment checkout contains uncommitted tracked changes')
    services = {
        'backend': process_proof('autotask-backend.service', checkout, 'server/bin/autotask'),
        'worker': process_proof('autotask-worker.service', checkout, 'packages/worker-rust/target/debug/autotask-worker'),
        'frontend': process_proof('autotask-frontend.service', checkout),
    }
    checks = []
    for url in ('http://127.0.0.1:8011/live', 'http://127.0.0.1:8011/ready', 'http://127.0.0.1:8012/'):
        with urllib.request.urlopen(url, timeout=10) as response:
            body = response.read(256_000)
            if response.status != 200:
                raise RuntimeError('target health probe failed')
            if url.endswith('/ready') and json.loads(body).get('ready') is not True:
                raise RuntimeError('backend is not ready')
            checks.append({'url': url, 'status': response.status})
    if git(checkout, 'rev-parse', 'HEAD') != revision or git(checkout, 'status', '--porcelain', '--untracked-files=all'):
        raise RuntimeError('deployment checkout changed during verification')
    return {'source_revision': revision, 'source_tree': git(checkout, 'rev-parse', revision + '^{tree}'), 'deploy_script_sha256': digest(deploy_script), 'worker_build_skipped': False, 'backend_build_skipped': False, 'services': services, 'checks': checks}


def logs(path):
    if not path.exists():
        return []
    entries = []
    with path.open('rb') as handle:
        handle.seek(max(0, path.stat().st_size - MAX_LOG_BYTES))
        if handle.tell():
            handle.readline()
        while True:
            raw = handle.readline()
            if not raw:
                break
            sequence = handle.tell()
            text = raw.decode(errors='replace').strip()
            text = re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]', '', text)
            text = re.sub(r'(?i)(bearer\s+)[^\s]+', r'\1[REDACTED]', text)
            for name, value in os.environ.items():
                if len(value) >= 12 and any(word in name.upper() for word in ('TOKEN', 'PASSWORD', 'SECRET', 'API_KEY')):
                    text = text.replace(value, '[REDACTED]')
            if text:
                entries.append({'sequence': sequence, 'level': 'info', 'message': text[:1000]})
    return entries[-30:]


def context(envelope, args):
    if envelope.get('version') != 1:
        raise RuntimeError('unsupported hook protocol version')
    run, lane = envelope['run'], envelope['lane']
    identifier = str(uuid.UUID(run['id']))
    generation = int(run['generation'])
    if generation < 1 or lane['id'] != 'autotask-dev' or lane['project'] != 'autotask' or lane['environment'] != 'dev' or run['lane_id'] != lane['id']:
        raise RuntimeError('this bridge is restricted to the configured AutoTask development lane')
    target = run['target']
    if not re.fullmatch(r'[a-fA-F0-9]{40}|[a-fA-F0-9]{64}', target['revision']) or target['components'] != ['app']:
        raise RuntimeError('unexpected frozen deployment target')
    directory = args.state_dir.resolve() / identifier
    unit = f'shipbraid-autotask-{identifier}-{generation}.service'
    # A rename must not orphan the durable identity of an already-started job.
    legacy_unit = f'auto-deliver-autotask-{identifier}-{generation}.service'
    recorded_ref = run.get('executor_ref')
    if recorded_ref == legacy_unit:
        unit = legacy_unit
    elif recorded_ref not in (None, unit):
        raise RuntimeError('executor reference does not match the durable job identity')
    return run, target, directory, unit


def hook(envelope, args):
    run, target, directory, unit = context(envelope, args)
    operation = envelope['operation']
    base = {'version': 1, 'run_id': run['id'], 'generation': run['generation'], 'executor_ref': unit}
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    request_path = directory / 'request.json'
    result_path = directory / 'result.json'
    if operation == 'dispatch':
        with (directory / 'dispatch.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            fresh = not request_path.exists()
            if not fresh:
                recorded = json.loads(request_path.read_text())
                if recorded['run']['id'] != run['id'] or recorded['run']['target'] != target or recorded['run']['generation'] != run['generation']:
                    raise RuntimeError('durable dispatch input conflicts with this request')
            else:
                if git(args.deploy_checkout, 'status', '--porcelain', '--untracked-files=all'):
                    raise RuntimeError('deployment checkout has tracked changes; refusing automatic WIP cleanup')
                atomic_json(request_path, envelope)
            state = unit_state(unit)
            if state.get('LoadState') == 'not-found':
                if not fresh:
                    return {**base, 'status': 'unknown', 'message': 'A previous dispatch intent exists without an observable job; it will not be replayed'}
                script = Path(__file__).resolve()
                subprocess.run(['systemd-run', '--user', '--quiet', '--unit', unit,
                    '--property=Type=exec', '--property=RemainAfterExit=yes', '--property=KillMode=control-group',
                    '--property=UMask=0077', '--property=StandardOutput=append:' + str(directory / 'deploy.log'),
                    '--property=StandardError=append:' + str(directory / 'deploy.log'),
                    sys.executable, str(script), '--state-dir', str(args.state_dir),
                    '--repository', str(args.repository), '--deploy-checkout', str(args.deploy_checkout), '--deploy-script', str(args.deploy_script),
                    '--worker', str(request_path)], check=True, timeout=15, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return {**base, 'status': 'started', 'message': 'Deployment handed to an independent systemd user unit'}
    if not request_path.exists():
        return {**base, 'status': 'unknown', 'message': 'No durable dispatch input is available'}
    recorded = json.loads(request_path.read_text())
    if (recorded['run'].get('id') != run['id']
            or recorded['run'].get('target') != target
            or recorded['run'].get('generation') != run['generation']
            or recorded.get('lane', {}).get('id') != run['lane_id']):
        raise RuntimeError('observation target conflicts with durable dispatch input')
    output = {**base, 'logs': logs(directory / 'deploy.log')}
    try:
        state = unit_state(unit)
    except Exception:
        return {**output, 'status': 'unknown', 'message': 'Independent task identity cannot be observed'}
    if state.get('LoadState') == 'not-found':
        return {**output, 'status': 'unknown', 'message': 'Independent task identity cannot be observed'}
    if state.get('SubState') in ('running', 'start', 'start-pre', 'start-post'):
        return {**output, 'status': 'running', 'message': 'AutoTask development deployment is running'}
    if not result_path.exists():
        return {**output, 'status': 'unknown', 'message': 'Task exited without a durable deployment result'}
    result = json.loads(result_path.read_text())
    if result.get('run_id') != run['id'] or result.get('generation') != run['generation']:
        return {**output, 'status': 'unknown', 'message': 'Durable completion evidence has a different run fence'}
    result_code = result.get('returncode')
    if isinstance(result_code, bool) or not isinstance(result_code, int):
        return {**output, 'status': 'unknown', 'message': 'Durable completion evidence has no valid exit status'}
    if result_code != 0:
        try:
            stopped = failure_proof(recorded, result, run, target, unit, state, request_path)
        except Exception:
            return {**output, 'status': 'unknown', 'message': 'Deployment failed, but a stable stopped-job proof is unavailable'}
        return {**output, 'status': 'failed', 'environment_known_unchanged': False,
            'failure_proof': stopped,
            'message': 'Development deployment failed and the independent supervisor proved the job stopped; inspect deployment logs and environment'}
    if state.get('SubState') != 'exited' or state.get('Result') != 'success' or state.get('ExecMainStatus') != '0':
        return {**output, 'status': 'unknown', 'message': 'Task completion is not confirmed by its independent supervisor'}
    if operation == 'observe':
        return {**output, 'status': 'completed', 'message': 'Task completed; independent environment verification required'}
    if operation != 'verify':
        return {**output, 'status': 'unknown', 'message': 'This adapter does not support cancellation'}
    proof = environment_proof(args.deploy_checkout, target['revision'], args.deploy_script)
    if proof != result['proof']:
        return {**output, 'status': 'unknown', 'message': 'Environment changed after this deployment completed'}
    material = json.dumps(proof, sort_keys=True).encode()
    receipt = {'id': str(uuid.uuid4()), 'run_id': run['id'], 'generation': run['generation'],
        'observed_at': now(), 'source_revision': target['revision'],
        'component_digests': {'app': hashlib.sha256(material).hexdigest()},
        'profile_digest': target['profile_digest'], 'result': 'success',
        'verification_report': json.dumps({'executor_ref': unit, 'independent_observation': proof}, sort_keys=True)}
    return {**output, 'status': 'verified', 'receipt': receipt, 'message': 'Frozen revision, running binaries, service identities and health independently verified'}


def worker(path, args):
    envelope = json.loads(path.read_text())
    run, target, directory, unit = context(envelope, args)
    before = {}
    invocation_id = None
    try:
        job_state = unit_state(unit)
        if job_state.get('LoadState') == 'loaded' and job_state.get('Id') == unit:
            invocation_id = job_state.get('InvocationID') or None
    except Exception:
        # A result can still be recorded, but it will not qualify as a stop
        # proof without the supervisor invocation identity.
        pass
    for name in ('autotask-backend.service', 'autotask-frontend.service', 'autotask-worker.service'):
        before[name] = unit_state(name).get('MainPID')
    code = 1
    try:
        if git(args.deploy_checkout, 'status', '--porcelain', '--untracked-files=all'):
            raise RuntimeError('deployment checkout is not pristine')
        job_env = {**os.environ, 'AUTOTASK_COORDINATED_DEPLOY': '1',
            'AUTOTASK_LOCAL_DEPLOY_SKIP_WORKER_BUILD': '0', 'AUTOTASK_SKIP_BACKEND_REBUILD': '0',
            'AUTOTASK_SHARED_REPO': str(args.repository), 'AUTOTASK_DEPLOY_REPO': str(args.deploy_checkout),
            'AUTOTASK_DEPLOY_LOCK': '/tmp/autotask-deploy-local.lock',
            'AUTOTASK_LOCAL_DEPLOY_LOCK_DIR': str(Path.home() / '.cache/autotask')}
        code = subprocess.run(['bash', str(args.deploy_script), target['revision']], check=False, env=job_env).returncode
        if code:
            raise RuntimeError('existing deployment script failed')
        proof = environment_proof(args.deploy_checkout, target['revision'], args.deploy_script)
        if any(str(value['pid']) == before[value['unit']] for value in proof['services'].values()):
            raise RuntimeError('a target service was not restarted by this deployment')
        atomic_json(directory / 'result.json', {'returncode': 0, 'finished_at': now(), 'run_id': run['id'],
            'generation': run['generation'], 'target': target, 'unit': unit,
            'invocation_id': invocation_id, 'proof': proof})
        print('AutoTask development source and runtime verified after deployment', flush=True)
        return 0
    except Exception as error:
        atomic_json(directory / 'result.json', {'returncode': code or 1, 'finished_at': now(), 'run_id': run['id'],
            'generation': run['generation'], 'target': target, 'unit': unit,
            'invocation_id': invocation_id, 'error': type(error).__name__})
        print('Deployment did not produce a verified result: ' + str(error), file=sys.stderr, flush=True)
        return code or 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state-dir', type=Path, required=True)
    parser.add_argument('--repository', type=Path, default=Path('/home/yahui/repos/autotask'))
    parser.add_argument('--deploy-checkout', type=Path, default=Path('/home/yahui/repos/autotask-head'))
    parser.add_argument('--deploy-script', type=Path, required=True, help='Reviewed immutable deployment script installed with the adapter')
    parser.add_argument('--worker', type=Path)
    args = parser.parse_args()
    if args.worker:
        return worker(args.worker, args)
    envelope = json.load(sys.stdin)
    try:
        reply = hook(envelope, args)
    except Exception as error:
        run = envelope.get('run', {})
        reply = {'version': 1, 'run_id': run.get('id'), 'generation': run.get('generation'),
            'executor_ref': run.get('executor_ref'), 'status': 'unknown',
            'message': 'Bridge could not establish deployment evidence: ' + type(error).__name__}
    print(json.dumps(reply))
    return 0


if __name__ == '__main__':
    sys.exit(main())
