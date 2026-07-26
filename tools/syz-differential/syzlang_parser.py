#!/usr/bin/env python3
"""Parser for a strict subset of syzkaller's syzlang description language.

The subset is defined in tools/syz-differential/README.md.  Anything outside
the subset is rejected with a SyzlangError naming the file, line, and reason —
the parser must never silently mis-parse.

Python 3 stdlib only.  No network, no third-party packages.
"""

from __future__ import annotations

import json
import re
import sys
from dataclasses import dataclass, field


class SyzlangError(Exception):
    """Raised for any input outside the supported syzlang subset."""

    def __init__(self, path: str, lineno: int, message: str):
        super().__init__(f"{path}:{lineno}: {message}")
        self.path = path
        self.lineno = lineno


# ---------------------------------------------------------------------------
# AST node types
# ---------------------------------------------------------------------------

INT_TYPES = ("int8", "int16", "int32", "int64", "intptr")
PTR_DIRS = ("in", "out", "inout")

IDENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
INT_RE = re.compile(r"^-?(0x[0-9a-fA-F]+|[0-9]+)$")


@dataclass(frozen=True)
class TypeRef:
    """A typed argument / field / return type.

    kind is one of:
      "int"      data: {"base": "int32"}
      "intrange" data: {"base": "int32", "lo": 0, "hi": 100}
      "fd"       data: {}                       (generic fd base type)
      "resource" data: {"name": "fd_event"}
      "flags"    data: {"set": "eventfd_flags", "base": "int32"}
      "const"    data: {"value": 0, "base": "intptr"}
      "ptr"      data: {"dir": "in", "elem": TypeRef}
      "buffer"   data: {"dir": "out"}
      "len"      data: {"target": "buf", "base": "intptr"}
      "struct"   data: {"name": "itimerspec"}
    """

    kind: str
    data: dict


@dataclass(frozen=True)
class Resource:
    name: str
    base: str  # only "fd" in this subset


@dataclass(frozen=True)
class FlagSet:
    name: str
    values: tuple  # tuple[str, ...] of C constant identifiers


@dataclass(frozen=True)
class StructField:
    name: str
    type: TypeRef


@dataclass(frozen=True)
class StructDef:
    name: str
    fields: tuple  # tuple[StructField, ...]


@dataclass(frozen=True)
class Param:
    name: str
    type: TypeRef


@dataclass(frozen=True)
class Syscall:
    name: str      # base syscall name, e.g. "read" (used for SYS_<name>)
    variant: str   # syzkaller variant suffix ("" if none), e.g. "eventfd"
    params: tuple  # tuple[Param, ...]
    ret: TypeRef | None

    @property
    def full_name(self) -> str:
        return f"{self.name}${self.variant}" if self.variant else self.name


@dataclass
class Description:
    path: str
    includes: list = field(default_factory=list)
    resources: dict = field(default_factory=dict)   # name -> Resource
    flagsets: dict = field(default_factory=dict)    # name -> FlagSet
    structs: dict = field(default_factory=dict)     # name -> StructDef
    syscalls: dict = field(default_factory=dict)    # full_name -> Syscall


# ---------------------------------------------------------------------------
# Tokenizing helpers
# ---------------------------------------------------------------------------


def _split_top(s: str, sep: str) -> list:
    """Split on sep at bracket depth zero (respects [ ] nesting)."""
    parts = []
    depth = 0
    cur = []
    for ch in s:
        if ch == "[":
            depth += 1
        elif ch == "]":
            depth -= 1
            if depth < 0:
                raise ValueError("unbalanced ']'")
        if ch == sep and depth == 0:
            parts.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
    if depth != 0:
        raise ValueError("unbalanced '['")
    parts.append("".join(cur))
    return parts


def _parse_int(tok: str) -> int:
    if not INT_RE.match(tok):
        raise ValueError(f"expected integer, got {tok!r}")
    return int(tok, 0)


# ---------------------------------------------------------------------------
# Type parsing
# ---------------------------------------------------------------------------


