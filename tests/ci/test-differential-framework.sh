#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
# shellcheck source=scripts/ci/differential/lib.sh
source "$REPO_ROOT/scripts/ci/differential/lib.sh"

tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT

unwaived_marker=THEKERNEL_DIFFERENTIAL_UNWAIVED_OK
waived_marker=THEKERNEL_DIFFERENTIAL_WAIVED_OK
applied="$tmp/applied.jsonl"

# No allowlist file is the normal strict path. The input marker must survive
# the Python program handoff and remain visible to the caller.
actual=$(printf '%s\n' "$unwaived_marker" |
    differential_apply_allowlist "$tmp/absent.json" 6.8.12 "$applied")
[ "$actual" = "$unwaived_marker" ]
[ ! -s "$applied" ]

# A matching waiver suppresses only its exact marker and records the
# application, while an unrelated marker remains a hard mismatch.
cat >"$tmp/allowlist.json" <<'JSON'
[
  {
    "marker": "THEKERNEL_DIFFERENTIAL_WAIVED_OK",
    "kernel_range": ">=6.8 <6.9",
    "reason": "fixture waiver proving the bounded range path"
  }
]
JSON
actual=$(printf '%s\n%s\n' "$waived_marker" "$unwaived_marker" |
    differential_apply_allowlist "$tmp/allowlist.json" 6.8.12 "$applied")
[ "$actual" = "$unwaived_marker" ]
[ "$(differential_applied_marker_count "$applied")" -eq 1 ]
python3 - "$applied" "$waived_marker" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    records = [json.loads(line) for line in source if line.strip()]
assert records == [
    {
        "kernel_range": ">=6.8 <6.9",
        "marker": sys.argv[2],
        "reason": "fixture waiver proving the bounded range path",
    }
]
PY

# A non-matching kernel range is inert and must not be reported as applied.
: >"$applied"
actual=$(printf '%s\n' "$waived_marker" |
    differential_apply_allowlist "$tmp/allowlist.json" 6.9.0 "$applied")
[ "$actual" = "$waived_marker" ]
[ ! -s "$applied" ]

# The guest epoll capability branch may differ only at the one lower-layer
# mechanism that is explicitly unsupported. All other Linux-observable
# markers stay shared and strict.
python3 - \
    "$REPO_ROOT/scripts/ci/differential/manifests/epoll.markers" \
    "$REPO_ROOT/scripts/ci/differential/manifests/epoll-guest.markers" <<'PY'
import sys


def markers(path):
    with open(path, encoding="utf-8") as source:
        return {line.strip() for line in source if line.strip()}


host = markers(sys.argv[1])
guest = markers(sys.argv[2])
assert host - guest == {"THEKERNEL_EPOLL_EXCLUSIVE_OK"}
assert guest - host == {"THEKERNEL_EPOLL_EXCLUSIVE_UNSUPPORTED_OK"}
PY

printf '%s\n' 'test-differential-framework: PASS'
