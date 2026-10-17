#!/usr/bin/env python3
"""Stdlib-only GitHub ping/push ingress for Shipbraid."""

from __future__ import annotations

import argparse
import hashlib
import hmac
import http.server
import ipaddress
import json
import os
import re
import socket
import ssl
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping


MAX_BODY_BYTES = 256 * 1024
BODY_READ_CHUNK_BYTES = 64 * 1024
MAX_REPOSITORY_ID = (1 << 63) - 1
FULL_SHA_RE = re.compile(r"^(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})$")
COMPONENT_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$")
ENV_NAME_RE = re.compile(r"^[A-Z_][A-Z0-9_]{0,127}$")
ROUTE_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$")
DELIVERY_RE = re.compile(
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
    r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
)


class ConfigError(ValueError):
    """Configuration error without secret values or filesystem paths."""


@dataclass(frozen=True)
class Route:
    identity: str
    repository_id: int
    ref: str
    lane_id: str
    components: tuple[str, ...]
    token: str


@dataclass(frozen=True)
class Config:
    listen_host: str
    listen_port: int
    path: str
    shipbraid_url: str
    timeout_seconds: int
    webhook_secret: bytes
    routes: Mapping[tuple[int, str], Route]


@dataclass(frozen=True)
class Reply:
    status: int
    message: str


def _is_loopback_host(host: str) -> bool:
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return host.lower() == "localhost"


def _check_keys(value: Mapping[str, Any], required: set[str], allowed: set[str], where: str) -> None:
    if set(value) - allowed or required - set(value):
        raise ConfigError(f"{where} has missing or unsupported fields")


