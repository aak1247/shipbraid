#!/usr/bin/env python3
"""Install a verified Shipbraid bundle into a NEW private prefix; never start services."""
import argparse
import hashlib
import ipaddress
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import secrets
import shutil
import stat
import subprocess
import sys


def sha256(path):
    with path.open('rb') as handle:
        return hashlib.file_digest(handle, 'sha256').hexdigest()


def regular_files(root):
    files = {}
    if root.is_symlink() or not root.is_dir():
        raise ValueError('bundle must be a real directory')
    for path in root.rglob('*'):
        if path.is_symlink():
            raise ValueError('symlinks are not permitted in bundles')
        if path.is_file():
            files[path.relative_to(root).as_posix()] = path
        elif not path.is_dir():
            raise ValueError('non-regular bundle entry')
    return files


def verify_bundle(root):
    files = regular_files(root)
    manifest = json.loads((root / 'manifest.json').read_text())
    if (manifest.get('schema_version') != 1
            or manifest.get('target') != 'x86_64-unknown-linux-gnu'
            or not isinstance(manifest.get('version'), str)
            or not re.fullmatch(r'\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?', manifest['version'])
            or not isinstance(manifest.get('files'), dict)):
        raise ValueError('unsupported release manifest')
    entries = manifest['files']
    if set(entries) != set(files) - {'manifest.json'}:
        raise ValueError('release manifest must cover every file exactly')
    for name, expected in entries.items():
        relative = PurePosixPath(name)
        if (relative.is_absolute() or '..' in relative.parts or str(relative) != name
                or '\\' in name or not isinstance(expected, str)
                or not re.fullmatch(r'[0-9a-f]{64}', expected)
                or sha256(files[name]) != expected):
            raise ValueError('release checksum mismatch or invalid path: ' + name)
    for name in ('bin/shipbraid', 'web/index.html', 'adapters/local/adapter.py',
                 'adapters/github/receiver.py', 'install.py', 'VERSION', 'LICENSE'):
        if name not in files:
            raise ValueError('release is missing ' + name)
    if (root / 'VERSION').read_text().strip() != manifest['version']:
        raise ValueError('VERSION disagrees with manifest')
    if platform.system() != 'Linux' or platform.machine() not in ('x86_64', 'amd64'):
        raise ValueError('this bundle supports Linux x86_64 only')
    result = subprocess.run([str(root / 'bin/shipbraid'), '--version'],
                            capture_output=True, text=True, timeout=10, check=True)
    if result.stdout.strip() != 'shipbraid ' + manifest['version']:
        raise ValueError('binary version disagrees with manifest')
    return manifest


def private_write(path, text):
    descriptor = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    with os.fdopen(descriptor, 'w') as handle:
        handle.write(text)
        handle.flush()
        os.fsync(handle.fileno())


def unit_quote(value, *, executable_argument=False):
    # systemd expands specifiers even in quotes, and expands $ in ExecStart.
    text = str(value).replace('%', '%%')
    if executable_argument:
        text = text.replace('$', '$$')
    return json.dumps(text)


