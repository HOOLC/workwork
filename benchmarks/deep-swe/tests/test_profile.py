import json
import tempfile
import unittest
from pathlib import Path

from zork_deepswe.profile import build_profile


class BuildDeepSweProfileTest(unittest.TestCase):
    def test_builds_openai_subscription_luna_max_from_codex_auth(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            auth_path = Path(directory) / "auth.json"
            auth_path.write_text(
                json.dumps(
                    {
                        "auth_mode": "chatgpt",
                        "tokens": {
                            "access_token": "header.payload.signature",
                            "refresh_token": "refresh-secret",
                            "account_id": "account-secret",
                        },
                    }
                )
            )

            profile = build_profile(
                "openai-subscription",
                auth_path,
                streaming=True,
                parallel_tool_calls=False,
                context_window_tokens=256_000,
                max_output_tokens=32_000,
            )

        self.assertEqual(profile["provider"], "openai")
        self.assertEqual(profile["billing"], "subscription")
        self.assertEqual(profile["base_url"], "https://chatgpt.com/backend-api/codex")
        self.assertEqual(profile["headers"], {"originator": "zork"})
        self.assertEqual(
            profile["auth"],
            {
                "type": "oauth",
                "access": "header.payload.signature",
                "refresh": "refresh-secret",
                "accountId": "account-secret",
            },
        )
        self.assertEqual(profile["models"][0]["id"], "gpt-5.6-luna")
        self.assertEqual(profile["models"][0]["api"], "openai-codex-responses")
        self.assertEqual(profile["models"][0]["service_tier"], "priority")
        self.assertIs(profile["models"][0]["parallel_tool_calls"], False)
        self.assertIs(profile["models"][0]["streaming"], True)
        self.assertIn("max", profile["models"][0]["thinking"])
        self.assertEqual(profile["models"][0]["default_thinking"], "max")
        self.assertEqual(
            profile["models"][0]["limits"],
            {"context_window_tokens": 256_000, "max_output_tokens": 32_000},
        )

    def test_transport_is_an_explicit_profile_choice(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            auth_path = Path(directory) / "auth.json"
            auth_path.write_text(
                json.dumps(
                    {
                        "auth_mode": "chatgpt",
                        "tokens": {
                            "access_token": "header.payload.signature",
                            "refresh_token": "refresh-secret",
                            "account_id": "account-secret",
                        },
                    }
                )
            )

            profile = build_profile(
                "openai-subscription",
                auth_path,
                streaming=False,
                parallel_tool_calls=False,
            )

        self.assertIs(profile["models"][0]["streaming"], False)
        self.assertIs(profile["models"][0]["parallel_tool_calls"], False)

    def test_rejects_non_chatgpt_codex_auth(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            auth_path = Path(directory) / "auth.json"
            auth_path.write_text(
                json.dumps(
                    {
                        "auth_mode": "apikey",
                        "tokens": {
                            "access_token": "access-secret",
                            "refresh_token": "refresh-secret",
                            "account_id": "account-secret",
                        },
                    }
                )
            )

            with self.assertRaisesRegex(ValueError, "ChatGPT subscription"):
                build_profile("openai-subscription", auth_path, streaming=True)

    def test_rejects_native_parallel_tool_calls_for_codex_lite(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            auth_path = Path(directory) / "auth.json"
            auth_path.write_text(
                json.dumps(
                    {
                        "auth_mode": "chatgpt",
                        "tokens": {
                            "access_token": "header.payload.signature",
                            "refresh_token": "refresh-secret",
                            "account_id": "account-secret",
                        },
                    }
                )
            )

            with self.assertRaisesRegex(ValueError, "Responses Lite requires"):
                build_profile(
                    "openai-subscription",
                    auth_path,
                    streaming=True,
                    parallel_tool_calls=True,
                )


if __name__ == "__main__":
    unittest.main()
