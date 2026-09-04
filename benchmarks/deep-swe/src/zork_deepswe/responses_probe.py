from __future__ import annotations

import argparse
import base64
import http.client
import json
import os
import threading
import urllib.parse
from collections.abc import Callable
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, BinaryIO

TERMINAL_EVENT_TYPES = {
    "response.completed",
    "response.incomplete",
    "response.failed",
    "error",
}
HOP_BY_HOP_HEADERS = {
    "connection",
    "content-length",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
}


@dataclass(frozen=True)
class HttpsProxy:
    host: str
    port: int
    authorization: str | None


class SseAudit:
    """Reduce raw SSE bytes to non-content protocol metadata."""

    def __init__(
        self,
        request_id: str,
        emit: Callable[[dict[str, object]], None],
    ) -> None:
        self._request_id = request_id
        self._emit = emit
        self._buffer = bytearray()
        self._sequence = 0
        self._terminal_seen = False
        self._done_seen = False
        self._finished = False

    def feed(self, chunk: bytes) -> None:
        if self._finished:
            raise RuntimeError("cannot feed a finished SSE audit")
        self._buffer.extend(chunk)
        while True:
            boundary = _find_event_boundary(self._buffer)
            if boundary is None:
                return
            offset, width = boundary
            frame = bytes(self._buffer[:offset])
            del self._buffer[: offset + width]
            self._consume_frame(frame)

    def finish(self, *, transport_error: bool = False) -> None:
        if self._finished:
            return
        self._finished = True
        if self._buffer:
            self._consume_frame(bytes(self._buffer))
            self._buffer.clear()
        self._emit(
            {
                "record": "transport_end",
                "request_id": self._request_id,
                "event_count": self._sequence,
                "terminal_seen": self._terminal_seen,
                "done_seen": self._done_seen,
                "transport_error": transport_error,
            }
        )

    def _consume_frame(self, frame: bytes) -> None:
        data_lines: list[bytes] = []
        for line in frame.replace(b"\r\n", b"\n").split(b"\n"):
            if line.startswith(b"data:"):
                value = line[5:]
                if value.startswith(b" "):
                    value = value[1:]
                data_lines.append(value)
        if not data_lines:
            return

        self._sequence += 1
        payload = b"\n".join(data_lines)
        if payload == b"[DONE]":
            self._done_seen = True
            self._emit(
                {
                    "record": "sse_event",
                    "request_id": self._request_id,
                    "sequence": self._sequence,
                    "type": "[DONE]",
                }
            )
            return

        try:
            document = json.loads(payload)
        except (UnicodeDecodeError, json.JSONDecodeError):
            self._emit(
                {
                    "record": "sse_event",
                    "request_id": self._request_id,
                    "sequence": self._sequence,
                    "type": "[unparseable]",
                }
            )
            return
        event_type = document.get("type") if isinstance(document, dict) else None
        safe_type = event_type if isinstance(event_type, str) else "[missing]"
        self._emit(
            {
                "record": "sse_event",
                "request_id": self._request_id,
                "sequence": self._sequence,
                "type": safe_type,
            }
        )
        if safe_type not in TERMINAL_EVENT_TYPES:
            return

        self._terminal_seen = True
        response = document.get("response") if isinstance(document, dict) else None
        response = response if isinstance(response, dict) else {}
        usage = response.get("usage")
        usage = usage if isinstance(usage, dict) else None
        terminal: dict[str, object] = {
            "record": "terminal",
            "request_id": self._request_id,
            "sequence": self._sequence,
            "type": safe_type,
            "status": response.get("status")
            if isinstance(response.get("status"), str)
            else None,
            "usage_present": usage is not None,
        }
        if usage is not None:
            input_tokens = usage.get("input_tokens")
            output_tokens = usage.get("output_tokens")
            if isinstance(input_tokens, int) and not isinstance(input_tokens, bool):
                terminal["input_tokens"] = input_tokens
            if isinstance(output_tokens, int) and not isinstance(output_tokens, bool):
                terminal["output_tokens"] = output_tokens
        self._emit(terminal)


def _find_event_boundary(buffer: bytearray) -> tuple[int, int] | None:
    candidates = [
        (index, len(delimiter))
        for delimiter in (b"\n\n", b"\r\n\r\n")
        if (index := buffer.find(delimiter)) >= 0
    ]
    return min(candidates, default=None)


