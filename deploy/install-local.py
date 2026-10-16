#!/usr/bin/env python3
"""Install the reviewed local-development service; does not submit a deployment."""
import argparse
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import urllib.parse
import tempfile


def quote(value):
    return json.dumps(str(value))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--listen', default='127.0.0.1:8787')
    parser.add_argument('--autotask-repository', type=Path, default=Path('/home/yahui/repos/autotask'))
    parser.add_argument('--autotask-checkout', type=Path, default=Path('/home/yahui/repos/autotask-head'))
    parser.add_argument('--autotask-script-ref', default='origin/vibe', help='Published trusted commit/ref containing the reviewed deployment script')
    args = parser.parse_args()
    endpoint = urllib.parse.urlsplit('http://' + args.listen)
    try:
        if not endpoint.hostname or not endpoint.port or not ipaddress.ip_address(endpoint.hostname).is_loopback:
            raise ValueError('non-loopback')
    except ValueError:
        parser.error('This development installer only permits a loopback IP listener; use SSH forwarding or a separately reviewed HTTPS ingress')
    root = Path(__file__).resolve().parent.parent
    binary = root / 'target/release/auto-deliver'
    dist = root / 'web/dist'
    if not binary.is_file() or not (dist / 'index.html').is_file():
        raise SystemExit('Run cargo build --release --locked and pnpm --dir web build first')
    config = Path.home() / '.config/auto-deliver'
    data = Path.home() / '.local/share/auto-deliver'
    unit_dir = Path.home() / '.config/systemd/user'
    for directory in (config, data, data / 'releases', data / 'mirrors', unit_dir):
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    # Upgrades require operator review; never replace the execution profile under a live job.
    active = subprocess.run(['systemctl', '--user', 'is-active', '--quiet', 'auto-deliver.service']).returncode == 0
    if active:
        raise SystemExit('Existing service is active. Drain, settle active tasks and stop it before this installer upgrades the immutable release/config')
    revision = subprocess.check_output(['git', '-C', str(root), 'rev-parse', 'HEAD'], text=True).strip()
    deploy_source_commit = subprocess.check_output(['git', '-C', str(args.autotask_repository), 'rev-parse', '--verify', '--end-of-options', args.autotask_script_ref + '^{commit}'], text=True).strip()
    deploy_source = subprocess.check_output(['git', '-C', str(args.autotask_repository), 'show', deploy_source_commit + ':scripts/deploy-local.sh'])
    stamp = hashlib.sha256(deploy_source + binary.read_bytes() + (root / 'integration/autotask/bridge.py').read_bytes() + (dist / 'index.html').read_bytes()).hexdigest()[:12]
    release = data / 'releases' / (revision[:12] + '-' + stamp)
    if not release.exists():
        with tempfile.TemporaryDirectory(prefix='.install-', dir=data / 'releases') as temporary:
            staged = Path(temporary)
            shutil.copy2(binary, staged / 'auto-deliver')
            shutil.copytree(dist, staged / 'web')
            shutil.copy2(root / 'integration/autotask/bridge.py', staged / 'autotask-bridge.py')
            (staged / 'deploy-local.sh').write_bytes(deploy_source)
            (staged / 'deploy-script-source.json').write_text(json.dumps({'source_commit': deploy_source_commit, 'path': 'scripts/deploy-local.sh', 'sha256': hashlib.sha256(deploy_source).hexdigest()}, indent=2))
            staged.rename(release)
    mirror = data / 'mirrors/autotask.git'
    if not mirror.exists():
        subprocess.run(['git', 'clone', '--quiet', '--mirror', str(args.autotask_repository), str(mirror)], check=True)
    # Bootstrap the allowed development line from the source actually deployed.
    # Shared source branches may diverge during parallel work; do not silently roll back.
    if not (data / 'coordinator.db').exists():
        deployed_revision = subprocess.check_output(['git', '-C', str(args.autotask_checkout), 'rev-parse', 'HEAD'], text=True).strip()
        subprocess.run(['git', '--git-dir', str(mirror), 'fetch', '--quiet', '--no-tags', str(args.autotask_repository), '+' + deployed_revision + ':refs/heads/vibe'], check=True)
    else:
        subprocess.run(['git', '--git-dir', str(mirror), 'fetch', '--quiet', '--no-tags', str(args.autotask_repository), 'refs/heads/vibe:refs/heads/vibe'], check=True)
    values = {}
    for name in ('operator', 'admin'):
        path = config / (name + '.token')
        if not path.exists():
            path.write_text(secrets.token_hex(32) + '\n')
        path.chmod(0o600)
        values[name] = path.read_text().strip()
    env_file = config / 'tokens.env'
    env_file.write_text('AUTO_DELIVER_OPERATOR_TOKEN=' + values['operator'] + '\nAUTO_DELIVER_ADMIN_TOKEN=' + values['admin'] + '\n')
    env_file.chmod(0o600)
    script = release / 'autotask-bridge.py'
    config_text = f'''# Local development installation. Production is explicitly disabled.
listen = {quote(args.listen)}
database_path = {quote(data / 'coordinator.db')}
dashboard_dir = {quote(release / 'web')}
max_active_runs = 2
tick_ms = 1000
coalesce_ms = 2000
[[tokens]]
id = "local-dev-operator"
token_env = "AUTO_DELIVER_OPERATOR_TOKEN"
lanes = ["autotask-dev"]
urgent = true
[[tokens]]
id = "local-administrator"
token_env = "AUTO_DELIVER_ADMIN_TOKEN"
admin = true
'''
    hook_args = ['--state-dir', str(data / 'autotask-runs'), '--repository', str(args.autotask_repository), '--deploy-checkout', str(args.autotask_checkout), '--deploy-script', str(release / 'deploy-local.sh')]
    for environment, enabled in [('dev', True), ('prod', False)]:
        config_text += f'''
[[lanes]]
id = "autotask-{environment}"
project = "autotask"
environment = "{environment}"
repository_id = "autotask"
repository_path = {quote(mirror)}
release_ref = "refs/heads/vibe"
profile_digest = "autotask-local-{environment}-v1"
components = ["app"]
allow_descendant = true
allow_preemption = false
enabled = {str(enabled).lower()}
[lanes.executor]
kind = "hooks"
'''
        for operation in ('dispatch', 'observe', 'verify'):
            config_text += f'''[lanes.executor.{operation}]
kind = "script"
path = {quote(script)}
interpreter = {quote(sys.executable)}
args = {json.dumps(hook_args)}
cwd = {quote(release)}
timeout_ms = 20000
'''
    config_path = config / 'config.toml'
    config_path.write_text(config_text)
    config_path.chmod(0o600)
    unit = f'''[Unit]
Description=auto-deliver deployment coordinator and dashboard
After=network.target
[Service]
Type=simple
WorkingDirectory={data}
ExecStart={release / 'auto-deliver'} serve --config {config_path}
EnvironmentFile={env_file}
Restart=on-failure
RestartSec=3
TimeoutStopSec=45
UMask=0077
NoNewPrivileges=true
[Install]
WantedBy=default.target
'''
    (unit_dir / 'auto-deliver.service').write_text(unit)
    subprocess.run(['systemctl', '--user', 'daemon-reload'], check=True)
    subprocess.run(['systemctl', '--user', 'enable', '--now', 'auto-deliver.service'], check=True)
    bin_dir = Path.home() / '.local/bin'
    bin_dir.mkdir(parents=True, exist_ok=True)
    link = bin_dir / 'auto-deliver'
    if link.exists() and not link.is_symlink():
        raise SystemExit('Refusing to overwrite an existing non-symlink auto-deliver binary')
    temporary = bin_dir / '.auto-deliver-install-link'
    if temporary.is_symlink():
        temporary.unlink()
    temporary.symlink_to(release / 'auto-deliver')
    os.replace(temporary, link)
    print(json.dumps({'unit': 'auto-deliver.service', 'listen': args.listen, 'release': str(release),
        'config': str(config_path), 'operator_token_file': str(config / 'operator.token'),
        'admin_token_file': str(config / 'admin.token'), 'production_enabled': False}, indent=2))


if __name__ == '__main__':
    main()
