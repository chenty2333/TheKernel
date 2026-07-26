#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=scripts/ci/differential/lib.sh
. "$SCRIPT_DIR/differential/lib.sh"
default_work_id="$(git -C "$REPO_ROOT" rev-parse --short=12 HEAD)-$(date -u +%Y%m%dT%H%M%SZ)-$$"
WORKDIR="$REPO_ROOT/.state/ci/packet-host-differential/$default_work_id"
RUNS=2
MAX_LOG_BYTES=262144

usage() {
    cat <<'EOF'
Usage: scripts/ci/packet-host-differential.sh [OPTIONS]

Options:
  --workdir DIR  Bounded evidence directory (default: unique run below .state/ci/packet-host-differential)
  --runs N       Independent Linux namespace runs, from 2 through 8 (default: 2)

Compiles the portable AF_PACKET smoke helper and runs it in fresh unprivileged
user and network namespaces. Lack of user namespaces, CAP_NET_RAW in the new
namespace, loopback setup, or a required Linux behavior is a hard failure. No
capability or environment skip is accepted by this differential gate.
EOF
}

while (($#)); do
    case "$1" in
        --workdir)
            if (($# < 2)) || [ -z "$2" ] || [[ "$2" == -* ]]; then
                printf '%s\n' 'packet-host-differential: --workdir requires a path' >&2
                exit 2
            fi
            WORKDIR=$2
            shift 2
            ;;
        --runs)
            if (($# < 2)) || ! [[ "$2" =~ ^[0-9]+$ ]]; then
                printf '%s\n' 'packet-host-differential: --runs requires an integer' >&2
                exit 2
            fi
            RUNS=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'packet-host-differential: unknown argument: %s\n' "$1" >&2
            exit 2
            ;;
    esac
done

if ((RUNS < 2 || RUNS > 8)); then
    printf 'packet-host-differential: --runs must be between 2 and 8, got %s\n' \
        "$RUNS" >&2
    exit 2
fi

for command in awk cat cc date env find git grep id ip python3 realpath sed sh sha256sum stat timeout uname unshare; do
    if ! command -v "$command" >/dev/null 2>&1; then
        printf 'packet-host-differential: required command missing: %s\n' \
            "$command" >&2
        exit 1
    fi
done

dirty_source=$(git -C "$REPO_ROOT" status --porcelain=v1 --untracked-files=all)
if [ -n "$dirty_source" ]; then
    printf '%s\n%s\n' \
        'packet-host-differential: refusing a dirty source checkout' \
        "$dirty_source" >&2
    exit 1
fi

source_head=$(git -C "$REPO_ROOT" rev-parse HEAD)
source_tree=$(git -C "$REPO_ROOT" rev-parse 'HEAD^{tree}')
SOURCE_REL=tests/guest/tools/packet-socket-smoke.c
SCRIPT_REL=scripts/ci/packet-host-differential.sh
LIB_REL=scripts/ci/differential/lib.sh
MANIFEST_REL=scripts/ci/differential/manifests/packet.markers
source_sha=$(git -C "$REPO_ROOT" show "$source_head:$SOURCE_REL" | sha256sum | awk '{print $1}')
script_sha=$(git -C "$REPO_ROOT" show "$source_head:$SCRIPT_REL" | sha256sum | awk '{print $1}')
lib_sha=$(git -C "$REPO_ROOT" show "$source_head:$LIB_REL" | sha256sum | awk '{print $1}')
manifest_sha=$(git -C "$REPO_ROOT" show "$source_head:$MANIFEST_REL" | sha256sum | awk '{print $1}')
[ "$(sha256sum "$REPO_ROOT/$SOURCE_REL" | awk '{print $1}')" = "$source_sha" ] || {
    printf '%s\n' 'packet-host-differential: helper differs from committed input' >&2
    exit 1
}
[ "$(sha256sum "${BASH_SOURCE[0]}" | awk '{print $1}')" = "$script_sha" ] || {
    printf '%s\n' 'packet-host-differential: gate script differs from committed input' >&2
    exit 1
}
[ "$(sha256sum "$REPO_ROOT/$LIB_REL" | awk '{print $1}')" = "$lib_sha" ] || {
    printf '%s\n' 'packet-host-differential: differential library differs from committed input' >&2
    exit 1
}
[ "$(sha256sum "$REPO_ROOT/$MANIFEST_REL" | awk '{print $1}')" = "$manifest_sha" ] || {
    printf '%s\n' 'packet-host-differential: marker manifest differs from committed input' >&2
    exit 1
}

case "$WORKDIR" in
    /*) ;;
    *) WORKDIR="$REPO_ROOT/$WORKDIR" ;;
esac
WORKDIR=$(realpath -m -- "$WORKDIR")
[ "$WORKDIR" != / ] || {
    printf '%s\n' 'packet-host-differential: refusing root as workdir' >&2
    exit 2
}
if [ -d "$WORKDIR" ] && find "$WORKDIR" -mindepth 1 -print -quit | grep -q .; then
    printf 'packet-host-differential: evidence directory is not empty: %s\n' \
        "$WORKDIR" >&2
    exit 1
fi
mkdir -p -- "$WORKDIR/input"
WORKDIR=$(cd -- "$WORKDIR" && pwd -P)

SOURCE="$WORKDIR/input/packet-socket-smoke.c"
FROZEN_SCRIPT="$WORKDIR/input/packet-host-differential.sh"
FROZEN_LIB="$WORKDIR/input/differential-lib.sh"
FROZEN_MANIFEST="$WORKDIR/input/packet.markers"
BINARY="$WORKDIR/packet-socket-smoke"
PREFLIGHT="$WORKDIR/preflight.log"
RECEIPT="$WORKDIR/receipt.tsv"
RECEIPT_JSON="$WORKDIR/receipt.json"
APPLIED="$WORKDIR/allowlist-applied.jsonl"
ALLOWLIST="$SCRIPT_DIR/differential/allowlist/packet.json"
SOURCE_SET="$WORKDIR/source-set.tsv"
INPUT_CHECKSUMS="$WORKDIR/inputs.sha256"
ARTIFACT_CHECKSUMS="$WORKDIR/artifacts.sha256"
BUNDLE_CHECKSUMS="$WORKDIR/bundle.sha256"

git -C "$REPO_ROOT" show "$source_head:$SOURCE_REL" >"$SOURCE"
git -C "$REPO_ROOT" show "$source_head:$SCRIPT_REL" >"$FROZEN_SCRIPT"
git -C "$REPO_ROOT" show "$source_head:$LIB_REL" >"$FROZEN_LIB"
git -C "$REPO_ROOT" show "$source_head:$MANIFEST_REL" >"$FROZEN_MANIFEST"
[ "$(sha256sum "$SOURCE" | awk '{print $1}')" = "$source_sha" ]
[ "$(sha256sum "$FROZEN_SCRIPT" | awk '{print $1}')" = "$script_sha" ]
[ "$(sha256sum "$FROZEN_LIB" | awk '{print $1}')" = "$lib_sha" ]
[ "$(sha256sum "$FROZEN_MANIFEST" | awk '{print $1}')" = "$manifest_sha" ]
{
    printf 'schema\tpacket-host-differential-source-set-v1\n'
    printf 'kind\tphase\tname\thead\ttree\tstate\tsha256\n'
    printf 'repository\tstart\tTheKernel\t%s\t%s\tclean\t-\n' \
        "$source_head" "$source_tree"
    printf 'input\tstart\t%s\t-\t-\tcommitted\t%s\n' "$SOURCE_REL" "$source_sha"
    printf 'input\tstart\t%s\t-\t-\tcommitted\t%s\n' "$SCRIPT_REL" "$script_sha"
    printf 'input\tstart\t%s\t-\t-\tcommitted\t%s\n' "$LIB_REL" "$lib_sha"
    printf 'input\tstart\t%s\t-\t-\tcommitted\t%s\n' "$MANIFEST_REL" "$manifest_sha"
} >"$SOURCE_SET"
{
    printf '%s  %s\n' "$source_sha" 'input/packet-socket-smoke.c'
    printf '%s  %s\n' "$script_sha" 'input/packet-host-differential.sh'
    printf '%s  %s\n' "$lib_sha" 'input/differential-lib.sh'
    printf '%s  %s\n' "$manifest_sha" 'input/packet.markers'
} >"$INPUT_CHECKSUMS"

differential_build_smoke "$BINARY" "$SOURCE" -O2 -std=c11 -Wall -Wextra -Werror

# The single-quoted program is intentionally expanded by the namespace shell.
# shellcheck disable=SC2016
preflight_status=0
differential_run_bounded "$PREFLIGHT" 10s 2s -- \
    env LC_ALL=C unshare -Urn --map-root-user sh -c '
        set -eu
        ip link set lo up
        printf "uid=%s\n" "$(id -u)"
        printf "lo_ifindex=%s\n" "$(cat /sys/class/net/lo/ifindex)"
        grep -m1 "^CapEff:" /proc/self/status
    ' || preflight_status=$?
if [ "$preflight_status" -ne 0 ]; then
    printf 'packet-host-differential: namespace/capability preflight failed: exit=%s\n' \
        "$preflight_status" >&2
    exit 1
fi
grep -Fqx 'uid=0' "$PREFLIGHT"
grep -Eq '^lo_ifindex=[1-9][0-9]*$' "$PREFLIGHT"
grep -Eq '^CapEff:[[:space:]]+[0-9a-fA-F]*[1-9a-fA-F][0-9a-fA-F]*$' "$PREFLIGHT"

markers_expected=$(differential_manifest_count "$FROZEN_MANIFEST")
kernel_release=$(uname -r)
send_flags_boundary=
send_flags_boundary+='THEKERNEL_PACKET_SEND_FLAGS_BOUNDARY '
send_flags_boundary+='accepted=OOB,MORE,DONTROUTE,EOR,CONFIRM,NOSIGNAL'

for ((run = 1; run <= RUNS; ++run)); do
    log="$WORKDIR/run-$run.log"
    # Positional $1 is intentionally expanded by the namespace shell.
    # shellcheck disable=SC2016
    run_status=0
    differential_run_bounded "$log" 45s 5s -- \
        env LC_ALL=C unshare -Urn --map-root-user sh -c '
            set -eu
            ip link set lo up
            exec "$1" --linux-host --require-options
        ' sh "$BINARY" || run_status=$?
    if [ "$run_status" -ne 0 ]; then
        printf 'packet-host-differential: run %s failed: exit=%s\n' \
            "$run" "$run_status" >&2
        exit 1
    fi
    log_size=$(differential_log_within_bound "$log" "$MAX_LOG_BYTES") || {
        printf 'packet-host-differential: run %s log exceeds bound: %s > %s\n' \
            "$run" "$log_size" "$MAX_LOG_BYTES" >&2
        exit 1
    }
    missing=$(differential_missing_markers "$log" "$FROZEN_MANIFEST" once || true)
    if [ -n "$missing" ]; then
        missing=$(printf '%s\n' "$missing" \
            | differential_apply_allowlist "$ALLOWLIST" "$kernel_release" "$APPLIED")
    fi
    if [ -n "$missing" ]; then
        printf 'packet-host-differential: run %s marker mismatch: %s\n' \
            "$run" "$(printf '%s\n' "$missing" | sed -n 1p)" >&2
        exit 1
    fi
    if grep -Fq 'THEKERNEL_PACKET_FAIL' "$log"; then
        printf 'packet-host-differential: run %s reported a test failure\n' \
            "$run" >&2
        exit 1
    fi
    if [ "$(grep -c '^THEKERNEL_PACKET_SEND_BOUNDARY ' "$log" || true)" -ne 3 ] ||
        [ "$(grep -c '^THEKERNEL_PACKET_SEND_FLAGS_BOUNDARY ' "$log" || true)" -ne 1 ] ||
        [ "$(grep -Fxc -- "$send_flags_boundary" "$log" || true)" -ne 1 ] ||
        [ "$(grep -c '^THEKERNEL_PACKET_NAME_BOUNDARY ' "$log" || true)" -ne 1 ] ||
        [ "$(grep -c '^THEKERNEL_PACKET_OPTION_BOUNDARY ' "$log" || true)" -ne 1 ] ||
        [ "$(grep -c '^THEKERNEL_PACKET_SOL_SOCKET_BOUNDARY ' "$log" || true)" -ne 1 ] ||
        [ "$(grep -c '^THEKERNEL_PACKET_ZERO_LENGTH_BOUNDARY ' "$log" || true)" -ne 1 ] ||
        [ "$(grep -c '^THEKERNEL_PACKET_CONTROL_BOUNDARY ' "$log" || true)" -ne 1 ]; then
        printf 'packet-host-differential: run %s boundary evidence incomplete\n' \
            "$run" >&2
        exit 1
    fi
done

binary_sha=$(sha256sum "$BINARY" | awk '{print $1}')

verify_final_source_identity() {
    final_head=$(git -C "$REPO_ROOT" rev-parse HEAD)
    final_tree=$(git -C "$REPO_ROOT" rev-parse 'HEAD^{tree}')
    final_source_sha=$(sha256sum "$REPO_ROOT/$SOURCE_REL" | awk '{print $1}')
    final_script_sha=$(sha256sum "${BASH_SOURCE[0]}" | awk '{print $1}')
    final_lib_sha=$(sha256sum "$REPO_ROOT/$LIB_REL" | awk '{print $1}')
    final_manifest_sha=$(sha256sum "$REPO_ROOT/$MANIFEST_REL" | awk '{print $1}')
    final_dirty=$(git -C "$REPO_ROOT" status --porcelain=v1 --untracked-files=all)
    if [ "$final_head" != "$source_head" ] || [ "$final_tree" != "$source_tree" ] ||
        [ "$final_source_sha" != "$source_sha" ] ||
        [ "$final_script_sha" != "$script_sha" ] ||
        [ "$final_lib_sha" != "$lib_sha" ] ||
        [ "$final_manifest_sha" != "$manifest_sha" ] || [ -n "$final_dirty" ]; then
        printf '%s\n' \
            'packet-host-differential: source identity or cleanliness changed during execution' >&2
        [ -z "$final_dirty" ] || printf '%s\n' "$final_dirty" >&2
        return 1
    fi
}

verify_final_source_identity
printf 'repository\tfinal\tTheKernel\t%s\t%s\tclean\t-\n' \
    "$final_head" "$final_tree" \
    >>"$SOURCE_SET"

(
    cd -- "$WORKDIR"
    sha256sum \
        input/packet-socket-smoke.c \
        input/packet-host-differential.sh \
        input/differential-lib.sh \
        input/packet.markers \
        packet-socket-smoke \
        preflight.log \
        run-*.log \
        source-set.tsv \
        inputs.sha256
) >"$ARTIFACT_CHECKSUMS"
artifact_checksums_sha=$(sha256sum "$ARTIFACT_CHECKSUMS" | awk '{print $1}')

# Hashing the bounded artifact set happens after the first final snapshot. Take
# one more source observation immediately before publishing the PASS receipt.
verify_final_source_identity
{
    printf 'schema\tpacket-host-differential-v2\n'
    printf 'status\tPASS\n'
    printf 'generated_utc\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'repository_head\t%s\n' "$source_head"
    printf 'repository_tree\t%s\n' "$source_tree"
    printf 'repository_state\tclean\n'
    printf 'source_set_revalidated\tPASS\n'
    printf 'artifact_checksums_sha256\t%s\n' "$artifact_checksums_sha"
    printf 'source_sha256\t%s\n' "$source_sha"
    printf 'script_sha256\t%s\n' "$script_sha"
    printf 'binary_sha256\t%s\n' "$binary_sha"
    printf 'host_kernel\t%s\n' "$(uname -srmo)"
    printf 'compiler\t%s\n' "$(cc --version | sed -n '1p')"
    printf 'namespace_mode\tunshare -Urn --map-root-user\n'
    printf 'helper_mode\t--linux-host --require-options\n'
    printf 'runs\t%s\n' "$RUNS"
    printf 'timeout_seconds_per_run\t45\n'
    printf 'max_log_bytes\t%s\n' "$MAX_LOG_BYTES"
    for ((run = 1; run <= RUNS; ++run)); do
        printf 'run_%s_sha256\t%s\n' "$run" \
            "$(sha256sum "$WORKDIR/run-$run.log" | awk '{print $1}')"
    done
} >"$RECEIPT"
waived_count=$(differential_applied_marker_count "$APPLIED")
differential_write_receipt "$RECEIPT_JSON" packet "$REPO_ROOT" \
    "$markers_expected" "$((markers_expected - waived_count))" "$APPLIED" pass
(
    cd -- "$WORKDIR"
    sha256sum receipt.tsv receipt.json artifacts.sha256
) >"$BUNDLE_CHECKSUMS"

printf 'packet-host-differential: PASS runs=%s source_sha256=%s evidence=%s\n' \
    "$RUNS" "$source_sha" "$WORKDIR"
