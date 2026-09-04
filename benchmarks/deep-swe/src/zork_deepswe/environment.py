from pier.environments.docker.docker import DockerEnvironment


class RetainedDockerEnvironment(DockerEnvironment):
    """Leave benchmark containers running until explicitly stopped by the user."""

    async def stop(self, delete: bool) -> None:
        await self.prepare_logs_for_host()
