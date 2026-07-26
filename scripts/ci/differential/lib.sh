#!/usr/bin/env bash

# Shared library for Linux-ABI differential host runners implementing the
# thekernel-differential contract v0. Callers must enable `set -euo pipefail`
# before sourcing this file. Functions are mechanism-only: they never print
# runner-branded diagnostics, so each runner keeps exact control over its
# user-visible messages and exit codes.
#
# Expected layout relative to the repository root:
#   scripts/ci/differential/lib.sh                    this file
#   scripts/ci/differential/manifests/<case>.markers  required marker set
#   scripts/ci/differential/allowlist/<case>.json     optional, empty by default
#   tests/guest/tools/<case>-smoke.c                  portable smoke program
#
# Runners need: bash, cc, git, grep, python3, sed, stat, timeout, uname.

DIFFERENTIAL_RECEIPT_SCHEMA=thekernel-differential-receipt-v0
DIFFERENTIAL_DEFAULT_CFLAGS=(-static -O2 -std=c11 -Wall -Wextra -Werror)

# Prints the absolute path of the scripts/ci/differential directory.
differential_lib_dir() {
    (cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
}

# Prints the repository root that contains this library.
differential_repo_root() {
    (cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
}

# Validates and prints the value of a `--workdir` option while parsing
# arguments. PROG is the runner name used in the diagnostic. Call with the
# remaining positional parameters starting at `--workdir` itself:
#   --workdir)
#       WORKDIR=$(differential_workdir_value "$prog" "$@") || exit $?
#       shift 2
#       ;;
differential_workdir_value() {
    local prog=$1
    shift
    if (($# < 2)) || [ -z "$2" ] || [[ "$2" == -* ]]; then
        printf '%s: --workdir requires a path\n' "$prog" >&2
        return 2
    fi
    printf '%s\n' "$2"
}

# Resolves WORKDIR (relative paths are anchored at REPO_ROOT), creates it,
# and prints the physical path.
differential_resolve_workdir() {
    local repo_root=$1
    local workdir=$2
    case "$workdir" in
        /*) ;;
        *) workdir="$repo_root/$workdir" ;;
    esac
    mkdir -p -- "$workdir"
    (cd -- "$workdir" && pwd -P)
}

# Builds a smoke helper: differential_build_smoke OUTPUT SOURCE [CFLAGS...].
# Without explicit CFLAGS the contract default set is used
# (-static -O2 -std=c11 -Wall -Wextra -Werror). Runners converted from the
# pre-library era pass their historical flag set explicitly so their build
# command stays byte-for-byte identical.
differential_build_smoke() {
    local output=$1
    local source=$2
    shift 2
    if (($#)); then
        cc "$@" "$source" -o "$output"
    else
        cc "${DIFFERENTIAL_DEFAULT_CFLAGS[@]}" "$source" -o "$output"
    fi
}

# Runs a command under `timeout`, capturing stdout+stderr into LOG:
#   differential_run_bounded LOG TIMEOUT KILL_AFTER -- COMMAND [ARGS...]
# Propagates the command's exit status (124 on timeout). Callers under
# errexit collect it as: status=0; differential_run_bounded ... || status=$?
differential_run_bounded() {
    local log=$1
    local timeout_spec=$2
    local kill_after=$3
    shift 3
    [ "${1:-}" != -- ] || shift
    timeout --kill-after="$kill_after" "$timeout_spec" "$@" >"$log" 2>&1
}

# Prints the byte size of LOG and fails when it exceeds MAX_BYTES.
differential_log_within_bound() {
    local log=$1
    local max_bytes=$2
    local size
    size=$(stat -c '%s' "$log")
    printf '%s\n' "$size"
    [ "$size" -le "$max_bytes" ]
}

# Prints the number of marker lines in MANIFEST. Blank lines and lines
# starting with '#' are ignored.
differential_manifest_count() {
    grep -c -v -e '^[[:space:]]*$' -e '^#' -- "$1" || true
}

# Prints every manifest marker missing from LOG, one per line, and returns 1
# when at least one is missing:
#   differential_missing_markers LOG MANIFEST [MODE]
# MODE "present" (default) requires each marker at least once (grep -Fqx);
# MODE "once" requires exactly one occurrence (grep -Fxc == 1).
differential_missing_markers() {
    local log=$1
    local manifest=$2
    local mode=${3:-present}
    local marker
    local count
    local missing=0
    case "$mode" in
        present|once) ;;
        *)
            printf 'differential: unknown manifest mode: %s\n' "$mode" >&2
            return 2
            ;;
    esac
    while IFS= read -r marker; do
        case "$marker" in
            ''|'#'*) continue ;;
        esac
        if [ "$mode" = present ]; then
            if grep -Fqx -- "$marker" "$log"; then
                continue
            fi
        else
            count=$(grep -Fxc -- "$marker" "$log" || true)
            if [ "$count" -eq 1 ]; then
                continue
            fi
        fi
        missing=1
        printf '%s\n' "$marker"
    done <"$manifest"
    [ "$missing" -eq 0 ]
}

# Applies an allowlist to the missing-marker list read from stdin:
#   differential_apply_allowlist ALLOWLIST_JSON KERNEL_RELEASE APPLIED_JSONL
# ALLOWLIST_JSON may be absent (the empty default). An entry waives a missing
# marker only when its kernel_range matches KERNEL_RELEASE; every application
# is appended to APPLIED_JSONL (one JSON object per line) for the receipt.
# Markers that remain unwaived are printed to stdout. Malformed allowlists
# are hard errors, never silent skips.
differential_apply_allowlist() {
    python3 - "$1" "$2" "$3" <<'PY'
import json
import re
import sys

allowlist_path, release, applied_path = sys.argv[1:4]
missing = [line for line in sys.stdin.read().splitlines() if line]


def release_tuple(text):
    match = re.match(r"(\d+)\.(\d+)(?:\.(\d+))?", text)
    if match is None:
        raise SystemExit(f"differential: unparseable kernel release: {text!r}")
    return tuple(int(part or 0) for part in match.groups())


def range_matches(spec, actual):
    operators = {
        ">=": lambda a, b: a >= b,
        "<=": lambda a, b: a <= b,
        "==": lambda a, b: a == b,
        ">": lambda a, b: a > b,
        "<": lambda a, b: a < b,
    }
    clauses = spec.split()
    if not clauses:
        raise SystemExit("differential: empty kernel_range in allowlist entry")
    for clause in clauses:
        match = re.fullmatch(r"(>=|<=|==|>|<)?(\d+(?:\.\d+){0,2})", clause)
        if match is None:
            raise SystemExit(
                f"differential: unparseable kernel_range clause: {clause!r}"
            )
        operator = match.group(1) or "=="
        if not operators[operator](actual, release_tuple(match.group(2))):
            return False
    return True


try:
    with open(allowlist_path, encoding="utf-8") as source:
        entries = json.load(source)
except FileNotFoundError:
    entries = []
if not isinstance(entries, list):
    raise SystemExit(f"differential: allowlist must be a JSON array: {allowlist_path}")

by_marker = {}
for entry in entries:
    if (
        not isinstance(entry, dict)
        or not isinstance(entry.get("marker"), str)
        or not isinstance(entry.get("kernel_range"), str)
        or not isinstance(entry.get("reason"), str)
        or not entry["marker"]
        or not entry["reason"]
    ):
        raise SystemExit(
            "differential: allowlist entries need non-empty marker/kernel_range/"
            f"reason strings: {allowlist_path}"
        )
    by_marker.setdefault(entry["marker"], []).append(entry)

actual = release_tuple(release)
with open(applied_path, "a", encoding="utf-8") as applied:
    for marker in missing:
        waived = False
        for entry in by_marker.get(marker, []):
            if range_matches(entry["kernel_range"], actual):
                record = {
                    "marker": marker,
                    "reason": entry["reason"],
                    "kernel_range": entry["kernel_range"],
                }
                applied.write(json.dumps(record, sort_keys=True) + "\n")
                waived = True
                break
        if not waived:
            print(marker)
PY
}

# Prints the number of distinct markers recorded in APPLIED_JSONL (0 when the
# file is absent or empty).
differential_applied_marker_count() {
    local applied_jsonl=$1
    if [ ! -s "$applied_jsonl" ]; then
        printf '0\n'
        return 0
    fi
    python3 - "$applied_jsonl" <<'PY'
import json
import sys

markers = set()
with open(sys.argv[1], encoding="utf-8") as source:
    for line in source:
        line = line.strip()
        if line:
            markers.add(json.loads(line)["marker"])
print(len(markers))
PY
}

# Writes RECEIPT per the thekernel-differential-receipt-v0 schema:
#   differential_write_receipt RECEIPT CASE REPO_ROOT EXPECTED MATCHED \
#       APPLIED_JSONL RESULT
# APPLIED_JSONL may be absent; duplicate applications are collapsed. RESULT
# must be "pass" or "fail".
differential_write_receipt() {
    local receipt=$1
    local case_name=$2
    local repo_root=$3
    local expected=$4
    local matched=$5
    local applied_jsonl=$6
    local result=$7
    local git_rev
    local kernel_release
    local version_line
    local cc_line
    git_rev=$(git -C "$repo_root" rev-parse HEAD)
    kernel_release=$(uname -r)
    version_line=$(sed -n 1p /proc/version)
    cc_line=$(cc --version | sed -n 1p)
    python3 - "$receipt" "$case_name" "$git_rev" "$kernel_release" \
        "$version_line" "$cc_line" "$expected" "$matched" "$applied_jsonl" \
        "$result" <<'PY'
import json
import os
import sys

(
    receipt,
    case_name,
    git_rev,
    release,
    version_line,
    cc_line,
    expected,
    matched,
    applied_path,
    result,
) = sys.argv[1:11]
if result not in ("pass", "fail"):
    raise SystemExit(f"differential: invalid receipt result: {result!r}")

applied = []
seen = set()
if applied_path and os.path.exists(applied_path):
    with open(applied_path, encoding="utf-8") as source:
        for line in source:
            line = line.strip()
            if not line:
                continue
            entry = json.loads(line)
            key = (entry["marker"], entry["kernel_range"], entry["reason"])
            if key in seen:
                continue
            seen.add(key)
            applied.append(entry)

document = {
    "schema": "thekernel-differential-receipt-v0",
    "case": case_name,
    "git_rev": git_rev,
    "reference": {
        "kind": "host-linux",
        "kernel_release": release,
        "kernel_version_line": version_line,
    },
    "toolchain": {"cc": cc_line},
    "markers_expected": int(expected),
    "markers_matched": int(matched),
    "allowlist_applied": applied,
    "result": result,
}
with open(receipt, "w", encoding="utf-8") as sink:
    json.dump(document, sink, indent=2)
    sink.write("\n")
PY
}
