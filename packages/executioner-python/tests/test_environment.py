from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path

from executioner_sdk import ExecutionerEnvironment


def executioner_binary() -> str:
    return os.environ.get(
        "EXECUTIONER_BIN",
        str(Path(__file__).resolve().parents[3] / "target" / "release" / "executioner"),
    )


class ExecutionerEnvironmentTests(unittest.TestCase):
    def test_write_read_edit_with_managed_worker(self) -> None:
        with tempfile.TemporaryDirectory() as workspace:
            with ExecutionerEnvironment.create(
                binaryPath=executioner_binary(),
                workspace={"kind": "existing", "root": workspace},
                worker={"kind": "managed", "id": "executioner-python-test-worker", "idleSleepMs": 1},
            ) as env:
                write = env.submit({
                    "toolName": "Write",
                    "arguments": {
                        "path": "hello.txt",
                        "content": "hello from python",
                    },
                })
                read = env.submit({
                    "toolName": "Read",
                    "arguments": {"path": "hello.txt"},
                })
                edit = env.edit({
                    "path": "hello.txt",
                    "oldString": "hello from python",
                    "newString": "hello from python edit",
                })
                edited = env.submit({
                    "toolName": "Read",
                    "arguments": {"path": "hello.txt"},
                })

            self.assertEqual(write.status, "success")
            self.assertEqual(read.output, "hello from python")
            self.assertEqual(edit.status, "success")
            self.assertEqual(edited.output, "hello from python edit")
            self.assertEqual(Path(workspace, "hello.txt").read_text(), "hello from python edit")


if __name__ == "__main__":
    unittest.main()
