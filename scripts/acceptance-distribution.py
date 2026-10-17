#!/usr/bin/env python3
"""Exercise an extracted binary bundle in a private synthetic environment, never a real project."""
import argparse
from contextlib import contextmanager
import hashlib
import hmac
import json
import os
from pathlib import Path, PurePosixPath
import secrets
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
import uuid


@contextmanager
def private_workspace():
    root = Path(tempfile.mkdtemp(prefix='shipbraid-distribution-'))
    try:
        yield root
    except BaseException:
        # Preserve task identities and private evidence if a worker's stop is uncertain.
        print('Failed acceptance retained private evidence at ' + str(root), file=sys.stderr)
        raise
    else:
        for directory in root.rglob('*'):
            if directory.is_dir() and not directory.is_symlink():
                directory.chmod(0o700)
        shutil.rmtree(root)


def extract_bundle(archive, destination):
    names = set()
    roots = set()
    total_bytes = 0
    with tarfile.open(archive, 'r:gz') as handle:
        for member in handle:
            name = PurePosixPath(member.name)
            if (name.is_absolute() or '..' in name.parts or not name.parts
                    or str(name) in names or not (member.isdir() or member.isfile())):
                raise ValueError('unsafe or duplicate release archive member')
            names.add(str(name))
            roots.add(name.parts[0])
            total_bytes += member.size
            if member.size > 256 * 1024 * 1024 or total_bytes > 1024 * 1024 * 1024:
                raise ValueError('release archive exceeds acceptance size limits')
            path = destination.joinpath(*name.parts)
            if member.isdir():
                path.mkdir(parents=True, exist_ok=True)
            else:
                path.parent.mkdir(parents=True, exist_ok=True)
                with handle.extractfile(member) as source, path.open('xb') as output:
                    while block := source.read(1024 * 1024):
                        output.write(block)
                path.chmod(member.mode & 0o755)
    if len(roots) != 1:
        raise ValueError('release archive must contain one root directory')
    return destination / roots.pop()


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def wait_for(check, timeout=30):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = check()
            if last:
                return last
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.1)
    raise AssertionError('acceptance condition timed out: ' + repr(last))


def request(url, body=None, headers=None, expected=200):
    data = None if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
    req = urllib.request.Request(url, data=data, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=15) as response:
            status, raw = response.status, response.read(1024 * 1024)
    except urllib.error.HTTPError as error:
        status, raw = error.code, error.read(1024 * 1024)
    assert status == expected, (url, status, expected, raw.decode(errors='replace'))
    return json.loads(raw)


