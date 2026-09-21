"""The in-crate module coupling baseline must match the real kernel sources, and
must reject both a widened edge set and a baseline that refuses to shrink."""
import contextlib
import io
import unittest
from pathlib import Path
from unittest import mock

from tests.support import load_script_module, test_tmpdir

edges_gate = load_script_module("check_kernel_module_edges", "scripts/ci/check_kernel_module_edges.py")


def run(main_argv=None) -> tuple[int, str]:
    """Run the gate and return its exit status with everything it printed."""
    captured = io.StringIO()
    with contextlib.redirect_stdout(captured), contextlib.redirect_stderr(captured):
        status = edges_gate.main(main_argv)
    return status, captured.getvalue()


class KernelModuleEdgeTests(unittest.TestCase):
    def test_the_committed_baseline_matches_the_crate(self):
        status, output = run()
        self.assertEqual(status, 0, output)
        self.assertIn("exactly the committed baseline", output)

    def test_a_new_module_edge_fails_the_run(self):
        """Reaching a new subsystem from inside the crate is a review decision."""
        measured = edges_gate.edges

        def widened():
            graph = measured()
            graph.setdefault("drm", set()).add("nfs_transport")
            return graph

        with mock.patch.object(edges_gate, "edges", widened):
            status, output = run()
        self.assertEqual(status, 1, output)
        self.assertIn("drm -> nfs_transport", output)
        self.assertIn("Lift the boundary instead of widening it", output)

    def test_a_retired_edge_is_reported_so_the_baseline_can_shrink(self):
        measured = edges_gate.edges

        def narrowed():
            graph = measured()
            graph["bpf"].discard("mm")
            return graph

        with mock.patch.object(edges_gate, "edges", narrowed):
            status, output = run()
        self.assertEqual(status, 0, output)
        self.assertIn("bpf -> mm", output)
        self.assertIn("may leave the list", output)

    def test_a_baseline_entry_is_read_as_the_module_pair_it_names(self):
        """Only the `use crate::…` import counts; the crate root appears once."""
        self.assertEqual(edges_gate.targets("use crate::file::{self, Table};"), {"file"})
        self.assertEqual(edges_gate.targets("use crate::mm::{self as memory, SharedPages};"), {"mm"})
        self.assertEqual(edges_gate.targets("use crate::{self as k, syscall};"), {"syscall"})
        self.assertEqual(edges_gate.targets("use crate::drm::DrmError as Error;"), {"drm"})

    def test_a_commented_out_edge_is_not_coupling(self):
        source = "// use crate::task::current;\nlet _ = 1;\n"
        self.assertEqual(edges_gate.targets(edges_gate.mask_rust_noncode(source)), set())

    def test_a_baseline_that_would_silently_stop_counting_is_rejected(self):
        for body, reason in (
            ('schema = 1\nedges = ["drm -> mm -> task"]\n', "exactly one module on each side"),
            ('schema = 1\nedges = ["drm"]\n', 'must read "source -> target"'),
            ('schema = 1\nedges = ["drm -> crate::mm"]\n', "repeats the crate root"),
            ('schema = 1\nedges = ["drm -> mm", "drm -> mm"]\n', "duplicate edge"),
            ('schema = 2\nedges = ["drm -> mm"]\n', "schema must be 1"),
            ('edges = ["drm -> mm"]\n', "schema must be 1"),
        ):
            with self.subTest(body=body), test_tmpdir() as directory:
                baseline = Path(directory) / "edges.toml"
                baseline.write_text(body, encoding="utf-8")
                with mock.patch.object(edges_gate, "BASELINE", baseline):
                    status, output = run()
                self.assertEqual(status, 1, output)
                self.assertIn(reason, output)

    def test_an_unreadable_baseline_is_an_error_not_a_pass(self):
        with test_tmpdir() as directory:
            with mock.patch.object(edges_gate, "BASELINE", Path(directory) / "absent.toml"):
                status, output = run()
        self.assertEqual(status, 1, output)
        self.assertIn("is unusable", output)

    def test_audit_inline_reports_hidden_coupling(self):
        status, output = run(["--audit-inline"])
        self.assertEqual(status, 0, output)
        self.assertIn("kernel module inline audit", output)
        self.assertIn("bypass use declarations (hidden coupling)", output)


if __name__ == "__main__":
    unittest.main()