def _parse_type(tok: str, desc: Description, err) -> TypeRef:
    tok = tok.strip()
    if not tok:
        raise err("empty type")

    # bracketed forms: name[args]
    m = re.match(r"^([A-Za-z_][A-Za-z0-9_]*)\[(.*)\]$", tok)
    if m:
        head, inner = m.group(1), m.group(2)
        args = [a.strip() for a in _split_top(inner, ",")]
        if head in INT_TYPES:
            # integer range: int32[lo:hi]
            if len(args) != 1 or ":" not in args[0]:
                raise err(f"integer range must be {head}[lo:hi]")
            lo_s, hi_s = args[0].split(":", 1)
            lo, hi = _parse_int(lo_s), _parse_int(hi_s)
            if lo > hi:
                raise err(f"range lo > hi in {tok!r}")
            return TypeRef("intrange", {"base": head, "lo": lo, "hi": hi})
        if head == "flags":
            if len(args) not in (1, 2):
                raise err("flags[] takes flags-set name and optional int base")
            base = args[1] if len(args) == 2 else "int32"
            if base not in INT_TYPES:
                raise err(f"unknown flags base type {base!r}")
            return TypeRef("flags", {"set": args[0], "base": base})
        if head == "const":
            if len(args) not in (1, 2):
                raise err("const[] takes a value and optional int base")
            base = args[1] if len(args) == 2 else "intptr"
            if base not in INT_TYPES:
                raise err(f"unknown const base type {base!r}")
            return TypeRef("const", {"value": _parse_int(args[0]), "base": base})
        if head == "ptr":
            if len(args) != 2 or args[0] not in PTR_DIRS:
                raise err("ptr[] must be ptr[in|out|inout, <type>]")
            elem = _parse_type(args[1], desc, err)
            if elem.kind not in ("int", "struct"):
                raise err("ptr element must be an int type or struct name")
            return TypeRef("ptr", {"dir": args[0], "elem": elem})
        if head == "buffer":
            if len(args) != 1 or args[0] not in PTR_DIRS:
                raise err("buffer[] must be buffer[in|out|inout]")
            return TypeRef("buffer", {"dir": args[0]})
        if head == "len":
            if len(args) not in (1, 2):
                raise err("len[] takes a target arg name and optional base")
            if not IDENT_RE.match(args[0]):
                raise err(f"len target must be an identifier, got {args[0]!r}")
            base = args[1] if len(args) == 2 else "intptr"
            if base not in INT_TYPES:
                raise err(f"unknown len base type {base!r}")
            return TypeRef("len", {"target": args[0], "base": base})
        raise err(f"unsupported bracketed type {head!r} in {tok!r}")

    if tok in INT_TYPES:
        return TypeRef("int", {"base": tok})
    if tok == "fd":
        return TypeRef("fd", {})
    if IDENT_RE.match(tok):
        # resource or struct reference; resolved during validation
        if tok in desc.resources:
            return TypeRef("resource", {"name": tok})
        return TypeRef("struct", {"name": tok})
    raise err(f"unsupported type syntax {tok!r}")


# ---------------------------------------------------------------------------
# Line parsing
# ---------------------------------------------------------------------------

INCLUDE_RE = re.compile(r"^include\s+<([A-Za-z0-9_./-]+)>$")
RESOURCE_RE = re.compile(r"^resource\s+([A-Za-z_][A-Za-z0-9_]*)\[([A-Za-z_][A-Za-z0-9_]*)\]$")
STRUCT_OPEN_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*)\s*\{$")
SYSCALL_RE = re.compile(
    r"^([A-Za-z_][A-Za-z0-9_]*)(?:\$([A-Za-z0-9_]+))?\s*\((.*)\)\s*"
    r"([A-Za-z_][A-Za-z0-9_\[\]:, ]*)?$"
)
FLAGSET_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+)$")


