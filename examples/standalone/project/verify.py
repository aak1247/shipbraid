#!/usr/bin/env python3
"""Read-only synthetic environment observer for the standalone demo bundle."""

import datetime
import hashlib
import json
import os
from pathlib import Path
import stat
import sys


def main() -> int:
    root_value = os.environ.get("SHIPBRAID_DEMO_ROOT", "")
    if not root_value or not Path(root_value).is_absolute():
        return 2
    root = Path(root_value)
    if root.is_symlink() or not root.is_dir():
        return 2
    root = root.resolve(strict=True)
    root_info = root.stat()
    if root_info.st_uid != os.getuid() or stat.S_IMODE(root_info.st_mode) & 0o077:
        return 2
    envelope = json.load(sys.stdin)
    if envelope.get("version") != 1 or envelope.get("operation") != "verify":
        return 2

    # Observe the files in the fake environment. Expected target values are only used
    # for comparison; the reported values come from disk and the artifact bytes.
    files = [root / "deployment.json", root / "health.json", root / "app-artifact.json"]
    if any(path.is_symlink() or not path.is_file() for path in files):
        return 2
    manifest = json.loads(files[0].read_text(encoding="utf-8"))
    health = json.loads(files[1].read_text(encoding="utf-8"))
    artifact_bytes = files[2].read_bytes()
    artifact = json.loads(artifact_bytes)
    actual_digest = hashlib.sha256(artifact_bytes).hexdigest()
    expected = envelope.get("run", {}).get("target", {})
    digests = manifest.get("component_digests", {})
    healthy = (
        manifest.get("synthetic") is True
        and health.get("synthetic") is True
        and health.get("healthy") is True
        and health.get("source_revision") == manifest.get("source_revision")
        and health.get("profile_digest") == manifest.get("profile_digest")
        and artifact.get("kind") == "synthetic-app"
        and artifact.get("source_revision") == manifest.get("source_revision")
        and artifact.get("profile_digest") == manifest.get("profile_digest")
        and manifest.get("source_revision") == expected.get("revision")
        and manifest.get("profile_digest") == expected.get("profile_digest")
        and expected.get("components") == ["app"]
        and digests == {"app": actual_digest}
    )
    response = {
        "source_revision": manifest.get("source_revision", ""),
        "component_digests": {"app": actual_digest},
        "profile_digest": manifest.get("profile_digest", ""),
        "observed_at": datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z"),
        "healthy": healthy,
        "verification_report": json.dumps({
            "synthetic": True,
            "health_file": "health.json",
            "artifact_file": "app-artifact.json",
            "artifact_sha256": actual_digest,
            "deployment_manifest": manifest,
        }, sort_keys=True, separators=(",", ":")),
    }
    json.dump(response, sys.stdout, sort_keys=True, separators=(",", ":"))
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
