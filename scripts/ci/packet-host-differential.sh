#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
WORKDIR="$REPO_ROOT/.state/ci/packet-host-differential"
RUNS=2
MAX_LOG_BYTES=262144

usage() {
    cat <<'EOF'
Usage: scripts/ci/packet-host-differential.sh [OPTIONS]

Options:
  --workdir DIR  Bounded evidence directory (default: .state/ci/packet-host-differential)
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

for command in awk cat cc date find git grep id ip sed sh sha256sum stat timeout uname unshare; do
    if ! command -v "$command" >/dev/null 2>&1; then
        printf 'packet-host-differential: required command missing: %s\n' \
            "$command" >&2
        exit 1
    fi
done

case "$WORKDIR" in
    /*) ;;
    *) WORKDIR="$REPO_ROOT/$WORKDIR" ;;
esac
mkdir -p -- "$WORKDIR"
WORKDIR=$(cd -- "$WORKDIR" && pwd -P)
if [ "$WORKDIR" = / ]; then
    printf '%s\n' 'packet-host-differential: refusing root as workdir' >&2
    exit 2
fi

SOURCE="$REPO_ROOT/tests/guest/tools/packet-socket-smoke.c"
BINARY="$WORKDIR/packet-socket-smoke"
PREFLIGHT="$WORKDIR/preflight.log"
RECEIPT="$WORKDIR/receipt.tsv"
INPUT_CHECKSUMS="$WORKDIR/inputs.sha256"
ARTIFACT_CHECKSUMS="$WORKDIR/artifacts.sha256"

rm -f -- "$BINARY" "$PREFLIGHT" "$RECEIPT" "$INPUT_CHECKSUMS" \
    "$ARTIFACT_CHECKSUMS"
find "$WORKDIR" -maxdepth 1 -type f -name 'run-*.log' -delete

cc -O2 -std=c11 -Wall -Wextra -Werror "$SOURCE" -o "$BINARY"

set +e
# The single-quoted program is intentionally expanded by the namespace shell.
# shellcheck disable=SC2016
LC_ALL=C timeout --kill-after=2s 10s \
    unshare -Urn --map-root-user sh -c '
        set -eu
        ip link set lo up
        printf "uid=%s\n" "$(id -u)"
        printf "lo_ifindex=%s\n" "$(cat /sys/class/net/lo/ifindex)"
        grep -m1 "^CapEff:" /proc/self/status
    ' >"$PREFLIGHT" 2>&1
preflight_status=$?
set -e
if [ "$preflight_status" -ne 0 ]; then
    printf 'packet-host-differential: namespace/capability preflight failed: exit=%s\n' \
        "$preflight_status" >&2
    exit 1
fi
grep -Fqx 'uid=0' "$PREFLIGHT"
grep -Eq '^lo_ifindex=[1-9][0-9]*$' "$PREFLIGHT"
grep -Eq '^CapEff:[[:space:]]+[0-9a-fA-F]*[1-9a-fA-F][0-9a-fA-F]*$' "$PREFLIGHT"

required_markers=(
    THEKERNEL_PACKET_CREATE_OK
    THEKERNEL_PACKET_RECEIVE_OK
    THEKERNEL_PACKET_FAULT_OWNERSHIP_OK
    THEKERNEL_PACKET_SEND_OK
    THEKERNEL_PACKET_OPTIONS_OK
    THEKERNEL_PACKET_OK
)

for ((run = 1; run <= RUNS; ++run)); do
    log="$WORKDIR/run-$run.log"
    set +e
    # Positional $1 is intentionally expanded by the namespace shell.
    # shellcheck disable=SC2016
    LC_ALL=C timeout --kill-after=5s 45s \
        unshare -Urn --map-root-user sh -c '
            set -eu
            ip link set lo up
            exec "$1" --linux-host --require-options
        ' sh "$BINARY" >"$log" 2>&1
    run_status=$?
    set -e
    if [ "$run_status" -ne 0 ]; then
        printf 'packet-host-differential: run %s failed: exit=%s\n' \
            "$run" "$run_status" >&2
        exit 1
    fi
    log_size=$(stat -c '%s' "$log")
    if ((log_size > MAX_LOG_BYTES)); then
        printf 'packet-host-differential: run %s log exceeds bound: %s > %s\n' \
            "$run" "$log_size" "$MAX_LOG_BYTES" >&2
        exit 1
    fi
    for marker in "${required_markers[@]}"; do
        if [ "$(grep -Fxc "$marker" "$log" || true)" -ne 1 ]; then
            printf 'packet-host-differential: run %s marker mismatch: %s\n' \
                "$run" "$marker" >&2
            exit 1
        fi
    done
    if grep -Fq 'THEKERNEL_PACKET_FAIL' "$log"; then
        printf 'packet-host-differential: run %s reported a test failure\n' \
            "$run" >&2
        exit 1
    fi
    if [ "$(grep -c '^THEKERNEL_PACKET_SEND_BOUNDARY ' "$log" || true)" -ne 3 ] ||
        [ "$(grep -c '^THEKERNEL_PACKET_NAME_BOUNDARY ' "$log" || true)" -ne 1 ] ||
        [ "$(grep -c '^THEKERNEL_PACKET_OPTION_BOUNDARY ' "$log" || true)" -ne 1 ] ||
        [ "$(grep -c '^THEKERNEL_PACKET_CONTROL_BOUNDARY ' "$log" || true)" -ne 1 ]; then
        printf 'packet-host-differential: run %s boundary evidence incomplete\n' \
            "$run" >&2
        exit 1
    fi
done

source_sha=$(sha256sum "$SOURCE" | awk '{print $1}')
script_sha=$(sha256sum "${BASH_SOURCE[0]}" | awk '{print $1}')
binary_sha=$(sha256sum "$BINARY" | awk '{print $1}')
{
    printf '%s  %s\n' "$source_sha" 'tests/guest/tools/packet-socket-smoke.c'
    printf '%s  %s\n' "$script_sha" 'scripts/ci/packet-host-differential.sh'
} >"$INPUT_CHECKSUMS"

(
    cd -- "$WORKDIR"
    sha256sum packet-socket-smoke preflight.log run-*.log
) >"$ARTIFACT_CHECKSUMS"

repository_state=clean
if [ -n "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=all)" ]; then
    repository_state=dirty
fi
{
    printf 'schema\tpacket-host-differential-v1\n'
    printf 'status\tPASS\n'
    printf 'generated_utc\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'repository_head\t%s\n' "$(git -C "$REPO_ROOT" rev-parse HEAD)"
    printf 'repository_state\t%s\n' "$repository_state"
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

printf 'packet-host-differential: PASS runs=%s source_sha256=%s evidence=%s\n' \
    "$RUNS" "$source_sha" "$WORKDIR"