def parse_file(path: str) -> Description:
    with open(path, "r", encoding="utf-8") as f:
        raw_lines = f.read().splitlines()
    return parse_lines(raw_lines, path)


def parse_lines(raw_lines, path: str) -> Description:
    desc = Description(path=path)
    struct_open = None  # (name, lineno, fields[(fname, ftype_str, lineno)])

    def make_err(lineno):
        def err(msg):
            return SyzlangError(path, lineno, msg)
        return err

    pending_structs = []  # deferred so structs may reference later structs? no:
    # struct field types are resolved after the full file is read.

    for lineno, raw in enumerate(raw_lines, 1):
        line = raw.split("#", 1)[0].rstrip()
        if not line.strip():
            continue
        err = make_err(lineno)

        if struct_open is not None:
            name, open_lineno, fields = struct_open
            stripped = line.strip()
            if stripped == "}":
                if not fields:
                    raise err(f"struct {name!r} has no fields")
                pending_structs.append((name, open_lineno, fields))
                struct_open = None
                continue
            parts = stripped.split(None, 1)
            if len(parts) != 2:
                raise err(f"struct field must be '<name> <type>', got {stripped!r}")
            fname, ftype = parts
            if not IDENT_RE.match(fname):
                raise err(f"bad struct field name {fname!r}")
            if any(f[0] == fname for f in fields):
                raise err(f"duplicate field {fname!r} in struct {name!r}")
            fields.append((fname, ftype, lineno))
            continue

        if line != line.lstrip() and struct_open is None:
            raise err("indentation is only valid inside a struct block")

        m = INCLUDE_RE.match(line)
        if m:
            desc.includes.append(m.group(1))
            continue

        m = RESOURCE_RE.match(line)
        if m:
            rname, base = m.group(1), m.group(2)
            if base != "fd":
                raise err(f"unsupported resource base type {base!r} (subset allows only 'fd')")
            if rname in desc.resources:
                raise err(f"duplicate resource {rname!r}")
            desc.resources[rname] = Resource(rname, base)
            continue

        m = STRUCT_OPEN_RE.match(line)
        if m:
            sname = m.group(1)
            if sname in desc.structs or any(p[0] == sname for p in pending_structs):
                raise err(f"duplicate struct {sname!r}")
            struct_open = (sname, lineno, [])
            continue

        # A syscall line has '(' before any '='.
        paren = line.find("(")
        eq = line.find("=")
        if paren != -1 and (eq == -1 or paren < eq):
            m = SYSCALL_RE.match(line)
            if not m:
                raise err(f"malformed syscall declaration {line!r}")
            sc_name, variant, params_s, ret_s = (
                m.group(1), m.group(2) or "", m.group(3), (m.group(4) or "").strip(),
            )
            params = []
            if params_s.strip():
                try:
                    raw_params = _split_top(params_s, ",")
                except ValueError as e:
                    raise err(str(e))
                for p in raw_params:
                    p = p.strip()
                    parts = p.split(None, 1)
                    if len(parts) != 2:
                        raise err(f"parameter must be '<name> <type>', got {p!r}")
                    pname, ptype_s = parts
                    if not IDENT_RE.match(pname):
                        raise err(f"bad parameter name {pname!r}")
                    if any(q.name == pname for q in params):
                        raise err(f"duplicate parameter {pname!r}")
                    params.append(Param(pname, _parse_type(ptype_s, desc, err)))
            ret = _parse_type(ret_s, desc, err) if ret_s else None
            if ret is not None and ret.kind not in ("int", "resource", "fd"):
                raise err(f"return type must be an int type or resource, got {ret_s!r}")
            sc = Syscall(sc_name, variant, tuple(params), ret)
            if sc.full_name in desc.syscalls:
                raise err(f"duplicate syscall {sc.full_name!r}")
            desc.syscalls[sc.full_name] = sc
            continue

        m = FLAGSET_RE.match(line)
        if m:
            fname, values_s = m.group(1), m.group(2)
            values = [v.strip() for v in values_s.split(",")]
            if not values or any(not IDENT_RE.match(v) for v in values):
                raise err(f"flag set values must be C identifiers, got {values_s!r}")
            if fname in desc.flagsets:
                raise err(f"duplicate flag set {fname!r}")
            desc.flagsets[fname] = FlagSet(fname, tuple(values))
            continue

        raise err(f"unrecognized construct outside the supported subset: {line!r}")

    if struct_open is not None:
        raise SyzlangError(path, struct_open[1], f"unterminated struct {struct_open[0]!r}")

    # Resolve struct field types now that all names are known.
    for sname, open_lineno, fields in pending_structs:
        resolved = []
        for fname, ftype_s, flineno in fields:
            err = make_err(flineno)
            t = _parse_type(ftype_s, desc, err)
            if t.kind not in ("int", "struct", "const"):
                raise err(
                    f"struct field type must be int/const/struct, got {ftype_s!r}"
                )
            resolved.append(StructField(fname, t))
        desc.structs[sname] = StructDef(sname, tuple(resolved))

    _validate(desc)
    return desc