def install(args):
    mode = getattr(args, 'mode', 'local')
    if mode not in ('local', 'server'):
        raise ValueError('mode must be local or server')
    prefix = args.prefix.expanduser().absolute()
    if prefix.exists() or prefix.is_symlink():
        raise ValueError('installation prefix already exists; use the documented upgrade procedure')
    bundle = args.bundle_root.expanduser().absolute()
    manifest = verify_bundle(bundle)
    if not re.fullmatch(r'[a-z][a-z0-9_-]{0,63}', args.project):
        raise ValueError('project must be a short lowercase identifier')
    components = args.components.split(',')
    if (not components or len(set(components)) != len(components)
            or any(not re.fullmatch(r'[a-z][a-z0-9_-]{0,63}', value) for value in components)):
        raise ValueError('components must be unique comma-separated identifiers')
    if args.environment not in ('dev', 'test'):
        raise ValueError('standalone initialization permits dev/test only')
    if mode == 'server':
        host, separator, port = args.listen.rpartition(':')
        try:
            if (not separator or not ipaddress.ip_address(host.strip('[]')).is_loopback
                    or (':' in host and not (host.startswith('[') and host.endswith(']')))
                    or not 1 <= int(port) <= 65535):
                raise ValueError()
        except ValueError as error:
            raise ValueError('listen must be a loopback IP:port') from error
    repository = args.repository.expanduser().resolve(strict=True)
    bare = subprocess.check_output(['git', '-C', str(repository), 'rev-parse',
                                    '--is-bare-repository'], text=True).strip()
    if bare != 'true':
        raise ValueError('repository must be a trusted local bare mirror')
    if not args.release_ref.startswith('refs/heads/'):
        raise ValueError('release-ref must be a full refs/heads/ branch')
    subprocess.run(['git', 'check-ref-format', args.release_ref], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(['git', '-C', str(repository), 'rev-parse', '--verify',
                    '--end-of-options', args.release_ref + '^{commit}'], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    project_bundle = args.project_bundle.expanduser().absolute()
    if (prefix.resolve().is_relative_to(bundle.resolve())
            or prefix.resolve().is_relative_to(project_bundle.resolve())):
        raise ValueError('installation prefix must be outside the release and project bundles')
    project_files = regular_files(project_bundle)
    for name in ('deploy.sh', 'verify.py'):
        if name not in project_files:
            raise ValueError('project bundle needs deploy.sh and verify.py')
    for name in project_files:
        if (any(part.startswith('.') or part == '__pycache__' for part in PurePosixPath(name).parts)
                or Path(name).name in ('id_rsa', 'id_ed25519')
                or name.lower().endswith(('.pem', '.key', '.db', '.sqlite', '.sqlite3', '.log', '.pyc'))):
            raise ValueError('use a curated project script bundle, without hidden files, credentials, databases or caches')
    # Validate everything above before creating any installation directories.
    prefix.parent.mkdir(parents=True, exist_ok=True)
    prefix.mkdir(mode=0o700)  # exclusive reservation; never merge with an existing installation
    for directory in ('etc', 'var', 'var/runs', 'var/work', 'bin', 'releases'):
        (prefix / directory).mkdir(mode=0o700)
    release_id = manifest['version'] + '-' + sha256(bundle / 'manifest.json')[:12]
    release = prefix / 'releases' / release_id
    shutil.copytree(bundle, release)
    project_dir = release / 'project'
    shutil.copytree(project_bundle, project_dir)
    lane = args.project + '-' + args.environment
    adapter_config = {
        'lane_id': lane, 'project': args.project, 'environment': args.environment,
        'repository_id': args.project, 'components': sorted(components),
        'state_dir': str(prefix / 'var/runs'), 'cwd': str(prefix / 'var/work'),
        'deploy': ['/bin/bash', str(project_dir / 'deploy.sh')],
        'verify': [sys.executable, str(project_dir / 'verify.py')],
        'environment_file': str(prefix / 'etc/deploy.env'),
    }
    private_write(release / 'adapter.json', json.dumps(adapter_config, indent=2) + '\n')
    private_write(prefix / 'etc/deploy.env',
                  '# Deployment/verification credentials only. KEY=value, no shell expansion.\n'
                  '# Synthetic example only: SHIPBRAID_DEMO_ROOT=/absolute/private/demo-environment\n')
    quote = lambda value: json.dumps(str(value), ensure_ascii=False)
    config = f'''# Standalone development/test installation. Review scripts before starting.
mode = "{mode}"
database_path = {quote(prefix / 'var/coordinator.db')}
max_active_runs = 2
'''

    if mode == 'server':
        values = {name: secrets.token_hex(32) for name in ('operator', 'admin')}
        for name, token in values.items():
            private_write(prefix / 'etc' / (name + '.token'), token + '\n')
        private_write(prefix / 'etc/tokens.env', ''.join(
            f'SHIPBRAID_{name.upper()}_TOKEN={token}\n' for name, token in values.items()))
        config += f'''listen = {quote(args.listen)}
dashboard_dir = {quote(release / 'web')}

[[tokens]]
id = "{args.project}-operator"
token_env = "SHIPBRAID_OPERATOR_TOKEN"
lanes = ["{lane}"]

[[tokens]]
id = "local-administrator"
token_env = "SHIPBRAID_ADMIN_TOKEN"
admin = true
'''

    config += f'''
[[lanes]]
id = "{lane}"
project = "{args.project}"
environment = "{args.environment}"
repository_id = "{args.project}"
repository_path = {quote(repository)}
release_ref = {quote(args.release_ref)}
profile_digest = "{args.project}-{args.environment}-v1"
components = {json.dumps(sorted(components))}
allow_descendant = false
allow_preemption = false
enabled = true

[lanes.executor]
kind = "hooks"
'''
    for operation in ('dispatch', 'observe', 'verify'):
        config += f'''
[lanes.executor.{operation}]
kind = "script"
path = {quote(release / 'adapters/local/adapter.py')}
interpreter = {quote(sys.executable)}
args = ["--config", {quote(release / 'adapter.json')}]
timeout_ms = 20000
'''
    private_write(prefix / 'etc/config.toml', config)
    if mode == 'server':
        unit = f'''[Unit]
Description=Shipbraid standalone request coordinator
After=network.target

[Service]
Type=simple
WorkingDirectory={unit_quote(prefix / 'var')}
ExecStart={unit_quote(release / 'bin/shipbraid', executable_argument=True)} serve --config {unit_quote(prefix / 'etc/config.toml', executable_argument=True)}
EnvironmentFile={unit_quote(prefix / 'etc/tokens.env')}
UMask=0077
Restart=on-failure
RestartSec=3
TimeoutStopSec=45
NoNewPrivileges=true

[Install]
WantedBy=default.target
'''
        private_write(prefix / 'shipbraid.service', unit)
    (prefix / 'bin/shipbraid').symlink_to(release / 'bin/shipbraid')
    # Freeze executable/config/script content. Writable state and credentials live outside.
    for path in release.rglob('*'):
        if path.is_file():
            path.chmod(0o555 if path.stat().st_mode & stat.S_IXUSR else 0o444)
    for path in sorted(release.rglob('*'), key=lambda value: len(value.parts), reverse=True):
        if path.is_dir():
            path.chmod(0o555)
    release.chmod(0o555)
    record = {
        'version': manifest['version'], 'mode': mode, 'release': str(release),
        'prefix': str(prefix), 'binary': str(prefix / 'bin/shipbraid'),
        'config': str(prefix / 'etc/config.toml'),
        'adapter_config': str(release / 'adapter.json'),
        'deploy_env': str(prefix / 'etc/deploy.env'),
        'database': str(prefix / 'var/coordinator.db'),
        'run_state_dir': str(prefix / 'var/runs'),
        'work_dir': str(prefix / 'var/work'),
        'lane_id': lane, 'service_started': False, 'deployment_submitted': False,
    }
    if mode == 'server':
        record.update({
            'unit': str(prefix / 'shipbraid.service'),
            'operator_token_file': str(prefix / 'etc/operator.token'),
            'admin_token_file': str(prefix / 'etc/admin.token'),
        })
    private_write(prefix / 'installation.json', json.dumps(record, indent=2) + '\n')
    return record


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bundle-root', type=Path, default=Path(__file__).resolve().parent)
    parser.add_argument('--prefix', type=Path, default=Path.home() / '.local/share/shipbraid')
    parser.add_argument('--project', required=True)
    parser.add_argument('--environment', choices=('dev', 'test'), default='dev')
    parser.add_argument('--repository', type=Path, required=True)
    parser.add_argument('--release-ref', default='refs/heads/main')
    parser.add_argument('--components', default='app')
    parser.add_argument('--project-bundle', type=Path, required=True,
                        help='Reviewed directory containing deploy.sh, verify.py and their dependencies')
    parser.add_argument('--mode', choices=('local', 'server'), default='local',
                        help='local foreground CLI (default) or shared HTTP coordinator')
    parser.add_argument('--listen', default='127.0.0.1:8787',
                        help='loopback listener for --mode server')
    return parser


def main():
    parser = build_parser()
    args = parser.parse_args()
    try:
        print(json.dumps(install(args), indent=2))
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        parser.exit(1, 'Installation refused or incomplete: ' + str(error) + '\n')


if __name__ == '__main__':
    main()
