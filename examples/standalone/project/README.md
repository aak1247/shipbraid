# Synthetic standalone project bundle

This bundle demonstrates Shipbraid's hooks v1 local adapter. It writes only under
the absolute directory named by `SHIPBRAID_DEMO_ROOT`; it does not use Git, Kubernetes, or a real service. Its receipt proves only that these synthetic files
match the requested revision, component, and profile.

The installer copies this directory into an immutable release and configures fixed
argv arrays for `deploy.sh` and `verify.py`. Put the temporary demo root in the
adapter's private environment file:

```text
SHIPBRAID_DEMO_ROOT=/absolute/path/to/a/private/temporary/demo-root
SHIPBRAID_DEMO_DELAY_SECONDS=1
```

`SHIPBRAID_DEMO_DELAY_SECONDS` is optional and accepts values from 0 to 10. The
deploy script appends one run ID to `deployment-starts` as soon as it starts, waits
for the configured delay, then writes a synthetic app artifact, deployment
manifest, and health file. The verifier rereads those files, hashes the actual
artifact bytes, and reports the values it observed. The marker is useful for
package end-to-end checks of duplicate dispatch and coordinator restarts.

Keep the demo root temporary and private. These scripts are examples for isolated
package tests, not production deployment or verification scripts.
