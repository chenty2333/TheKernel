from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from tools.oscomp_eval.run_inputs import RunInputError, capture_input_file


class RunInputsTests(unittest.TestCase):
    def test_capture_input_file_copies_to_inputs_dir(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "focused-plan.txt"
            source.write_text("/musl basic\n", encoding="utf-8")

            relative = capture_input_file(
                run_dir=root / "run",
                source=source,
                name="plan.txt",
            )

            self.assertEqual(relative, "inputs/plan.txt")
            self.assertEqual(
                (root / "run" / relative).read_text(encoding="utf-8"),
                "/musl basic\n",
            )

    def test_capture_input_name_must_be_plain_filename(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "plan.txt"
            source.write_text("/musl basic\n", encoding="utf-8")

            with self.assertRaisesRegex(RunInputError, "plain filename"):
                capture_input_file(
                    run_dir=root / "run",
                    source=source,
                    name="../plan.txt",
                )


if __name__ == "__main__":
    unittest.main()
