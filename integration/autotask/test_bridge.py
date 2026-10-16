#!/usr/bin/env python3
"""Bridge safety tests; mocked supervisor, no real AutoTask writes."""
import copy
import importlib.util
import json
import io
import os
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch
import uuid

spec = importlib.util.spec_from_file_location('autotask_bridge', Path(__file__).with_name('bridge.py'))
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class BridgeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.args = SimpleNamespace(state_dir=Path(self.temp.name), repository=Path('/fixture/repo'), deploy_checkout=Path('/fixture/deployed'), deploy_script=Path('/fixture/release/deploy-local.sh'))
        self.identifier = str(uuid.uuid4())
        self.envelope = {'version': 1, 'operation': 'dispatch', 'command': {'id': str(uuid.uuid4())},
            'run': {'id': self.identifier, 'lane_id': 'autotask-dev', 'generation': 1, 'executor_ref': None,
                'target': {'revision': 'a' * 40, 'components': ['app'], 'profile_digest': 'frozen-profile'}},
            'lane': {'id': 'autotask-dev', 'project': 'autotask', 'environment': 'dev'}}
        _, _, self.directory, self.unit = bridge.context(self.envelope, self.args)
        self.directory.mkdir()

    def tearDown(self):
        self.temp.cleanup()

    def record(self):
        bridge.atomic_json(self.directory / 'request.json', self.envelope)

    def test_lost_supervisor_identity_never_replays_dispatch(self):
        self.record()
        with patch.object(bridge, 'unit_state', return_value={'LoadState': 'not-found'}), patch.object(bridge.subprocess, 'run') as start:
            result = bridge.hook(self.envelope, self.args)
        self.assertEqual(result['status'], 'unknown')
        start.assert_not_called()

    def test_duplicate_dispatch_observes_existing_supervisor(self):
        self.record()
        with patch.object(bridge, 'unit_state', return_value={'LoadState': 'loaded', 'SubState': 'running'}), patch.object(bridge.subprocess, 'run') as start:
            result = bridge.hook(self.envelope, self.args)
        self.assertEqual(result['status'], 'started')
        self.assertEqual(result['executor_ref'], self.unit)
        start.assert_not_called()

    def test_frozen_target_and_environment_cannot_be_replaced(self):
        self.record()
        changed = copy.deepcopy(self.envelope)
        changed['run']['target']['revision'] = 'b' * 40
        with self.assertRaises(RuntimeError):
            bridge.hook(changed, self.args)
        changed = copy.deepcopy(self.envelope)
        changed['lane']['environment'] = 'prod'
        with self.assertRaises(RuntimeError):
            bridge.hook(changed, self.args)

    def test_successful_exit_only_requests_independent_verification(self):
        self.record()
        bridge.atomic_json(self.directory / 'result.json', {'returncode': 0, 'run_id': self.identifier, 'generation': 1, 'proof': {'observed': 'original'}})
        state = {'LoadState': 'loaded', 'SubState': 'exited', 'Result': 'success', 'ExecMainStatus': '0'}
        self.envelope['operation'] = 'observe'
        with patch.object(bridge, 'unit_state', return_value=state), patch.object(bridge, 'environment_proof') as verify:
            result = bridge.hook(self.envelope, self.args)
        self.assertEqual(result['status'], 'completed')
        self.assertNotIn('receipt', result)
        verify.assert_not_called()
        self.envelope['operation'] = 'verify'
        with patch.object(bridge, 'unit_state', return_value=state), patch.object(bridge, 'environment_proof', return_value={'observed': 'changed'}):
            result = bridge.hook(self.envelope, self.args)
        self.assertEqual(result['status'], 'unknown')
        self.assertNotIn('receipt', result)
        with patch.object(bridge, 'unit_state', return_value=state), patch.object(bridge, 'environment_proof', return_value={'observed': 'original'}):
            result = bridge.hook(self.envelope, self.args)
        self.assertEqual(result['status'], 'verified')
        self.assertEqual(result['receipt']['source_revision'], 'a' * 40)
        self.assertEqual(result['receipt']['profile_digest'], 'frozen-profile')

    def test_worker_uses_pinned_script_and_cannot_inherit_build_skips(self):
        self.record()
        proof = {'services': {'backend': {'unit': 'autotask-backend.service', 'pid': 2}}}
        with patch.dict(os.environ, {'AUTOTASK_SKIP_BACKEND_REBUILD': '1', 'AUTOTASK_LOCAL_DEPLOY_SKIP_WORKER_BUILD': '1'}), \
             patch.object(bridge, 'git', return_value=''), \
             patch.object(bridge, 'unit_state', return_value={'MainPID': '1'}), \
             patch.object(bridge, 'environment_proof', return_value=proof), \
             patch.object(bridge.subprocess, 'run', return_value=SimpleNamespace(returncode=0)) as execute, \
             patch('sys.stdout', io.StringIO()):
            code = bridge.worker(self.directory / 'request.json', self.args)
        self.assertEqual(code, 0)
        self.assertEqual(execute.call_args.args[0][1], str(self.args.deploy_script))
        env = execute.call_args.kwargs['env']
        self.assertEqual(env['AUTOTASK_SKIP_BACKEND_REBUILD'], '0')
        self.assertEqual(env['AUTOTASK_LOCAL_DEPLOY_SKIP_WORKER_BUILD'], '0')
        self.assertEqual(env['AUTOTASK_COORDINATED_DEPLOY'], '1')
        self.assertEqual(env['AUTOTASK_DEPLOY_REPO'], str(self.args.deploy_checkout))

    def test_log_offsets_are_stable_bounded_and_credentials_redacted(self):
        path = self.directory / 'deploy.log'
        path.write_text('building\nAuthorization: Bearer fixture-credential\nfinished\n')
        result = bridge.logs(path)
        self.assertEqual([entry['sequence'] for entry in result], sorted({entry['sequence'] for entry in result}))
        self.assertEqual(result, bridge.logs(path))
        self.assertNotIn('fixture-credential', json.dumps(result))
        self.assertIn('[REDACTED]', json.dumps(result))


if __name__ == '__main__':
    unittest.main()
