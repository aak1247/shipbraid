#!/bin/bash
set -euo pipefail

exec python3 -c '
import datetime
import hashlib
import json
import os
from pathlib import Path
import stat
import sys
import time

root_value = os.environ.get("SHIPBRAID_DEMO_ROOT", "")
if not root_value or not Path(root_value).is_absolute():
    raise SystemExit("SHIPBRAID_DEMO_ROOT must name an absolute temporary demo directory")
root = Path(root_value)
root.mkdir(mode=0o700, parents=True, exist_ok=True)
if root.is_symlink() or not root.is_dir():
    raise SystemExit("demo root must be a real directory")
root = root.resolve(strict=True)
root_info = root.stat()
if root_info.st_uid != os.getuid() or stat.S_IMODE(root_info.st_mode) & 0o077:
    raise SystemExit("demo root must be user-owned and private")

delay_value = os.environ.get("SHIPBRAID_DEMO_DELAY_SECONDS", "0")
try:
    delay = float(delay_value)
except ValueError as error:
    raise SystemExit("SHIPBRAID_DEMO_DELAY_SECONDS must be a number from 0 to 10") from error
if delay < 0 or delay > 10:
    raise SystemExit("SHIPBRAID_DEMO_DELAY_SECONDS must be a number from 0 to 10")

envelope = json.load(sys.stdin)
if envelope.get("version") != 1 or envelope.get("operation") != "dispatch":
    raise SystemExit("expected a hooks v1 dispatch envelope")
run = envelope.get("run", {})
target = run.get("target", {})
if target.get("components") != ["app"]:
    raise SystemExit("the synthetic project supports only the app component")
revision = target.get("revision")
profile = target.get("profile_digest")
if not isinstance(revision, str) or not isinstance(profile, str):
    raise SystemExit("dispatch target is incomplete")

marker_fd = os.open(root / "deployment-starts", os.O_WRONLY | os.O_CREAT | os.O_APPEND |
                    getattr(os, "O_NOFOLLOW", 0), 0o600)
try:
    os.write(marker_fd, (str(run.get("id", "unknown")) + "\n").encode("utf-8"))
    os.fsync(marker_fd)
finally:
    os.close(marker_fd)
if delay:
    time.sleep(delay)

artifact = json.dumps({"kind": "synthetic-app", "source_revision": revision,
                       "profile_digest": profile}, sort_keys=True, separators=(",", ":")).encode("utf-8") + b"\n"
artifact_digest = hashlib.sha256(artifact).hexdigest()

manifest = {"synthetic": True, "source_revision": revision, "profile_digest": profile,
            "component_digests": {"app": artifact_digest},
            "deployed_at": datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")}

def write_file(name, content):
    descriptor = os.open(root / name, os.O_WRONLY | os.O_CREAT | os.O_TRUNC |
                         getattr(os, "O_NOFOLLOW", 0), 0o600)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(content)
        handle.flush()
        os.fsync(handle.fileno())

write_file("app-artifact.json", artifact)
write_file("deployment.json", json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode("utf-8") + b"\n")
health = {"synthetic": True, "healthy": True, "source_revision": revision, "profile_digest": profile}
write_file("health.json", json.dumps(health, sort_keys=True, separators=(",", ":")).encode("utf-8") + b"\n")
'
