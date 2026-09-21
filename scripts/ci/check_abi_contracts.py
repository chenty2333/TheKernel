#!/usr/bin/env python3
"""Cross-check the ABI differential registry against its guest C sources.

The runner's registry (``tools/qemu_runner/abi_differential.py``) and the
guest programs (``tests/guest/portable/<program>-differential.c``) describe
the same assertion set from two sides, and only the KVM `abi` tier observes
them together.  This gate runs at the static tier instead:

- every registry marker, case anchor, and completion marker for a program
  must be present in that program's source (a registry entry whose guest
  assertion was renamed or deleted cannot pass);
- every marker the source emits through the assert protocol must be claimed
  by the registry (an assertion the registry cannot see is coverage the
  contract ledger does not account for, and the runtime parse would only
  reject it after a full KVM boot);
- every differential program a `[[cell]]` binds in
  ``config/linux-contracts.toml`` must be a program the runner registers, so a
  runtime claim in the ledger cannot point at something `--suite abi` would
  never boot;
- and in the other direction, every program the runner registers must either be
  bound by a `[[cell]]` or named by the shrink-only
  ``ratchet.unbound_programs`` baseline, so differential evidence the ledger does
  not attribute stays a stated hole instead of an unnoticed one.

Emission is recognized in every generation of the programs: hardcoded
``THEKERNEL_ABI_`` record strings, helpers (functions and ``#define``
macros, transitively) whose replacement body prints an assert record, and
whole-literal completion constants.
"""
from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from tools.qemu_runner.abi_differential import (  # noqa: E402
    CONTRACTS,
    PROGRAM_CASES,
    PROGRAM_SUCCESS,
)

PORTABLE = ROOT / "tests/guest/portable"
LEDGER = ROOT / "config/linux-contracts.toml"
# A cell binds a differential program by naming a symbol in its guest source;
# only the program part matters here, because the symbol itself is resolved by
# scripts/ci/linux_abi_gate.py against the file on disk.
LEDGER_PROGRAM = re.compile(r"tests/guest/portable/(?P<program>.+)-differential\.c:")
# A quoted literal that could be a marker argument: the uppercase constant
# style or the dashed lowercase style.  Paths, formats and sysctl names
# never match.
MARKER_LIKE = re.compile(r'"([A-Z][A-Z0-9_]{4,}|[a-z0-9]+(?:-[a-z0-9]+){2,})"')
# Tokens inside a hardcoded record string; dotted tokens are case anchors.
RECORD_TOKEN = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_.-]*")
RECORD_LINES = ("THEKERNEL_ABI_ASSERT", "THEKERNEL_ABI_CASE", "THEKERNEL_ABI_RESULT")
DEFINITION = re.compile(r"(?:^|\n)[A-Za-z_][A-Za-z0-9_ \t*]*\b([a-z_]+)\s*\(")
MACRO = re.compile(r"(?:^|\n)#define\s+([A-Za-z_][A-Za-z0-9_]*)\(")
EMIT_RECORD = "THEKERNEL_ABI_ASSERT"


def mask_c_noncode(source: str) -> str:
    """Mask C comments and string/character literals with spaces, preserving length and newlines."""
    def replacer(match: re.Match) -> str:
        s = match.group(0)
        return "".join("\n" if c == "\n" else " " for c in s)

    pattern = re.compile(
        r"/\*[\s\S]*?\*/|//[^\n]*|\"(?:\\.|[^\"\\])*\"|\'(?:\\.|[^\'\\])*\'"
    )
    return pattern.sub(replacer, source)


def _matching_brace(text: str, open_brace: int, masked: str | None = None) -> int:
    scan = masked if masked is not None else mask_c_noncode(text)
    depth = 0
    for index in range(open_brace, len(scan)):
        if scan[index] == "{":
            depth += 1
        elif scan[index] == "}":
            depth -= 1
            if depth == 0:
                return index
    return len(text)


def _call_arguments(text: str, open_paren: int, masked: str | None = None) -> str:
    scan = masked if masked is not None else mask_c_noncode(text)
    depth = 0
    for index in range(open_paren, len(scan)):
        if scan[index] == "(":
            depth += 1
        elif scan[index] == ")":
            depth -= 1
            if depth == 0:
                return text[open_paren:index]
    return ""