def _require_object(value: Any, message: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ConfigError(message)
    return value


def _validate_shipbraid_url(value: Any) -> str:
    if not isinstance(value, str) or not value or any(ord(char) < 0x20 for char in value):
        raise ConfigError("shipbraid_url must be a loopback HTTP or HTTPS URL")
    try:
        parsed = urllib.parse.urlsplit(value)
        port = parsed.port
    except ValueError:
        raise ConfigError("shipbraid_url must be a loopback HTTP or HTTPS URL") from None
    if (
        parsed.scheme not in {"http", "https"}
        or parsed.hostname is None
        or not _is_loopback_host(parsed.hostname)
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
        or "?" in value
        or "#" in value
        or parsed.path not in {"", "/"}
        or (port is not None and not 1 <= port <= 65535)
    ):
        raise ConfigError("shipbraid_url must be loopback HTTP or HTTPS without credentials, path, query, or fragment")
    return value.rstrip("/")


def _valid_branch_ref(value: Any) -> bool:
    if not isinstance(value, str) or not value.startswith("refs/heads/"):
        return False
    if any(ord(char) <= 0x20 or ord(char) == 0x7F for char in value):
        return False
    if any(char in value for char in "~^:?*[\\") or ".." in value or "@{" in value:
        return False
    suffix = value[len("refs/heads/") :]
    if not suffix or suffix == "@" or suffix.startswith("/") or suffix.endswith(("/", ".")) or "//" in suffix:
        return False
    return all(
        segment and not segment.startswith(".") and not segment.endswith((".", ".lock"))
        for segment in suffix.split("/")
    )


def parse_config(raw: Any, environ: Mapping[str, str] | None = None) -> Config:
    """Validate the schema and resolve secret values from the process environment."""
    env = os.environ if environ is None else environ
    data = _require_object(raw, "configuration must be a JSON object")
    top_keys = {
        "listen_host",
        "listen_port",
        "path",
        "shipbraid_url",
        "timeout_seconds",
        "secret_env",
        "routes",
    }
    _check_keys(data, top_keys, top_keys, "configuration")

    host = data["listen_host"]
    if not isinstance(host, str) or not _is_loopback_host(host):
        raise ConfigError("listen_host must be a loopback address")
    port = data["listen_port"]
    if not isinstance(port, int) or isinstance(port, bool) or not 1 <= port <= 65535:
        raise ConfigError("listen_port must be an integer from 1 to 65535")
    if data["path"] != "/github":
        raise ConfigError("path must be /github")
    timeout = data["timeout_seconds"]
    if not isinstance(timeout, int) or isinstance(timeout, bool) or not 1 <= timeout <= 60:
        raise ConfigError("timeout_seconds must be an integer from 1 to 60")

    secret_env = data["secret_env"]
    if not isinstance(secret_env, str) or not ENV_NAME_RE.fullmatch(secret_env):
        raise ConfigError("secret_env must name an uppercase environment variable")
    secret_value = env.get(secret_env)
    try:
        secret_bytes = secret_value.encode("utf-8") if isinstance(secret_value, str) else b""
    except UnicodeEncodeError:
        secret_bytes = b""
    if len(secret_bytes) < 32:
        raise ConfigError("the webhook secret environment value must contain at least 32 UTF-8 bytes")

    url = _validate_shipbraid_url(data["shipbraid_url"])
    route_data = data["routes"]
    if not isinstance(route_data, list) or not route_data:
        raise ConfigError("routes must be a nonempty array")

    routes: dict[tuple[int, str], Route] = {}
    identities: set[str] = set()
    for item in route_data:
        route = _require_object(item, "each route must be an object")
        route_keys = {"id", "repository_id", "ref", "lane_id", "components", "token_env"}
        _check_keys(route, route_keys, route_keys, "route")

        identity = route["id"]
        if not isinstance(identity, str) or not ROUTE_ID_RE.fullmatch(identity) or identity in identities:
            raise ConfigError("each route id must be a unique stable identifier")
        identities.add(identity)

        repository_id = route["repository_id"]
        if (
            not isinstance(repository_id, int)
            or isinstance(repository_id, bool)
            or not 1 <= repository_id <= MAX_REPOSITORY_ID
        ):
            raise ConfigError("route repository_id must be a positive numeric GitHub id")
        ref = route["ref"]
        if not _valid_branch_ref(ref):
            raise ConfigError("route ref must be an exact refs/heads/ branch ref")
        route_key = (repository_id, ref)
        if route_key in routes:
            raise ConfigError("repository_id and ref route pairs must be unique")

        lane_id = route["lane_id"]
        if (
            not isinstance(lane_id, str)
            or not lane_id
            or len(lane_id.encode("utf-8")) > 128
            or any(ord(char) < 0x21 or ord(char) == 0x7F for char in lane_id)
        ):
            raise ConfigError("route lane_id must be a nonempty safe identifier")

        components = route["components"]
        if not isinstance(components, list) or not 1 <= len(components) <= 64:
            raise ConfigError("route components must contain between 1 and 64 names")
        if any(not isinstance(component, str) or not COMPONENT_RE.fullmatch(component) for component in components):
            raise ConfigError("route component names must be safe identifiers")
        if len(set(components)) != len(components):
            raise ConfigError("route component names must be unique")

        token_env = route["token_env"]
        if not isinstance(token_env, str) or not ENV_NAME_RE.fullmatch(token_env):
            raise ConfigError("token_env must name an uppercase environment variable")
        if token_env == secret_env:
            raise ConfigError("webhook secret and Shipbraid token must use separate environment variables")
        token = env.get(token_env)
        try:
            token_bytes = token.encode("utf-8") if isinstance(token, str) else b""
        except UnicodeEncodeError:
            token_bytes = b""
        if len(token_bytes) < 32:
            raise ConfigError("a route token environment value must contain at least 32 UTF-8 bytes")

        routes[route_key] = Route(identity, repository_id, ref, lane_id, tuple(components), token)

    return Config(
        listen_host=host,
        listen_port=port,
        path=data["path"],
        shipbraid_url=url,
        timeout_seconds=timeout,
        webhook_secret=secret_bytes,
        routes=routes,
    )


def load_config(path: str | os.PathLike[str], environ: Mapping[str, str] | None = None) -> Config:
    try:
        raw = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError):
        raise ConfigError("configuration file could not be read as JSON") from None
    return parse_config(raw, environ)


def verify_signature(body: bytes, secret: bytes, signature_header: str | None) -> bool:
    """Verify a raw GitHub body with a constant-time comparison."""
    expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
    shaped = isinstance(signature_header, str) and re.fullmatch(r"sha256=[0-9a-f]{64}", signature_header) is not None
    candidate = signature_header if shaped else "sha256=" + ("0" * 64)
    matched = hmac.compare_digest(expected, candidate)
    return bool(shaped and matched)


def _decode_json_object(body: bytes) -> dict[str, Any] | None:
    def unique_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate JSON key")
            result[key] = value
        return result

    try:
        value = json.loads(body, object_pairs_hook=unique_pairs)
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError):
        return None
    return value if isinstance(value, dict) else None


def _delivery_identity(value: str | None) -> str | None:
    if value is None or not DELIVERY_RE.fullmatch(value):
        return None
    try:
        return str(uuid.UUID(value))
    except ValueError:
        return None


