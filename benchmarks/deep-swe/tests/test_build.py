import unittest
from pathlib import Path

from zork_deepswe.build import docker_build_command, native_linux_platform


class BuildZorkAgentTest(unittest.TestCase):
    def test_builds_the_requested_linux_export_from_the_repository(self) -> None:
        command = docker_build_command(
            Path("/repo"),
            Path("/project/zork-agent.Dockerfile"),
            Path("/output"),
            "linux/arm64",
        )

        self.assertEqual(command[:3], ["docker", "buildx", "build"])
        self.assertIn("linux/arm64", command)
        self.assertIn("/project/zork-agent.Dockerfile", command)
        self.assertEqual(command[-1], "/repo")

    def test_maps_native_machine_names_to_linux_platforms(self) -> None:
        self.assertEqual(native_linux_platform("aarch64"), "linux/arm64")
        self.assertEqual(native_linux_platform("x86_64"), "linux/amd64")


if __name__ == "__main__":
    unittest.main()