def _bodies(text: str) -> dict[str, str]:
    """Function and object-like macro replacement bodies, by name."""
    bodies: dict[str, str] = {}
    masked = mask_c_noncode(text)
    for match in DEFINITION.finditer(masked):
        name, params_at = match.group(1), match.end()
        close_paren = masked.find(")", params_at)
        open_brace = masked.find("{", params_at)
        if close_paren == -1 or open_brace == -1:
            continue
        if masked[params_at:close_paren].count(";"):
            continue  # a call, a declaration, or a prototype, not a definition
        bodies[name] = text[open_brace:_matching_brace(text, open_brace, masked=masked) + 1]
    for match in MACRO.finditer(text):
        name = match.group(1)
        start = match.end()
        end = start
        while text.endswith("\\", end - 1) or "\\\n" in text[end:min(end + 2, len(text))]:
            end = text.find("\n", end)
            if end == -1:
                end = len(text)
                break
            end += 1
        bodies.setdefault(name, text[start:end])
    return bodies


def _emitters(bodies: dict[str, str]) -> set[str]:
    """Names whose own replacement body prints a pass assert record.

    Deliberately not transitive: a helper that merely *calls* such a helper
    also receives failure-detail and stage strings that are not asserted
    markers, so only the printing bodies themselves qualify.
    """
    return {
        name
        for name, body in bodies.items()
        if EMIT_RECORD in body and re.search(r"pass\\n|pass\"", body)
    }


def emitted_markers(text: str, program_cases: tuple[str, ...]) -> set[str]:
    """Literals this source prints as ``THEKERNEL_ABI_ASSERT`` markers."""
    markers: set[str] = set()
    defined = set(re.findall(r"^#define\s+([A-Za-z_][A-Za-z0-9_]*)", text, re.M))
    for line in text.splitlines():
        if not any(record in line for record in RECORD_LINES):
            continue
        for quoted in re.findall(r'"([^"]*)"', line):
            for token in RECORD_TOKEN.findall(quoted):
                if (
                    token.startswith("THEKERNEL_ABI_")
                    or token in defined
                    or token in program_cases  # a case name, not a marker
                    or "." in token  # a case anchor or a %s composition
                ):
                    continue
                if MARKER_LIKE.fullmatch(f'"{token}"'):
                    markers.add(token)
    bodies = _bodies(text)
    masked = mask_c_noncode(text)
    for name in sorted(_emitters(bodies)):
        for call in re.finditer(rf"\b{name}\s*\(", masked):
            arguments = _call_arguments(text, call.start(), masked=masked)
            # A constant-false outcome is a failure-path label: it prints
            # the FAIL record, never a pass assertion.
            if re.search(r",\s*(0|NULL|false)\s*$", arguments):
                continue
            for token in MARKER_LIKE.findall(arguments):
                if token in program_cases or token in defined or "." in token:
                    continue
                markers.add(token)
    return markers


def claimed_markers(program: str) -> set[str]:
    claimed: set[str] = set()
    for case in PROGRAM_CASES[program]:
        suffix, _outcome, markers = CONTRACTS[case]
        claimed.update(markers.split())
    claimed.add(PROGRAM_SUCCESS[program])
    return claimed


def program_errors(program: str) -> list[str]:
    source = PORTABLE / f"{program}-differential.c"
    if not source.is_file():
        return [f"{program}: missing guest source {source.relative_to(ROOT)}"]
    text = source.read_text(encoding="utf-8", errors="replace")
    errors: list[str] = []
    where = source.relative_to(ROOT)
    for case in PROGRAM_CASES[program]:
        anchor = f"{case}.{CONTRACTS[case][0]}"
        if anchor not in text and (case not in text or CONTRACTS[case][0] not in text):
            errors.append(f'{program}: case anchor "{anchor}" is absent from {where}')
        for marker in CONTRACTS[case][2].split():
            if marker not in text:
                errors.append(
                    f'{program}: registry marker "{marker}" ({case}) '
                    f"is not in {where}"
                )
    if PROGRAM_SUCCESS[program] not in text:
        errors.append(
            f'{program}: completion marker "{PROGRAM_SUCCESS[program]}" '
            f"is not in {where}"
        )
    # Emission-side check: any marker the source really emits through the
    # assert protocol must be claimed.  Completion constants other than this
    # program's registered one are inner stage markers and need no claim.
    emitted = {
        marker
        for marker in emitted_markers(text, tuple(PROGRAM_CASES[program]))
        if not marker.startswith("THEKERNEL_") or marker == PROGRAM_SUCCESS[program]
    }
    claimed = claimed_markers(program)
    for marker in sorted(emitted - claimed):
        errors.append(
            f'{program}: source emits "{marker}" but the registry does not '
            f"claim it for any case of this program"
        )
    return errors