# ---------------------------------------------------------------------------
# Whole-description validation
# ---------------------------------------------------------------------------


def _validate(desc: Description) -> None:
    def check_type(t: TypeRef, ctx: str, param_names=()):
        if t.kind == "flags" and t.data["set"] not in desc.flagsets:
            raise SyzlangError(desc.path, 0, f"{ctx}: unknown flag set {t.data['set']!r}")
        if t.kind == "struct":
            if t.data["name"] not in desc.structs:
                raise SyzlangError(
                    desc.path, 0,
                    f"{ctx}: unknown type {t.data['name']!r} "
                    f"(not an int type, resource, or struct)",
                )
        if t.kind == "len" and t.data["target"] not in param_names:
            raise SyzlangError(
                desc.path, 0,
                f"{ctx}: len target {t.data['target']!r} is not a parameter",
            )
        if t.kind == "ptr":
            check_type(t.data["elem"], ctx, param_names)

    for s in desc.structs.values():
        for f in s.fields:
            check_type(f.type, f"struct {s.name}.{f.name}")
    for sc in desc.syscalls.values():
        names = tuple(p.name for p in sc.params)
        for p in sc.params:
            check_type(p.type, f"syscall {sc.full_name} arg {p.name}", names)
        if sc.ret is not None:
            check_type(sc.ret, f"syscall {sc.full_name} return")


# ---------------------------------------------------------------------------
# CLI: dump the AST as JSON for inspection / smoke-testing the parser
# ---------------------------------------------------------------------------


def _type_to_json(t: TypeRef):
    d = dict(t.data)
    if t.kind == "ptr":
        d["elem"] = _type_to_json(t.data["elem"])
    return {"kind": t.kind, **d}


def description_to_json(desc: Description) -> dict:
    return {
        "path": desc.path,
        "includes": list(desc.includes),
        "resources": {r.name: r.base for r in desc.resources.values()},
        "flagsets": {f.name: list(f.values) for f in desc.flagsets.values()},
        "structs": {
            s.name: [{"name": f.name, "type": _type_to_json(f.type)} for f in s.fields]
            for s in desc.structs.values()
        },
        "syscalls": {
            sc.full_name: {
                "syscall": sc.name,
                "params": [
                    {"name": p.name, "type": _type_to_json(p.type)} for p in sc.params
                ],
                "ret": _type_to_json(sc.ret) if sc.ret else None,
            }
            for sc in desc.syscalls.values()
        },
    }


def main(argv):
    if len(argv) < 2:
        print("usage: syzlang_parser.py <description.txt>...", file=sys.stderr)
        return 2
    out = {}
    for path in argv[1:]:
        try:
            desc = parse_file(path)
        except SyzlangError as e:
            print(f"syzlang_parser: error: {e}", file=sys.stderr)
            return 1
        out[path] = description_to_json(desc)
    json.dump(out, sys.stdout, indent=2, sort_keys=True)
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
