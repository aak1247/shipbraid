#!/usr/bin/env python3
"""Installer ownership preflight tests; no systemd or filesystem mutations outside temp."""
import importlib.util
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('shipbraid_installer', Path(__file__).with_name('install-local.py'))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class InstallationPreflightTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='shipbraid-install-test-')
        self.home = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def test_active_new_owner_blocks_installation(self):
        with patch.object(installer.subprocess, 'run', return_value=SimpleNamespace(returncode=0)) as run:
            with self.assertRaisesRegex(SystemExit, 'shipbraid.service is active'):
                installer.check_existing_installation(self.home)
        self.assertEqual(run.call_count, 1)
        self.assertEqual(list(self.home.iterdir()), [])

    def test_active_legacy_owner_blocks_second_coordinator(self):
        with patch.object(installer.subprocess, 'run', side_effect=[SimpleNamespace(returncode=3), SimpleNamespace(returncode=0)]) as run:
            with self.assertRaisesRegex(SystemExit, 'auto-deliver.service is active'):
                installer.check_existing_installation(self.home)
        self.assertEqual([call.args[0][-1] for call in run.call_args_list], ['shipbraid.service', 'auto-deliver.service'])
        self.assertEqual(list(self.home.iterdir()), [])

    def test_failed_ownership_probe_is_not_treated_as_stopped(self):
        with patch.object(installer.subprocess, 'run', return_value=SimpleNamespace(returncode=1)):
            with self.assertRaisesRegex(SystemExit, 'Cannot verify'):
                installer.check_existing_installation(self.home)

    def test_legacy_database_is_not_replaced_by_empty_new_installation(self):
        data = self.home / '.local/share/auto-deliver'
        data.mkdir(parents=True)
        database = data / 'coordinator.db'
        database.write_bytes(b'fixture-existing-ledger')
        with patch.object(installer.subprocess, 'run', return_value=SimpleNamespace(returncode=3)):
            with self.assertRaisesRegex(SystemExit, 'Legacy installation detected'):
                installer.check_existing_installation(self.home)
        self.assertEqual(database.read_bytes(), b'fixture-existing-ledger')
        self.assertFalse((self.home / '.local/share/shipbraid').exists())

    def test_fresh_install_and_inactive_new_install_can_proceed(self):
        for code in (3, 4):
            with patch.object(installer.subprocess, 'run', return_value=SimpleNamespace(returncode=code)):
                installer.check_existing_installation(self.home)
        self.assertEqual(list(self.home.iterdir()), [])


if __name__ == '__main__':
    unittest.main()
