from __future__ import annotations

import tempfile
import unittest
from contextlib import redirect_stdout
from argparse import Namespace
from io import StringIO
from pathlib import Path
from unittest.mock import patch

from tools.oscomp_eval.lab.cli import build_focus_plan, list_cmd
from tools.oscomp_eval.lab.selection import SelectionError, parse_selection
from tools.oscomp_eval.lab.plugins import plugin_for
from tools.oscomp_eval.lab.model import PayloadDraft
from tools.oscomp_eval.lab.payload import write_payload


class LabSelectionTests(unittest.TestCase):
    def test_parse_group_selector(self) -> None:
        selection = parse_selection("basic-musl")

        self.assertEqual(selection.group, "basic")
        self.assertEqual(selection.libc, "musl")
        self.assertIsNone(selection.expr)
        self.assertEqual(selection.group_id, "basic-musl")

    def test_parse_case_selector(self) -> None:
        selection = parse_selection("ltp-glibc:openat01")

        self.assertEqual(selection.group, "ltp")
        self.assertEqual(selection.libc, "glibc")
        self.assertEqual(selection.expr, "openat01")
        self.assertEqual(selection.text, "ltp-glibc:openat01")

    def test_rejects_missing_libc(self) -> None:
        with self.assertRaises(SelectionError):
            parse_selection("ltp")


class LabCliTests(unittest.TestCase):
    def test_list_prints_plugin_selector_capabilities(self) -> None:
        output = StringIO()

        with redirect_stdout(output):
            self.assertEqual(list_cmd(Namespace()), 0)

        text = output.getvalue()
        self.assertIn("ltp (case-level: exact, prefix=..., regex=...)", text)
        self.assertIn("basic (group-level)", text)


class LabPayloadTests(unittest.TestCase):
    def test_generic_group_rejects_case_level_selector(self) -> None:
        selection = parse_selection("basic-musl:brk")
        draft = PayloadDraft()

        with self.assertRaisesRegex(ValueError, "does not support case-level"):
            plugin_for("basic").apply(selection, draft, root=Path.cwd())

    def test_ltp_case_selector_generates_focused_payload(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "scripts").mkdir()
            (root / "scripts" / "oscomp.sh").write_text("", encoding="utf-8")
            (root / ".git").mkdir()
            (root / "ltp_test.txt").write_text(
                "openat01 openat01\nread01\n",
                encoding="utf-8",
            )
            selection = parse_selection("ltp-glibc:openat01")
            draft = PayloadDraft()

            plugin_for("ltp").apply(selection, draft, root=root)
            plan = write_payload(arch="rv", selections=(selection,), draft=draft, root=root)

            self.assertEqual(plan.group_matrix, (("ltp", "glibc"),))
            self.assertEqual([case.name for case in plan.cases], ["openat01"])
            self.assertEqual(plan.plan_path.read_text(encoding="utf-8"), "/glibc ltp\n")
            self.assertEqual(plan.cases_path.read_text(encoding="utf-8"), "ltp-glibc openat01\n")
            self.assertEqual(plan.ltp_list_path.read_text(encoding="utf-8"), "openat01 openat01\n")

    def test_ltp_prefix_selector_uses_common_case_filter(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "ltp_test.txt").write_text(
                "openat01 openat01\nopenat02 openat02\nread01 read01\n",
                encoding="utf-8",
            )
            selection = parse_selection("ltp-musl:prefix=openat")
            draft = PayloadDraft()

            plugin_for("ltp").apply(selection, draft, root=root)

            self.assertEqual([case.name for case in draft.cases], ["openat01", "openat02"])

    def test_ltp_regex_selector_uses_common_case_filter(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "ltp_test.txt").write_text(
                "openat01 openat01\nopenat02 openat02\nread01 read01\n",
                encoding="utf-8",
            )
            selection = parse_selection("ltp-glibc:regex=^openat0[12]$")
            draft = PayloadDraft()

            plugin_for("ltp").apply(selection, draft, root=root)

            self.assertEqual([case.name for case in draft.cases], ["openat01", "openat02"])

    def test_build_focus_plan_explain_does_not_build_support_image(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "ltp_test.txt").write_text("read01 read01\n", encoding="utf-8")
            args = Namespace(arch="rv", select=["basic-musl"])

            with patch("tools.oscomp_eval.lab.cli.repo_root", return_value=root):
                plan = build_focus_plan(args, build_support=False)

        self.assertEqual(plan.group_matrix, (("basic", "musl"),))
        self.assertIsNone(plan.support_image)
        self.assertFalse(plan.plan_path.exists())
        self.assertFalse(plan.cases_path.exists())
        self.assertFalse(plan.ltp_list_path.exists())


if __name__ == "__main__":
    unittest.main()
