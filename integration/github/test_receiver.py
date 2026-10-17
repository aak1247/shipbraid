"""Signed fixture tests for the standalone GitHub ingress."""

from __future__ import annotations

import hashlib
import hmac
import json
import socket
import threading
import time
import urllib.error
import urllib.request
import unittest
from dataclasses import replace
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from unittest.mock import patch

import receiver


SECRET = "test-webhook-secret-with-more-than-32-bytes"
TOKEN = "test-coordinator-token-do-not-return"
DELIVERY = "d9a8f2c0-5341-4a9d-aeed-138f879f3c01"
REPOSITORY_ID = 482516
REVISION = "a" * 40


class FakeCoordinatorHandler(BaseHTTPRequestHandler):
    def log_message(self, format_string: str, *args: Any) -> None:
        return

    def do_POST(self) -> None:
        size = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(size)
        record = {
            "path": self.path,
            "authorization": self.headers.get("Authorization"),
            "key": self.headers.get("Idempotency-Key"),
            "body": body,
        }
        self.server.records.append(record)  # type: ignore[attr-defined]

        if self.server.redirect_location is not None:  # type: ignore[attr-defined]
            self.send_response(302)
            self.send_header("Location", self.server.redirect_location)  # type: ignore[attr-defined]
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if self.server.error_status is not None:  # type: ignore[attr-defined]
            response = self.server.error_body  # type: ignore[attr-defined]
            self.send_response(self.server.error_status)  # type: ignore[attr-defined]
        else:
            key = record["key"]
            prior = self.server.by_key.get(key)  # type: ignore[attr-defined]
            if prior is not None and prior != body:
                response = b'{"code":"idempotency_conflict","message":"private response detail"}'
                self.send_response(409)
            else:
                self.server.by_key[key] = body  # type: ignore[attr-defined]
                response = b'{"status":"accepted"}'
                self.send_response(202)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)


