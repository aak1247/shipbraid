#!/usr/bin/env python3
"""Assemble the supported Linux x86_64 GNU release bundle."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import posixpath
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from pathlib import Path, PurePosixPath
from typing import Iterable
from urllib.parse import quote, unquote, urlsplit

DEFAULT_TARGET = "x86_64-unknown-linux-gnu"
REPO_ROOT = Path(__file__).resolve().parent.parent
REQUIRED_FILES = (
    ("integration/local/adapter.py", "adapters/local/adapter.py"),
    ("integration/github/receiver.py", "adapters/github/receiver.py"),
    ("integration/local/README.md", "integration/local/README.md"),
    ("integration/github/README.md", "integration/github/README.md"),
    ("deploy/install-standalone.py", "install.py"),
    ("docs/distribution.md", "docs/distribution.md"),
    ("docs/hooks.md", "docs/hooks.md"),
    ("docs/project-integration.md", "docs/project-integration.md"),
    ("docs/api.md", "docs/api.md"),
    ("docs/agents.md", "docs/agents.md"),
    ("docs/naming.md", "docs/naming.md"),
    ("docs/request-parameters.md", "docs/request-parameters.md"),
    ("LICENSE", "LICENSE"),
)
REQUIRED_DIRECTORIES = (("examples/standalone", "examples/standalone"),)
SENSITIVE_NAMES = {".env", ".git", "__pycache__", "id_rsa", "id_ed25519"}
SENSITIVE_SUFFIXES = (
    ".db",
    ".dump",
    ".env",
    ".key",
    ".log",
    ".pem",
    ".pyc",
    ".sqlite",
    ".sqlite3",
)
VERSION_RE = re.compile(r"\A[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?\Z")
TARGET_RE = re.compile(r"\A[A-Za-z0-9][A-Za-z0-9_.-]*\Z")
MARKDOWN_LINK_RE = re.compile(
    r"(?P<prefix>!?\[[^\]]*\]\()(?P<target><[^>]*>|[^)\s]+)(?P<suffix>(?:\s+[^)]*)?\))"
)


class PackagingError(Exception):
    """A clear, user-actionable packaging failure."""


def _check_safe_name(name: str, context: str) -> None:
    lowered = name.casefold()
    if name.startswith(".") or lowered in SENSITIVE_NAMES:
        raise PackagingError(f"refusing hidden or private-looking release input: {context}")
    if lowered.endswith(SENSITIVE_SUFFIXES):
        raise PackagingError(f"refusing private-data file in release input: {context}")


def _check_source_path(root: Path, relative: str, *, want_directory: bool) -> Path:
    """Resolve a repo-relative input while rejecting symlinks in its path."""
    current = root
    try:
        root_mode = current.lstat().st_mode
    except OSError as exc:
        raise PackagingError(f"cannot inspect repository root: {exc}") from exc
    if stat.S_ISLNK(root_mode) or not stat.S_ISDIR(root_mode):
        raise PackagingError("repository root must be a real directory")

    for part in PurePosixPath(relative).parts:
        if part in ("", ".", ".."):
            raise PackagingError(f"invalid release input path: {relative}")
        _check_safe_name(part, relative)
        current = current / part
        try:
            mode = current.lstat().st_mode
        except FileNotFoundError as exc:
            raise PackagingError(f"required release input is missing: {relative}") from exc
        except OSError as exc:
            raise PackagingError(f"cannot inspect release input {relative}: {exc}") from exc
        if stat.S_ISLNK(mode):
            raise PackagingError(f"release input path contains a symlink: {relative}")

    mode = current.lstat().st_mode
    valid = stat.S_ISDIR(mode) if want_directory else stat.S_ISREG(mode)
    if not valid:
        kind = "directory" if want_directory else "regular file"
        raise PackagingError(f"release input must be a {kind}: {relative}")
    return current


def _check_external_file(path: Path, label: str) -> Path:
    try:
        mode = path.lstat().st_mode
    except FileNotFoundError as exc:
        raise PackagingError(f"required {label} is missing: {path}") from exc
    except OSError as exc:
        raise PackagingError(f"cannot inspect {label} {path}: {exc}") from exc
    if stat.S_ISLNK(mode) or not stat.S_ISREG(mode):
        raise PackagingError(f"{label} must be a regular file and cannot be a symlink: {path}")
    return path


def _copy_file(source: Path, destination: Path, context: str, *, executable: bool = False) -> None:
    try:
        source_mode = source.lstat().st_mode
    except OSError as exc:
        raise PackagingError(f"cannot inspect release input {context}: {exc}") from exc
    if stat.S_ISLNK(source_mode) or not stat.S_ISREG(source_mode):
        raise PackagingError(f"release input must be a regular file and cannot be a symlink: {context}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    try:
        shutil.copyfile(source, destination, follow_symlinks=False)
        mode = stat.S_IMODE(source_mode) & 0o755
        if executable:
            mode |= 0o111
        os.chmod(destination, mode or 0o644)
    except OSError as exc:
        raise PackagingError(f"cannot copy release input {context}: {exc}") from exc


def _copy_tree(source: Path, destination: Path, source_label: str) -> None:
    """Copy a tree without following links or collecting dotfiles/secrets."""
    try:
        source_mode = source.lstat().st_mode
    except OSError as exc:
        raise PackagingError(f"cannot inspect release directory {source_label}: {exc}") from exc
    if stat.S_ISLNK(source_mode) or not stat.S_ISDIR(source_mode):
        raise PackagingError(f"release input must be a directory and cannot be a symlink: {source_label}")
    destination.mkdir(parents=True, exist_ok=True)

    def visit(src_dir: Path, dst_dir: Path, rel_dir: str) -> int:
        copied_files = 0
        try:
            entries = sorted(os.scandir(src_dir), key=lambda entry: entry.name)
        except OSError as exc:
            raise PackagingError(f"cannot read release directory {rel_dir}: {exc}") from exc
        for entry in entries:
            relative = f"{rel_dir}/{entry.name}" if rel_dir else entry.name
            _check_safe_name(entry.name, relative)
            try:
                mode = entry.stat(follow_symlinks=False).st_mode
            except OSError as exc:
                raise PackagingError(f"cannot inspect release input {relative}: {exc}") from exc
            src = Path(entry.path)
            dst = dst_dir / entry.name
            if stat.S_ISLNK(mode):
                raise PackagingError(f"release input tree contains a symlink: {relative}")
            if stat.S_ISDIR(mode):
                dst.mkdir(exist_ok=True)
                copied_files += visit(src, dst, relative)
            elif stat.S_ISREG(mode):
                _copy_file(src, dst, relative)
                copied_files += 1
            else:
                raise PackagingError(f"release input tree contains a non-regular file: {relative}")
        return copied_files

    if visit(source, destination, source_label) == 0:
        raise PackagingError(f"release directory contains no files: {source_label}")


def _validate_tree(source: Path, source_label: str) -> None:
    """Preflight a tree before creating any output, without following links."""
    try:
        mode = source.lstat().st_mode
    except OSError as exc:
        raise PackagingError(f"cannot inspect release directory {source_label}: {exc}") from exc
    if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
        raise PackagingError(f"release input must be a directory and cannot be a symlink: {source_label}")

    def visit(directory: Path, relative_directory: str) -> int:
        count = 0
        try:
            entries = sorted(os.scandir(directory), key=lambda entry: entry.name)
        except OSError as exc:
            raise PackagingError(f"cannot read release directory {relative_directory}: {exc}") from exc
        for entry in entries:
            relative = f"{relative_directory}/{entry.name}"
            _check_safe_name(entry.name, relative)
            try:
                entry_mode = entry.stat(follow_symlinks=False).st_mode
            except OSError as exc:
                raise PackagingError(f"cannot inspect release input {relative}: {exc}") from exc
            if stat.S_ISLNK(entry_mode):
                raise PackagingError(f"release input tree contains a symlink: {relative}")
            if stat.S_ISDIR(entry_mode):
                count += visit(Path(entry.path), relative)
            elif stat.S_ISREG(entry_mode):
                count += 1
            else:
                raise PackagingError(f"release input tree contains a non-regular file: {relative}")
        return count

    if visit(source, source_label) == 0:
        raise PackagingError(f"release directory contains no files: {source_label}")


def _source_package_paths(repo_root: Path) -> dict[str, str]:
    """Map repository-relative source files to their release-relative locations."""
    mapping = {source: destination for source, destination in REQUIRED_FILES}
    for source_dir, destination_dir in REQUIRED_DIRECTORIES:
        source_root = repo_root / source_dir
        for current, dirs, names in os.walk(source_root, topdown=True, followlinks=False):
            dirs.sort()
            names.sort()
            current_path = Path(current)
            for name in names:
                source = (current_path / name).relative_to(repo_root).as_posix()
                relative = (current_path / name).relative_to(source_root).as_posix()
                mapping[source] = (PurePosixPath(destination_dir) / relative).as_posix()
    return mapping


def _resolve_relative_path(base_file: str, path: str) -> str | None:
    """Normalize a relative link while rejecting traversal above its root."""
    parts = list(PurePosixPath(base_file).parent.parts)
    for part in PurePosixPath(path).parts:
        if part in ("", "."):
            continue
        if part == "..":
            if not parts:
                return None
            parts.pop()
        else:
            parts.append(part)
    return PurePosixPath(*parts).as_posix() if parts else ""


def _rewrite_markdown_links(package_root: Path, version: str, source_to_package: dict[str, str]) -> None:
    """Keep packaged relative links valid and point omitted repo files at the release tag."""
    package_to_source = {destination: source for source, destination in source_to_package.items()}
    package_paths = {""}
    for path in package_root.rglob("*"):
        package_paths.add(path.relative_to(package_root).as_posix())

    for markdown_path in sorted(package_root.rglob("*.md")):
        package_relative = markdown_path.relative_to(package_root).as_posix()
        source_relative = package_to_source.get(package_relative)
        if source_relative is None:
            continue
        original = markdown_path.read_text(encoding="utf-8")

        def replace_link(match: re.Match[str]) -> str:
            raw_target = match.group("target")
            angled = raw_target.startswith("<") and raw_target.endswith(">")
            target = raw_target[1:-1] if angled else raw_target
            if target.startswith("#") or target.startswith("/") or "\\" in target:
                return match.group(0)
            parsed = urlsplit(target)
            if parsed.scheme or parsed.netloc or parsed.query:
                return match.group(0)
            path_part, anchor_marker, anchor = target.partition("#")
            if not path_part:
                return match.group(0)

            decoded_path = unquote(path_part)
            package_target = _resolve_relative_path(package_relative, decoded_path)
            if package_target is None:
                raise PackagingError(
                    f"markdown link escapes the release root: {package_relative}: {target}"
                )
            if package_target in package_paths:
                return match.group(0)

            source_target = _resolve_relative_path(source_relative, decoded_path)
            if source_target is None:
                raise PackagingError(
                    f"markdown link escapes the repository root: {source_relative}: {target}"
                )
            packaged_target = source_to_package.get(source_target)
            if packaged_target is not None:
                rewritten_path = posixpath.relpath(
                    packaged_target,
                    start=posixpath.dirname(package_relative) or ".",
                )
            else:
                repository_path = quote(source_target, safe="/")
                rewritten_path = (
                    f"https://github.com/aak1247/shipbraid/blob/v{version}/{repository_path}"
                )
            rewritten = rewritten_path + (f"#{anchor}" if anchor_marker else "")
            rendered_target = f"<{rewritten}>" if angled else rewritten
            return f"{match.group('prefix')}{rendered_target}{match.group('suffix')}"

        updated = MARKDOWN_LINK_RE.sub(replace_link, original)
        if updated != original:
            markdown_path.write_text(updated, encoding="utf-8")


def _read_version(repo_root: Path) -> str:
    cargo_toml = _check_source_path(repo_root, "Cargo.toml", want_directory=False)
    try:
        with cargo_toml.open("rb") as stream:
            metadata = tomllib.load(stream)
        package = metadata["package"]
        name = package["name"]
        version = package["version"]
    except (OSError, KeyError, TypeError, tomllib.TOMLDecodeError) as exc:
        raise PackagingError(f"cannot read package name/version from Cargo.toml: {exc}") from exc
    if name != "shipbraid":
        raise PackagingError(f"Cargo package name must be shipbraid, found {name!r}")
    if not isinstance(version, str) or not VERSION_RE.fullmatch(version):
        raise PackagingError("Cargo package version must be a valid numeric SemVer version")
    return version


def _verify_binary(binary: Path, version: str) -> None:
    binary = _check_external_file(binary, "built shipbraid binary")
    if not os.access(binary, os.X_OK):
        raise PackagingError(f"built shipbraid binary is not executable: {binary}")
    try:
        result = subprocess.run(
            [str(binary), "--version"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=15,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise PackagingError(f"could not run built binary with --version: {exc}") from exc
    if result.returncode != 0:
        raise PackagingError("built binary --version exited unsuccessfully")
    if result.stdout.strip() != f"shipbraid {version}":
        raise PackagingError("built binary --version does not exactly match Cargo.toml package version")


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _package_files(package_root: Path) -> list[Path]:
    files: list[Path] = []
    for current, dirs, names in os.walk(package_root, topdown=True, followlinks=False):
        current_path = Path(current)
        dirs.sort()
        names.sort()
        for dirname in dirs:
            path = current_path / dirname
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
                raise PackagingError(f"staging tree contains a non-directory or symlink: {path}")
        for filename in names:
            path = current_path / filename
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode) or not stat.S_ISREG(mode):
                raise PackagingError(f"staging tree contains a non-regular file or symlink: {path}")
            files.append(path)
    return sorted(files, key=lambda path: path.relative_to(package_root).as_posix())


def _write_manifest(package_root: Path, version: str, target: str) -> None:
    files = _package_files(package_root)
    file_hashes = {
        path.relative_to(package_root).as_posix(): _sha256(path)
        for path in files
    }
    manifest = {
        "schema_version": 1,
        "version": version,
        "target": target,
        "files": file_hashes,
    }
    try:
        (package_root / "manifest.json").write_text(
            json.dumps(manifest, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
    except OSError as exc:
        raise PackagingError(f"cannot write release manifest: {exc}") from exc


def _write_tarball(package_root: Path, archive_path: Path) -> None:
    try:
        with archive_path.open("wb") as raw:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                    paths: list[Path] = [package_root]
                    for current, dirs, names in os.walk(package_root, topdown=True, followlinks=False):
                        current_path = Path(current)
                        dirs.sort()
                        names.sort()
                        paths.extend(current_path / dirname for dirname in dirs)
                        paths.extend(current_path / filename for filename in names)
                    for path in paths:
                        relative = path.relative_to(package_root.parent).as_posix()
                        mode = path.lstat().st_mode
                        info = tarfile.TarInfo(relative)
                        info.uid = 0
                        info.gid = 0
                        info.uname = ""
                        info.gname = ""
                        info.mtime = 0
                        if stat.S_ISDIR(mode):
                            info.type = tarfile.DIRTYPE
                            info.mode = 0o755
                            info.size = 0
                            archive.addfile(info)
                        elif stat.S_ISREG(mode):
                            info.type = tarfile.REGTYPE
                            info.mode = stat.S_IMODE(mode) & 0o755
                            info.size = path.stat().st_size
                            with path.open("rb") as stream:
                                archive.addfile(info, stream)
                        else:
                            raise PackagingError(f"cannot archive non-regular release input: {path}")
            raw.flush()
            os.fsync(raw.fileno())
    except PackagingError:
        raise
    except OSError as exc:
        raise PackagingError(f"could not create release tar.gz: {exc}") from exc


def _fsync_directory(path: Path) -> None:
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def create_release_bundle(
    repo_root: Path,
    *,
    binary: Path,
    web_dir: Path,
    output_dir: Path,
    target: str = DEFAULT_TARGET,
) -> tuple[Path, Path]:
    """Create the tar.gz and sha256 sidecar; leave existing output untouched."""
    repo_root = Path(repo_root)
    binary = Path(binary)
    web_dir = Path(web_dir)
    output_dir = Path(output_dir)
    if not TARGET_RE.fullmatch(target):
        raise PackagingError(f"invalid target name: {target!r}")
    version = _read_version(repo_root)
    _verify_binary(binary, version)
    try:
        web_dir_mode = web_dir.lstat().st_mode
    except FileNotFoundError as exc:
        raise PackagingError(f"required web build directory is missing: {web_dir}") from exc
    except OSError as exc:
        raise PackagingError(f"cannot inspect web build directory {web_dir}: {exc}") from exc
    if stat.S_ISLNK(web_dir_mode) or not stat.S_ISDIR(web_dir_mode):
        raise PackagingError(f"web build input must be a directory and cannot be a symlink: {web_dir}")
    _validate_tree(web_dir, "web build directory")
    for source_relative, _ in REQUIRED_FILES:
        _check_source_path(repo_root, source_relative, want_directory=False)
    for source_relative, _ in REQUIRED_DIRECTORIES:
        source = _check_source_path(repo_root, source_relative, want_directory=True)
        _validate_tree(source, source_relative)

    try:
        if output_dir.is_symlink():
            raise PackagingError(f"output directory cannot be a symlink: {output_dir}")
        output_dir.mkdir(parents=True, exist_ok=True)
        output_mode = output_dir.lstat().st_mode
        if stat.S_ISLNK(output_mode) or not stat.S_ISDIR(output_mode):
            raise PackagingError(f"output path must be a real directory: {output_dir}")
    except OSError as exc:
        raise PackagingError(f"cannot prepare output directory {output_dir}: {exc}") from exc

    package_name = f"shipbraid-{version}-{target}"
    archive_path = output_dir / f"{package_name}.tar.gz"
    checksum_path = output_dir / f"{package_name}.tar.gz.sha256"
    if archive_path.exists() or archive_path.is_symlink() or checksum_path.exists() or checksum_path.is_symlink():
        raise PackagingError(f"release output already exists; refusing to overwrite: {archive_path}")

    pending_archive: Path | None = None
    pending_checksum: Path | None = None
    published_archive = False
    published_checksum = False
    success = False
    try:
        archive_fd, pending_archive_name = tempfile.mkstemp(prefix=".shipbraid-archive-", suffix=".tmp", dir=output_dir)
        os.close(archive_fd)
        pending_archive = Path(pending_archive_name)
        checksum_fd, pending_checksum_name = tempfile.mkstemp(
            prefix=".shipbraid-checksum-", suffix=".tmp", dir=output_dir
        )
        os.close(checksum_fd)
        pending_checksum = Path(pending_checksum_name)
        with tempfile.TemporaryDirectory(prefix=".shipbraid-package-", dir=output_dir) as temporary:
            package_root = Path(temporary) / package_name
            package_root.mkdir(mode=0o755)
            (package_root / "bin").mkdir()
            _copy_file(binary, package_root / "bin/shipbraid", "built shipbraid binary", executable=True)
            _copy_tree(web_dir, package_root / "web", "web build directory")
            for source_relative, destination_relative in REQUIRED_FILES:
                source = _check_source_path(repo_root, source_relative, want_directory=False)
                _copy_file(source, package_root / destination_relative, source_relative)
            for source_relative, destination_relative in REQUIRED_DIRECTORIES:
                source = _check_source_path(repo_root, source_relative, want_directory=True)
                _copy_tree(source, package_root / destination_relative, source_relative)

            _rewrite_markdown_links(
                package_root,
                version,
                _source_package_paths(repo_root),
            )
            (package_root / "VERSION").write_text(f"{version}\n", encoding="utf-8")
            _write_manifest(package_root, version, target)
            _write_tarball(package_root, pending_archive)

        pending_checksum.write_text(
            f"{_sha256(pending_archive)}  {archive_path.name}\n",
            encoding="ascii",
        )
        os.chmod(pending_archive, 0o644)
        os.chmod(pending_checksum, 0o644)
        with pending_checksum.open("rb") as stream:
            os.fsync(stream.fileno())
        # Hard-link publication fails atomically if another process created the destination.
        os.link(pending_archive, archive_path)
        published_archive = True
        os.link(pending_checksum, checksum_path)
        published_checksum = True
        _fsync_directory(output_dir)
        success = True
        return archive_path, checksum_path
    except PackagingError:
        raise
    except FileExistsError as exc:
        raise PackagingError(f"release output appeared during packaging; refusing to overwrite: {exc.filename}") from exc
    except OSError as exc:
        raise PackagingError(f"could not publish release output: {exc}") from exc
    finally:
        if not success and published_archive:
            archive_path.unlink(missing_ok=True)
        if not success and published_checksum:
            checksum_path.unlink(missing_ok=True)
        if pending_archive is not None:
            pending_archive.unlink(missing_ok=True)
        if pending_checksum is not None:
            pending_checksum.unlink(missing_ok=True)


def _absolute_argument_path(value: str | None, default: Path) -> Path:
    path = Path(value) if value is not None else default
    if not path.is_absolute():
        path = Path.cwd() / path
    return path


def main(argv: Iterable[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Package a Linux x86_64 GNU shipbraid release")
    parser.add_argument("--binary", default=str(REPO_ROOT / "target/release/shipbraid"))
    parser.add_argument("--web-dir", default=str(REPO_ROOT / "web/dist"))
    parser.add_argument("--output-dir", default=str(REPO_ROOT / "target/dist"))
    parser.add_argument("--target", default=DEFAULT_TARGET)
    args = parser.parse_args(argv)
    try:
        archive, checksum = create_release_bundle(
            REPO_ROOT,
            binary=_absolute_argument_path(args.binary, REPO_ROOT / "target/release/shipbraid"),
            web_dir=_absolute_argument_path(args.web_dir, REPO_ROOT / "web/dist"),
            output_dir=_absolute_argument_path(args.output_dir, REPO_ROOT / "target/dist"),
            target=args.target,
        )
    except PackagingError as exc:
        print(f"package-release: error: {exc}", file=sys.stderr)
        return 1
    except OSError as exc:
        print(f"package-release: error: {exc}", file=sys.stderr)
        return 1
    print(f"Created {archive}")
    print(f"Created {checksum}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
