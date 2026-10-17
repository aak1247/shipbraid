#!/usr/bin/env python3
"""Admission fencing must never turn an unproven absence into a stop proof."""
import copy
import fcntl
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import uuid

import recover_unstarted as recovery


class RecoveryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.identifier = str(uuid.uuid4())
        self.unit = f'shipbraid-autotask-prod-{self.identifier}-1.service'
        self.directory = self.root / self.identifier
        self.directory.mkdir()
        self.mask_dir = self.root / 'units'
        self.mask_dir.mkdir()
        self.mask = self.mask_dir / self.unit
        self.mask.symlink_to('/dev/null')
        self.envelope = {
            'version': 1, 'operation': 'observe',
            'run': {'id': self.identifier, 'generation': 1, 'executor_ref': None,
                    'lane_id': 'autotask-prod', 'target': {'profile_digest': 'f' * 64,
                    'revision': 'a' * 40, 'parameters': {'database_backup': 'skip'}}},
            'lane': {'id': 'autotask-prod', 'project': 'autotask',
                     'environment': 'prod', 'profile_digest': 'f' * 64}}
        original = copy.deepcopy(self.envelope)
        original['operation'] = 'dispatch'
        self.raw = json.dumps(original).encode()
        (self.directory / 'request.json').write_bytes(self.raw)
        (self.directory / 'dispatch.lock').touch()
        self.record = self.root / 'recovery.json'
        self.record.write_text(json.dumps({
            'run_id': self.identifier, 'generation': 1, 'unit': self.unit,
            'target': self.envelope['run']['target'],
            'request_sha256': hashlib.sha256(self.raw).hexdigest(),
            'fenced_at': recovery.bridge.now()}))
        self.state = {'Id': self.unit, 'LoadState': 'masked', 'ActiveState': 'inactive',
                      'SubState': 'dead', 'MainPID': '0', 'ControlGroup': '',
                      'Job': '', 'FragmentPath': str(self.mask)}
        self.addCleanup(patch.stopall)
        patch.object(recovery, 'USER_UNIT_DIR', self.mask_dir).start()
        self.supervisor = patch.object(recovery, '_supervisor', return_value=self.state).start()
        self.workers = patch.object(recovery, '_matching_workers', return_value=[]).start()

    def observe(self):
        return recovery.observe(self.envelope, self.root, self.record)

    def test_fresh_failure_proof_preserves_original_and_does_not_claim_success(self):
        reply = self.observe()
        self.assertEqual(reply['status'], 'failed')
        self.assertEqual(reply['executor_ref'], self.unit)
        self.assertNotIn('receipt', reply)
        self.assertIn('observed_at', reply['failure_proof'])
        self.assertFalse(reply['environment_known_unchanged'])
        self.assertEqual((self.directory / 'request.json').read_bytes(), self.raw)
        self.assertEqual({p.name for p in self.directory.iterdir()}, {'request.json', 'dispatch.lock'})
        self.assertEqual(self.supervisor.call_count, 2)
        self.assertEqual(self.workers.call_count, 2)

    def test_missing_mask_never_proves_stop(self):
        self.mask.unlink()
        with self.assertRaises(FileNotFoundError):
            self.observe()

    def test_incomplete_or_active_supervisor_never_proves_stop(self):
        for key, value in [('LoadState', 'not-found'), ('MainPID', '42'),
                           ('ControlGroup', '/user.slice/job'), ('Job', '12'),
                           ('ActiveState', 'active'), ('SubState', 'running'),
                           ('Id', 'different.service')]:
            with self.subTest(key=key):
                self.supervisor.return_value = dict(self.state, **{key: value})
                with self.assertRaises(RuntimeError):
                    self.observe()
        incomplete = dict(self.state)
        del incomplete['MainPID']
        self.supervisor.return_value = incomplete
        with self.assertRaises(RuntimeError):
            self.observe()

    def test_matching_worker_blocks_recovery(self):
        self.workers.return_value = [42]
        with self.assertRaises(RuntimeError):
            self.observe()

    def test_evidence_changing_between_samples_blocks_recovery(self):
        def revoke_mask(*_):
            self.mask.unlink()
            return []
        self.workers.side_effect = revoke_mask
        with self.assertRaises(FileNotFoundError):
            self.observe()

    def test_existing_execution_artifact_blocks_recovery(self):
        for name in ('result.json', 'deploy.log'):
            with self.subTest(name=name):
                path = self.directory / name
                path.touch()
                with self.assertRaises(RuntimeError):
                    self.observe()
                path.unlink()

    def test_busy_dispatch_lock_blocks_recovery(self):
        with (self.directory / 'dispatch.lock').open('rb') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaises(BlockingIOError):
                self.observe()

    def test_mismatched_fence_or_modified_request_blocks_recovery(self):
        for key, value in [('id', str(uuid.uuid4())), ('generation', 2), ('executor_ref', 'other.service')]:
            with self.subTest(key=key):
                original = self.envelope['run'][key]
                self.envelope['run'][key] = value
                with self.assertRaises(RuntimeError):
                    self.observe()
                self.envelope['run'][key] = original
        (self.directory / 'request.json').write_bytes(self.raw + b' ')
        with self.assertRaises(RuntimeError):
            self.observe()

    def test_request_changed_during_observation_blocks_recovery(self):
        calls = 0
        def change_request(*_):
            nonlocal calls
            calls += 1
            if calls == 2:
                (self.directory / 'request.json').write_bytes(self.raw + b' ')
            return []
        self.workers.side_effect = change_request
        with self.assertRaises(RuntimeError):
            self.observe()


if __name__ == '__main__':
    unittest.main()
