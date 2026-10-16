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
import subprocess
import sys
import tempfile
import urllib.request
import uuid

MAX_LOG_BYTES = 12_000


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
        '-p', 'LoadState', '-p', 'ActiveState', '-p', 'SubState', '-p', 'MainPID',
        '-p', 'ExecMainCode', '-p', 'ExecMainStatus', '-p', 'Result'], text=True)
    return dict(line.split('=', 1) for line in raw.splitlines() if '=' in line)


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
    unit = f'auto-deliver-autotask-{identifier}-{generation}.service'
    if run.get('executor_ref') not in (None, unit):
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
    if recorded['run']['target'] != target or recorded['run']['generation'] != run['generation']:
        raise RuntimeError('observation target conflicts with durable dispatch input')
    output = {**base, 'logs': logs(directory / 'deploy.log')}
    state = unit_state(unit)
    if state.get('LoadState') == 'not-found':
        return {**output, 'status': 'unknown', 'message': 'Independent task identity cannot be observed'}
    if state.get('SubState') in ('running', 'start', 'start-pre', 'start-post'):
        return {**output, 'status': 'running', 'message': 'AutoTask development deployment is running'}
    if not result_path.exists():
        return {**output, 'status': 'unknown', 'message': 'Task exited without a durable deployment result'}
    result = json.loads(result_path.read_text())
    if result.get('run_id') != run['id'] or result.get('generation') != run['generation']:
        return {**output, 'status': 'unknown', 'message': 'Durable completion evidence has a different run fence'}
    if result['returncode'] != 0:
        return {**output, 'status': 'failed', 'environment_known_unchanged': False, 'message': 'Development deployment failed; inspect deployment logs and environment'}
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
        atomic_json(directory / 'result.json', {'returncode': 0, 'finished_at': now(), 'run_id': run['id'], 'generation': run['generation'], 'proof': proof})
        print('AutoTask development source and runtime verified after deployment', flush=True)
        return 0
    except Exception as error:
        atomic_json(directory / 'result.json', {'returncode': code or 1, 'finished_at': now(), 'run_id': run['id'], 'generation': run['generation'], 'error': type(error).__name__})
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
