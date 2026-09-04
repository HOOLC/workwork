import tempfile
import unittest
from pathlib import Path

from zork_deepswe.prepare import collect_unique_images


class PrepareDeepSweTest(unittest.TestCase):
    def test_collects_images_once_in_task_order(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tasks = []
            for name, image in (
                ("a", "registry/a:1"),
                ("b", "registry/b:1"),
                ("c", "registry/a:1"),
            ):
                task = root / name
                task.mkdir()
                (task / "task.toml").write_text(
                    f'[environment]\ndocker_image = "{image}"\n'
                )
                tasks.append(task)

            self.assertEqual(
                collect_unique_images(tasks),
                ["registry/a:1", "registry/b:1"],
            )


if __name__ == "__main__":
    unittest.main()