def ledger_errors() -> list[str]:
    """Reject a contract-ledger binding the KVM differential cannot execute.

    ``scripts/ci/linux_abi_gate.py`` only demands this of an ``implemented``
    cell that claims ``validation_gaps = ["explicit-none"]``: it is the claim of
    verified runtime behavior.  Every other binding in the ledger is still a
    promise that this program runs, and a program renamed in the runner while
    the ledger kept pointing at the old name would silently drop that
    differential out of the `--suite abi` comparison.
    """
    try:
        ledger = tomllib.loads(LEDGER.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        return [f"abi ledger: cannot read {LEDGER.relative_to(ROOT)}: {error}"]
    cells = ledger.get("cell")
    if not isinstance(cells, list):
        return ["abi ledger: [cell] entries are missing"]
    bound: set[str] = set()
    for cell in cells:
        number = cell.get("number") if isinstance(cell, dict) else None
        for test in (cell.get("tests") or []) if isinstance(cell, dict) else []:
            match = LEDGER_PROGRAM.search(test if isinstance(test, str) else "")
            if match:
                bound.add(match.group("program"))
            elif isinstance(test, str) and "differential" in test and not test.startswith("tests/guest/portable/"):
                return [f"abi ledger: cell {number} binds a differential outside tests/guest/portable: {test}"]
    missing = sorted(bound - set(PROGRAM_CASES))
    errors = [
        f"abi ledger: cell tests bind program \"{program}\" that the differential runner does not register"
        for program in missing
    ]
    free = sorted(set(PROGRAM_CASES) - bound)
    ratchet = ledger.get("ratchet")
    baseline = ratchet.get("unbound_programs") if isinstance(ratchet, dict) else None
    if not isinstance(baseline, list) or not all(isinstance(item, str) for item in baseline):
        errors.append("abi ledger: [ratchet] must declare unbound_programs as an array of strings")
    else:
        # The other direction of the same drift: a program that boots and asserts
        # but that no cell binds is evidence the ledger attributes to nothing, so
        # it has to be named as a hole in the records, and the naming may only
        # shrink as those records gain the vocabulary the program tests.
        allowed = set(baseline)
        fresh = sorted(set(free) - allowed)
        if fresh:
            errors.append(
                "abi ledger: registered differential programs are bound by no [[cell]] "
                f"and are not listed in ratchet.unbound_programs: {fresh}"
            )
        retired = sorted(allowed - set(free))
        if retired:
            print(f"abi ledger: {len(retired)} programs now have a cell binding and may leave ratchet.unbound_programs: {retired}")
    print(
        f"abi ledger: {len(bound)} of {len(PROGRAM_CASES)} registered differential programs "
        f"are bound by a contract cell; unbound={','.join(free) or 'none'}"
    )
    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--program",
        action="append",
        help="check only this program (repeatable); default is all of them",
    )
    parser.add_argument(
        "--skip-ledger",
        action="store_true",
        help="skip the contract-ledger to runner binding check",
    )
    args = parser.parse_args(argv)
    programs = tuple(args.program) if args.program else tuple(PROGRAM_CASES)
    unknown = [name for name in programs if name not in PROGRAM_CASES]
    if unknown:
        print(f"abi contracts: unknown programs {unknown}", file=sys.stderr)
        return 1
    errors: list[str] = []
    if not args.skip_ledger:
        errors.extend(ledger_errors())
    for program in programs:
        errors.extend(program_errors(program))
    if errors:
        print("abi contract cross-check violations:", file=sys.stderr)
        print(*errors, sep="\n", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