def stop_process(process):
    if process is not None and process.poll() is None:
        process.terminate()
        process.wait(timeout=20)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bundle', type=Path, required=True)
    parser.add_argument('--report', type=Path, help='write a secret-free JSON acceptance report')
    args = parser.parse_args()
    archive = args.bundle.resolve(strict=True)
    # This check is mandatory. Missing supervisor support is a failure, never a skipped E2E.
    subprocess.run(['systemctl', '--user', 'show-environment'], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    assert Path('/sys/fs/cgroup/cgroup.controllers').is_file(), 'cgroup v2 is required'
    report = {'synthetic_only': True, 'real_project_deployments': 0, 'checks': []}
    with private_workspace() as root:
        bundle = extract_bundle(archive, root / 'unpacked')
        source = root / 'source'
        subprocess.run(['git', 'init', '-qb', 'main', str(source)], check=True)
        revisions = []
        for index in range(2):
            subprocess.run(['git', '-C', str(source), '-c', 'user.name=Bundle acceptance', '-c',
                            'user.email=fixture@example.invalid', 'commit', '--allow-empty',
                            '-qm', f'synthetic revision {index}'], check=True)
            revisions.append(subprocess.check_output(['git', '-C', str(source), 'rev-parse', 'HEAD'],
                                                      text=True).strip())
        mirror = root / 'source.git'
        subprocess.run(['git', 'clone', '-q', '--mirror', str(source), str(mirror)], check=True)
        prefix = root / 'installation with spaces'
        port = free_port()
        url = f'http://127.0.0.1:{port}'
        installation = subprocess.run([
            sys.executable, str(bundle / 'install.py'), '--prefix', str(prefix),
            '--repository', str(mirror), '--project', 'bundle', '--environment', 'test',
            '--project-bundle', str(bundle / 'examples/standalone/project'),
            '--listen', f'127.0.0.1:{port}',
        ], check=True, text=True, capture_output=True)
        record = json.loads(installation.stdout)
        assert not record['service_started'] and not record['deployment_submitted']
        release = Path(record['release'])
        binary = prefix / 'bin/shipbraid'
        demo = root / 'fake-environment'
        (prefix / 'etc/deploy.env').write_text(
            f'SHIPBRAID_DEMO_ROOT={demo}\nSHIPBRAID_DEMO_DELAY_SECONDS=3\n')
        assert (prefix / 'etc/deploy.env').stat().st_mode & 0o777 == 0o600
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(('SHIPBRAID_', 'AUTO_DELIVER_'))}
        env['PYTHONDONTWRITEBYTECODE'] = '1'
        for line in (prefix / 'etc/tokens.env').read_text().splitlines():
            key, value = line.split('=', 1)
            env[key] = value
        token = (prefix / 'etc/operator.token').read_text().strip()
        auth = {'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'}
        cli_env = {**env, 'SHIPBRAID_URL': url,
                   'SHIPBRAID_TOKEN_FILE': str(prefix / 'etc/operator.token')}
        service = receiver = hook_server = failure_service = None
        units = []
        log = (root / 'acceptance.log').open('w+')
        passed = False

        def start_service():
            process = subprocess.Popen([str(binary), 'serve', '--config', str(prefix / 'etc/config.toml')],
                                       env=env, stdout=log, stderr=log)
            def ready():
                if process.poll() is not None:
                    log.flush()
                    log.seek(0)
                    raise AssertionError('bundle service failed: ' + log.read())
                return request(url + '/ready')
            wait_for(ready)
            return process

        def cli(*arguments, expected=0):
            result = subprocess.run([str(binary), '--json', *arguments], env=cli_env,
                                    text=True, capture_output=True, timeout=40)
            assert result.returncode == expected, (arguments, result.returncode, result.stderr, result.stdout)
            return json.loads(result.stdout)

        def get_run(identifier):
            return request(url + '/v1/runs/' + identifier, headers=auth)

        def stopped(unit):
            state = subprocess.check_output(['systemctl', '--user', 'show', unit, '-p', 'MainPID',
                                             '-p', 'SubState'], text=True)
            return 'MainPID=0' in state and ('SubState=exited' in state or 'SubState=failed' in state)

        try:
            service = start_service()
            assert cli('system')['api_version'] == '1'
            with urllib.request.urlopen(url, timeout=5) as response:
                assert b'<html' in response.read(64000).lower()
            report['checks'].append('installed verified bundle serves CLI/API and prebuilt Dashboard without source toolchains')
            submitted = cli('submit', '--lane', 'bundle-test', '--revision', revisions[0],
                            '--components', 'app', '--idempotency-key', 'bundle-restart')
            identifier = submitted['id']
            marker = demo / 'deployment-starts'
            wait_for(lambda: marker.exists() and identifier is not None)
            run = wait_for(lambda: cli('inspect', 'request', identifier).get('current_run_id'))
            current = get_run(run)
            unit = current.get('executor_ref')
            if unit:
                units.append(unit)
            # The job is still running independently while this coordinator stops and restarts.
            stop_process(service)
            service = start_service()
            completed = cli('wait', identifier, '--timeout-seconds', '30')
            assert completed['status'] == 'satisfied', completed
            inspected = cli('inspect', 'request', identifier)
            receipt = inspected['receipt']
            assert receipt['source_revision'] == revisions[0]
            assert receipt['component_digests']['app'] == hashlib.sha256((demo / 'app-artifact.json').read_bytes()).hexdigest()
            assert marker.read_text().splitlines() == [run]
            replayed = cli('submit', '--lane', 'bundle-test', '--revision', revisions[0],
                           '--components', 'app', '--idempotency-key', 'bundle-restart')
            assert replayed['id'] == identifier and replayed['reused']
            run_view = get_run(run)
            unit = run_view['executor_ref']
            if unit not in units:
                units.append(unit)
            wait_for(lambda: stopped(unit))
            report['checks'].append('coordinator restart during an independent systemd deployment preserves one dispatch and a verified receipt')

            # Replay the retained immutable envelope as a read-only verify after changing actual environment.
            request_files = list((prefix / 'var/runs' / run).rglob('request.json'))
            assert len(request_files) == 1, request_files
            durable = json.loads(request_files[0].read_text())
            envelope = durable['envelope']
            envelope['operation'] = 'verify'
            envelope['command'] = None
            envelope['run']['executor_ref'] = unit
            original = (demo / 'health.json').read_text()
            (demo / 'health.json').write_text('{"healthy":false,"synthetic":true}')
            rejected = subprocess.run([
                sys.executable, str(release / 'adapters/local/adapter.py'), '--config',
                str(release / 'adapter.json')], input=json.dumps(envelope),
                capture_output=True, text=True, check=True, env=env, timeout=20)
            rejection = json.loads(rejected.stdout)
            assert rejection['status'] == 'unknown' and 'receipt' not in rejection, rejection
            (demo / 'health.json').write_text(original)
            report['checks'].append('fresh environment verification rejects unhealthy/tampered actual state')

            hook_port = free_port()
            hook_token = secrets.token_hex(32)
            hook_server = subprocess.Popen([
                sys.executable, str(release / 'adapters/local/adapter.py'), '--config',
                str(release / 'adapter.json'), '--listen', f'127.0.0.1:{hook_port}',
                '--token-env', 'SHIPBRAID_HOOK_FIXTURE_TOKEN'],
                env={**env, 'SHIPBRAID_HOOK_FIXTURE_TOKEN': hook_token}, stdout=log, stderr=log)
            def hook_ready():
                with socket.create_connection(('127.0.0.1', hook_port), timeout=1):
                    return True
            wait_for(hook_ready)
            hook_endpoint = f'http://127.0.0.1:{hook_port}/hook'
            try:
                urllib.request.urlopen(urllib.request.Request(hook_endpoint, data=b'{}'), timeout=5)
                raise AssertionError('unauthenticated hook request was accepted')
            except urllib.error.HTTPError as error:
                assert error.code == 401
                error.close()
            remote_verified = request(hook_endpoint, envelope,
                                      {'Authorization': 'Bearer ' + hook_token, 'Content-Type': 'application/json'})
            assert remote_verified['status'] == 'verified'
            assert remote_verified['receipt']['source_revision'] == revisions[0]
            report['checks'].append('packaged HTTP /hook rejects missing credentials and returns a fresh matching verification receipt')

            # Start the packaged GitHub receiver, with a dedicated lane-scoped operator identity.
            receiver_port = free_port()
            webhook_secret = secrets.token_hex(32)
            webhook_config = root / 'github.json'
            webhook_config.write_text(json.dumps({
                'listen_host': '127.0.0.1', 'listen_port': receiver_port, 'path': '/github',
                'shipbraid_url': url, 'timeout_seconds': 10, 'secret_env': 'GITHUB_FIXTURE_SECRET',
                'routes': [{'id': 'bundle-route-v1', 'repository_id': 12345,
                            'ref': 'refs/heads/main', 'lane_id': 'bundle-test',
                            'components': ['app'], 'token_env': 'GITHUB_FIXTURE_TOKEN'}],
            }))
            receiver = subprocess.Popen([
                sys.executable, str(release / 'adapters/github/receiver.py'), '--config', str(webhook_config)],
                env={**env, 'GITHUB_FIXTURE_SECRET': webhook_secret, 'GITHUB_FIXTURE_TOKEN': token},
                stdout=log, stderr=log)
            endpoint = f'http://127.0.0.1:{receiver_port}/github'
            def receiver_ready():
                with socket.create_connection(('127.0.0.1', receiver_port), timeout=1):
                    return True
            wait_for(receiver_ready)
            payload = json.dumps({'repository': {'id': 12345}, 'ref': 'refs/heads/main',
                                  'after': revisions[1], 'deleted': False}).encode()
            delivery = str(uuid.uuid4())
            signed = {'Content-Type': 'application/json', 'X-GitHub-Event': 'push',
                      'X-GitHub-Delivery': delivery,
                      'X-Hub-Signature-256': 'sha256=' + hmac.new(webhook_secret.encode(), payload, hashlib.sha256).hexdigest()}
            request(endpoint, payload, {**signed, 'X-Hub-Signature-256': 'sha256=' + '0' * 64}, expected=401)
            request(endpoint, payload, signed, expected=202)
            request(endpoint, payload, signed, expected=202)
            listed = cli('list', 'request', '--lane', 'bundle-test')['items']
            webhook_requests = [item for item in listed if item['reason'].startswith('GitHub delivery ')]
            assert len(webhook_requests) == 1, webhook_requests
            webhook_request = webhook_requests[0]
            webhook_completed = cli('wait', webhook_request['id'], '--timeout-seconds', '30')
            assert webhook_completed['status'] == 'satisfied'
            webhook_run = cli('inspect', 'request', webhook_request['id'])['satisfied_by_run']
            webhook_unit = get_run(webhook_run)['executor_ref']
            units.append(webhook_unit)
            wait_for(lambda: stopped(webhook_unit))
            assert marker.read_text().splitlines() == [run, webhook_run]
            report['checks'].append('packaged signed GitHub push receiver rejects bad signatures and deduplicates repeated delivery through the real coordinator')

            # A separate synthetic lane performs no writes and exits nonzero. It can
            # fail requests only after the independent supervisor proves the job stopped.
            failure_bundle = root / 'failure-scripts'
            failure_bundle.mkdir(mode=0o700)
            (failure_bundle / 'deploy.sh').write_text('#!/bin/bash\nexit 7\n')
            (failure_bundle / 'verify.py').write_text('raise SystemExit("must never verify a failed task")\n')
            failure_prefix = root / 'failure-installation'
            failure_port = free_port()
            failure_url = f'http://127.0.0.1:{failure_port}'
            subprocess.run([
                sys.executable, str(bundle / 'install.py'), '--prefix', str(failure_prefix),
                '--repository', str(mirror), '--project', 'failure', '--environment', 'test',
                '--project-bundle', str(failure_bundle), '--listen', f'127.0.0.1:{failure_port}',
            ], check=True, text=True, capture_output=True)
            failure_env = {key: value for key, value in env.items()
                           if key not in ('SHIPBRAID_OPERATOR_TOKEN', 'SHIPBRAID_ADMIN_TOKEN')}
            for line in (failure_prefix / 'etc/tokens.env').read_text().splitlines():
                key, value = line.split('=', 1)
                failure_env[key] = value
            failure_service = subprocess.Popen([
                str(failure_prefix / 'bin/shipbraid'), 'serve', '--config', str(failure_prefix / 'etc/config.toml')],
                env=failure_env, stdout=log, stderr=log)
            wait_for(lambda: request(failure_url + '/ready'))
            failed_token = (failure_prefix / 'etc/operator.token').read_text().strip()
            failed_auth = {'Authorization': 'Bearer ' + failed_token, 'Content-Type': 'application/json',
                           'Idempotency-Key': 'bundle-proven-failure'}
            failed_request = request(failure_url + '/v1/requests',
                                     {'lane_id': 'failure-test', 'revision': revisions[0], 'components': ['app'],
                                      'coverage_mode': 'exact', 'priority': 'normal'}, failed_auth, expected=202)
            def failure_settled():
                observation = request(failure_url + '/v1/requests/' + failed_request['id'], headers=failed_auth)
                return observation if observation['status'] == 'needs_attention' else None
            failed = wait_for(failure_settled)
            assert failed['receipt'] is None and failed['satisfied_by_run'] is None
            events = request(failure_url + '/v1/events?lane_id=failure-test', headers=failed_auth)
            stopped_events = [event for event in events['items'] if event['kind'] == 'failure_stop_verified']
            assert len(stopped_events) == 1
            failed_run = request(failure_url + '/v1/runs/' + stopped_events[0]['run_id'], headers=failed_auth)
            units.append(failed_run['executor_ref'])
            assert failed_run['state'] == 'failed'
            failed_lane = request(failure_url + '/v1/lanes/failure-test', headers=failed_auth)
            assert failed_lane['failure_paused'] and failed_lane['active_run_id'] is None
            report['checks'].append('a nonzero deployment exits under systemd, produces fresh independent stop proof, pauses the lane, and never satisfies the request')
            assert not (prefix / 'etc/config.toml').stat().st_mode & 0o077
            report['version'] = json.loads((bundle / 'manifest.json').read_text())['version']
            report['run_ids'] = {'restart': run, 'github': webhook_run, 'failure': failed_run['id']}
            report['receipt_ids'] = [receipt['id'], webhook_completed['receipt']['id']]
            if args.report:
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(json.dumps(report, indent=2) + '\n')
            print(json.dumps(report, indent=2))
            passed = True
        finally:
            stop_process(receiver)
            stop_process(hook_server)
            stop_process(failure_service)
            stop_process(service)
            # Stop only proven exited synthetic units. A failed test must never kill an unknown worker.
            for unit in units:
                if passed and stopped(unit):
                    subprocess.run(['systemctl', '--user', 'stop', unit], check=False,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                    subprocess.run(['systemctl', '--user', 'reset-failed', unit], check=False,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            log.close()


if __name__ == '__main__':
    main()
