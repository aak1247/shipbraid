#!/usr/bin/env python3
"""Read-only failure observer for an administrator-fenced unstarted dispatch.

A missing unit is never a stop proof. Recovery additionally requires a durable
mask, the original dispatch lock and request, no worker process, and two stable
supervisor observations. This observer neither starts nor cancels a deployment.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import uuid

import bridge

USER_UNIT_DIR = Path.home() / '.config/systemd/user'
FIELDS = ('Id', 'LoadState', 'ActiveState', 'SubState', 'MainPID',
          'ControlGroup', 'Job', 'FragmentPath')


def _sha(value):
    return hashlib.sha256(value).hexdigest()


def _supervisor(unit):
    output = subprocess.check_output(
        ['systemctl', '--user', 'show', unit,
         *[item for key in FIELDS for item in ('-p', key)]],
        text=True, stderr=subprocess.DEVNULL, timeout=10)
    return dict(line.split('=', 1) for line in output.splitlines() if '=' in line)


def _mask_identity(path):
    info = path.lstat()
    if (not stat.S_ISLNK(info.st_mode) or info.st_uid != os.getuid()
            or os.readlink(path) != '/dev/null' or path.resolve(strict=True) != Path('/dev/null')):
        raise RuntimeError('the exact job unit has no administrator-owned persistent mask')
    return {'path': str(path), 'target': '/dev/null', 'uid': info.st_uid,
            'inode': info.st_ino, 'mtime_ns': info.st_mtime_ns}


def _matching_workers(request_path, run_id):
    matches = []
    for item in Path('/proc').iterdir():
        if not item.name.isdecimal():
            continue
        try:
            if item.stat().st_uid != os.getuid():
                continue
            arguments = (item / 'cmdline').read_bytes().split(b'\0')
        except (FileNotFoundError, ProcessLookupError):
            continue
        # Unreadable processes belonging to this user are unproven, not empty.
        if b'--worker' in arguments and (
                os.fsencode(request_path) in arguments
                or any(run_id.encode() in argument for argument in arguments)):
            matches.append(int(item.name))
    return matches


def _check_supervisor(state, unit):
    if (any(key not in state for key in FIELDS)
            or state['Id'] != unit or state['LoadState'] != 'masked'
            or state['ActiveState'] != 'inactive' or state['SubState'] != 'dead'
            or state['MainPID'] != '0' or state['ControlGroup'] != ''
            or state['Job'] not in ('', '0')
            or Path(state['FragmentPath']).resolve(strict=True) != Path('/dev/null')):
        raise RuntimeError('supervisor has not confirmed an idle masked admission boundary')


def observe(envelope, state_dir, recovery_record):
    record_bytes = Path(recovery_record).read_bytes()
    if len(record_bytes) > 16 * 1024:
        raise RuntimeError('recovery record is oversized')
    record = json.loads(record_bytes)
    run, lane = envelope['run'], envelope['lane']
    identifier = str(uuid.UUID(record['run_id']))
    generation = record['generation']
    unit = f'shipbraid-autotask-prod-{identifier}-{generation}.service'
    if (envelope.get('version') != 1 or envelope.get('operation') != 'observe'
            or isinstance(generation, bool) or not isinstance(generation, int) or generation < 1
            or run.get('id') != identifier or run.get('generation') != generation
            or run.get('target') != record['target'] or record['unit'] != unit
            or run.get('executor_ref') not in (None, unit)
            or run.get('lane_id') != 'autotask-prod' or lane.get('id') != 'autotask-prod'
            or lane.get('project') != 'autotask' or lane.get('environment') != 'prod'
            or lane.get('profile_digest') != record['target']['profile_digest']):
        raise RuntimeError('recovery proof does not match the frozen production run')
    directory = Path(state_dir).resolve(strict=True) / identifier
    if directory.is_symlink() or not directory.is_dir():
        raise RuntimeError('durable job directory is unavailable')
    request_path = directory / 'request.json'
    lock_path = directory / 'dispatch.lock'
    if request_path.is_symlink() or lock_path.is_symlink():
        raise RuntimeError('durable dispatch files are not regular owned files')
    mask_path = USER_UNIT_DIR / unit
    with lock_path.open('rb') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if set(item.name for item in directory.iterdir()) != {'request.json', 'dispatch.lock'}:
            raise RuntimeError('job has execution artifacts; unstarted recovery is not applicable')
        raw = request_path.read_bytes()
        if _sha(raw) != record['request_sha256']:
            raise RuntimeError('the original dispatch input changed')
        original = json.loads(raw)
        if (original.get('run', {}).get('id') != identifier
                or original['run'].get('generation') != generation
                or original['run'].get('target') != record['target']
                or original.get('lane', {}).get('id') != lane['id']):
            raise RuntimeError('durable dispatch identity does not match')
        mask = _mask_identity(mask_path)
        first = _supervisor(unit)
        _check_supervisor(first, unit)
        if _matching_workers(request_path, identifier):
            raise RuntimeError('a matching worker process still exists')
        second = _supervisor(unit)
        _check_supervisor(second, unit)
        if (first != second or _mask_identity(mask_path) != mask
                or _matching_workers(request_path, identifier)
                or request_path.read_bytes() != raw
                or set(item.name for item in directory.iterdir()) != {'request.json', 'dispatch.lock'}):
            raise RuntimeError('admission or execution evidence changed during recovery')
        evidence = {'kind': 'unstarted_dispatch_admission_fenced',
                    'run_id': identifier, 'generation': generation,
                    'request_sha256': record['request_sha256'],
                    'recovery_record_sha256': _sha(record_bytes),
                    'fenced_at': record['fenced_at'], 'persistent_mask': mask,
                    'supervisor': second, 'dispatch_lock_held': True,
                    'matching_worker_pids': [], 'execution_artifacts_absent': True}
        return {'version': 1, 'run_id': identifier, 'generation': generation,
                'executor_ref': unit, 'status': 'failed',
                'environment_known_unchanged': False,
                'failure_proof': {'observed_at': bridge.now(), 'evidence': evidence},
                'message': 'Unstarted production handoff fenced and independently verified; no deployment success is claimed'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state-dir', type=Path, required=True)
    parser.add_argument('--recovery-record', type=Path, required=True)
    args = parser.parse_args()
    envelope = json.load(sys.stdin)
    try:
        result = observe(envelope, args.state_dir, args.recovery_record)
    except Exception:
        run = envelope.get('run', {})
        result = {'version': 1, 'run_id': run.get('id'), 'generation': run.get('generation'),
                  'executor_ref': run.get('executor_ref'), 'status': 'unknown',
                  'message': 'Independent fenced-admission recovery could not prove a stop'}
    print(json.dumps(result))


if __name__ == '__main__':
    main()
