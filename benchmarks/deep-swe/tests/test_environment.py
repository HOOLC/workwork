import unittest
from unittest.mock import AsyncMock

from zork_deepswe.environment import RetainedDockerEnvironment


class RetainedDockerEnvironmentTest(unittest.IsolatedAsyncioTestCase):
    async def test_cleanup_keeps_containers_running_even_when_delete_is_requested(self):
        for delete in (False, True):
            with self.subTest(delete=delete):
                environment = RetainedDockerEnvironment.__new__(RetainedDockerEnvironment)
                environment.prepare_logs_for_host = AsyncMock()
                environment._run_docker_compose_command = AsyncMock()

                await environment.stop(delete=delete)

                environment.prepare_logs_for_host.assert_awaited_once_with()
                environment._run_docker_compose_command.assert_not_called()
