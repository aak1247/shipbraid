#!/usr/bin/env python3
"""Standalone install integrity/ownership tests; isolated files and Git fixtures only."""
import hashlib
import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace
import subprocess
import tempfile
import tomllib
import unittest


spec = importlib.util.spec_from_file_location('standalone_installer',
                                             Path(__file__).with_name('install-standalone.py'))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class StandaloneInstallTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='shipbraid-standalone-test-')
        self.root = Path(self.temp.name)
        self.bundle = self.root / 'bundle'
        files = {
            'bin/shipbraid': '#!/bin/sh\necho "shipbraid 0.2.0"\n',
            'web/index.html': '<html>fixture</html>',
            'adapters/local/adapter.py': '# fixture',
            'adapters/github/receiver.py': '# fixture',
            'install.py': '# fixture', 'LICENSE': 'fixture', 'VERSION': '0.2.0\n',
        }
        for name, content in files.items():
            path = self.bundle / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
        (self.bundle / 'bin/shipbraid').chmod(0o755)
        self.manifest = {'schema_version': 1, 'version': '0.2.0',
                         'target': 'x86_64-unknown-linux-gnu',
                         'files': {name: hashlib.sha256(content.encode()).hexdigest()
                                   for name, content in files.items()}}
        self.write_manifest()
        self.project = self.root / 'project'
        self.project.mkdir()
        (self.project / 'deploy.sh').write_text('exit 0\n')
        (self.project / 'verify.py').write_text('# not real verification\n')
        repo = self.root / 'source'
        subprocess.run(['git', 'init', '-qb', 'main', str(repo)], check=True)
        subprocess.run(['git', '-C', str(repo), '-c', 'user.name=Fixture', '-c',
                        'user.email=fixture@example.invalid', 'commit', '--allow-empty',
                        '-qm', 'fixture'], check=True)
        mirror = self.root / 'mirror.git'
        subprocess.run(['git', 'clone', '-q', '--mirror', str(repo), str(mirror)], check=True)
        self.args = SimpleNamespace(prefix=self.root / 'installation with space',
                                    bundle_root=self.bundle, project='fixture', environment='dev',
                                    components='app', listen='127.0.0.1:8787', repository=mirror,
                                    release_ref='refs/heads/main', project_bundle=self.project)

    def tearDown(self):
        # Only fixtures owned by this test: make frozen release directories removable.
        for path in self.root.rglob('*'):
            if path.is_dir() and not path.is_symlink():
                path.chmod(0o700)
        self.temp.cleanup()

    def write_manifest(self):
        (self.bundle / 'manifest.json').write_text(json.dumps(self.manifest))

    def test_fresh_install_has_private_scoped_config_and_no_execution(self):
        before = subprocess.check_output(['git', '-C', str(self.args.repository), 'show-ref'])
        result = installer.install(self.args)
        prefix = self.args.prefix
        self.assertFalse(result['service_started'])
        self.assertFalse(result['deployment_submitted'])
        self.assertEqual(list((prefix / 'var/runs').iterdir()), [])
        config = tomllib.loads((prefix / 'etc/config.toml').read_text())
        self.assertFalse(config['lanes'][0]['allow_preemption'])
        self.assertEqual(config['tokens'][0]['lanes'], ['fixture-dev'])
        self.assertFalse(config['tokens'][0].get('admin', False))
        for name in ('admin.token', 'operator.token', 'tokens.env', 'deploy.env'):
            self.assertEqual((prefix / 'etc' / name).stat().st_mode & 0o777, 0o600)
        self.assertNotEqual((prefix / 'etc/admin.token').read_text(),
                            (prefix / 'etc/operator.token').read_text())
        self.assertTrue((prefix / 'bin/shipbraid').is_symlink())
        self.assertEqual(before, subprocess.check_output(['git', '-C', str(self.args.repository), 'show-ref']))
        release = Path(result['release'])
        self.assertEqual((release / 'adapter.json').stat().st_mode & 0o222, 0)
        self.assertEqual(release.stat().st_mode & 0o222, 0)
        self.assertEqual((release / 'project').stat().st_mode & 0o222, 0)
        self.assertIn('ExecStart="' + str(release / 'bin/shipbraid') + '"',
                      (prefix / 'shipbraid.service').read_text())

    def test_reinstall_preserves_all_original_files(self):
        result = installer.install(self.args)
        original = (self.args.prefix / 'etc/operator.token').read_bytes()
        with self.assertRaisesRegex(ValueError, 'already exists'):
            installer.install(self.args)
        self.assertEqual((self.args.prefix / 'etc/operator.token').read_bytes(), original)
        self.assertEqual(json.loads((self.args.prefix / 'installation.json').read_text()), result)

    def test_corrupted_or_unlisted_bundle_does_not_write_prefix(self):
        for name in ('web/index.html', 'unexpected-file'):
            path = self.bundle / name
            original = path.read_bytes() if path.exists() else None
            path.write_text('tampered')
            with self.assertRaises(ValueError):
                installer.install(self.args)
            self.assertFalse(self.args.prefix.exists())
            if original is None:
                path.unlink()
            else:
                path.write_bytes(original)

    def test_symlink_bundle_or_prefix_refused(self):
        (self.bundle / 'link').symlink_to('/etc/passwd')
        with self.assertRaisesRegex(ValueError, 'symlinks'):
            installer.install(self.args)
        (self.bundle / 'link').unlink()
        self.args.prefix.symlink_to(self.root / 'nonexistent')
        with self.assertRaisesRegex(ValueError, 'already exists'):
            installer.install(self.args)

    def test_unsafe_inputs_never_create_prefix(self):
        for field, value in [('project', '../outside'), ('components', 'app,app'),
                             ('listen', '0.0.0.0:8787'), ('listen', '127.0.0.1:0'),
                             ('environment', 'prod'), ('release_ref', '--all')]:
            original = getattr(self.args, field)
            setattr(self.args, field, value)
            with self.assertRaises(ValueError):
                installer.install(self.args)
            self.assertFalse(self.args.prefix.exists())
            setattr(self.args, field, original)

    def test_overlapping_prefix_cannot_recursively_copy_itself(self):
        for parent in (self.project, self.bundle):
            self.args.prefix = parent / 'nested'
            with self.assertRaisesRegex(ValueError, 'outside'):
                installer.install(self.args)
            self.assertFalse(self.args.prefix.exists())

    def test_unit_specifiers_and_environment_expansion_are_escaped(self):
        self.assertEqual(installer.unit_quote('/home/fixture%a/$name'),
                         '"/home/fixture%%a/$name"')
        self.assertEqual(installer.unit_quote('/home/fixture%a/$name', executable_argument=True),
                         '"/home/fixture%%a/$$name"')

    def test_secrets_in_project_bundle_are_refused_before_install(self):
        for name in ('.env', 'id_ed25519', 'private.pem', 'database.db'):
            path = self.project / name
            path.write_text('fixture private data, never copied')
            with self.assertRaisesRegex(ValueError, 'curated'):
                installer.install(self.args)
            self.assertFalse(self.args.prefix.exists())
            path.unlink()

    def test_ipv6_listener_requires_socket_address_brackets(self):
        self.args.listen = '::1:8787'
        with self.assertRaisesRegex(ValueError, 'loopback'):
            installer.install(self.args)
        self.args.listen = '[::1]:8787'
        record = installer.install(self.args)
        config = tomllib.loads(Path(record['config']).read_text())
        self.assertEqual(config['listen'], '[::1]:8787')

    def test_semver_build_metadata_is_accepted(self):
        self.manifest['version'] = '0.2.0+fixture'
        for name, text in [('VERSION', '0.2.0+fixture\n'),
                           ('bin/shipbraid', '#!/bin/sh\necho "shipbraid 0.2.0+fixture"\n')]:
            (self.bundle / name).write_text(text)
            self.manifest['files'][name] = hashlib.sha256(text.encode()).hexdigest()
        self.write_manifest()
        self.assertEqual(installer.install(self.args)['version'], '0.2.0+fixture')


if __name__ == '__main__':
    unittest.main()
