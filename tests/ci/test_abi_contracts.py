"""The static ABI contract cross-check must hold on the real sources, and
must reject each direction of drift when it is injected."""
import unittest
from pathlib import Path
from unittest import mock

from tests.support import load_script_module

gate = load_script_module("check_abi_contracts", "scripts/ci/check_abi_contracts.py")


class AbiContractsTests(unittest.TestCase):
    def test_every_registered_program_matches_its_source(self):
        self.assertEqual(gate.main([]), 0)

    def test_the_committed_ledger_only_binds_programs_the_runner_registers(self):
        self.assertEqual(gate.ledger_errors(), [])
        bound = {
            match.group("program")
            for cell in gate.tomllib.loads(gate.LEDGER.read_text(encoding="utf-8"))["cell"]
            for test in cell["tests"]
            if (match := gate.LEDGER_PROGRAM.search(test))
        }
        self.assertTrue(bound <= set(gate.PROGRAM_CASES))

    def test_a_ledger_binding_the_runner_dropped_is_rejected(self):
        """A renamed differential program must not stay a ledger promise."""
        without = {name: cases for name, cases in gate.PROGRAM_CASES.items() if name != "eventfd"}
        with mock.patch.object(gate, "PROGRAM_CASES", without):
            errors = gate.ledger_errors()
        self.assertEqual(len(errors), 1, errors)
        self.assertIn('bind program "eventfd" that the differential runner does not register', errors[0])

    def test_an_unbound_program_outside_the_baseline_is_rejected(self):
        """Registered differential evidence must be attributed or named as a hole."""
        with mock.patch.dict(gate.PROGRAM_CASES, {"unattributed-new-case": ("portable-differential", "pass", "ONE")}):
            errors = gate.ledger_errors()
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("bound by no [[cell]] and are not listed in ratchet.unbound_programs", errors[0])
        self.assertIn("unattributed-new-case", errors[0])

    def test_a_ledger_that_drops_the_unbound_baseline_is_rejected(self):
        text = gate.LEDGER.read_text(encoding="utf-8")
        start = text.index("unbound_programs = [")
        end = text.index("]", start) + 1
        stripped = text[:start] + text[end:]
        real_read_text = Path.read_text

        def patched(path, *args, **kwargs):
            return stripped if path == gate.LEDGER else real_read_text(path, *args, **kwargs)

        with mock.patch.object(Path, "read_text", patched):
            errors = gate.ledger_errors()
        self.assertIn("[ratchet] must declare unbound_programs", "\n".join(errors))

    def test_a_registry_marker_missing_from_the_source_is_rejected(self):
        original = gate.CONTRACTS["tty-job-control"]
        trimmed = (original[0], original[1], original[2].split()[0])
        with mock.patch.dict(gate.CONTRACTS, {"tty-job-control": trimmed}):
            self.assertEqual(gate.main(["--program", "tty-job-control"]), 1)

    def test_an_unregistered_emitted_marker_is_rejected(self):
        source = gate.PORTABLE / "tty-job-control-differential.c"
        text = source.read_text(encoding="utf-8")
        injected = text.replace(
            "THEKERNEL_ABI_ASSERT tty-job-control.portable-differential AUTO_CTTY_NOCTTY pass",
            "THEKERNEL_ABI_ASSERT tty-job-control.portable-differential AUTO_CTTY_NOCTTY pass\n"
            '    puts("THEKERNEL_ABI_ASSERT tty-job-control.portable-differential STALE_MARKER pass");',
        )
        real_read_text = Path.read_text

        def patched(path, *args, **kwargs):
            if path == source:
                return injected
            return real_read_text(path, *args, **kwargs)

        with mock.patch.object(Path, "read_text", patched):
            self.assertEqual(gate.main(["--program", "tty-job-control"]), 1)

    def test_matching_brace_masks_comments_and_strings(self):
        code = """int foo() {
            /* { in comment } */
            const char *s = "}";
            char c = '{';
            // } in line comment
            return 42;
        }"""
        open_brace = code.index("{")
        close_brace = code.rindex("}")
        self.assertEqual(gate._matching_brace(code, open_brace), close_brace)


if __name__ == "__main__":
    unittest.main()
