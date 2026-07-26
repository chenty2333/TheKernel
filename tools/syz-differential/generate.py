#!/usr/bin/env python3
"""Generate a contract-conforming differential C smoke case from a parsed
syzlang-subset description plus a hand-written semantics annotation.

Usage:
    generate.py --description descriptions/eventfd.txt \
                --semantics semantics/eventfd.json \
                --out-dir generated

Emits generated/<name>-gen-smoke.c and generated/<name>-gen-smoke.markers.

Everything the annotation references is cross-validated against the parsed
description AST: syscall names and arities, flag-set membership of named flag
values, pointer args, resource threading via saved return values.  Output is
fully deterministic: no randomness, no timestamps, no environment influence.

Python 3 stdlib only.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys

import syzlang_parser as sp

IDENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
PATH_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)*$")
INT_RE = re.compile(r"^-?(0x[0-9a-fA-F]+|[0-9]+)$")
ERRNO_RE = re.compile(r"^E[A-Z0-9]+$")
MARKER_RE = re.compile(r"^[A-Z][A-Z0-9_]*$")
PREFIX_RE = re.compile(r"^[A-Z][A-Z0-9]*GEN$")
RET_RE = re.compile(r"^(==|>=)?(-?(0x[0-9a-fA-F]+|[0-9]+))$")
CMP_OPS = ("==", "!=", ">", ">=", "<", "<=")

# Local variable types the annotation may declare, mapped to C.
LOCAL_TYPES = {
    "u64": ("uint64_t", "scalar"),
    "i64": ("int64_t", "scalar"),
    "int": ("int", "scalar"),
    "itimerspec": ("struct itimerspec", "struct"),
    "timespec": ("struct timespec", "struct"),
}

BASE_INCLUDES = [
    "errno.h",
    "stdint.h",
    "stdio.h",
    "stdlib.h",
    "string.h",
    "sys/syscall.h",
    "unistd.h",
]


class GenError(Exception):
    pass


def die(ctx: str, msg: str):
    raise GenError(f"{ctx}: {msg}")


# ---------------------------------------------------------------------------
# Semantics validation against the AST
# ---------------------------------------------------------------------------


def render_arg(arg, param, ctx, desc, known_vars, locals_map):
    """Validate one annotation argument against the AST parameter and return
    the C expression for it."""
    if not isinstance(arg, str) or not arg:
        die(ctx, f"argument must be a non-empty string, got {arg!r}")
    t = param.type

    if arg.startswith("@"):
        name = arg[1:]
        if name not in known_vars:
            die(ctx, f"reference {arg!r} to a value never saved in this check")
        if t.kind not in ("resource", "fd", "int", "intrange"):
            die(ctx, f"saved value {arg!r} passed for non-scalar param "
                     f"{param.name!r} ({t.kind})")
        return name

    if arg.startswith("&"):
        name = arg[1:]
        if name not in locals_map:
            die(ctx, f"address-of {arg!r} refers to an undeclared local")
        if t.kind not in ("ptr", "buffer"):
            die(ctx, f"pointer argument {arg!r} passed for non-pointer param "
                     f"{param.name!r} ({t.kind})")
        return f"&{name}"

    if INT_RE.match(arg):
        # Raw integers are always allowed: they are how negative tests pass
        # out-of-range values (bad flags, bad fds, bad lengths).
        if t.kind in ("ptr", "buffer") and int(arg, 0) != 0:
            die(ctx, f"only 0 (NULL) integer allowed for pointer param "
                     f"{param.name!r}, got {arg!r}")
        if t.kind == "ptr" and int(arg, 0) == 0:
            return "(void *)0"
        return arg

    # Named-constant expression: A|B|C, only meaningful for flags[] params.
    toks = [x.strip() for x in arg.split("|")]
    if any(not IDENT_RE.match(x) for x in toks):
        die(ctx, f"unsupported argument syntax {arg!r}")
    if t.kind != "flags":
        die(ctx, f"named constant(s) {arg!r} used for param {param.name!r} "
                 f"which is not a flags[] type ({t.kind})")
    fset = desc.flagsets[t.data["set"]]
    for tok in toks:
        if tok not in fset.values:
            die(ctx, f"flag {tok!r} is not in flag set {fset.name!r} "
                     f"(allowed: {', '.join(fset.values)})")
    return " | ".join(toks)


def parse_ret_expect(expect, ctx):
    """Return (op, value_str, errno_name_or_None)."""
    if not isinstance(expect, dict) or "ret" not in expect:
        die(ctx, "every call step needs an \"expect\" with \"ret\"")
    unknown = set(expect) - {"ret", "errno"}
    if unknown:
        die(ctx, f"unknown expect keys {sorted(unknown)}")
    ret = expect["ret"]
    m = RET_RE.match(ret) if isinstance(ret, str) else None
    if not m:
        die(ctx, f"expect.ret must be like \"0\", \"==8\", \">=0\", \"-1\"; "
                 f"got {ret!r}")
    op = m.group(1) or "=="
    val = m.group(2)
    err = expect.get("errno")
    if err is not None:
        if not (isinstance(err, str) and ERRNO_RE.match(err)):
            die(ctx, f"expect.errno must be an E* constant name, got {err!r}")
        if not (op == "==" and int(val, 0) == -1):
            die(ctx, "expect.errno requires expect.ret == \"-1\"")
    return op, val, err


# ---------------------------------------------------------------------------
# C emission
# ---------------------------------------------------------------------------


def emit_check(check, idx, desc, prefix, out):
    ctx0 = f"check[{idx}]"
    marker = check.get("marker")
    if not (isinstance(marker, str) and MARKER_RE.match(marker)):
        die(ctx0, f"marker must match [A-Z][A-Z0-9_]*, got {marker!r}")
    ctx0 = f"check {marker}"
    unknown = set(check) - {"marker", "locals", "steps", "comment"}
    if unknown:
        die(ctx0, f"unknown check keys {sorted(unknown)}")

    locals_map = check.get("locals", {})
    if not isinstance(locals_map, dict):
        die(ctx0, "locals must be an object")
    for lname, ltype in locals_map.items():
        if not IDENT_RE.match(lname):
            die(ctx0, f"bad local name {lname!r}")
        if ltype not in LOCAL_TYPES:
            die(ctx0, f"unknown local type {ltype!r} "
                      f"(allowed: {', '.join(sorted(LOCAL_TYPES))})")

    steps = check.get("steps")
    if not isinstance(steps, list) or not steps:
        die(ctx0, "steps must be a non-empty array")

    fn = f"check_{marker.lower()}"
    lines = [f"static void {fn}(void) {{"]
    lines.append("    long ret;")

    # Pre-scan for saved return values so they can be declared up front.
    saved = []
    for step in steps:
        if isinstance(step, dict) and "save" in step:
            sname = step["save"]
            if not (isinstance(sname, str) and IDENT_RE.match(sname)):
                die(ctx0, f"bad save name {sname!r}")
            if sname in locals_map or sname in saved:
                die(ctx0, f"save name {sname!r} collides with another variable")
            saved.append(sname)
    for sname in saved:
        lines.append(f"    long {sname} = -1;")
    for lname, ltype in locals_map.items():
        ctype, kind = LOCAL_TYPES[ltype]
        if kind == "scalar":
            lines.append(f"    {ctype} {lname} = 0;")
        else:
            lines.append(f"    {ctype} {lname};")
    for lname, ltype in locals_map.items():
        if LOCAL_TYPES[ltype][1] == "struct":
            lines.append(f"    memset(&{lname}, 0, sizeof({lname}));")
    lines.append("")

    known_vars = set()
    for sidx, step in enumerate(steps, 1):
        ctx = f"{ctx0} step {sidx}"
        if not isinstance(step, dict):
            die(ctx, "each step must be an object")
        kind_keys = [k for k in ("call", "set", "expect") if k in step]
        if "call" in step:
            kind = "call"
        elif "set" in step:
            kind = "set"
        elif kind_keys == ["expect"]:
            kind = "expect_var"
        else:
            die(ctx, f"step must be a call/set/expect step, got keys "
                     f"{sorted(step)}")

        if kind == "set":
            unknown = set(step) - {"set", "value", "comment"}
            if unknown:
                die(ctx, f"unknown set-step keys {sorted(unknown)}")
            path, value = step["set"], step.get("value")
            if not (isinstance(path, str) and PATH_RE.match(path)):
                die(ctx, f"set path must be var(.field)*, got {path!r}")
            root = path.split(".")[0]
            if root not in locals_map:
                die(ctx, f"set path root {root!r} is not a declared local")
            if not isinstance(value, int):
                die(ctx, f"set value must be an integer, got {value!r}")
            lines.append(f"    {path} = {value};")
            continue

        if kind == "expect_var":
            unknown = set(step) - {"expect", "comment"}
            if unknown:
                die(ctx, f"unknown expect-step keys {sorted(unknown)}")
            e = step["expect"]
            if not isinstance(e, dict) or set(e) - {"var", "cmp", "value"}:
                die(ctx, "expect step needs exactly var/cmp/value")
            var, cmp_op, value = e.get("var"), e.get("cmp"), e.get("value")
            if not (isinstance(var, str) and PATH_RE.match(var)):
                die(ctx, f"expect var must be var(.field)*, got {var!r}")
            root = var.split(".")[0]
            if root not in locals_map and root not in known_vars:
                die(ctx, f"expect var root {root!r} is neither a local nor a "
                         f"saved value")
            if cmp_op not in CMP_OPS:
                die(ctx, f"cmp must be one of {CMP_OPS}, got {cmp_op!r}")
            if not isinstance(value, int):
                die(ctx, f"expect value must be an integer, got {value!r}")
            stage = f"{marker.lower()}:s{sidx}:expect_{var.replace('.', '_')}"
            lines.append(
                f"    if (!((long)({var}) {cmp_op} {value}L))\n"
                f"        gen_fail(\"{stage}\", (long)({var}), {value}L, 0);"
            )
            continue

        # call step
        unknown = set(step) - {"call", "args", "save", "expect", "comment"}
        if unknown:
            die(ctx, f"unknown call-step keys {sorted(unknown)}")
        call = step["call"]
        if call not in desc.syscalls:
            die(ctx, f"syscall {call!r} is not declared in {desc.path} "
                     f"(declared: {', '.join(sorted(desc.syscalls))})")
        sc = desc.syscalls[call]
        args = step.get("args", [])
        if not isinstance(args, list):
            die(ctx, "args must be an array of strings")
        if len(args) != len(sc.params):
            die(ctx, f"{call} takes {len(sc.params)} args per the "
                     f"description, annotation supplies {len(args)}")
        c_args = [
            render_arg(a, p, f"{ctx} arg {p.name!r}", desc, known_vars,
                       locals_map)
            for a, p in zip(args, sc.params)
        ]
        op, val, errno_name = parse_ret_expect(step.get("expect"), ctx)
        stage = f"{marker.lower()}:s{sidx}:{sc.name}"
        arg_str = "".join(f", {a}" for a in c_args)
        lines.append("    errno = 0;")
        lines.append(f"    ret = syscall(SYS_{sc.name}{arg_str});")
        cond = f"ret {'==' if op == '==' else '>='} {val}L"
        lines.append(f"    if (!({cond}))")
        lines.append(f"        gen_fail(\"{stage}\", ret, {val}L, errno);")
        if errno_name is not None:
            lines.append(f"    if (errno != {errno_name})")
            lines.append(
                f"        gen_fail(\"{stage}:errno\", errno, {errno_name}, "
                f"errno);"
            )
        if "save" in step:
            lines.append(f"    {step['save']} = ret;")
            known_vars.add(step["save"])
        lines.append("")

    while lines and lines[-1] == "":
        lines.pop()
    lines.append("}")
    out.append("\n".join(lines))
    return fn, f"THEKERNEL_{prefix}_{marker}_OK"


def generate(desc, sem, sem_path):
    if sem.get("schema") != "syz-differential-semantics-v0":
        die(sem_path, f"unsupported schema {sem.get('schema')!r}")
    unknown = set(sem) - {"schema", "name", "marker_prefix", "c_includes",
                          "checks"}
    if unknown:
        die(sem_path, f"unknown top-level keys {sorted(unknown)}")
    name = sem.get("name")
    if not (isinstance(name, str) and re.match(r"^[a-z][a-z0-9_]*$", name)):
        die(sem_path, f"bad name {name!r}")
    prefix = sem.get("marker_prefix")
    if not (isinstance(prefix, str) and PREFIX_RE.match(prefix)):
        die(sem_path, f"marker_prefix must match [A-Z][A-Z0-9]*GEN, "
                      f"got {prefix!r}")
    if prefix != name.upper().replace("_", "") + "GEN":
        die(sem_path, f"marker_prefix {prefix!r} must be "
                      f"{name.upper().replace('_', '')}GEN")
    includes = sem.get("c_includes", [])
    if not isinstance(includes, list) or any(
        not re.match(r"^[A-Za-z0-9_./-]+\.h$", i) for i in includes
    ):
        die(sem_path, "c_includes must be a list of header paths")
    checks = sem.get("checks")
    if not isinstance(checks, list) or not checks:
        die(sem_path, "checks must be a non-empty array")

    bodies = []
    fns = []
    markers = []
    seen_markers = set()
    for idx, check in enumerate(checks):
        fn, marker_line = emit_check(check, idx, desc, prefix, bodies)
        if marker_line in seen_markers:
            die(sem_path, f"duplicate marker {marker_line}")
        seen_markers.add(marker_line)
        fns.append((fn, marker_line))
        markers.append(marker_line)
    final_marker = f"THEKERNEL_{prefix}_OK"
    markers.append(final_marker)

    hdr_lines = "\n".join(f"#include <{h}>" for h in BASE_INCLUDES + includes)
    fail_fn = (
        "static void gen_fail(const char *stage, long actual, long expected,\n"
        "                     int err) {\n"
        f"    fprintf(stderr,\n"
        f"            \"THEKERNEL_{prefix}_FAIL %s actual=%ld expected=%ld \"\n"
        f"            \"errno=%d (%s)\\n\",\n"
        "            stage, actual, expected, err, strerror(err));\n"
        "    exit(EXIT_FAILURE);\n"
        "}"
    )
    main_lines = ["int main(void) {"]
    for fn, marker_line in fns:
        main_lines.append(f"    {fn}();")
        main_lines.append(f"    printf(\"{marker_line}\\n\");")
    main_lines.append(f"    printf(\"{final_marker}\\n\");")
    main_lines.append("    return 0;")
    main_lines.append("}")

    c_text = "\n\n".join(
        [
            "/*\n"
            f" * Generated by tools/syz-differential/generate.py.  DO NOT EDIT.\n"
            f" *\n"
            f" * Inputs:\n"
            f" *   description: {os.path.basename(desc.path)}\n"
            f" *   semantics:   {os.path.basename(sem_path)}\n"
            f" *\n"
            f" * Deterministic output: no timestamps, no randomness.\n"
            f" * Follows the differential-case contract v0 marker/fail protocol.\n"
            " */\n"
            "#define _GNU_SOURCE\n\n" + hdr_lines,
            fail_fn,
        ]
        + bodies
        + ["\n".join(main_lines)]
    ) + "\n"
    return name, c_text, "\n".join(markers) + "\n"


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--description", required=True)
    ap.add_argument("--semantics", required=True)
    ap.add_argument("--out-dir", required=True)
    args = ap.parse_args(argv[1:])

    try:
        desc = sp.parse_file(args.description)
    except sp.SyzlangError as e:
        print(f"generate: description error: {e}", file=sys.stderr)
        return 1
    try:
        with open(args.semantics, "r", encoding="utf-8") as f:
            sem = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        print(f"generate: cannot load semantics: {e}", file=sys.stderr)
        return 1

    try:
        name, c_text, markers_text = generate(desc, sem, args.semantics)
    except GenError as e:
        print(f"generate: semantics error: {e}", file=sys.stderr)
        return 1

    os.makedirs(args.out_dir, exist_ok=True)
    c_path = os.path.join(args.out_dir, f"{name}-gen-smoke.c")
    m_path = os.path.join(args.out_dir, f"{name}-gen-smoke.markers")
    with open(c_path, "w", encoding="utf-8") as f:
        f.write(c_text)
    with open(m_path, "w", encoding="utf-8") as f:
        f.write(markers_text)
    n_checks = markers_text.count("\n") - 1
    print(f"generate: wrote {c_path} ({n_checks} checks) and {m_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
