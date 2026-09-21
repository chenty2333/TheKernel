#!/usr/bin/env python3
"""Materialize and statically inventory Linux v7.2.3 x86_64 syscall routing.

This gate never treats a dispatch route as semantic handler evidence.  Deeper
handler ENOSYS behavior belongs to contract and differential gates.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tomllib
from collections import Counter
from pathlib import Path
from typing import Sequence

ROOT = Path(__file__).resolve().parents[2]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
MANIFEST = ROOT / "config/linux-abi.toml"
CONTRACTS = ROOT / "config/linux-contracts.toml"
SOURCE = Path.home() / ".cache/thekernel-targets/linux-v7.2.3"
DISPATCH = ROOT / "kernel/src/syscall/dispatch.rs"
SYSNO = re.compile(r"\bSysno::([A-Za-z_][A-Za-z0-9_]*)\b")
WITNESS = 'cfg(feature = "bpf")'


class GateError(ValueError):
    pass


def load_manifest(path: Path) -> dict:
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        raise GateError(f"cannot read manifest {path}: {error}") from error
    if set(data) != {"schema", "linux", "routing_inventory", "routing_witness", "terminal"} or data.get("schema") != 3:
        raise GateError("manifest must contain schema = 3, linux, routing_inventory, routing_witness, and terminal")
    linux = data["linux"]
    expected = {"repository": "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git", "tag": "v7.2.3", "table": "arch/x86/entry/syscalls/syscall_64.tbl"}
    if linux != expected:
        raise GateError("Linux release does not match the selected x86_64 baseline")
    if not isinstance(data["routing_inventory"], dict) or set(data["routing_inventory"]) != {"ordinary_explicit", "explicit_enosys", "native_fallback"}:
        raise GateError("routing inventory fields are invalid")
    if data["routing_witness"] != {"bpf": WITNESS}:
        raise GateError("routing_witness must declare only the exact BPF feature witness")
    terminal = data["terminal"]
    if (
        not isinstance(terminal, dict)
        or set(terminal) != {"ordinary_explicit", "explicit_enosys", "native_fallback"}
        or not all(isinstance(count, int) and count >= 0 for count in terminal.values())
    ):
        raise GateError("terminal routing expectation fields are invalid")
    for key, count in terminal.items():
        if len(numbers(data["routing_inventory"][key], key)) != count:
            raise GateError(f"terminal routing expectation {key}={count} does not match the routing inventory")
    return data


def numbers(values: object, label: str) -> set[int]:
    if not isinstance(values, list):
        raise GateError(f"routing_inventory.{label} must be an array")
    result: set[int] = set()
    for value in values:
        if isinstance(value, int) and value >= 0:
            expanded = range(value, value + 1)
        elif isinstance(value, str) and re.fullmatch(r"0|[1-9]\d*", value):
            expanded = range(int(value), int(value) + 1)
        elif isinstance(value, str) and re.fullmatch(r"(?:0|[1-9]\d*)-(?:0|[1-9]\d*)", value):
            first, last = map(int, value.split("-"))
            if first > last:
                raise GateError(f"routing_inventory.{label} has reversed range {value}")
            expanded = range(first, last + 1)
        else:
            raise GateError(f"routing_inventory.{label} has invalid number/range {value!r}")
        for number in expanded:
            if number in result:
                raise GateError(f"routing_inventory.{label} repeats syscall {number}")
            result.add(number)
    return result


def parse_table(path: Path) -> dict[int, str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        raise GateError(f"cannot read syscall table {path}: {error}") from error
    entries: dict[int, str] = {}
    for line in lines:
        fields = line.split("#", 1)[0].split()
        if not fields or len(fields) < 3 or fields[1] not in {"common", "64"}:
            continue
        if not fields[0].isdigit() or not re.fullmatch(r"[A-Za-z0-9_]+", fields[2]):
            raise GateError(f"invalid native syscall table line: {line}")
        if int(fields[0]) in entries:
            raise GateError(f"duplicate native syscall number {fields[0]}")
        entries[int(fields[0])] = fields[2]
    if len(entries) != 385:
        raise GateError(f"expected 385 native common+64 syscalls, found {len(entries)}")
    return entries


def states(manifest: dict, entries: dict[int, str]) -> dict[int, str]:
    result: dict[int, str] = {}
    for state, key in (("ordinary-explicit", "ordinary_explicit"), ("explicit-enosys", "explicit_enosys"), ("native-fallback", "native_fallback")):
        for number in numbers(manifest["routing_inventory"][key], key):
            if number not in entries or number in result:
                raise GateError(f"invalid routing inventory syscall {number}")
            result[number] = state
    if set(result) != set(entries):
        raise GateError(f"routing inventory does not cover table syscalls: {sorted(set(entries) - set(result))}")
    return result


def blank(text: str) -> str:
    return "".join("\n" if char == "\n" else " " for char in text)


def mask_rust_noncode(source: str) -> str:
    """Mask comments and normal/byte/raw/raw-byte strings and char literals."""
    result: list[str] = []
    index = 0
    block = 0
    while index < len(source):
        if block:
            if source.startswith("/*", index): block += 1; result.append("  "); index += 2
            elif source.startswith("*/", index): block -= 1; result.append("  "); index += 2
            else: result.append("\n" if source[index] == "\n" else " "); index += 1
            continue
        if source.startswith("//", index):
            end = source.find("\n", index); end = len(source) if end < 0 else end
            result.append(blank(source[index:end])); index = end; continue
        if source.startswith("/*", index): block = 1; result.append("  "); index += 2; continue
        prefix = "br" if source.startswith("br", index) else "r"
        quote = index + len(prefix)
        while quote < len(source) and source[quote] == "#": quote += 1
        if source.startswith(prefix, index) and quote < len(source) and source[quote] == '"':
            endmark = '"' + source[index + len(prefix):quote]
            end = source.find(endmark, quote + 1)
            if end < 0: raise GateError("unterminated raw string in dispatch")
            end += len(endmark); result.append(blank(source[index:end])); index = end; continue
        if source.startswith('b"', index) or source[index] == '"':
            start = index + 1 if source.startswith('b"', index) else index
            end = start + 1
            while end < len(source) and source[end] != '"': end += 2 if source[end] == "\\" else 1
            if end >= len(source): raise GateError("unterminated string in dispatch")
            end += 1; result.append(blank(source[index:end])); index = end; continue
        opening = index + 1 if source.startswith("b'", index) else index
        if source[index] == "'" or source.startswith("b'", index):
            end = opening + 1; limit = min(len(source), opening + 32)
            while end < limit and source[end] not in "\n'": end += 2 if source[end] == "\\" else 1
            if end < limit and source[end] == "'": end += 1; result.append(blank(source[index:end])); index = end; continue
        result.append(source[index]); index += 1
    if block: raise GateError("unterminated block comment in dispatch")
    return "".join(result)


def matching_end(masked: str, opening: int, context: str) -> int:
    depth = 0
    for index in range(opening, len(masked)):
        if masked[index] == "{": depth += 1
        elif masked[index] == "}":
            depth -= 1
            if depth == 0: return index
    raise GateError(f"{context} is unterminated")


def brace_depth(masked: str, end: int) -> int:
    return sum(1 if char == "{" else -1 if char == "}" else 0 for char in masked[:end])


def top_level_match(masked: str, beginning: int, finish: int, pattern: re.Pattern[str], context: str) -> re.Match[str]:
    candidates = [
        match for match in pattern.finditer(masked, beginning, finish)
        if brace_depth(masked[beginning:finish], match.start() - beginning) == 0
    ]
    if len(candidates) != 1:
        raise GateError(f"{context} must have exactly one top-level match")
    return candidates[0]


def arms(path: Path) -> list[tuple[str, str]]:
    source = path.read_text(encoding="utf-8")
    masked = mask_rust_noncode(source)
    function = top_level_match(
        masked, 0, len(masked),
        re.compile(r"(?m)^\s*(?:#\[[^\]]+\]\s*)*(?:pub(?:\s*\([^)]*\))?\s+)?fn\s+dispatch_syscall\s*\("),
        "dispatch_syscall function",
    )
    begin = masked.find("{", function.end())
    finish = matching_end(masked, begin, "dispatch_syscall function")
    match = top_level_match(
        masked, begin + 1, finish, re.compile(r"\bmatch\s+sysno\s*\{"),
        "dispatch_syscall match sysno",
    )
    begin = masked.find("{", match.start(), match.end())
    finish = matching_end(masked, begin, "dispatch_syscall match sysno")
    body, masked = source[begin + 1:finish], masked[begin + 1:finish]
    result: list[tuple[str, str]] = []; start = index = depth = 0
    while index < len(body):
        char = masked[index]
        if char in "{([": depth += 1
        elif char in "})]": depth -= 1
        elif depth == 0 and masked[index:index + 2] == "=>":
            pattern = body[start:index].strip(); expression = index + 2; index = expression; inner = 0
            while index < len(body):
                char = masked[index]
                if char in "{([": inner += 1
                elif char in "})]": inner -= 1
                elif char == "," and inner == 0:
                    result.append((pattern, body[expression:index].strip())); start = index + 1; break
                elif inner == 0 and masked.startswith("Sysno::", index) and body[expression:index].strip():
                    result.append((pattern, body[expression:index].strip())); start = index; break
                index += 1
            else:
                # A trailing block arm ends at the match boundary without a
                # comma (rustfmt drops it); flush it instead of losing it.
                if body[expression:index].strip():
                    result.append((pattern, body[expression:index].strip()))
        index += 1
    return result


def routes(path: Path, table: set[str], witness: str) -> tuple[set[str], set[str], list[tuple[str, str]]]:
    parsed = arms(path)
    raw = mask_rust_noncode(path.read_text(encoding="utf-8"))
    if path.resolve() == DISPATCH.resolve():
        entry = mask_rust_noncode(path.with_name("mod.rs").read_text(encoding="utf-8"))
        seccomp = entry.find("seccomp::enforce_syscall_seccomp(uctx)")
        explicit = entry.find("dispatch::dispatch_new_syscall(uctx.sysno())")
        decode = entry.find("Sysno::new(uctx.sysno())")
        if not 0 <= seccomp < explicit < decode:
            raise GateError("raw Linux dispatch must run after seccomp and before enum decoding")
    if {"listns", "rseq_slice_yield"} <= table and re.search(r"fn\s+dispatch_new_syscall\(number: usize\)\s*->\s*Option<AxResult<isize>>\s*\{\s*match number\s*\{\s*470\s*\|\s*471\s*=>\s*Some\(sys_ni_syscall\(\)\),\s*_\s*=>\s*None,?\s*\}\s*\}", raw) is None:
        raise GateError("missing explicit Linux 7.2 raw-number ENOSYS routing")
    for pattern, _ in parsed:
        masked_pattern = mask_rust_noncode(pattern)
        names = set(SYSNO.findall(masked_pattern)) & table
        if not names: continue
        if re.search(r"\bif\b", masked_pattern): raise GateError(f"native syscall route(s) {sorted(names)} may not use a match guard")
        # Attributes carry string literals (cfg(feature = "bpf")), so read them
        # from the raw pattern; the masked form would blank the feature name.
        attrs = [re.sub(r"\s+", " ", item.strip()) for item in re.findall(r"#\[([^]]+)\]", pattern, re.DOTALL)]
        if attrs and not (names == {"bpf"} and attrs == [witness]) and not (names == {"vfork"} and attrs == ['cfg(target_arch = "x86_64")']):
            raise GateError(f"native syscall route(s) {sorted(names)} have unsupported conditional attribute(s) {attrs}")
    ni_patterns = [pattern for pattern, expression in parsed if re.fullmatch(r"\s*sys_ni_syscall\s*\(\s*\)\s*", mask_rust_noncode(expression))]
    if len(ni_patterns) != 1: raise GateError("dispatch has no explicit sys_ni_syscall arm")
    raw_names = " | Sysno::listns | Sysno::rseq_slice_yield" if {"listns", "rseq_slice_yield"} <= table else ""
    parsed = [(pattern + raw_names if pattern == ni_patterns[0] else pattern, expression) for pattern, expression in parsed]
    ni_patterns[0] += raw_names
    ni = set(SYSNO.findall(mask_rust_noncode(ni_patterns[0]))); all_routes = [name for pattern, _ in parsed for name in SYSNO.findall(mask_rust_noncode(pattern))]
    repeats = sorted(name for name, count in Counter(all_routes).items() if count > 1)
    if repeats: raise GateError(f"dispatch repeats syscall route(s): {repeats}")
    found = set(all_routes)
    if found - table: raise GateError(f"dispatch names absent from selected Linux table: {sorted(found - table)}")
    if sum(re.sub(r"(?s)^\s*#\[[^]]+\]\s*", "", pattern).strip() == "_" for pattern, _ in parsed) != 1:
        raise GateError("dispatch must have exactly one default arm for table-external syscall numbers")
    return found, ni, parsed


def inventory(manifest_path: Path, source: Path, dispatch: Path) -> None:
    manifest = load_manifest(manifest_path); entries = parse_table(source / manifest["linux"]["table"]); matrix = states(manifest, entries)
    found, ni, _ = routes(dispatch, set(entries.values()), manifest["routing_witness"]["bpf"])
    expected_ni = {entries[number] for number, state in matrix.items() if state == "explicit-enosys"}; fallback = {entries[number] for number, state in matrix.items() if state == "native-fallback"}
    if ni != expected_ni or found != set(entries.values()) - fallback or found & fallback: raise GateError("explicit dispatch routes do not match routing inventory")
    counts = Counter(matrix.values()); print(f"linux-abi inventory: ordinary-explicit={counts['ordinary-explicit']} explicit-enosys={counts['explicit-enosys']} native-fallback={counts['native-fallback']}")


CONTRACT_FIELDS = {
    "id", "flags", "structs", "multiplexer_commands", "provider_ioctls",
    "errno_order", "usercopy", "state", "concurrency", "teardown",
}
CELL_FIELDS = {"number", "name", "status", "contract", "handler", "conditional", "tests", "validation_gaps", "limitations"}
# Citation convention (docs/design/abi-contract-system.md): a contract record
# cites the pinned Linux release as `Linux <path>:<line>`, with the path
# relative to the release root and an optional `-<line>` end.  A record that
# does not carry one is not unverifiable: it is listed in
# `ratchet.uncited_contracts`, which may only shrink.
LINUX_CITATION = re.compile(r"Linux [A-Za-z0-9_./*-]+:[0-9]+(?:-[0-9]+)?")
# The ordering sentence a record carries until somebody compares that syscall's
# real errno order against the pinned release and writes down what they found.
# It used to be the same boilerplate as every other record's, which let 177 of
# 368 contracts read like reviewed ordering claims; now it names itself, so the
# ledger says out loud which half of it is still a template.  Records carrying
# it are listed in `ratchet.unreviewed_errno_order`, which may only shrink.
UNREVIEWED_ERRNO_ORDER = "errno:unreviewed - no per-syscall Linux ordering comparison recorded against the pinned release"
# `unbound_programs` is validated for shape here because the ledger's [ratchet]
# table must stay closed against silently dropping a baseline, but its content is
# enforced by scripts/ci/check_abi_contracts.py, which owns the runner registry.
RATCHET_FIELDS = {"uncited_contracts", "unreviewed_errno_order", "final_static_allowlist", "unbound_programs"}
def expected_dispatch_call(handler: str) -> str:
    """Derive the expected dispatch call pattern for a handler.

    Handlers in submodules (e.g. kernel/src/syscall/fs/...) may be called directly
    or through their submodule path (super::fs::...) in dispatch.rs.
    """
    path_str, _, symbol = handler.rpartition(":")
    parts = Path(path_str).parts
    if "syscall" in parts:
        idx = parts.index("syscall")
        if len(parts) > idx + 2:
            submod = parts[idx + 1]
            return f"(?:super::{submod}::|(?<![A-Za-z0-9_:])){re.escape(symbol)}"
    return rf"(?<![A-Za-z0-9_:]){re.escape(symbol)}"



def load_toml(path: Path, label: str) -> dict:
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        raise GateError(f"cannot read {label} {path}: {error}") from error
    if not isinstance(data, dict):
        raise GateError(f"{label} must be a TOML table")
    return data


def repository_path(value: object, label: str, kind: str, require_exists: bool = True) -> Path:
    """Resolve an in-tree descriptor and optionally require it to exist."""
    if not isinstance(value, str) or not value:
        raise GateError(f"{label} path is invalid")
    path = (ROOT / value).resolve()
    try:
        path.relative_to(ROOT)
    except ValueError as error:
        raise GateError(f"{label} escapes the repository: {value}") from error
    if require_exists and kind == "file" and not path.is_file():
        raise GateError(f"{label} does not exist as a file: {value}")
    if require_exists and kind == "dir" and not path.is_dir():
        raise GateError(f"{label} does not exist as a directory: {value}")
    return path


def rust_function(path: Path, symbol: str, conditional: str) -> None:
    """Require a Rust function item, rather than a substring in source text."""
    if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", symbol):
        raise GateError(f"contract handler symbol is invalid: {symbol!r}")
    raw_source = path.read_text(encoding="utf-8")
    source = mask_rust_noncode(raw_source)
    definition = re.compile(rf"(?m)^(?P<attrs>(?:\s*#\[[^\]]+\]\s*\n)*)\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:unsafe\s+)?(?:async\s+)?fn\s+{re.escape(symbol)}\s*(?:<[^{{;]*>)?\s*\(")
    if re.search(rf"\bfn\s+{re.escape(symbol)}\s*(?:<[^{{;]*>)?\s*\(", source) is None:
        raise GateError(f"contract handler is not a Rust function definition: {path.relative_to(ROOT)}:{symbol}")
    found = top_level_match(source, 0, len(source), definition, f"contract handler {path.relative_to(ROOT)}:{symbol}")
    # Array types contain semicolons (for example UserPtr<[i32; 2]>).
    # Only an item-level semicolon can turn this signature into a declaration.
    nesting = [")"]
    opening = -1
    for index in range(found.end(), len(source)):
        char = source[index]
        if char in "([":
            nesting.append({"(": ")", "[": "]"}[char])
        elif nesting and char == nesting[-1]:
            nesting.pop()
        elif not nesting and char in "{;":
            if char == "{":
                opening = index
            break
    if opening < 0:
        raise GateError(f"contract handler has no function body: {path.relative_to(ROOT)}:{symbol}")
    attrs = [re.sub(r"\s+", " ", attr.strip()) for attr in re.findall(r"#\[([^\]]+)\]", raw_source[found.start("attrs"):found.end("attrs")], re.DOTALL)]
    for cfg in (attr for attr in attrs if attr.startswith("cfg")):
        if cfg == 'cfg(target_arch = "x86_64")':
            continue  # The product and this syscall table support x86_64 only.
        feature = re.fullmatch(r'cfg\(feature\s*=\s*"([A-Za-z0-9_-]+)"\)', cfg)
        if feature is None or conditional != feature.group(1):
            raise GateError(f"contract handler cfg does not match cell conditional: {path.relative_to(ROOT)}:{symbol}")


def handler_route(value: object, conditional: str) -> str:
    if not isinstance(value, str):
        raise GateError("contract handler is invalid")
    relative, separator, symbol = value.rpartition(":")
    if not separator or not relative or not symbol:
        raise GateError("contract handler must be path:symbol")
    path = repository_path(relative, "contract handler", "file")
    if path.suffix != ".rs":
        raise GateError("contract handler must name Rust source")
    rust_function(path, symbol, conditional)
    return value


GRAPH_PREFIX = {"flags": "flag:", "structs": "struct:", "multiplexer_commands": "mux:", "provider_ioctls": "ioctl:", "errno_order": "errno:", "usercopy": "usercopy:", "state": "state:", "concurrency": "concurrency:", "teardown": "teardown:"}


def graph_field(value: object, contract: str, field: str) -> list[str]:
    if not isinstance(value, list) or not value or not all(isinstance(item, str) and item for item in value):
        raise GateError(f"contract {contract}.{field} must be a non-empty typed list")
    banned = re.compile(r"\b(?:contract-defined|Linux syscall-specific|todo|tbd|unknown|generic|handler-defined)\b", re.IGNORECASE)
    if any(banned.search(item) for item in value):
        raise GateError(f"contract {contract}.{field} uses a generic placeholder")
    if "explicit-none" in value and value != ["explicit-none"]:
        raise GateError(f"contract {contract}.{field} mixes explicit-none with content")
    if value != ["explicit-none"]:
        prefix = GRAPH_PREFIX[field]
        if any(not item.startswith(prefix) or not item[len(prefix):].strip() for item in value):
            raise GateError(f"contract {contract}.{field} has no non-empty typed grammar")
    return value


def load_ratchet(data: dict) -> dict[str, set[str]]:
    """Read the four shrink-only baselines that keep the ledger honest.

    `uncited_contracts` lists contract ids that carry no `Linux <path>:<line>`
    citation yet, `unreviewed_errno_order` lists the contracts whose ordering
    sentence is still the placeholder, and `final_static_allowlist` lists the
    `number:name` cells the final-strength check still tolerates for a declared
    gap, a missing test binding, or a fully implemented syscall whose contract
    record has not been compared with Linux.  `unbound_programs` lists the
    differential programs no cell binds; this gate only pins its shape, and
    scripts/ci/check_abi_contracts.py compares it with the runner registry.  No
    list may grow: the caller compares each against reality.
    """
    table = data.get("ratchet")
    if not isinstance(table, dict) or set(table) != RATCHET_FIELDS:
        raise GateError(f"ratchet must declare exactly {sorted(RATCHET_FIELDS)}")
    result: dict[str, set[str]] = {}
    for field in sorted(RATCHET_FIELDS):
        value = table[field]
        if not isinstance(value, list) or not all(isinstance(item, str) and item.strip() for item in value) or len(value) != len(set(value)):
            raise GateError(f"ratchet.{field} must be a duplicate-free array of strings")
        result[field] = set(value)
    return result


def cell_identity(cell: dict) -> str:
    return f"{cell['number']}:{cell['name']}"


def record_ratchet(definitions: dict[str, dict], baseline: dict[str, set[str]]) -> None:
    """Fail on a new uncited or unreviewed contract; report a shrinking backlog.

    Two independent lists, because they measure different things:
    `uncited_contracts` says whether a reader can check a record against the
    pinned release at all, `unreviewed_errno_order` says whether anybody
    compared its ordering sentence with Linux.  They overlap almost completely
    today, and both have to empty out before the ledger means what it looks
    like.
    """
    uncited = {
        ident for ident, definition in definitions.items()
        if not any(LINUX_CITATION.search(item) for field, value in definition.items() if field != "id" for item in value)
    }
    unreviewed = {
        ident for ident, definition in definitions.items()
        if definition["errno_order"] == [UNREVIEWED_ERRNO_ORDER]
    }
    fresh = sorted(uncited - baseline["uncited_contracts"])
    if fresh:
        raise GateError(f"contracts carry no Linux <path>:<line> citation and are not baselined: {fresh}")
    fresh = sorted(unreviewed - baseline["unreviewed_errno_order"])
    if fresh:
        raise GateError(f"contracts carry the unreviewed errno_order placeholder and are not baselined: {fresh}")
    for name, current, allowed in (
        ("citation", uncited, baseline["uncited_contracts"]),
        ("errno-order", unreviewed, baseline["unreviewed_errno_order"]),
    ):
        retired = sorted(allowed - current)
        if retired:
            print(f"linux-abi {name} ratchet: {len(current)} of {len(definitions)} contracts remain (baseline {len(allowed)}, may not grow); {len(retired)} are now clean and may leave the list: {retired}")
        else:
            print(f"linux-abi {name} ratchet: {len(current)} of {len(definitions)} contracts remain (baseline {len(allowed)}, may not grow)")
    # Adding a citation to a contract is only honest once its ordering sentence
    # says what was actually compared, so a cited record may not keep the
    # placeholder that describes an ordering nobody checked.
    escaping = sorted(unreviewed - uncited)
    if escaping:
        raise GateError(f"cited contracts still carry the unreviewed errno_order: {escaping}")


def final_static(contracts_path: Path, cells: dict[int, dict], entries: dict[int, str]) -> None:
    """Require the final-strength static shape within a shrink-only allowlist."""
    baseline = load_ratchet(load_toml(contracts_path, "contracts"))["final_static_allowlist"]
    gapped = {cell_identity(cell) for cell in cells.values()
              if cell["status"] == "implemented" and (not cell["tests"] or cell["validation_gaps"] != ["explicit-none"])}
    untested = {cell_identity(cell) for cell in cells.values() if not cell["tests"]}
    # An `implemented` cell is a claim that the syscall matches Linux, so it
    # cannot be final while its contract still carries the placeholder that
    # says nobody compared the errno ordering.
    unrecorded = {cell_identity(cell) for cell in cells.values()
                  if cell["status"] == "implemented" and cell["errno_order"] == [UNREVIEWED_ERRNO_ORDER]}
    claims = gapped | untested | unrecorded
    offenders = sorted(claims - baseline)
    if len(cells) != len(entries) or offenders:
        raise GateError(
            "final ABI static prerequisites incomplete: "
            f"unknown={len(entries) - len(cells)}, "
            f"outside the final allowlist={offenders}"
        )
    retired = sorted(baseline - claims)
    if retired:
        print(f"linux-abi final ratchet: {len(claims)} cells still need runtime or review evidence; {len(retired)} allowlisted cells are clean and may leave the list: {retired}")
    else:
        print(f"linux-abi final ratchet: {len(claims)} allowlisted cells still need runtime or review evidence (baseline {len(baseline)}, may not grow)")


def validate_citations(definitions: dict[str, dict], linux_tree: Path | None = None) -> int:
    """If a full Linux source tree is available, verify that cited files and lines exist."""
    if linux_tree is None or not (linux_tree / "include/uapi").is_dir():
        return 0

    citation_pattern = re.compile(r"Linux ([A-Za-z0-9_./*-]+):([0-9]+)(?:-([0-9]+))?")
    checked = 0
    for ident, definition in definitions.items():
        for field, value in definition.items():
            if field == "id" or not isinstance(value, list):
                continue
            for item in value:
                for match in citation_pattern.finditer(item):
                    rel_path, start_s, end_s = match.groups()
                    target = linux_tree / rel_path
                    if not target.is_file():
                        raise GateError(
                            f"contract {ident} cites nonexistent Linux file '{rel_path}' in {linux_tree}"
                        )
                    line_count = len(target.read_text(encoding="utf-8", errors="replace").splitlines())
                    start = int(start_s)
                    end = int(end_s) if end_s else start
                    if start <= 0 or start > line_count or end < start or end > line_count:
                        raise GateError(
                            f"contract {ident} cites invalid line range {start}-{end} in {rel_path} (file has {line_count} lines)"
                        )
                    checked += 1
    return checked


def find_linux_tree(explicit: Path | None = None, fallback: Path | None = None) -> Path | None:
    candidates = [
        explicit,
        os.environ.get("THEKERNEL_LINUX_SOURCE"),
        os.environ.get("LINUX_TREE"),
        fallback,
        Path.home() / "Desktop/linux-7.2.3",
    ]
    for c in candidates:
        if c:
            p = Path(c)
            if (p / "include/uapi").is_dir():
                return p
    return None


def contract_cells(contracts_path: Path, entries: dict[int, str], dispatch: Path | None = None, linux_tree: Path | None = None) -> dict[int, dict]:
    data = load_toml(contracts_path, "contracts")
    if set(data) != {"schema", "linux_manifest", "progress", "ratchet", "contract", "cell"} or data["schema"] != 3:
        raise GateError("contracts schema is invalid")
    if data["linux_manifest"] != "linux-abi.toml":
        raise GateError("contracts must reference the pinned Linux manifest")
    ratchet = load_ratchet(data)
    definitions: dict[str, dict] = {}
    for item in data["contract"]:
        if not isinstance(item, dict) or set(item) != CONTRACT_FIELDS or not isinstance(item.get("id"), str) or item["id"] in definitions:
            raise GateError("contract definition is invalid or duplicate")
        for field in CONTRACT_FIELDS - {"id"}:
            graph_field(item[field], item["id"], field)
        definitions[item["id"]] = item
    record_ratchet(definitions, ratchet)
    if linux_tree is not None:
        count = validate_citations(definitions, linux_tree)
        if count:
            print(f"linux-abi citations: {count} citations verified against {linux_tree}")
    cells: dict[int, dict] = {}
    used_implemented: set[str] = set()
    for item in data["cell"]:
        if not isinstance(item, dict) or set(item) != CELL_FIELDS:
            raise GateError("contract cell must bind exactly number, name, status, contract, and handler")
        number, name = item["number"], item["name"]
        if not isinstance(number, int) or number not in entries or item["name"] != entries[number] or number in cells:
            raise GateError("contract cell has duplicate number or does not match pinned syscall name")
        if item["status"] not in {"implemented", "explicit-enosys", "partial"} or item["contract"] not in definitions:
            raise GateError("contract cell has unknown status or contract")
        if item["status"] == "implemented" and (item["contract"] in used_implemented or item["contract"] != f"linux-{name}"):
            raise GateError("implemented cells require a name-bound, non-reused contract")
        used_implemented.add(item["contract"])
        if item["conditional"] != "explicit-none" and (not isinstance(item["conditional"], str) or not item["conditional"]):
            raise GateError("contract cell conditional is invalid")
        for field in ("validation_gaps", "limitations"):
            value = item[field]
            if not isinstance(value, list) or not value or not all(isinstance(entry, str) and entry.strip() for entry in value):
                raise GateError(f"contract cell {name}.{field} must describe its scope or explicit-none")
            if "explicit-none" in value and value != ["explicit-none"]:
                raise GateError(f"contract cell {name}.{field} mixes explicit-none with content")
        if item["status"] == "implemented" and item["limitations"] != ["explicit-none"]:
            raise GateError(f"implemented contract cell {name} cannot have implementation limitations")
        if item["status"] == "partial" and item["limitations"] == ["explicit-none"]:
            raise GateError(f"partial contract cell {name} needs a concrete implementation limitation")
        tests = item["tests"]
        if not isinstance(tests, list) or not all(isinstance(test, str) for test in tests) or len(tests) != len(set(tests)):
            raise GateError(f"contract cell {name} tests are invalid")
        for test in tests:
            relative, separator, symbol = test.rpartition(":")
            if not separator or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", symbol):
                raise GateError(f"contract test must name path:symbol: {test}")
            path = repository_path(relative, "contract test", "file")
            text = path.read_text(encoding="utf-8")
            masked = mask_rust_noncode(text)
            definition = rf"\bfn\s+{re.escape(symbol)}\s*\(|\b(?:int|void|bool)\s+{re.escape(symbol)}\s*\([^;{{}}]*\)\s*{{"
            if not re.search(definition, masked):
                raise GateError(f"contract test symbol does not exist: {test}")
            if path.suffix == ".rs":
                attributed = rf"(?m)^\s*#\[test\]\s*(?:#\[[^\]]+\]\s*)*fn\s+{re.escape(symbol)}\s*\("
                if not re.search(attributed, masked):
                    raise GateError(f"contract Rust test must have #[test]: {test}")
            elif path.suffix != ".c":
                raise GateError(f"contract test source language is not supported: {test}")
        if not tests and item["validation_gaps"] == ["explicit-none"]:
            raise GateError(f"contract cell {name} without tests must report a validation gap")
        if item["status"] == "implemented" and item["validation_gaps"] == ["explicit-none"]:
            from tools.qemu_runner.abi_differential import CONTRACTS as runtime_cases, PROGRAMS, SYSCALL_CASES
            program, case = SYSCALL_CASES.get(number, (None, None))
            expected_test = f"tests/guest/portable/{program}-differential.c:main"
            if program not in PROGRAMS or case not in runtime_cases or expected_test not in tests:
                raise GateError(f"implemented contract cell {name} claims validation without a registered differential case")
        cells[number] = {**item, **{field: definitions[item["contract"]][field] for field in CONTRACT_FIELDS - {"id"}}}
        cells[number]["handler"] = handler_route(item["handler"], item["conditional"])
    counts = Counter(cell["status"] for cell in cells.values())
    progress = data["progress"]
    actual = {
        "reviewed": len(cells),
        "resolved": counts["implemented"] + counts["explicit-enosys"],
        "implemented": counts["implemented"],
        "explicit_enosys": counts["explicit-enosys"],
        "fallback": 0,
        "partial": counts["partial"],
        "unknown": len(entries) - len(cells),
    }
    if progress != actual:
        raise GateError(f"contract progress is invalid: {progress}, expected {actual}")
    if dispatch is not None:
        _, ni, parsed = routes(dispatch, set(entries.values()), WITNESS)
        bindings: dict[str, tuple[str, list[str]]] = {}
        for pattern, expression in parsed:
            masked_pattern = mask_rust_noncode(pattern)
            # As in routes(), attributes carry string literals and must be read
            # from the raw pattern rather than the masked form.
            attrs = [re.sub(r"\s+", " ", item.strip()) for item in re.findall(r"#\[([^]]+)\]", pattern, re.DOTALL)]
            for name in SYSNO.findall(masked_pattern):
                bindings[name] = (mask_rust_noncode(expression), attrs)
        for cell in cells.values():
            if cell["status"] == "explicit-enosys":
                if (cell["handler"] != "kernel/src/syscall/dispatch.rs:sys_ni_syscall"
                        or cell["name"] not in ni
                        or bindings.get(cell["name"], ("", []))[0] != "sys_ni_syscall()"):
                    raise GateError(f"explicit ENOSYS cell is not bound to its actual NI arm: {cell['number']}:{cell['name']}")
                continue
            binding = bindings.get(cell["name"])
            pat_str = expected_dispatch_call(cell["handler"])
            call = re.compile(rf"{pat_str}\s*\(")
            if binding is None or call.search(binding[0]) is None:
                raise GateError(f"non-NI cell is not bound to its actual dispatch handler: {cell['number']}:{cell['name']}")
            expected_cfg = [] if cell["conditional"] == "explicit-none" else [f'cfg(feature = "{cell["conditional"]}")']
            # x86_64 is the sole product architecture, not an optional feature.
            actual_cfg = [cfg for cfg in binding[1] if cfg != 'cfg(target_arch = "x86_64")']
            if actual_cfg != expected_cfg:
                raise GateError(f"cell conditional does not match its dispatch profile: {cell['number']}:{cell['name']}")
    return cells


def schema(manifest_path: Path, contracts_path: Path, source: Path, dispatch: Path = DISPATCH, *, final: bool = False, linux_tree: Path | None = None) -> None:
    manifest = load_manifest(manifest_path)
    entries = parse_table(source / manifest["linux"]["table"])
    tree = find_linux_tree(linux_tree, source)
    cells = contract_cells(contracts_path, entries, dispatch, linux_tree=tree)
    routing = states(manifest, entries)
    ni_mismatch = sorted(number for number, cell in cells.items()
                         if (cell["status"] == "explicit-enosys") != (routing[number] == "explicit-enosys"))
    if ni_mismatch:
        raise GateError(f"contract explicit ENOSYS set disagrees with routing inventory: {ni_mismatch}")
    counts = Counter(cell["status"] for cell in cells.values())
    print(f"linux-abi schema: reviewed={len(cells)} implemented={counts['implemented']} explicit-enosys={counts['explicit-enosys']} partial={counts['partial']} unknown={len(entries) - len(cells)}")
    unvalidated = [cell["name"] for cell in cells.values()
                   if cell["status"] == "implemented" and (not cell["tests"] or cell["validation_gaps"] != ["explicit-none"])]
    print(f"linux-abi static declarations: implemented-with-gaps={len(unvalidated)}; runtime execution is not established by this gate")
    if final:
        final_static(contracts_path, cells, entries)
        print("linux-abi final static prerequisites satisfied within the committed allowlist; current-run guest results are still required")


def run_git(directory: Path, *args: str) -> str:
    result = subprocess.run(["git", "-C", str(directory), *args], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if result.returncode: raise GateError(result.stderr.strip() or result.stdout.strip())
    return result.stdout.strip()


def materialize(manifest_path: Path, destination: Path) -> None:
    manifest = load_manifest(manifest_path); linux = manifest["linux"]; destination = destination.resolve()
    if str(destination).startswith(("/tmp/", "/dev/shm/")): raise GateError("Linux source may not be materialized on tmpfs")
    if not destination.exists():
        destination.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "clone", "--depth=1", "--filter=blob:none", "--no-checkout", "--branch", linux["tag"], linux["repository"], str(destination)], check=True)
        subprocess.run(["git", "-C", str(destination), "sparse-checkout", "init", "--no-cone"], check=True)
        subprocess.run(["git", "-C", str(destination), "sparse-checkout", "set", "--no-cone", linux["table"]], check=True)
        subprocess.run(["git", "-C", str(destination), "checkout", "--detach", linux["tag"]], check=True)
    if not (destination / ".git").is_dir() or run_git(destination, "remote", "get-url", "origin") != linux["repository"]: raise GateError("materialized Linux source origin is invalid")
    if run_git(destination, "rev-parse", "HEAD^{commit}") != run_git(destination, "rev-parse", f"{linux['tag']}^{{commit}}") or run_git(destination, "status", "--porcelain", "--untracked-files=all"):
        raise GateError("materialized Linux source is not the clean selected release")
    if not (destination / linux["table"]).is_file(): raise GateError("materialized Linux source does not contain syscall table")


def main(argv: Sequence[str] | None = None) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    parser = argparse.ArgumentParser(description=__doc__); parser.add_argument("command", choices=("materialize", "inventory", "schema", "all")); parser.add_argument("--manifest", type=Path, default=MANIFEST); parser.add_argument("--contracts", type=Path, default=CONTRACTS); parser.add_argument("--linux-src", type=Path, default=SOURCE); parser.add_argument("--dispatch", type=Path, default=DISPATCH)
    parser.add_argument("--linux-tree", type=Path, default=None, help="path to full Linux source tree for citation verification")
    parser.add_argument("--final", action="store_true", help="require final static prerequisites within ratchet.final_static_allowlist; does not establish guest runtime acceptance")
    args = parser.parse_args(arguments)
    if args.final and args.command not in {"schema", "all"}:
        parser.error("--final requires schema or all")
    try:
        if args.command in {"materialize", "all"}: materialize(args.manifest, args.linux_src)
        if args.command in {"inventory", "all"}: inventory(args.manifest, args.linux_src, args.dispatch)
        if args.command in {"schema", "all"}: schema(args.manifest, args.contracts, args.linux_src, args.dispatch, final=args.final, linux_tree=args.linux_tree)
    except (GateError, OSError, subprocess.CalledProcessError) as error:
        print(f"linux-abi: {error}", file=sys.stderr); return 1
    return 0


if __name__ == "__main__": raise SystemExit(main())
