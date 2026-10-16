#!/usr/bin/env python3
"""Bridge safety tests; mocked supervisor, no real AutoTask writes."""
import copy
import importlib.util
import json
import io
import os
from pathlib import Path
import sys
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

    def failed_result(self, **overrides):
        result = {'returncode': 7, 'finished_at': bridge.now(), 'run_id': self.identifier,
            'generation': 1, 'target': copy.deepcopy(self.envelope['run']['target']),
            'unit': self.unit, 'invocation_id': 'invocation-1'}
        result.update(overrides)
        return result

    def failed_unit_state(self, **overrides):
        state = {'Id': self.unit, 'InvocationID': 'invocation-1', 'LoadState': 'loaded',
            'ActiveState': 'failed', 'SubState': 'failed', 'MainPID': '0',
            'ExecMainCode': 'exited', 'ExecMainStatus': '7',
            'ExecMainExitTimestamp': bridge.now(), 'Result': 'exit-code',
            'ControlGroup': '', 'ExecStart': ''}
        state.update(overrides)
        return state

    def observe_failure(self, result, first_state, second_state=None):
        self.record()
        bridge.atomic_json(self.directory / 'result.json', result)
        envelope = copy.deepcopy(self.envelope)
        envelope['operation'] = 'observe'
        states = [first_state, second_state or first_state]
        with patch.object(bridge, 'unit_state', side_effect=states):
            return bridge.hook(envelope, self.args)

    def make_cgroup(self, path, pids='', populated=0):
        path.mkdir(parents=True, exist_ok=True)
        (path / 'cgroup.procs').write_text(pids)
        (path / 'cgroup.events').write_text(f'populated {populated}\nfrozen 0\n')

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

    def test_renamed_bridge_observes_existing_legacy_job_identity(self):
        self.assertTrue(self.unit.startswith('shipbraid-autotask-'))
        legacy_unit = f'auto-deliver-autotask-{self.identifier}-1.service'
        self.envelope['run']['executor_ref'] = legacy_unit
        self.record()
        self.envelope['operation'] = 'observe'
        with patch.object(bridge, 'unit_state', return_value={'LoadState': 'loaded', 'SubState': 'running'}) as observe, patch.object(bridge.subprocess, 'run') as start:
            result = bridge.hook(self.envelope, self.args)
        self.assertEqual(result['executor_ref'], legacy_unit)
        observe.assert_called_once_with(legacy_unit)
        start.assert_not_called()
        self.envelope['run']['executor_ref'] = f'auto-deliver-autotask-{uuid.uuid4()}-1.service'
        with self.assertRaises(RuntimeError):
            bridge.context(self.envelope, self.args)

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

    def test_failed_result_has_fresh_proof_only_after_supervisor_stops(self):
        result = self.failed_result()
        state = self.failed_unit_state()
        for operation in ('observe', 'verify'):
            with self.subTest(operation=operation):
                self.record()
                bridge.atomic_json(self.directory / 'result.json', result)
                envelope = copy.deepcopy(self.envelope)
                envelope['operation'] = operation
                with patch.object(bridge, 'unit_state', side_effect=[state, state]):
                    reply = bridge.hook(envelope, self.args)
                self.assertEqual(reply['status'], 'failed')
                self.assertIs(reply['environment_known_unchanged'], False)
                proof = reply['failure_proof']
                observed = __import__('datetime').datetime.fromisoformat(proof['observed_at'].replace('Z', '+00:00'))
                self.assertEqual(observed.utcoffset().total_seconds(), 0)
                self.assertEqual(proof['evidence']['unit'], self.unit)
                self.assertEqual(proof['evidence']['result']['returncode'], 7)
                self.assertEqual(proof['evidence']['process_check']['state'], 'destroyed')
                self.assertLessEqual(len(json.dumps(proof['evidence']).encode()), 8192)

    def test_failure_proof_accepts_empty_and_destroyed_cgroups(self):
        root = Path(self.temp.name) / 'cgroup'
        group = root / 'user.slice' / 'job.service'
        self.make_cgroup(group)
        self.make_cgroup(group / 'empty-child')
        with patch.object(bridge, 'CGROUP_ROOT', root):
            for control_group, expected in (
                ('/user.slice/job.service', 'empty'),
                ('/user.slice/removed.service', 'destroyed'),
            ):
                with self.subTest(control_group=control_group):
                    state = self.failed_unit_state(ControlGroup=control_group)
                    reply = self.observe_failure(self.failed_result(), state)
                    self.assertEqual(reply['status'], 'failed')
                    self.assertEqual(reply['failure_proof']['evidence']['process_check']['state'], expected)
                    if expected == 'empty':
                        self.assertEqual(reply['failure_proof']['evidence']['process_check']['groups_scanned'], 2)

    def test_empty_parent_with_live_descendant_blocks_failure_proof(self):
        root = Path(self.temp.name) / 'cgroup'
        group = root / 'user.slice' / 'job.service'
        self.make_cgroup(group, populated=1)
        self.make_cgroup(group / 'child' / 'grandchild', pids='4321\n', populated=1)
        state = self.failed_unit_state(ControlGroup='/user.slice/job.service')
        with patch.object(bridge, 'CGROUP_ROOT', root):
            reply = self.observe_failure(self.failed_result(), state)
        self.assertEqual(reply['status'], 'unknown')
        self.assertNotIn('failure_proof', reply)

    def test_recursive_scan_checks_child_cgroup_processes(self):
        root = Path(self.temp.name) / 'cgroup'
        group = root / 'user.slice' / 'job.service'
        self.make_cgroup(group, populated=0)
        self.make_cgroup(group / 'child' / 'grandchild', pids='4321\n', populated=0)
        state = self.failed_unit_state(ControlGroup='/user.slice/job.service')
        with patch.object(bridge, 'CGROUP_ROOT', root):
            reply = self.observe_failure(self.failed_result(), state)
        self.assertEqual(reply['status'], 'unknown')
        self.assertNotIn('failure_proof', reply)

    def test_symlink_or_unreadable_cgroup_child_blocks_failure_proof(self):
        root = Path(self.temp.name) / 'cgroup'
        group = root / 'user.slice' / 'job.service'
        self.make_cgroup(group)
        external = Path(self.temp.name) / 'outside'
        self.make_cgroup(external)
        (group / 'child-link').symlink_to(external, target_is_directory=True)
        state = self.failed_unit_state(ControlGroup='/user.slice/job.service')
        with patch.object(bridge, 'CGROUP_ROOT', root):
            reply = self.observe_failure(self.failed_result(), state)
        self.assertEqual(reply['status'], 'unknown')
        self.assertNotIn('failure_proof', reply)

        (group / 'child-link').unlink()
        child = group / 'child'
        self.make_cgroup(child)
        original = Path.read_text

        def unreadable(path, *args, **kwargs):
            if path == child / 'cgroup.procs':
                raise PermissionError('fixture unreadable')
            return original(path, *args, **kwargs)

        with patch.object(bridge, 'CGROUP_ROOT', root), patch.object(Path, 'read_text', new=unreadable):
            reply = self.observe_failure(self.failed_result(), state)
        self.assertEqual(reply['status'], 'unknown')
        self.assertNotIn('failure_proof', reply)

    def test_cgroup_tree_change_between_scans_blocks_failure_proof(self):
        root = Path(self.temp.name) / 'cgroup'
        group = root / 'user.slice' / 'job.service'
        child = group / 'child'
        self.make_cgroup(group)
        self.make_cgroup(child)
        state = self.failed_unit_state(ControlGroup='/user.slice/job.service')
        original = Path.read_text
        root_event_reads = 0

        def mutate_after_first_scan(path, *args, **kwargs):
            nonlocal root_event_reads
            value = original(path, *args, **kwargs)
            if path == group / 'cgroup.events':
                root_event_reads += 1
                if root_event_reads == 4:
                    (child / 'cgroup.procs').write_text('9876\n')
            return value

        with patch.object(bridge, 'CGROUP_ROOT', root), patch.object(Path, 'read_text', new=mutate_after_first_scan):
            reply = self.observe_failure(self.failed_result(), state)
        self.assertEqual(reply['status'], 'unknown')
        self.assertNotIn('failure_proof', reply)

    def test_supervisor_race_or_missing_unit_blocks_failure_proof(self):
        state = self.failed_unit_state()
        changed = self.failed_unit_state(InvocationID='replacement-invocation')
        reply = self.observe_failure(self.failed_result(), state, changed)
        self.assertEqual(reply['status'], 'unknown')
        self.assertNotIn('failure_proof', reply)

        self.record()
        bridge.atomic_json(self.directory / 'result.json', self.failed_result())
        envelope = copy.deepcopy(self.envelope)
        envelope['operation'] = 'observe'
        with patch.object(bridge, 'unit_state', side_effect=FileNotFoundError('unit lost')):
            reply = bridge.hook(envelope, self.args)
        self.assertEqual(reply['status'], 'unknown')
        self.assertNotIn('failure_proof', reply)

    def test_missing_cgroup_property_cannot_mean_destroyed_cgroup(self):
        for value in (None, False, 0, 'missing'):
            with self.subTest(value=value):
                state = self.failed_unit_state(ControlGroup=value)
                if value == 'missing':
                    state.pop('ControlGroup')
                reply = self.observe_failure(self.failed_result(), state)
                self.assertEqual(reply['status'], 'unknown')
                self.assertNotIn('failure_proof', reply)

    def test_failure_result_identity_mismatch_never_gets_proof(self):
        state = self.failed_unit_state()
        mismatches = [
            self.failed_result(target={'revision': 'b' * 40}),
            self.failed_result(unit='other.service'),
            self.failed_result(run_id=str(uuid.uuid4())),
            self.failed_result(returncode=0),
        ]
        for result in mismatches:
            with self.subTest(result=result):
                reply = self.observe_failure(result, state)
                self.assertEqual(reply['status'], 'unknown')
                self.assertNotIn('failure_proof', reply)

    def test_legacy_failure_result_is_read_only_and_bound_to_supervisor_invocation(self):
        self.record()
        result = {'run_id': self.identifier, 'generation': 1, 'returncode': 7,
            'finished_at': bridge.now()}
        result_path = self.directory / 'result.json'
        bridge.atomic_json(result_path, result)
        original = result_path.read_bytes()
        state = self.failed_unit_state(ExecMainExitTimestamp=result['finished_at'])
        worker_argv = ' '.join((sys.executable, str(Path(bridge.__file__).resolve()),
            '--worker', str((self.directory / 'request.json').resolve())))
        state['ExecStart'] = f'{{ path={sys.executable} ; argv[]={worker_argv} ; ignore_errors=no }}'
        envelope = copy.deepcopy(self.envelope)
        envelope['operation'] = 'observe'
        with patch.object(bridge, 'unit_state', side_effect=[state, state]):
            reply = bridge.hook(envelope, self.args)
        self.assertEqual(reply['status'], 'failed')
        self.assertIs(reply['environment_known_unchanged'], False)
        self.assertIs(reply['failure_proof']['evidence']['legacy_result_read_only'], True)
        self.assertEqual(result_path.read_bytes(), original)

    def test_legacy_failure_without_exact_worker_identity_has_no_proof(self):
        self.record()
        result = {'run_id': self.identifier, 'generation': 1, 'returncode': 7,
            'finished_at': bridge.now()}
        bridge.atomic_json(self.directory / 'result.json', result)
        state = self.failed_unit_state(ExecMainExitTimestamp=result['finished_at'], ExecStart='argv[]=/usr/bin/other --worker /wrong/request.json')
        envelope = copy.deepcopy(self.envelope)
        envelope['operation'] = 'observe'
        with patch.object(bridge, 'unit_state', side_effect=[state, state]):
            reply = bridge.hook(envelope, self.args)
        self.assertEqual(reply['status'], 'unknown')
        self.assertNotIn('failure_proof', reply)

    def test_legacy_failure_requires_matching_exit_time_and_unit_identity(self):
        self.record()
        result = {'run_id': self.identifier, 'generation': 1, 'returncode': 7,
            'finished_at': bridge.now()}
        bridge.atomic_json(self.directory / 'result.json', result)
        envelope = copy.deepcopy(self.envelope)
        envelope['operation'] = 'observe'
        request_path = str((self.directory / 'request.json').resolve())
        exec_start = f'{{ argv[]={sys.executable} {Path(bridge.__file__).resolve()} --worker {request_path} ; ignore_errors=no }}'
        cases = [
            self.failed_unit_state(ExecMainExitTimestamp='2000-01-01T00:00:00Z', ExecStart=exec_start),
            self.failed_unit_state(Id='different.service', ExecMainExitTimestamp=result['finished_at'], ExecStart=exec_start),
        ]
        for state in cases:
            with self.subTest(state=state):
                with patch.object(bridge, 'unit_state', side_effect=[state, state]):
                    reply = bridge.hook(envelope, self.args)
                self.assertEqual(reply['status'], 'unknown')
                self.assertNotIn('failure_proof', reply)

    def test_worker_uses_pinned_script_and_cannot_inherit_build_skips(self):
        self.record()
        proof = {'services': {'backend': {'unit': 'autotask-backend.service', 'pid': 2}}}
        with patch.dict(os.environ, {'AUTOTASK_SKIP_BACKEND_REBUILD': '1', 'AUTOTASK_LOCAL_DEPLOY_SKIP_WORKER_BUILD': '1'}), \
             patch.object(bridge, 'git', return_value=''), \
             patch.object(bridge, 'unit_state', return_value={'MainPID': '1', 'LoadState': 'loaded', 'Id': self.unit, 'InvocationID': 'invocation-1'}), \
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