class JsonlSink:
    def __init__(self, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        self._stream = path.open("a", encoding="utf-8")
        self._lock = threading.Lock()

    def emit(self, record: dict[str, object]) -> None:
        line = json.dumps(record, separators=(",", ":"), sort_keys=True)
        with self._lock:
            self._stream.write(line + "\n")
            self._stream.flush()


class ProbeServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(
        self,
        address: tuple[str, int],
        upstream_base: str,
        sink: JsonlSink,
    ) -> None:
        super().__init__(address, ProbeHandler)
        self.upstream_base = upstream_base.rstrip("/")
        self.sink = sink
        self._request_sequence = 0
        self._request_lock = threading.Lock()

    def next_request_id(self) -> str:
        with self._request_lock:
            self._request_sequence += 1
            return f"request-{self._request_sequence}"


class ProbeHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: ProbeServer

    def do_GET(self) -> None:  # noqa: N802
        if self.path != "/healthz":
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self) -> None:  # noqa: N802
        request_id = self.server.next_request_id()
        content_length = self.headers.get("Content-Length")
        try:
            length = int(content_length) if content_length is not None else 0
        except ValueError:
            self.send_error(400)
            return
        body = self.rfile.read(length)
        target = f"{self.server.upstream_base}/{self.path.lstrip('/')}"
        parsed_target = urllib.parse.urlparse(target)
        self.server.sink.emit(
            {
                "record": "request_start",
                "request_id": request_id,
                "method": "POST",
                "path": self.path,
                "upstream_host": parsed_target.hostname,
            }
        )

        headers = {
            name: value
            for name, value in self.headers.items()
            if name.lower() not in HOP_BY_HOP_HEADERS
            and name.lower() != "accept-encoding"
        }
        try:
            connection, response = _open_upstream(target, body, headers)
        except Exception as error:  # transport class only; never log message/body
            self.server.sink.emit(
                {
                    "record": "upstream_error",
                    "request_id": request_id,
                    "error_class": type(error).__name__,
                }
            )
            self.send_response(502)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return

        status = getattr(response, "status", response.getcode())
        self.server.sink.emit(
            {
                "record": "response_start",
                "request_id": request_id,
                "http_status": status,
            }
        )
        self.send_response(status)
        content_type = response.headers.get("Content-Type")
        if content_type:
            self.send_header("Content-Type", content_type)
        for name in ("x-request-id", "request-id"):
            value = response.headers.get(name)
            if value:
                self.send_header(name, value)
        self.send_header("Transfer-Encoding", "chunked")
        self.send_header("Connection", "close")
        self.end_headers()

        audit = SseAudit(request_id, self.server.sink.emit)
        downstream_open = True
        transport_error = False
        try:
            while True:
                chunk = _read_available(response)
                if not chunk:
                    break
                audit.feed(chunk)
                if downstream_open:
                    try:
                        self.wfile.write(f"{len(chunk):x}\r\n".encode("ascii"))
                        self.wfile.write(chunk)
                        self.wfile.write(b"\r\n")
                        self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError):
                        downstream_open = False
        except Exception as error:  # transport class only; never log message/body
            transport_error = True
            self.server.sink.emit(
                {
                    "record": "upstream_stream_error",
                    "request_id": request_id,
                    "error_class": type(error).__name__,
                }
            )
        finally:
            audit.finish(transport_error=transport_error)
            response.close()
            connection.close()
        if downstream_open:
            try:
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass
        self.close_connection = True

    def log_message(self, _format: str, *_args: Any) -> None:
        return


def _read_available(response: BinaryIO) -> bytes:
    read1 = getattr(response, "read1", None)
    if callable(read1):
        return read1(64 * 1024)
    return response.read(64 * 1024)


def _open_upstream(
    target: str,
    body: bytes,
    headers: dict[str, str],
) -> tuple[http.client.HTTPConnection, http.client.HTTPResponse]:
    parsed = urllib.parse.urlparse(target)
    if parsed.scheme == "https":
        proxy = _https_proxy(os.environ)
        if proxy is None:
            connection: http.client.HTTPConnection = http.client.HTTPSConnection(
                parsed.hostname,
                parsed.port or 443,
                timeout=None,
            )
        else:
            connection = http.client.HTTPSConnection(
                proxy.host,
                proxy.port,
                timeout=None,
            )
            tunnel_headers = (
                {"Proxy-Authorization": proxy.authorization}
                if proxy.authorization is not None
                else {}
            )
            connection.set_tunnel(
                parsed.hostname,
                parsed.port or 443,
                headers=tunnel_headers,
            )
    elif parsed.scheme == "http":
        connection = http.client.HTTPConnection(
            parsed.hostname,
            parsed.port or 80,
            timeout=None,
        )
    else:
        raise ValueError("upstream base must use http or https")
    path = urllib.parse.urlunparse(
        ("", "", parsed.path or "/", parsed.params, parsed.query, "")
    )
    connection.request("POST", path, body=body, headers=headers)
    return connection, connection.getresponse()


def _https_proxy(environ: dict[str, str] | os._Environ[str]) -> HttpsProxy | None:
    value = environ.get("HTTPS_PROXY") or environ.get("https_proxy")
    if not value:
        return None
    parsed = urllib.parse.urlparse(value)
    if parsed.scheme != "http" or parsed.hostname is None:
        raise ValueError("HTTPS_PROXY must be an http proxy URL")
    authorization = None
    if parsed.username is not None:
        username = urllib.parse.unquote(parsed.username)
        password = urllib.parse.unquote(parsed.password or "")
        credential = base64.b64encode(f"{username}:{password}".encode()).decode("ascii")
        authorization = f"Basic {credential}"
    return HttpsProxy(
        host=parsed.hostname,
        port=parsed.port or 80,
        authorization=authorization,
    )


def _parse_listen(value: str) -> tuple[str, int]:
    host, separator, raw_port = value.rpartition(":")
    if not separator or not host:
        raise argparse.ArgumentTypeError("listen address must be HOST:PORT")
    try:
        port = int(raw_port)
    except ValueError as error:
        raise argparse.ArgumentTypeError("listen port must be an integer") from error
    if not 1 <= port <= 65_535:
        raise argparse.ArgumentTypeError("listen port must be between 1 and 65535")
    return host, port


def add_arguments(parser: argparse.ArgumentParser) -> None:
    parser.description = (
        "Forward a Responses API stream while logging only protocol metadata."
    )
    parser.add_argument("--listen", type=_parse_listen, default="127.0.0.1:3021")
    parser.add_argument("--upstream-base", required=True)
    parser.add_argument("--log", type=Path, required=True)


def execute(args: argparse.Namespace) -> None:
    server = ProbeServer(args.listen, args.upstream_base, JsonlSink(args.log))
    print(
        f"responses wire probe ready http://{args.listen[0]}:{args.listen[1]}",
        flush=True,
    )
    server.serve_forever()


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(
        description="Forward a Responses API stream while logging only protocol metadata."
    )
    add_arguments(parser)
    execute(parser.parse_args(argv))


if __name__ == "__main__":
    main()
