from __future__ import annotations

import http.client
import json
import threading
import unittest
from base64 import b64encode
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from zork_deepswe.responses_probe import ProbeServer, SseAudit, _https_proxy


class SseAuditTests(unittest.TestCase):
    def test_records_event_types_and_terminal_metadata_across_chunk_boundaries(
        self,
    ) -> None:
        records: list[dict[str, object]] = []
        audit = SseAudit("request-1", records.append)

        audit.feed(
            b'data: {"type":"response.created","response":{"id":"secret-id"}}\r\n\r\n'
            b'data: {"type":"response.output_text.delta","delta":"do not log me"}\r\n'
        )
        audit.feed(
            b'\r\ndata: {"type":"response.completed","response":{"status":"completed",'
            b'"usage":{"input_tokens":12,"output_tokens":3},'
            b'"output":[{"content":"also secret"}]}}\r\n\r\ndata: [DO'
        )
        audit.feed(b"NE]\r\n\r\n")
        audit.finish()

        self.assertEqual(
            [record["type"] for record in records if record["record"] == "sse_event"],
            [
                "response.created",
                "response.output_text.delta",
                "response.completed",
                "[DONE]",
            ],
        )
        terminal = next(record for record in records if record["record"] == "terminal")
        self.assertEqual(
            terminal,
            {
                "record": "terminal",
                "request_id": "request-1",
                "sequence": 3,
                "type": "response.completed",
                "status": "completed",
                "usage_present": True,
                "input_tokens": 12,
                "output_tokens": 3,
            },
        )
        transport = records[-1]
        self.assertEqual(transport["record"], "transport_end")
        self.assertIs(transport["terminal_seen"], True)
        self.assertIs(transport["done_seen"], True)
        serialized = "\n".join(json.dumps(record) for record in records)
        self.assertNotIn("secret-id", serialized)
        self.assertNotIn("do not log me", serialized)
        self.assertNotIn("also secret", serialized)

    def test_eof_without_terminal_is_explicit(self) -> None:
        records: list[dict[str, object]] = []
        audit = SseAudit("request-2", records.append)
        audit.feed(
            b'data: {"type":"response.output_item.added",'
            b'"item":{"type":"reasoning","id":"do-not-log"}}\n\n'
        )

        audit.finish()

        self.assertEqual(records[-1]["record"], "transport_end")
        self.assertIs(records[-1]["terminal_seen"], False)
        self.assertIs(records[-1]["done_seen"], False)
        self.assertEqual(records[-1]["event_count"], 1)
        self.assertNotIn("do-not-log", json.dumps(records))

    def test_terminal_without_usage_is_not_confused_with_missing_terminal(self) -> None:
        records: list[dict[str, object]] = []
        audit = SseAudit("request-3", records.append)
        audit.feed(
            b'data: {"type":"response.incomplete",'
            b'"response":{"status":"incomplete","usage":null}}\n\n'
        )
        audit.finish()

        terminal = records[-2]
        self.assertEqual(terminal["record"], "terminal")
        self.assertEqual(terminal["type"], "response.incomplete")
        self.assertIs(terminal["usage_present"], False)
        self.assertIs(records[-1]["terminal_seen"], True)


class ProbeForwardingTests(unittest.TestCase):
    def test_https_proxy_credentials_are_used_for_connect_tunnel(self) -> None:
        proxy = _https_proxy(
            {
                "HTTPS_PROXY": "http://agent:p%40ss@pier-egress-proxy:8080",
                "https_proxy": "http://ignored.invalid:9999",
            }
        )

        self.assertIsNotNone(proxy)
        assert proxy is not None
        self.assertEqual(proxy.host, "pier-egress-proxy")
        self.assertEqual(proxy.port, 8080)
        self.assertEqual(
            proxy.authorization,
            "Basic " + b64encode(b"agent:p@ss").decode("ascii"),
        )

    def test_forwards_body_and_caller_headers_without_inventing_user_agent(
        self,
    ) -> None:
        captured: dict[str, object] = {}

        class UpstreamHandler(BaseHTTPRequestHandler):
            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers["Content-Length"])
                captured.update(
                    {
                        "path": self.path,
                        "headers": {
                            name.lower(): value for name, value in self.headers.items()
                        },
                        "body": self.rfile.read(length),
                    }
                )
                body = (
                    b'data: {"type":"response.completed",'
                    b'"response":{"status":"completed","usage":null}}\n\n'
                )
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, _format: str, *_args: object) -> None:
                return

        class MemorySink:
            def __init__(self) -> None:
                self.records: list[dict[str, object]] = []

            def emit(self, record: dict[str, object]) -> None:
                self.records.append(record)

        upstream = ThreadingHTTPServer(("127.0.0.1", 0), UpstreamHandler)
        upstream_thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        upstream_thread.start()
        sink = MemorySink()
        probe = ProbeServer(
            ("127.0.0.1", 0),
            f"http://127.0.0.1:{upstream.server_port}/base",
            sink,  # type: ignore[arg-type]
        )
        probe_thread = threading.Thread(target=probe.serve_forever, daemon=True)
        probe_thread.start()
        try:
            connection = http.client.HTTPConnection("127.0.0.1", probe.server_port)
            connection.request(
                "POST",
                "/responses",
                body=b'{"stream":true}',
                headers={
                    "Authorization": "Bearer not-a-real-secret",
                    "Content-Type": "application/json",
                    "X-Caller": "zork",
                },
            )
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            response.read()
            connection.close()
        finally:
            probe.shutdown()
            probe.server_close()
            upstream.shutdown()
            upstream.server_close()

        self.assertEqual(captured["path"], "/base/responses")
        self.assertEqual(captured["body"], b'{"stream":true}')
        headers = captured["headers"]
        assert isinstance(headers, dict)
        self.assertEqual(headers["authorization"], "Bearer not-a-real-secret")
        self.assertEqual(headers["x-caller"], "zork")
        self.assertNotIn("user-agent", headers)
        self.assertTrue(any(record["record"] == "terminal" for record in sink.records))


if __name__ == "__main__":
    unittest.main()
