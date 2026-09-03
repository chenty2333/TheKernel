#!/usr/bin/env python3
"""Materialize and statically inventory Linux v6.18 x86_64 syscall routing.

This gate never treats a dispatch route as semantic handler evidence.  Deeper
handler ENOSYS behavior belongs to contract and differential gates.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from collections import Counter
from pathlib import Path
from typing import Sequence

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "config/linux-v6.18-abi.toml"
CONTRACTS = ROOT / "config/linux-v6.18-contracts.toml"
SOURCE = ROOT / ".state/linux-v6.18"
DISPATCH = ROOT / "kernel/src/syscall/dispatch.rs"
HEX40 = re.compile(r"^[0-9a-f]{40}$")
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
    if not isinstance(linux, dict) or set(linux) != {"repository", "tag", "tag_object", "commit", "table"}:
        raise GateError("linux manifest fields are invalid")
    if linux != {**linux, "repository": "https://github.com/torvalds/linux.git", "tag": "v6.18", "tag_object": "f7b88edb52c8dd01b7e576390d658ae6eef0e134", "commit": "7d0a66e4bb9081d75c82ec4957c50034cb0ea449", "table": "arch/x86/entry/syscalls/syscall_64.tbl"}:
        raise GateError("linux v6.18 pin does not match the required source")
    if not all(isinstance(value, str) for value in linux.values()) or not HEX40.fullmatch(linux["tag_object"]) or not HEX40.fullmatch(linux["commit"]):
        raise GateError("linux manifest object IDs are invalid")
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
    if len(entries) != 383:
        raise GateError(f"expected 383 native common+64 syscalls, found {len(entries)}")
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
    ni = set(SYSNO.findall(mask_rust_noncode(ni_patterns[0]))); all_routes = [name for pattern, _ in parsed for name in SYSNO.findall(mask_rust_noncode(pattern))]
    repeats = sorted(name for name, count in Counter(all_routes).items() if count > 1)
    if repeats: raise GateError(f"dispatch repeats syscall route(s): {repeats}")
    found = set(all_routes)
    if found - table: raise GateError(f"dispatch names absent from v6.18 table: {sorted(found - table)}")
    if sum(re.sub(r"(?s)^\s*#\[[^]]+\]\s*", "", pattern).strip() == "_" for pattern, _ in parsed) != 1:
        raise GateError("dispatch must have exactly one default arm for table-external syscall numbers")
    return found, ni, parsed


def inventory(manifest_path: Path, source: Path, dispatch: Path) -> None:
    manifest = load_manifest(manifest_path); entries = parse_table(source / manifest["linux"]["table"]); matrix = states(manifest, entries)
    found, ni, _ = routes(dispatch, set(entries.values()), manifest["routing_witness"]["bpf"])
    expected_ni = {entries[number] for number, state in matrix.items() if state == "explicit-enosys"}; fallback = {entries[number] for number, state in matrix.items() if state == "native-fallback"}
    if ni != expected_ni or found != set(entries.values()) - fallback or found & fallback: raise GateError("explicit dispatch routes do not match routing inventory")
    counts = Counter(matrix.values()); print(f"linux-v6.18-abi inventory: ordinary-explicit={counts['ordinary-explicit']} explicit-enosys={counts['explicit-enosys']} native-fallback={counts['native-fallback']}")


CONTRACT_FIELDS = {
    "id", "flags", "structs", "multiplexer_commands", "provider_ioctls",
    "errno_order", "usercopy", "state", "concurrency", "teardown",
}
CELL_FIELDS = {"number", "name", "status", "contract", "handler", "conditional"}
DISPATCH_CALLS = {
    "kernel/src/syscall/dispatch.rs:sys_ni_syscall": "sys_ni_syscall",
    "kernel/src/syscall/bpf/mod.rs:sys_bpf": "super::bpf::sys_bpf",
    "kernel/src/syscall/task/uprobe.rs:sys_uretprobe": "super::task::sys_uretprobe",
    "kernel/src/syscall/task/uprobe.rs:sys_uprobe": "super::task::sys_uprobe",
    "kernel/src/syscall/fs/xattr.rs:sys_setxattrat": "super::fs::sys_setxattrat",
    "kernel/src/syscall/fs/xattr.rs:sys_getxattrat": "super::fs::sys_getxattrat",
    "kernel/src/syscall/fs/xattr.rs:sys_listxattrat": "super::fs::sys_listxattrat",
    "kernel/src/syscall/fs/xattr.rs:sys_removexattrat": "super::fs::sys_removexattrat",
    "kernel/src/syscall/fs/mount.rs:sys_open_tree_attr": "super::fs::sys_open_tree_attr",
    "kernel/src/syscall/fs/fileattr.rs:sys_file_getattr": "super::fs::sys_file_getattr",
    "kernel/src/syscall/fs/fileattr.rs:sys_file_setattr": "super::fs::sys_file_setattr",
}


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
    source = mask_rust_noncode(path.read_text(encoding="utf-8"))
    definition = re.compile(rf"(?m)^(?P<attrs>(?:\s*#\[[^\]]+\]\s*\n)*)\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:unsafe\s+)?(?:async\s+)?fn\s+{re.escape(symbol)}\s*(?:<[^{{;]*>)?\s*\(")
    if re.search(rf"\bfn\s+{re.escape(symbol)}\s*(?:<[^{{;]*>)?\s*\(", source) is None:
        raise GateError(f"contract handler is not a Rust function definition: {path.relative_to(ROOT)}:{symbol}")
    found = top_level_match(source, 0, len(source), definition, f"contract handler {path.relative_to(ROOT)}:{symbol}")
    opening = source.find("{", found.end())
    declaration = source.find(";", found.end())
    if opening < 0 or (declaration >= 0 and declaration < opening):
        raise GateError(f"contract handler has no function body: {path.relative_to(ROOT)}:{symbol}")
    attrs = [re.sub(r"\s+", " ", attr.strip()) for attr in re.findall(r"#\[([^\]]+)\]", found.group("attrs"), re.DOTALL)]
    for cfg in (attr for attr in attrs if attr.startswith("cfg")):
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


def contract_cells(contracts_path: Path, entries: dict[int, str], dispatch: Path | None = None) -> dict[int, dict]:
    data = load_toml(contracts_path, "contracts")
    if set(data) != {"schema", "linux_manifest", "progress", "contract", "cell"} or data["schema"] != 2:
        raise GateError("contracts schema is invalid")
    if data["linux_manifest"] != "linux-v6.18-abi.toml":
        raise GateError("contracts must reference the pinned Linux manifest")
    definitions: dict[str, dict] = {}
    for item in data["contract"]:
        if not isinstance(item, dict) or set(item) != CONTRACT_FIELDS or not isinstance(item.get("id"), str) or item["id"] in definitions:
            raise GateError("contract definition is invalid or duplicate")
        for field in CONTRACT_FIELDS - {"id"}:
            graph_field(item[field], item["id"], field)
        definitions[item["id"]] = item
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
        if item["status"] == "implemented" and (item["contract"] in used_implemented or item["contract"] != f"v618-{name}"):
            raise GateError("implemented cells require a name-bound, non-reused contract")
        used_implemented.add(item["contract"])
        if item["conditional"] != "explicit-none" and (not isinstance(item["conditional"], str) or not item["conditional"]):
            raise GateError("contract cell conditional is invalid")
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
            expected_call = DISPATCH_CALLS.get(cell["handler"])
            if expected_call is None:
                raise GateError(f"contract handler has no approved dispatch call binding: {cell['handler']}")
            if cell["status"] == "explicit-enosys":
                if (cell["handler"] != "kernel/src/syscall/dispatch.rs:sys_ni_syscall"
                        or cell["name"] not in ni
                        or bindings.get(cell["name"], ("", []))[0] != "sys_ni_syscall()"):
                    raise GateError(f"explicit ENOSYS cell is not bound to its actual NI arm: {cell['number']}:{cell['name']}")
                continue
            binding = bindings.get(cell["name"])
            call = re.compile(rf"(?<![A-Za-z0-9_:]){re.escape(expected_call)}\s*\(")
            if binding is None or call.search(binding[0]) is None:
                raise GateError(f"non-NI cell is not bound to its actual dispatch handler: {cell['number']}:{cell['name']}")
            expected_cfg = [] if cell["conditional"] == "explicit-none" else [f'cfg(feature = "{cell["conditional"]}")']
            if binding[1] != expected_cfg:
                raise GateError(f"cell conditional does not match its dispatch profile: {cell['number']}:{cell['name']}")
    return cells


def schema(manifest_path: Path, contracts_path: Path, source: Path, dispatch: Path = DISPATCH) -> None:
    manifest = load_manifest(manifest_path)
    entries = parse_table(source / manifest["linux"]["table"])
    cells = contract_cells(contracts_path, entries, dispatch)
    routing = states(manifest, entries)
    ni_mismatch = sorted(number for number, cell in cells.items()
                         if (cell["status"] == "explicit-enosys") != (routing[number] == "explicit-enosys"))
    if ni_mismatch:
        raise GateError(f"contract explicit ENOSYS set disagrees with routing inventory: {ni_mismatch}")
    counts = Counter(cell["status"] for cell in cells.values())
    print(f"linux-v6.18-abi schema: reviewed={len(cells)} implemented={counts['implemented']} explicit-enosys={counts['explicit-enosys']} partial={counts['partial']} unknown={len(entries) - len(cells)}")


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
        subprocess.run(["git", "-C", str(destination), "checkout", "--detach", linux["commit"]], check=True)
    if not (destination / ".git").is_dir() or run_git(destination, "remote", "get-url", "origin") != linux["repository"]: raise GateError("materialized Linux source origin is invalid")
    remote = subprocess.run(["git", "ls-remote", linux["repository"], f"refs/tags/{linux['tag']}", f"refs/tags/{linux['tag']}^{{}}"], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if remote.returncode: raise GateError(f"cannot query remote tag: {remote.stderr.strip()}")
    refs = {line.split()[1]: line.split()[0] for line in remote.stdout.splitlines() if len(line.split()) == 2}
    tag = f"refs/tags/{linux['tag']}"
    if refs.get(tag) != linux["tag_object"] or refs.get(f"{tag}^{{}}") != linux["commit"]: raise GateError("remote v6.18 tag object or peeled commit changed")
    if run_git(destination, "rev-parse", "HEAD^{commit}") != linux["commit"] or run_git(destination, "status", "--porcelain", "--untracked-files=all"): raise GateError("materialized Linux source is not the clean pinned commit")
    if not (destination / linux["table"]).is_file(): raise GateError("materialized Linux source does not contain syscall table")


def main(argv: Sequence[str] | None = None) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    parser = argparse.ArgumentParser(description=__doc__); parser.add_argument("command", choices=("materialize", "inventory", "schema", "all")); parser.add_argument("--manifest", type=Path, default=MANIFEST); parser.add_argument("--contracts", type=Path, default=CONTRACTS); parser.add_argument("--linux-src", type=Path, default=SOURCE); parser.add_argument("--dispatch", type=Path, default=DISPATCH)
    args = parser.parse_args(arguments)
    try:
        if args.command in {"materialize", "all"}: materialize(args.manifest, args.linux_src)
        if args.command in {"inventory", "all"}: inventory(args.manifest, args.linux_src, args.dispatch)
        if args.command in {"schema", "all"}: schema(args.manifest, args.contracts, args.linux_src, args.dispatch)
    except (GateError, OSError, subprocess.CalledProcessError) as error:
        print(f"linux-v6.18-abi: {error}", file=sys.stderr); return 1
    return 0


if __name__ == "__main__": raise SystemExit(main())