def idempotency_key(route_identity: str, delivery_id: str) -> str:
    digest = hashlib.sha256((route_identity + "\0" + delivery_id).encode("utf-8")).hexdigest()
    return "github-" + digest


class NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    def redirect_request(
        self,
        req: urllib.request.Request,
        fp: Any,
        code: int,
        msg: str,
        headers: Any,
        newurl: str,
    ) -> None:
        return None


def _coordinator_opener() -> urllib.request.OpenerDirector:
    # The destination is loopback; do not send the bearer token through ambient proxies.
    return urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirectHandler())


def forward_push(config: Config, route: Route, delivery_id: str, raw_body: bytes, revision: str) -> Reply:
    payload_digest = hashlib.sha256(raw_body).hexdigest()
    request_body = {
        "lane_id": route.lane_id,
        "revision": revision,
        "components": list(route.components),
        "coverage_mode": "exact",
        "priority": "normal",
        "deadline_at": None,
        "reason": f"GitHub delivery {delivery_id} payload-sha256:{payload_digest}",
    }
    encoded_body = json.dumps(request_body, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    request = urllib.request.Request(
        config.shipbraid_url + "/v1/requests",
        data=encoded_body,
        method="POST",
        headers={
            "Authorization": "Bearer " + route.token,
            "Content-Type": "application/json",
            "Accept": "application/json",
            "Idempotency-Key": idempotency_key(route.identity, delivery_id),
        },
    )
    try:
        with _coordinator_opener().open(request, timeout=config.timeout_seconds) as response:
            status = response.status
    except urllib.error.HTTPError as error:
        status = error.code
        try:
            error.close()
        except OSError:
            pass
    except (urllib.error.URLError, TimeoutError, socket.timeout, OSError, ssl.SSLError, ValueError):
        return Reply(503, "Shipbraid could not be reached; GitHub may redeliver this event.")
    except Exception:
        return Reply(502, "Shipbraid returned an unusable response; GitHub may redeliver this event.")

    if status == 202:
        return Reply(202, "release request accepted")
    if status == 409:
        return Reply(409, "this delivery id was already used with different signed content")
    if 300 <= status < 400:
        return Reply(502, "Shipbraid redirects are not followed; check the loopback URL configuration.")
    if 500 <= status <= 599 or status == 429:
        return Reply(503, "Shipbraid could not complete the request; GitHub may redeliver this event.")
    return Reply(502, "Shipbraid rejected the request; inspect the coordinator and mirror, then redeliver if safe.")


def handle_delivery(
    config: Config,
    body: bytes,
    signature_header: str | None,
    event_header: str | None,
    delivery_header: str | None,
    content_type: str | None = "application/json",
) -> Reply:
    # Authenticate raw bytes before checking content type, JSON, event, or routing.
    if not verify_signature(body, config.webhook_secret, signature_header):
        return Reply(401, "invalid webhook signature")
    if content_type is None or content_type.split(";", 1)[0].strip().lower() != "application/json":
        return Reply(415, "application/json is required")
    if event_header not in {"ping", "push"}:
        return Reply(404, "unsupported GitHub event")
    delivery_id = _delivery_identity(delivery_header)
    if delivery_id is None:
        return Reply(400, "invalid GitHub delivery id")
    payload = _decode_json_object(body)
    if payload is None:
        return Reply(400, "invalid JSON webhook payload")
    if event_header == "ping":
        return Reply(200, "pong")

    repository = payload.get("repository")
    if not isinstance(repository, dict):
        return Reply(400, "invalid push payload")
    repository_id = repository.get("id")
    ref = payload.get("ref")
    revision = payload.get("after")
    deleted = payload.get("deleted")
    if (
        not isinstance(repository_id, int)
        or isinstance(repository_id, bool)
        or not 1 <= repository_id <= MAX_REPOSITORY_ID
        or not _valid_branch_ref(ref)
        or not isinstance(revision, str)
        or FULL_SHA_RE.fullmatch(revision) is None
        or not isinstance(deleted, bool)
    ):
        return Reply(400, "invalid push payload")

    route = config.routes.get((repository_id, ref))
    if route is None:
        return Reply(403, "repository or branch is not authorized")
    if deleted:
        return Reply(202, "deleted branch push ignored")
    return forward_push(config, route, delivery_id, body, revision)


class ReceiverHTTPServer(http.server.HTTPServer):
    allow_reuse_address = True
    request_queue_size = 16

    def __init__(
        self,
        server_address: tuple[str, int],
        config: Config,
        handler_class: type[http.server.BaseHTTPRequestHandler],
    ):
        self.config = config
        super().__init__(server_address, handler_class)

    def handle_error(self, request: Any, client_address: Any) -> None:
        # Malformed or timed-out clients must not stop serve_forever or print request data.
        return


class IPv6ReceiverHTTPServer(ReceiverHTTPServer):
    address_family = socket.AF_INET6


class RequestHandler(http.server.BaseHTTPRequestHandler):
    server_version = "ShipbraidGitHubReceiver"
    sys_version = ""
    protocol_version = "HTTP/1.0"

    @property
    def config(self) -> Config:
        return self.server.config  # type: ignore[attr-defined,return-value]

    def setup(self) -> None:
        super().setup()
        self.connection.settimeout(self.config.timeout_seconds)

    def log_message(self, format_string: str, *args: Any) -> None:
        return

    def send_error(self, code: int, message: str | None = None, explain: str | None = None) -> None:
        self._send_reply(Reply(400 if code < 500 else 500, "invalid HTTP request"))

    def do_POST(self) -> None:
        try:
            self._handle_post()
        except (socket.timeout, TimeoutError):
            self._send_reply(Reply(408, "webhook request timed out"))
        except (BrokenPipeError, ConnectionResetError):
            return
        except Exception:
            self._send_reply(Reply(500, "request could not be processed"))

    def do_GET(self) -> None:
        self._send_reply(Reply(405, "method not allowed"))

    do_HEAD = do_GET
    do_PUT = do_GET
    do_PATCH = do_GET
    do_DELETE = do_GET

    def _single_header(self, name: str) -> str | None:
        values = self.headers.get_all(name, [])
        if len(values) != 1:
            return None
        return values[0].strip()

    def _handle_post(self) -> None:
        if self.path != self.config.path:
            self.close_connection = True
            self._send_reply(Reply(404, "route not found"))
            return
        if self.headers.get_all("Transfer-Encoding", []):
            self.close_connection = True
            self._send_reply(Reply(400, "unsupported request framing"))
            return
        lengths = self.headers.get_all("Content-Length", [])
        if len(lengths) != 1 or not re.fullmatch(r"[0-9]{1,10}", lengths[0].strip()):
            self.close_connection = True
            self._send_reply(Reply(411, "a valid Content-Length is required"))
            return
        content_length = int(lengths[0])
        if content_length > MAX_BODY_BYTES:
            self.close_connection = True
            self._send_reply(Reply(413, "webhook body exceeds 256 KiB"))
            return
        body = bytearray()
        deadline = time.monotonic() + self.config.timeout_seconds
        while len(body) < content_length:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self.close_connection = True
                self._send_reply(Reply(408, "webhook request timed out"))
                return
            self.connection.settimeout(remaining)
            try:
                chunk = self.rfile.read1(min(BODY_READ_CHUNK_BYTES, content_length - len(body)))
            except (socket.timeout, TimeoutError):
                self.close_connection = True
                self._send_reply(Reply(408, "webhook request timed out"))
                return
            if not chunk:
                break
            body.extend(chunk)
        if time.monotonic() > deadline:
            self.close_connection = True
            self._send_reply(Reply(408, "webhook request timed out"))
            return
        if len(body) != content_length:
            self.close_connection = True
            self._send_reply(Reply(400, "incomplete webhook body"))
            return
        reply = handle_delivery(
            self.config,
            bytes(body),
            self._single_header("X-Hub-Signature-256"),
            self._single_header("X-GitHub-Event"),
            self._single_header("X-GitHub-Delivery"),
            self._single_header("Content-Type"),
        )
        self._send_reply(reply)

    def _send_reply(self, reply: Reply) -> None:
        body = json.dumps({"message": reply.message}, separators=(",", ":")).encode("utf-8")
        try:
            self.send_response(reply.status)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError, socket.timeout):
            pass
        self.close_connection = True


def serve(config: Config) -> None:
    server_class: type[ReceiverHTTPServer] = IPv6ReceiverHTTPServer if ":" in config.listen_host else ReceiverHTTPServer
    server = server_class((config.listen_host, config.listen_port), config, RequestHandler)
    try:
        server.serve_forever(poll_interval=0.5)
    finally:
        server.server_close()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Receive signed GitHub ping and push webhooks for Shipbraid.")
    parser.add_argument("--config", required=True, help="private JSON configuration file")
    args = parser.parse_args(argv)
    try:
        config = load_config(args.config)
    except ConfigError as error:
        print(f"github receiver: {error}", file=sys.stderr)
        return 2
    try:
        serve(config)
    except OSError:
        print("github receiver: could not bind the configured loopback listener", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
