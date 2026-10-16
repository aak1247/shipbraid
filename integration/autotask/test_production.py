#!/usr/bin/env python3
"""Production hook safety tests; all external production systems are mocked."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import sys
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch
import uuid


HERE = Path(__file__).parent
bridge_spec = importlib.util.spec_from_file_location('bridge', HERE / 'bridge.py')
bridge = importlib.util.module_from_spec(bridge_spec)
sys.modules['bridge'] = bridge
bridge_spec.loader.exec_module(bridge)
production_spec = importlib.util.spec_from_file_location('production', HERE / 'production.py')
production = importlib.util.module_from_spec(production_spec)
production_spec.loader.exec_module(production)


class ProductionHookTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.run_id = str(uuid.uuid4())
        self.revision = 'a' * 40
        self.profile = 'b' * 64
        self.args = SimpleNamespace(
            state_dir=self.root / 'state',
            repository=self.root / 'repo',
            reviewed_release_commit='c' * 40,
            tool_path=':'.join((
                '/home/yahui/.nvm/versions/node/v24.20.0/bin',
                '/home/yahui/.cargo/bin',
                '/home/yahui/.pyenv/versions/3.14.7/bin',
                '/usr/local/sbin', '/usr/local/bin', '/usr/sbin', '/usr/bin', '/sbin', '/bin',
            )),
        )
        self.envelope = {
            'version': 1,
            'operation': 'dispatch',
            'command': {'id': str(uuid.uuid4())},
            'run': {
                'id': self.run_id,
                'lane_id': 'autotask-prod',
                'generation': 4,
                'executor_ref': None,
                'target': {
                    'revision': self.revision,
                    'components': ['app', 'website', 'worker'],
                    'profile_digest': self.profile,
                    'parameters': {'database_backup': 'include'},
                },
            },
            'lane': {
                'id': 'autotask-prod',
                'project': 'autotask',
                'environment': 'prod',
                'profile_digest': self.profile,
            },
        }
        self.run, self.target, self.directory, self.unit, self.repository = production.context(
            self.envelope, self.args)
        self.directory.mkdir(parents=True)

    def tearDown(self):
        self.temp.cleanup()

    def write_request(self):
        bridge.atomic_json(self.directory / 'request.json', self.envelope)

    def write_success_result(self, core=None):
        if core is None:
            core = {
                'source_revision': self.revision,
                'target_parameters': dict(self.target['parameters']),
                'database_backup_policy': self.target['parameters']['database_backup'],
                'components': {
                    'app': {'image_digest': 'sha256:' + '1' * 64},
                    'website': {'image_digest': 'sha256:' + '2' * 64},
                    'worker': {'before': {
                        'invocation_id': 'previous-invocation', 'main_pid': 123,
                        'start_ticks': '1234', 'executable_sha256': '3' * 64,
                    }, 'invocation_id': 'current-invocation'},
                },
            }
        proof = {
            'observed_at': bridge.now(),
            'core': core,
            'component_digests': {
                'app': 'sha256:' + '1' * 64,
                'website': 'sha256:' + '2' * 64,
                'worker': 'sha256:' + '3' * 64,
            },
            'proof_sha256': production._canonical_digest(core),
        }
        bridge.atomic_json(self.directory / 'result.json', {
            'returncode': 0,
            'finished_at': bridge.now(),
            'run_id': self.run_id,
            'generation': 4,
            'target': self.target,
            'unit': self.unit,
            'invocation_id': 'job-invocation',
            'reviewed_release_commit': self.args.reviewed_release_commit,
            'pipeline_blobs': {'scripts/release.sh': 'reviewed-blob'},
            'proof': proof,
        })
        return proof

    def write_running_guard(self, **overrides):
        result = {
            'returncode': 255,
            'phase': 'running',
            'finished_at': bridge.now(),
            'run_id': self.run_id,
            'generation': 4,
            'target': self.target,
            'unit': self.unit,
            'invocation_id': 'job-invocation',
        }
        result.update(overrides)
        path = self.directory / 'result.json'
        bridge.atomic_json(path, result)
        return result, path.read_bytes()

    def exited_job_state(self):
        return {
            'Id': self.unit,
            'InvocationID': 'job-invocation',
            'LoadState': 'loaded',
            'ActiveState': 'active',
            'SubState': 'exited',
            'MainPID': '0',
            'ExecMainStatus': '0',
            'Result': 'success',
        }

    def failed_job_state(self):
        return {
            'Id': self.unit,
            'InvocationID': 'job-invocation',
            'LoadState': 'loaded',
            'ActiveState': 'failed',
            'SubState': 'failed',
            'MainPID': '0',
            'ExecMainStatus': '7',
            'ExecMainExitTimestamp': bridge.now(),
            'Result': 'exit-code',
            'ControlGroup': '',
        }

    def ready_rollout(self, name, uid='pod-uid-1', digest='sha256:' + '1' * 64):
        deployment = {
            'metadata': {'generation': 2},
            'spec': {'replicas': 1, 'selector': {'matchLabels': {'app': name}},
                'template': {'spec': {'containers': [{'name': name,
                    'image': production.APP_IMAGE_REPOSITORY + '@' + digest}]}}},
            'status': {'observedGeneration': 2, 'updatedReplicas': 1,
                'readyReplicas': 1, 'availableReplicas': 1, 'unavailableReplicas': 0},
        }
        pod = {
            'metadata': {'name': name + '-abc', 'uid': uid, 'labels': {'app': name}},
            'spec': {'containers': [{'name': name,
                'image': production.APP_IMAGE_REPOSITORY + '@' + digest}]},
            'status': {'phase': 'Running', 'conditions': [{'type': 'Ready', 'status': 'True'}],
                'containerStatuses': [{'name': name, 'ready': True,
                    'imageID': 'docker-pullable://' + production.APP_IMAGE_REPOSITORY + '@' + digest}]},
        }
        return deployment, [pod]

    def test_rejects_wrong_sha_and_profile_fingerprint_drift(self):
        malformed = copy.deepcopy(self.envelope)
        malformed['run']['target']['revision'] = 'not-a-full-sha'
        with self.assertRaises(RuntimeError):
            production.context(malformed, self.args)

        drifted = copy.deepcopy(self.envelope)
        drifted['lane']['profile_digest'] = 'd' * 64
        with self.assertRaises(RuntimeError):
            production.context(drifted, self.args)

    def test_database_backup_policy_is_required_and_rejects_extra_parameters(self):
        invalid_parameters = (None, {}, {'database_backup': 'automatic'},
            {'database_backup': 'include', 'skip': True})
        for parameters in invalid_parameters:
            with self.subTest(parameters=parameters):
                malformed = copy.deepcopy(self.envelope)
                if parameters is None:
                    malformed['run']['target'].pop('parameters')
                else:
                    malformed['run']['target']['parameters'] = parameters
                with patch.object(production, '_run') as start:
                    with self.assertRaisesRegex(RuntimeError, 'database_backup'):
                        production.hook(malformed, self.args)
                start.assert_not_called()

    def test_worker_selects_only_the_frozen_database_backup_flag(self):
        for policy, expected_flag, forbidden_flag in (
                ('include', '--include-db-backup', '--skip-db-backup'),
                ('skip', '--skip-db-backup', '--include-db-backup')):
            with self.subTest(policy=policy):
                case_state = self.root / policy
                case_args = SimpleNamespace(**vars(self.args))
                case_args.state_dir = case_state
                case_dir = case_state / self.run_id
                case_dir.mkdir(parents=True)
                request_path = case_dir / 'request.json'
                envelope = copy.deepcopy(self.envelope)
                envelope['run']['target']['parameters'] = {'database_backup': policy}
                # A top-level caller field has no authority over the durable target.
                envelope['database_backup'] = 'skip' if policy == 'include' else 'include'
                bridge.atomic_json(request_path, envelope)
                runner = case_dir / 'reviewed-runner'
                release_script = runner / 'scripts' / 'release.sh'
                release_script.parent.mkdir(parents=True)
                release_script.write_text('#!/bin/bash\n')
                command = SimpleNamespace(returncode=0)
                proof = {'core': {
                    'target_parameters': {'database_backup': policy},
                    'database_backup_policy': policy,
                }}
                with patch.object(production.bridge, 'unit_state', return_value={
                        'Id': self.unit, 'LoadState': 'loaded', 'InvocationID': 'job-invocation'}), \
                        patch.object(production, '_worker_observation', return_value={
                            'invocation_id': 'worker-before', 'main_pid': 1,
                            'start_ticks': '1', 'executable_sha256': 'a' * 64}), \
                        patch.object(production, 'validate_published_revision'), \
                        patch.object(production, 'validate_pipeline_blobs', return_value={
                            'scripts/release.sh': 'reviewed-blob'}), \
                        patch.object(production, '_prepare_runner', return_value=runner), \
                        patch.object(production, '_cleanup_runner'), \
                        patch.object(production, 'capture_proof', return_value=proof), \
                        patch.object(production.subprocess, 'run', return_value=command) as execute:
                    self.assertEqual(production.worker(request_path, case_args), 0)

                argv = execute.call_args.args[0]
                self.assertIn(expected_flag, argv)
                self.assertNotIn(forbidden_flag, argv)
                result = production._read_json(case_dir / 'result.json')
                self.assertEqual(result['target']['parameters'], {'database_backup': policy})
                self.assertEqual(result['proof']['core']['database_backup_policy'], policy)

    def test_rejects_target_not_on_published_origin_vibe(self):
        with patch.object(production, '_git', side_effect=[self.revision, self.args.reviewed_release_commit]), \
                patch.object(production.subprocess, 'run', return_value=SimpleNamespace(returncode=1)):
            with self.assertRaisesRegex(RuntimeError, 'published origin/vibe'):
                production.validate_published_revision(
                    self.repository, self.revision, self.args.reviewed_release_commit)

    def test_target_pipeline_drift_fails_closed(self):
        # The first target file has different content from the administrator pin.
        with patch.object(production, '_git', side_effect=['reviewed-blob', 'changed-blob']):
            with self.assertRaisesRegex(RuntimeError, 'administrator-reviewed pipeline'):
                production.validate_pipeline_blobs(
                    self.repository, self.revision, self.args.reviewed_release_commit)

    def test_duplicate_dispatch_observes_existing_unit_without_replay(self):
        self.write_request()
        envelope = copy.deepcopy(self.envelope)
        with patch.object(production.bridge, 'unit_state', return_value={
                'Id': self.unit, 'InvocationID': 'job-invocation', 'LoadState': 'loaded',
                'ActiveState': 'active', 'SubState': 'running'}), \
                patch.object(production, '_run') as start:
            reply = production.hook(envelope, self.args)
        self.assertEqual(reply['status'], 'started')
        self.assertEqual(reply['executor_ref'], self.unit)
        start.assert_not_called()

    def test_missing_supervisor_after_intent_is_unknown_and_never_replayed(self):
        self.write_request()
        with patch.object(production.bridge, 'unit_state', return_value={'LoadState': 'not-found'}), \
                patch.object(production, '_run') as start:
            reply = production.hook(self.envelope, self.args)
        self.assertEqual(reply['status'], 'unknown')
        self.assertIn('will not be replayed', reply['message'])
        start.assert_not_called()

    def test_dispatch_uses_pinned_tool_path_without_inheriting_tokens_or_skip_flags(self):
        effective_path = production._validate_tool_path(self.args.tool_path)
        captured = []

        def capture_command(argv, **kwargs):
            captured.append((argv, production._runtime_environment()))
            return b''

        with patch.dict(os.environ, {
                'PATH': '/untrusted/manager/bin',
                'TOKEN': 'must-not-propagate',
                'AUTOTASK_SKIP_BUILD': '1',
                'RELEASE_SKIP_ASSET_GATE': '1'}, clear=False), \
                patch.object(production.bridge, 'unit_state', return_value={
                    'LoadState': 'not-found'}), \
                patch.object(production, '_run', side_effect=capture_command):
            reply = production.hook(self.envelope, self.args)

        self.assertEqual(reply['status'], 'started')
        self.assertEqual(len(captured), 1)
        argv, child_environment = captured[0]
        self.assertIn('--setenv=PATH=' + effective_path, argv)
        self.assertEqual(child_environment['PATH'], effective_path)
        self.assertNotIn('TOKEN', child_environment)
        self.assertNotIn('AUTOTASK_SKIP_BUILD', child_environment)
        self.assertNotIn('RELEASE_SKIP_ASSET_GATE', child_environment)
        worker_path_index = argv.index('--tool-path')
        self.assertEqual(argv[worker_path_index + 1], effective_path)

    def test_worker_probe_uses_noninteractive_sudo_with_pinned_path(self):
        effective_path = production._activate_tool_path(self.args.tool_path)
        response = json.dumps({
            'invocation_id': 'worker-invocation', 'main_pid': 123,
            'start_ticks': '42', 'executable_sha256': 'd' * 64,
        }).encode()
        calls = []

        def capture_command(argv, **kwargs):
            calls.append((argv, production._runtime_environment()))
            return response

        with patch.dict(os.environ, {
                'PATH': '/untrusted/manager/bin', 'TOKEN': 'must-not-propagate'}, clear=False), \
                patch.object(production, '_run', side_effect=capture_command):
            observation = production._worker_observation()

        self.assertEqual(observation['invocation_id'], 'worker-invocation')
        argv, child_environment = calls[0]
        self.assertIn('sudo -n python3', argv[-1])
        self.assertEqual(child_environment['PATH'], effective_path)
        self.assertNotIn('TOKEN', child_environment)

    def test_tool_path_rejects_relative_missing_and_empty_components(self):
        for invalid in ('relative/bin:/usr/bin', '/usr/bin::/bin', '/does/not/exist'):
            with self.subTest(invalid=invalid), self.assertRaises(RuntimeError):
                production._validate_tool_path(invalid)

    def test_failure_requires_shared_bridge_stop_proof(self):
        self.write_request()
        bridge.atomic_json(self.directory / 'result.json', {
            'returncode': 7,
            'finished_at': bridge.now(),
            'run_id': self.run_id,
            'generation': 4,
            'target': self.target,
            'unit': self.unit,
            'invocation_id': 'job-invocation',
        })
        proof = {'observed_at': bridge.now(), 'evidence': {'unit': self.unit, 'stopped': True}}
        with patch.object(production.bridge, 'unit_state', return_value=self.failed_job_state()), \
                patch.object(production.bridge, 'failure_proof', return_value=proof) as prove:
            reply = production.hook({**self.envelope, 'operation': 'observe'}, self.args)
        self.assertEqual(reply['status'], 'failed')
        self.assertIs(reply['environment_known_unchanged'], False)
        self.assertEqual(reply['failure_proof'], proof)
        prove.assert_called_once()

        with patch.object(production.bridge, 'unit_state', return_value=self.failed_job_state()), \
                patch.object(production.bridge, 'failure_proof', side_effect=RuntimeError('not stopped')):
            reply = production.hook({**self.envelope, 'operation': 'verify'}, self.args)
        self.assertEqual(reply['status'], 'unknown')
        self.assertNotIn('failure_proof', reply)

    def test_killed_running_guard_uses_systemd_status_without_rewriting_record(self):
        self.write_request()
        guard, original = self.write_running_guard()
        state = self.failed_job_state()
        state['ExecMainStatus'] = '9'
        state['ControlGroup'] = ''
        with patch.object(production.bridge, 'unit_state', return_value=state):
            reply = production.hook({**self.envelope, 'operation': 'observe'}, self.args)

        self.assertEqual(reply['status'], 'failed')
        proof = reply['failure_proof']
        self.assertEqual(proof['evidence']['terminal_status_source'], 'systemd')
        self.assertEqual(proof['evidence']['result']['returncode'], 9)
        self.assertEqual(proof['evidence']['durable_running_guard'], guard)
        self.assertEqual(proof['evidence']['durable_running_guard_sha256'],
            production._canonical_digest(guard))
        self.assertEqual((self.directory / 'result.json').read_bytes(), original)

    def test_running_guard_with_wrong_invocation_or_live_descendant_has_no_proof(self):
        self.write_request()
        self.write_running_guard()
        state = self.failed_job_state()
        state['ExecMainStatus'] = '9'
        state['InvocationID'] = 'different-invocation'
        with patch.object(production.bridge, 'unit_state', return_value=state):
            reply = production.hook({**self.envelope, 'operation': 'observe'}, self.args)
        self.assertEqual(reply['status'], 'unknown')
        self.assertNotIn('failure_proof', reply)

        root = self.root / 'cgroup'
        group = root / 'user.slice' / 'production-job.service'
        child = group / 'child'
        for directory, pids, populated in ((group, '', 1), (child, '4321\n', 1)):
            directory.mkdir(parents=True, exist_ok=True)
            (directory / 'cgroup.procs').write_text(pids)
            (directory / 'cgroup.events').write_text(f'populated {populated}\nfrozen 0\n')
        state['InvocationID'] = 'job-invocation'
        state['ControlGroup'] = '/user.slice/production-job.service'
        with patch.object(production.bridge, 'CGROUP_ROOT', root), \
                patch.object(production.bridge, 'unit_state', return_value=state):
            reply = production.hook({**self.envelope, 'operation': 'observe'}, self.args)
        self.assertEqual(reply['status'], 'unknown')
        self.assertNotIn('failure_proof', reply)

    def test_component_drift_after_result_blocks_receipt(self):
        self.write_request()
        saved = self.write_success_result()
        changed = copy.deepcopy(saved['core'])
        changed['components']['website']['image_digest'] = 'sha256:' + '9' * 64
        fresh = {**saved, 'core': changed}
        with patch.object(production.bridge, 'unit_state', return_value=self.exited_job_state()), \
                patch.object(production, 'validate_pipeline_blobs', return_value={'scripts/release.sh': 'reviewed-blob'}), \
                patch.object(production, 'capture_proof', return_value=fresh):
            reply = production.hook({**self.envelope, 'operation': 'verify'}, self.args)
        self.assertEqual(reply['status'], 'unknown')
        self.assertNotIn('receipt', reply)

    def test_verify_rejects_saved_proof_with_different_backup_policy(self):
        self.write_request()
        self.write_success_result()
        result_path = self.directory / 'result.json'
        result = production._read_json(result_path)
        result['proof']['core']['database_backup_policy'] = 'skip'
        bridge.atomic_json(result_path, result)
        with patch.object(production.bridge, 'unit_state', return_value=self.exited_job_state()), \
                patch.object(production, 'validate_pipeline_blobs', return_value={
                    'scripts/release.sh': 'reviewed-blob'}), \
                patch.object(production, 'capture_proof') as capture:
            reply = production.hook({**self.envelope, 'operation': 'verify'}, self.args)
        self.assertEqual(reply['status'], 'unknown')
        self.assertNotIn('receipt', reply)
        capture.assert_not_called()

    def test_fresh_verification_returns_three_component_receipt(self):
        self.write_request()
        saved = self.write_success_result()
        fresh = {**saved, 'observed_at': bridge.now()}
        with patch.object(production.bridge, 'unit_state', return_value=self.exited_job_state()), \
                patch.object(production, 'validate_pipeline_blobs', return_value={'scripts/release.sh': 'reviewed-blob'}), \
                patch.object(production, 'capture_proof', return_value=fresh) as observe:
            reply = production.hook({**self.envelope, 'operation': 'verify'}, self.args)
        self.assertEqual(reply['status'], 'verified')
        receipt = reply['receipt']
        self.assertEqual(receipt['source_revision'], self.revision)
        self.assertEqual(receipt['profile_digest'], self.profile)
        self.assertEqual(set(receipt['component_digests']), {'app', 'website', 'worker'})
        report = json.loads(receipt['verification_report'])
        self.assertEqual(report['database_backup_policy'], 'include')
        self.assertEqual(report['target_parameters'], {'database_backup': 'include'})
        self.assertGreaterEqual(receipt['observed_at'], fresh['observed_at'])
        observe.assert_called_once_with(self.repository, self.target,
            saved['core']['components']['worker']['before'])

    def test_replica_image_digest_must_match_immutable_source_tag(self):
        digest = 'sha256:' + '1' * 64
        deployment = {
            'metadata': {'generation': 2},
            'spec': {'replicas': 1, 'selector': {'matchLabels': {'app': 'autotask'}},
                'template': {'spec': {'containers': [{'name': 'autotask',
                    'image': production.APP_IMAGE_REPOSITORY + '@' + digest}]}}},
            'status': {'observedGeneration': 2, 'updatedReplicas': 1,
                'readyReplicas': 1, 'availableReplicas': 1, 'unavailableReplicas': 0},
        }
        pod = {
            'metadata': {'name': 'autotask-abc', 'uid': 'autotask-pod-uid',
                'labels': {'app': 'autotask'}},
            'spec': {'containers': [{'name': 'autotask',
                'image': production.APP_IMAGE_REPOSITORY + '@' + digest}]},
            'status': {'phase': 'Running', 'conditions': [{'type': 'Ready', 'status': 'True'}],
                'containerStatuses': [{'name': 'autotask', 'ready': True,
                    'imageID': 'docker-pullable://' + production.APP_IMAGE_REPOSITORY + '@' + digest}]},
        }
        _, _, pods = production.validate_replicas(deployment, [pod], 'autotask',
            production.APP_IMAGE_REPOSITORY, digest)
        self.assertEqual(pods, [{'name': 'autotask-abc', 'uid': 'autotask-pod-uid',
            'image_digest': digest, 'ready': True}])
        with self.assertRaisesRegex(RuntimeError, 'source-tag digest'):
            production.validate_replicas(deployment, [pod], 'autotask',
                production.APP_IMAGE_REPOSITORY, 'sha256:' + '9' * 64)

    def test_rollout_uid_change_during_observation_fails_closed(self):
        initial_deployment, initial_pods = self.ready_rollout('autotask')
        image_ref, image_digest, replicas = production.validate_replicas(
            initial_deployment, initial_pods, 'autotask', production.APP_IMAGE_REPOSITORY,
            'sha256:' + '1' * 64)
        initial = production._rollout_snapshot(
            initial_deployment, image_ref, image_digest, replicas)
        changed_deployment, changed_pods = self.ready_rollout('autotask', uid='replacement-uid')
        with patch.object(production, '_docker_tag_digest', return_value=image_digest), \
                patch.object(production, '_get_workload', return_value=(changed_deployment, changed_pods)):
            with self.assertRaisesRegex(RuntimeError, 'changed during independent verification'):
                production._assert_rollout_unchanged('autotask', 'autotask',
                    production.APP_IMAGE_REPOSITORY, 'autotask:tag', initial)

    def test_worker_supervisor_change_during_observation_fails_closed(self):
        app_snapshot, website_snapshot = {}, {}
        observed_worker = {'invocation_id': 'before', 'main_pid': 11,
            'start_ticks': '100', 'executable_sha256': 'a' * 64}
        changed_worker = {**observed_worker, 'invocation_id': 'after', 'main_pid': 22}
        with patch.object(production, '_assert_rollout_unchanged'), \
                patch.object(production, '_worker_observation', return_value=changed_worker):
            with self.assertRaisesRegex(RuntimeError, 'supervisor changed during independent verification'):
                production._assert_observation_unchanged(
                    app_snapshot, website_snapshot, 'app:tag', 'website:tag', observed_worker)

    def test_worker_must_change_supervisor_identity(self):
        before = {'invocation_id': 'same', 'main_pid': 123, 'start_ticks': '42'}
        with self.assertRaisesRegex(RuntimeError, 'identity did not change'):
            production._assert_worker_restarted(before, {
                'invocation_id': 'same', 'main_pid': 456, 'start_ticks': '100',
            })
        production._assert_worker_restarted(before, {
            'invocation_id': 'new', 'main_pid': 456, 'start_ticks': '100',
        })


if __name__ == '__main__':
    unittest.main()
