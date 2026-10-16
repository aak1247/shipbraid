#!/usr/bin/env python3
"""Measure API submissions. Requires an explicitly configured ScriptedExecutor server.

No real deployment target is permitted. Credentials come from AUTO_DELIVER_TOKEN.
"""
import argparse
import concurrent.futures
import json
import os
import statistics
import time
import urllib.error
import urllib.request
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:8787")
    parser.add_argument("--lane", required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--components", default="app")
    parser.add_argument("--rate", type=int, default=50)
    parser.add_argument("--seconds", type=int, default=60)
    args = parser.parse_args()
    if args.rate <= 0 or args.seconds <= 0:
        parser.error("rate and seconds must be positive")
    token = os.environ["AUTO_DELIVER_TOKEN"]
    base = args.url.rstrip("/")
    headers = {"Authorization": "Bearer " + token, "Content-Type": "application/json"}
    with urllib.request.urlopen(urllib.request.Request(base + "/v1/system", headers=headers), timeout=10) as response:
        system = json.load(response)
    if system.get("executor") != "scripted" or system.get("draining"):
        raise SystemExit("load measurement requires an active ScriptedExecutor lane")
    body = json.dumps({"lane_id": args.lane, "revision": args.revision, "components": args.components.split(","), "coverage_mode":"exact", "priority":"normal", "reason":"SYNTHETIC load acceptance"}).encode()
    batch = str(uuid.uuid4())

    def submit(index):
        started = time.monotonic()
        request = urllib.request.Request(base + "/v1/requests", data=body, headers={**headers, "Idempotency-Key":f"load-{batch}-{index}"})
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                response.read()
                status = response.status
        except urllib.error.HTTPError as error:
            error.read()
            status = error.code
        except OSError:
            status = 0
        return status, (time.monotonic() - started) * 1000

    began = time.monotonic()
    futures = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=64) as pool:
        for index in range(args.rate * args.seconds):
            delay = began + index / args.rate - time.monotonic()
            if delay > 0:
                time.sleep(delay)
            futures.append(pool.submit(submit, index))
        results = [future.result() for future in futures]
    samples = sorted(latency for status, latency in results if status == 202)
    statuses = {}
    for status, _ in results:
        statuses[str(status)] = statuses.get(str(status), 0) + 1
    report = {"executor":"scripted", "rate":args.rate, "scheduled_seconds":args.seconds, "total_seconds":round(time.monotonic()-began,3), "submitted":len(results), "statuses":statuses, "p50_ms":round(statistics.median(samples),3) if samples else None, "p95_ms":round(samples[min(len(samples)-1, int(len(samples)*0.95))],3) if samples else None, "includes_trusted_git_resolution":True, "nfr_001_pass":len(samples)==len(results) and bool(samples) and samples[min(len(samples)-1,int(len(samples)*0.95))]<250}
    print(json.dumps(report, indent=2))
    return 0 if report["nfr_001_pass"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
