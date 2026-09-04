#!/usr/bin/env python3
"""Real-process contract for zork-gateway's built-in desktop IM entry."""

from __future__ import annotations

import json
import socket
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


ROOT = Path(__file__).resolve().parents[3]
TARGET = ROOT / "target" / "debug"


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


class GatewayEntryContractTest(unittest.TestCase):
    temp: tempfile.TemporaryDirectory[str]
    agent: subprocess.Popen[bytes]
    gateway: subprocess.Popen[bytes]
    agent_url: str
    gateway_url: str
    workspace: Path

    @classmethod
    def setUpClass(cls) -> None:
        required = [TARGET / "zork-agent", TARGET / "zork-gateway", TARGET / "zork-call", TARGET / "zork-gh"]
        missing = [str(path) for path in required if not path.is_file()]
        if missing:
            raise RuntimeError(
                "build the real-process fixtures first: cargo build -p zork-agent -p zork-gateway -p zork-call; missing "
                + ", ".join(missing)
            )

        cls.temp = tempfile.TemporaryDirectory(prefix="zork-gateway-entry-")
        data_root = Path(cls.temp.name)
        cls.workspace = data_root / "shared-project"
        cls.workspace.mkdir()
        profiles = data_root / "profiles"
        profiles.mkdir()
        (profiles / "fixture.json").write_text(
            json.dumps(
                {
                    "provider": "openai",
                    "billing": "usage",
                    "base_url": "http://127.0.0.1:9/v1",
                    "auth": {"type": "api_key", "key": "sk-test"},
                    "models": [
                        {
                            "id": "fixture-model",
                            "api": "openai-completions",
                            "streaming": False,
                            "thinking": ["off"],
                            "default_thinking": "off",
                            "capabilities": {"input": ["text"]},
                            "limits": {
                                "context_window_tokens": 100000,
                                "max_output_tokens": 10000,
                            },
                            "default": True,
                        }
                    ],
                }
            )
        )
        gateway_port, runtime_port, control_port, agent_port = (
            free_port(),
            free_port(),
            free_port(),
            free_port(),
        )
        (data_root / "config.json").write_text(
            json.dumps(
                {
                    "im_connections": [],
                    "bind": {
                        "gateway": f"127.0.0.1:{gateway_port}",
                        "runtime": f"127.0.0.1:{runtime_port}",
                        "control": f"127.0.0.1:{control_port}",
                        "agent": f"127.0.0.1:{agent_port}",
                    },
                    "urls": {},
                    "admin": {},
                }
            )
        )
        cls.agent_url = f"http://127.0.0.1:{agent_port}"
        cls.gateway_url = f"http://127.0.0.1:{runtime_port}"
        cls.agent = subprocess.Popen(
            [
                str(TARGET / "zork-agent"),
                "--data",
                str(data_root),
                "--fake-agent",
                "--no-streaming",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        cls.wait_http(cls.agent_url, "/readyz")
        cls.gateway = subprocess.Popen(
            [str(TARGET / "zork-gateway"), "--data", str(data_root)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            cls.wait_http(cls.gateway_url, "/readyz")
        except Exception:
            cls.stop_process(cls.gateway)
            cls.stop_process(cls.agent)
            cls.temp.cleanup()
            raise

    @classmethod
    def tearDownClass(cls) -> None:
        cls.stop_process(cls.gateway)
        cls.stop_process(cls.agent)
        cls.temp.cleanup()

    @staticmethod
    def stop_process(process: subprocess.Popen[bytes]) -> None:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)

    @classmethod
    def wait_http(cls, base_url: str, path: str) -> None:
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            try:
                status, _ = cls.request_at(base_url, "GET", path)
                if status == 200:
                    return
            except (ConnectionError, URLError):
                pass
            time.sleep(0.05)
        raise RuntimeError(f"service did not become ready: {base_url}{path}")

    @staticmethod
    def request_at(
        base_url: str,
        method: str,
        path: str,
        body: dict[str, object] | None = None,
    ) -> tuple[int, dict[str, object]]:
        encoded = None if body is None else json.dumps(body).encode()
        request = Request(
            f"{base_url}{path}",
            data=encoded,
            method=method,
            headers={"content-type": "application/json"},
        )
        try:
            response = urlopen(request, timeout=3)
        except HTTPError as error:
            response = error
        with response:
            payload = response.read()
            return response.status, json.loads(payload) if payload else {}

    @classmethod
    def request(
        cls, method: str, path: str, body: dict[str, object] | None = None
    ) -> tuple[int, dict[str, object]]:
        return cls.request_at(cls.gateway_url, method, path, body)

    @classmethod
    def create_session(cls) -> str:
        status, body = cls.request(
            "POST",
            "/v1/im/sessions",
            {
                "profile_id": "fixture",
                "model": "fixture-model",
                "thinking": "off",
                "workspace": str(cls.workspace),
            },
        )
        if status != 201:
            raise AssertionError((status, body))
        return str(body["session_id"])

    @classmethod
    def messages(cls, session_id: str) -> list[dict[str, object]]:
        status, body = cls.request(
            "GET", f"/v1/im/sessions/{session_id}/messages?limit=100"
        )
        if status != 200:
            raise AssertionError((status, body))
        return list(body["items"])

    @staticmethod
    def wait_until(predicate: object, description: str) -> None:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if callable(predicate) and predicate():
                return
            time.sleep(0.05)
        raise AssertionError(f"timed out waiting for {description}")

    def test_only_gateway_delivery_becomes_a_message_and_same_cwd_is_exact(self) -> None:
        profile_status, profiles = self.request("GET", "/v1/im/profiles")
        self.assertEqual(profile_status, 200)
        self.assertEqual(profiles["items"][0]["profile_id"], "fixture")

        first = self.create_session()
        context_path = f"/v1/im/sessions/{first}/context"
        self.assertEqual(self.request("GET", context_path),
                         (200, {"strategy": "compaction", "keep_recent_tokens": 20000}))
        updated_context = {"strategy": "handoff", "keep_recent_tokens": 8000}
        self.assertEqual(self.request("PUT", context_path, updated_context), (200, updated_context))
        status, agent_session = self.request_at(self.agent_url, "GET", f"/sessions/{first}")
        self.assertEqual(status, 200)
        self.assertEqual(agent_session["context"], updated_context)
        invalid_status, _ = self.request("PUT", context_path,
                                         {"strategy": "compaction", "keep_recent_tokens": -1})
        self.assertEqual(invalid_status, 422)
        self.assertEqual(self.request("GET", context_path), (200, updated_context))
        missing_status, _ = self.request("GET", "/v1/im/sessions/not-a-session/context")
        self.assertEqual(missing_status, 404)
        send_status, _ = self.request(
            "POST",
            f"/v1/im/sessions/{first}/messages",
            {"content": "internal assistant echo must stay hidden"},
        )
        self.assertEqual(send_status, 202)

        def agent_has_internal_assistant() -> bool:
            status, page = self.request_at(
                self.agent_url, "GET", f"/sessions/{first}/messages?limit=100"
            )
            return status == 200 and any(
                item.get("role") == "assistant" for item in page.get("items", [])
            )

        self.wait_until(agent_has_internal_assistant, "internal Agent assistant transcript")
        self.assertEqual(
            [(item["role"], item["content"]) for item in self.messages(first)],
            [("user", "internal assistant echo must stay hidden")],
        )

        second = self.create_session()
        explicit = json.dumps(
            {
                "fake_tool": {
                    "name": "shell.run",
                    "input": {
                        "command": "zork-call chat post-message --text 'reply for task two' --kind final"
                    },
                }
            }
        )
        second_send, _ = self.request(
            "POST",
            f"/v1/im/sessions/{second}/messages",
            {"content": explicit},
        )
        self.assertEqual(second_send, 202)
        self.wait_until(
            lambda: any(
                item.get("role") == "assistant" for item in self.messages(second)
            ),
            "explicit gateway reply",
        )

        first_messages = self.messages(first)
        second_messages = self.messages(second)
        self.assertFalse(
            any(item.get("role") == "assistant" for item in first_messages),
            "same-workspace lookup delivered task two's reply into task one",
        )
        self.assertEqual(
            [item["content"] for item in second_messages if item["role"] == "assistant"],
            ["reply for task two"],
        )
        self.assertEqual(
            {item["role"] for item in second_messages},
            {"user", "assistant"},
        )


if __name__ == "__main__":
    unittest.main()
