from __future__ import annotations

import hashlib
import importlib.util
import json
import tarfile
import tempfile
import unittest
from pathlib import Path


SCRIPT_PATH = Path(__file__).with_name("package-release.py")
SPEC = importlib.util.spec_from_file_location("package_release", SCRIPT_PATH)
assert SPEC and SPEC.loader
package_release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(package_release)


class PackageReleaseTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "repo"
        self.root.mkdir()
        self.version = "1.2.3"
        (self.root / "Cargo.toml").write_text(
            f'[package]\nname = "shipbraid"\nversion = "{self.version}"\n', encoding="utf-8"
        )
        self._write("LICENSE", "MIT license fixture\n")
        self._write("deploy/install-standalone.py", "print('installer')\n")
        self._write("integration/local/adapter.py", "print('local adapter')\n")
        self._write("integration/github/receiver.py", "print('github receiver')\n")
        self._write("integration/local/README.md", "# Local adapter\n")
        self._write("integration/github/README.md", "# GitHub receiver\n")
        for name in (
            "distribution.md",
            "diagnostics.md",
            "local-cli.md",
            "hooks.md",
            "project-integration.md",
            "api.md",
            "agents.md",
            "naming.md",
            "request-parameters.md",
        ):
            self._write(f"docs/{name}", f"# {name}\n")
        self._write("examples/standalone/README.md", "Standalone example fixture\n")
        self.binary = self.root / "built-shipbraid"
        self.binary.write_text(
            f"#!/bin/sh\nprintf '%s\\n' 'shipbraid {self.version}'\n", encoding="utf-8"
        )
        self.binary.chmod(0o755)
        self.web_dir = self.root / "web/dist"
        self.web_dir.mkdir(parents=True)
        (self.web_dir / "index.html").write_text("<html>dashboard</html>\n", encoding="utf-8")
        self.output_dir = self.root / "target/dist"

    def _write(self, relative: str, content: str) -> Path:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        return path

    def _create(self) -> tuple[Path, Path]:
        return package_release.create_release_bundle(
            self.root,
            binary=self.binary,
            web_dir=self.web_dir,
            output_dir=self.output_dir,
        )

    def test_creates_complete_manifested_bundle_and_sha256_sidecar(self) -> None:
        archive_path, checksum_path = self._create()
        package_name = f"shipbraid-{self.version}-x86_64-unknown-linux-gnu"
        self.assertEqual(archive_path.name, f"{package_name}.tar.gz")
        self.assertEqual(checksum_path.name, f"{archive_path.name}.sha256")

        actual_archive_hash = hashlib.sha256(archive_path.read_bytes()).hexdigest()
        digest, filename = checksum_path.read_text(encoding="ascii").split()
        self.assertEqual(digest, actual_archive_hash)
        self.assertEqual(filename, archive_path.name)
        self.assertEqual(len(checksum_path.read_text(encoding="ascii").split()), 2)

        with tarfile.open(archive_path, "r:gz") as bundle:
            members = bundle.getmembers()
            self.assertTrue(all(member.isfile() or member.isdir() for member in members))
            names = {member.name for member in members if member.isfile()}
            prefix = f"{package_name}/"
            self.assertIn(f"{prefix}bin/shipbraid", names)
            self.assertIn(f"{prefix}web/index.html", names)
            self.assertIn(f"{prefix}adapters/local/adapter.py", names)
            self.assertIn(f"{prefix}adapters/github/receiver.py", names)
            self.assertIn(f"{prefix}install.py", names)
            self.assertIn(f"{prefix}examples/standalone/README.md", names)
            self.assertIn(f"{prefix}VERSION", names)
            self.assertIn(f"{prefix}manifest.json", names)
            self.assertEqual(bundle.getmember(f"{prefix}bin/shipbraid").mode & 0o111, 0o111)

            manifest_stream = bundle.extractfile(f"{prefix}manifest.json")
            assert manifest_stream
            manifest = json.load(manifest_stream)
            self.assertEqual(manifest["schema_version"], 1)
            self.assertEqual(manifest["version"], self.version)
            self.assertEqual(manifest["target"], "x86_64-unknown-linux-gnu")
            manifest_paths = set(manifest["files"])
            archived_paths = {name[len(prefix) :] for name in names if name != f"{prefix}manifest.json"}
            self.assertEqual(manifest_paths, archived_paths)
            for relative, expected_hash in manifest["files"].items():
                stream = bundle.extractfile(f"{prefix}{relative}")
                assert stream
                self.assertEqual(hashlib.sha256(stream.read()).hexdigest(), expected_hash)
            self.assertEqual(manifest["files"]["VERSION"], hashlib.sha256(b"1.2.3\n").hexdigest())

    def test_rejects_binary_version_mismatch_without_publishing(self) -> None:
        self.binary.write_text("#!/bin/sh\nprintf '%s\\n' 'shipbraid 9.9.9'\n", encoding="utf-8")
        self.binary.chmod(0o755)
        with self.assertRaisesRegex(package_release.PackagingError, "does not exactly match"):
            self._create()
        self.assertFalse(self.output_dir.exists())

    def test_rejects_symlink_in_packaged_tree(self) -> None:
        outside = self.root / "outside.txt"
        outside.write_text("not package data", encoding="utf-8")
        (self.web_dir / "linked.txt").symlink_to(outside)
        with self.assertRaisesRegex(package_release.PackagingError, "symlink"):
            self._create()
        self.assertFalse(self.output_dir.exists())

    def test_rejects_hidden_or_private_looking_example_files(self) -> None:
        self._write("examples/standalone/.env", "PRIVATE_TOKEN=do-not-package\n")
        with self.assertRaisesRegex(package_release.PackagingError, "hidden or private-looking"):
            self._create()
        self.assertFalse(self.output_dir.exists())

    def test_rejects_python_cache_files_in_examples(self) -> None:
        self._write("examples/standalone/project/__pycache__/verify.cpython-311.pyc", "cache data")
        with self.assertRaisesRegex(package_release.PackagingError, "hidden or private-looking|private-data"):
            self._create()
        self.assertFalse(self.output_dir.exists())

    def test_refuses_to_overwrite_existing_release_output(self) -> None:
        self.output_dir.mkdir(parents=True)
        existing = self.output_dir / "shipbraid-1.2.3-x86_64-unknown-linux-gnu.tar.gz"
        existing.write_bytes(b"user data")
        with self.assertRaisesRegex(package_release.PackagingError, "refusing to overwrite"):
            self._create()
        self.assertEqual(existing.read_bytes(), b"user data")

    def test_rewrites_unbundled_repository_links_without_editing_sources(self) -> None:
        source_entry = self._write("src/main.rs", "// source-only fixture\n")
        distribution_source = self.root / "docs/distribution.md"
        distribution_text = "# Distribution\n\n[CLI source](../src/main.rs#cli).\n"
        distribution_source.write_text(distribution_text, encoding="utf-8")
        adapter_readme_source = self.root / "integration/local/README.md"
        adapter_readme_text = "# Local adapter\n\n[Adapter source](adapter.py#dispatch).\n"
        adapter_readme_source.write_text(adapter_readme_text, encoding="utf-8")

        archive_path, _ = self._create()
        package_name = f"shipbraid-{self.version}-x86_64-unknown-linux-gnu"
        with tarfile.open(archive_path, "r:gz") as bundle:
            distribution = bundle.extractfile(f"{package_name}/docs/distribution.md")
            adapter_readme = bundle.extractfile(f"{package_name}/integration/local/README.md")
            assert distribution and adapter_readme
            distribution_text_in_bundle = distribution.read().decode("utf-8")
            adapter_readme_in_bundle = adapter_readme.read().decode("utf-8")
            manifest_stream = bundle.extractfile(f"{package_name}/manifest.json")
            assert manifest_stream
            manifest = json.load(manifest_stream)

        self.assertIn(
            "https://github.com/aak1247/shipbraid/blob/v1.2.3/src/main.rs#cli",
            distribution_text_in_bundle,
        )
        self.assertIn("../../adapters/local/adapter.py#dispatch", adapter_readme_in_bundle)
        self.assertNotIn("src/main.rs", manifest["files"])
        self.assertEqual(distribution_source.read_text(encoding="utf-8"), distribution_text)
        self.assertEqual(adapter_readme_source.read_text(encoding="utf-8"), adapter_readme_text)
        self.assertTrue(source_entry.is_file())


if __name__ == "__main__":
    unittest.main()