class ReceiverTests(unittest.TestCase):
    def setUp(self) -> None:
        self.upstream = ThreadingHTTPServer(("127.0.0.1", 0), FakeCoordinatorHandler)
        self.upstream.records = []
        self.upstream.by_key = {}
        self.upstream.redirect_location = None
        self.upstream.error_status = None
        self.upstream.error_body = b'{"message":"private token/path/log"}'
        self.upstream_thread = threading.Thread(target=self.upstream.serve_forever, daemon=True)
        self.upstream_thread.start()

        raw_config = {
            "listen_host": "127.0.0.1",
            "listen_port": 18787,
            "path": "/github",
            "shipbraid_url": f"http://127.0.0.1:{self.upstream.server_port}",
            "timeout_seconds": 2,
            "secret_env": "GITHUB_WEBHOOK_SECRET",
            "routes": [
                {
                    "id": "org-app-main-v1",
                    "repository_id": REPOSITORY_ID,
                    "ref": "refs/heads/main",
                    "lane_id": "app-test",
                    "components": ["app", "worker"],
                    "token_env": "SHIPBRAID_GITHUB_TOKEN",
                }
            ],
        }
        self.config = receiver.parse_config(
            raw_config,
            {"GITHUB_WEBHOOK_SECRET": SECRET, "SHIPBRAID_GITHUB_TOKEN": TOKEN},
        )
        self.server = receiver.ReceiverHTTPServer(("127.0.0.1", 0), self.config, receiver.RequestHandler)
        self.server_thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.server_thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}/github"

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.server_thread.join(timeout=2)
        self.upstream.shutdown()
        self.upstream.server_close()
        self.upstream_thread.join(timeout=2)

    @staticmethod
    def push_payload(**changes: Any) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "ref": "refs/heads/main",
            "after": REVISION,
            "deleted": False,
            "repository": {"id": REPOSITORY_ID, "full_name": "org/app"},
        }
        payload.update(changes)
        return payload

    def post(
        self,
        body: bytes,
        event: str = "push",
        delivery: str = DELIVERY,
        signature: str | None = None,
    ) -> tuple[int, bytes]:
        if signature is None:
            signature = "sha256=" + hmac.new(SECRET.encode(), body, hashlib.sha256).hexdigest()
        request = urllib.request.Request(
            self.url,
            data=body,
            method="POST",
            headers={
                "Content-Type": "application/json",
                "X-GitHub-Event": event,
                "X-GitHub-Delivery": delivery,
                "X-Hub-Signature-256": signature,
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=4) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as error:
            with error:
                return error.code, error.read()

    def signed_json(self, value: Any) -> bytes:
        return json.dumps(value, separators=(",", ":")).encode()

    def test_github_reference_vector_and_wrong_signature(self) -> None:
        known_body = b"Hello, World!"
        known_secret = b"It's a Secret to Everybody"
        signature = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        self.assertTrue(receiver.verify_signature(known_body, known_secret, signature))
        self.assertFalse(receiver.verify_signature(known_body, known_secret, "sha256=" + "0" * 64))

        status, response = self.post(self.signed_json(self.push_payload()), signature="sha256=" + "0" * 64)
        self.assertEqual(status, 401)
        self.assertNotIn(TOKEN.encode(), response)
        self.assertEqual(self.upstream.records, [])

    def test_ping_and_unsupported_event_are_signed_but_never_forwarded(self) -> None:
        status, response = self.post(self.signed_json({"zen": "keep it logically awesome"}), event="ping")
        self.assertEqual(status, 200)
        self.assertIn(b"pong", response)
        status, _ = self.post(b"{}", event="issues")
        self.assertEqual(status, 404)
        self.assertEqual(self.upstream.records, [])

    def test_push_shape_repository_and_ref_are_checked(self) -> None:
        invalid = [
            self.push_payload(after="abc"),
            self.push_payload(repository={"id": True}),
            self.push_payload(repository={"id": "482516"}),
            self.push_payload(ref="refs/tags/v1"),
            self.push_payload(deleted="false"),
        ]
        for payload in invalid:
            with self.subTest(payload=payload):
                status, _ = self.post(self.signed_json(payload))
                self.assertEqual(status, 400)
        for payload in (
            self.push_payload(repository={"id": REPOSITORY_ID + 1}),
            self.push_payload(ref="refs/heads/release"),
        ):
            with self.subTest(payload=payload):
                status, _ = self.post(self.signed_json(payload))
                self.assertEqual(status, 403)
        self.assertEqual(self.upstream.records, [])

    def test_deleted_push_is_ignored_after_authorization(self) -> None:
        payload = self.push_payload(after="0" * 40, deleted=True)
        status, response = self.post(self.signed_json(payload))
        self.assertEqual(status, 202)
        self.assertIn(b"ignored", response)
        self.assertEqual(self.upstream.records, [])

    def test_same_delivery_reuses_key_and_changed_signed_body_conflicts(self) -> None:
        first_body = self.signed_json(self.push_payload(sender={"login": "alice"}))
        status, _ = self.post(first_body)
        self.assertEqual(status, 202)
        status, _ = self.post(first_body)
        self.assertEqual(status, 202)

        first, replay = self.upstream.records
        self.assertEqual(first["path"], "/v1/requests")
        self.assertEqual(first["authorization"], "Bearer " + TOKEN)
        self.assertEqual(first["key"], replay["key"])
        self.assertEqual(first["body"], replay["body"])
        request_body = json.loads(first["body"])
        self.assertEqual(request_body["lane_id"], "app-test")
        self.assertEqual(request_body["revision"], REVISION)
        self.assertEqual(request_body["components"], ["app", "worker"])
        self.assertEqual(request_body["coverage_mode"], "exact")
        self.assertEqual(request_body["priority"], "normal")
        self.assertIsNone(request_body["deadline_at"])
        self.assertNotIn("time", request_body)

        changed_body = self.signed_json(self.push_payload(sender={"login": "bob"}))
        status, response = self.post(changed_body)
        self.assertEqual(status, 409)
        self.assertNotIn(b"private response detail", response)

        other_delivery = "ef28f8f3-9997-47b4-910f-e2e717a4f391"
        status, _ = self.post(changed_body, delivery=other_delivery)
        self.assertEqual(status, 202)
        self.assertNotEqual(self.upstream.records[0]["key"], self.upstream.records[-1]["key"])

    def test_upstream_errors_and_redirects_do_not_leak_response_body(self) -> None:
        self.upstream.error_status = 500
        status, response = self.post(self.signed_json(self.push_payload()))
        self.assertEqual(status, 503)
        self.assertNotIn(b"private token/path/log", response)
        self.upstream.error_status = None

        self.upstream.redirect_location = "/redirect-target"
        status, response = self.post(self.signed_json(self.push_payload()), delivery="490e6d64-6811-4e9d-8878-f155c95b4d27")
        self.assertEqual(status, 502)
        self.assertNotIn(b"redirect-target", response)
        self.assertEqual(len(self.upstream.records), 2)

    def test_transport_failure_is_retryable_and_does_not_leak_url(self) -> None:
        unreachable = receiver.parse_config(
            {
                "listen_host": "127.0.0.1",
                "listen_port": 18788,
                "path": "/github",
                "shipbraid_url": "http://127.0.0.1:1",
                "timeout_seconds": 1,
                "secret_env": "GITHUB_WEBHOOK_SECRET",
                "routes": [
                    {
                        "id": "org-app-main-v1",
                        "repository_id": REPOSITORY_ID,
                        "ref": "refs/heads/main",
                        "lane_id": "app-test",
                        "components": ["app"],
                        "token_env": "SHIPBRAID_GITHUB_TOKEN",
                    }
                ],
            },
            {"GITHUB_WEBHOOK_SECRET": SECRET, "SHIPBRAID_GITHUB_TOKEN": TOKEN},
        )
        reply = receiver.handle_delivery(
            unreachable,
            self.signed_json(self.push_payload()),
            "sha256=" + hmac.new(SECRET.encode(), self.signed_json(self.push_payload()), hashlib.sha256).hexdigest(),
            "push",
            DELIVERY,
        )
        self.assertEqual(reply.status, 503)
        self.assertNotIn("127.0.0.1:1", reply.message)

    def test_coordinator_timeout_is_bounded_and_retryable(self) -> None:
        class TimeoutOpener:
            timeout: int | None = None

            def open(self, request: Any, timeout: int) -> Any:
                self.timeout = timeout
                raise TimeoutError

        opener = TimeoutOpener()
        payload = self.signed_json(self.push_payload())
        signature = "sha256=" + hmac.new(SECRET.encode(), payload, hashlib.sha256).hexdigest()
        with patch.object(receiver, "_coordinator_opener", return_value=opener):
            reply = receiver.handle_delivery(self.config, payload, signature, "push", DELIVERY)
        self.assertEqual(reply.status, 503)
        self.assertEqual(opener.timeout, self.config.timeout_seconds)

    def test_body_limit_and_route_validation(self) -> None:
        body = b"x" * (receiver.MAX_BODY_BYTES + 1)
        status, _ = self.post(body)
        self.assertEqual(status, 413)

        raw = {
            "listen_host": "127.0.0.1",
            "listen_port": 18789,
            "path": "/github",
            "shipbraid_url": "http://user:password@127.0.0.1:8787",
            "timeout_seconds": 2,
            "secret_env": "GITHUB_WEBHOOK_SECRET",
            "routes": [],
        }
        with self.assertRaises(receiver.ConfigError):
            receiver.parse_config(raw, {"GITHUB_WEBHOOK_SECRET": SECRET})

    def test_route_token_requires_at_least_32_utf8_bytes(self) -> None:
        route = next(iter(self.config.routes.values()))
        raw = {
            "listen_host": self.config.listen_host,
            "listen_port": self.config.listen_port,
            "path": self.config.path,
            "shipbraid_url": self.config.shipbraid_url,
            "timeout_seconds": self.config.timeout_seconds,
            "secret_env": "GITHUB_WEBHOOK_SECRET",
            "routes": [
                {
                    "id": route.identity,
                    "repository_id": route.repository_id,
                    "ref": route.ref,
                    "lane_id": route.lane_id,
                    "components": list(route.components),
                    "token_env": "SHIPBRAID_GITHUB_TOKEN",
                }
            ],
        }
        with self.assertRaisesRegex(receiver.ConfigError, "at least 32 UTF-8 bytes"):
            receiver.parse_config(
                raw,
                {"GITHUB_WEBHOOK_SECRET": SECRET, "SHIPBRAID_GITHUB_TOKEN": "t" * 31},
            )
        accepted = receiver.parse_config(
            raw,
            {"GITHUB_WEBHOOK_SECRET": SECRET, "SHIPBRAID_GITHUB_TOKEN": "t" * 32},
        )
        self.assertEqual(accepted.routes[(route.repository_id, route.ref)].token, "t" * 32)

    def test_body_timeout_is_total_even_when_slow_chunks_keep_arriving(self) -> None:
        short_config = replace(self.config, timeout_seconds=1)
        server = receiver.ReceiverHTTPServer(
            ("127.0.0.1", 0), short_config, receiver.RequestHandler
        )
        server_thread = threading.Thread(target=server.serve_forever, daemon=True)
        server_thread.start()
        body = b"xxxxx"
        signature = "sha256=" + hmac.new(SECRET.encode(), body, hashlib.sha256).hexdigest()
        headers = (
            f"POST /github HTTP/1.1\r\n"
            f"Host: 127.0.0.1\r\n"
            f"Content-Type: application/json\r\n"
            f"Content-Length: {len(body)}\r\n"
            f"X-GitHub-Event: push\r\n"
            f"X-GitHub-Delivery: {DELIVERY}\r\n"
            f"X-Hub-Signature-256: {signature}\r\n"
            f"Connection: close\r\n\r\n"
        ).encode("ascii")
        def send_slow_body(client: socket.socket) -> None:
            try:
                client.sendall(headers + body[:1])
                for chunk in (body[1:2], body[2:3], body[3:]):
                    time.sleep(0.4)
                    client.sendall(chunk)
            except OSError:
                return

        try:
            with socket.create_connection(server.server_address, timeout=3) as client:
                client.settimeout(3)
                sender = threading.Thread(target=send_slow_body, args=(client,), daemon=True)
                sender.start()
                with client.makefile("rb") as response:
                    response_line = response.readline()
                sender.join(timeout=3)
                self.assertFalse(sender.is_alive(), "slow body sender did not finish")
                self.assertTrue(response_line.startswith(b"HTTP/1.0 408 "), response_line)
            self.assertEqual(self.upstream.records, [])
        finally:
            server.shutdown()
            server.server_close()
            server_thread.join(timeout=2)


if __name__ == "__main__":
    unittest.main()
