#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
PORTABLE_DIR="$REPO_ROOT/tests/guest/portable"
CC=${CC:-cc}

usage() {
    cat <<'EOF'
Usage: scripts/host-differential.sh [tests/guest/portable/<test>.c ...]

Build and run every portable guest C test against the host Linux kernel.  With
one or more paths, build and run only those existing portable C tests.
Each test returns 0 for PASS, 1 for FAIL, or 4 for SKIP.
EOF
}

if (($# == 1)); then
    case $1 in
        -h|--help) usage; exit 0 ;;
    esac
fi

command -v "$CC" >/dev/null 2>&1 || {
    printf 'host-differential: C compiler not found: %s\n' "$CC" >&2
    exit 1
}
command -v timeout >/dev/null 2>&1 || {
    printf 'host-differential: timeout command not found\n' >&2
    exit 1
}

shopt -s nullglob
all_sources=("$PORTABLE_DIR"/*.c)
if ((${#all_sources[@]} == 0)); then
    printf 'host-differential: no portable tests in %s\n' "$PORTABLE_DIR" >&2
    exit 1
fi

if (($# == 0)); then
    sources=("${all_sources[@]}")
else
    declare -A selected=()
    for requested in "$@"; do
        relative=${requested#./}
        case $relative in
            tests/guest/portable/*.c)
                filename=${relative#tests/guest/portable/}
                ;;
            *)
                printf 'host-differential: invalid portable test: %s\n' "$requested" >&2
                exit 2
                ;;
        esac

        # The selected path must name a direct child of portable/, which keeps
        # syntactic paths such as "../" from escaping the allowed directory.
        if [[ $filename == */* ]] || [[ -z $filename ]] || [[ ! -f $PORTABLE_DIR/$filename ]]; then
            printf 'host-differential: invalid portable test: %s\n' "$requested" >&2
            exit 2
        fi
        if [[ -v selected[$filename] ]]; then
            printf 'host-differential: duplicate portable test: %s\n' "$requested" >&2
            exit 2
        fi
        selected[$filename]=1
    done

    # Preserve the portable directory's deterministic glob order rather than
    # inheriting caller argument order.
    sources=()
    for source in "${all_sources[@]}"; do
        filename=${source##*/}
        if [[ -v selected[$filename] ]]; then
            sources+=("$source")
        fi
    done
fi

state_dir="$REPO_ROOT/.state/host-differential"
mkdir -p -- "$state_dir"
workdir=$(mktemp -d "$state_dir/run.XXXXXX")
trap 'rm -rf -- "$workdir"' EXIT

passes=0
failures=0
skips=0
printf 'KTAP version 1\n1..%d\n' "${#sources[@]}"

emit_diagnostics() {
    local name=$1
    local log=$2
    while IFS= read -r line || [ -n "$line" ]; do
        printf '# %s: %s\n' "$name" "$line"
    done <"$log"
}

run_portable() {
    local name=$1
    local binary=$2
    local log=$3

    case "$name" in
        packet-socket-smoke)
            local packet_status=0
            if ! command -v unshare >/dev/null 2>&1; then
                printf '%s\n' \
                    'host-differential: unshare is required for AF_PACKET capability isolation' \
                    >"$log"
                return 1
            fi
            if ! command -v ip >/dev/null 2>&1; then
                printf '%s\n' \
                    'host-differential: ip is required to initialize the isolated loopback device' \
                    >"$log"
                return 1
            fi
            # Mapping the caller to root in a fresh user namespace grants the
            # namespaced capabilities needed to create and exercise AF_PACKET
            # sockets without granting CAP_NET_RAW in the surrounding CI job.
            # Failure to establish that standard Linux isolation is a failed
            # oracle, not a permanent capability skip.
            THEKERNEL_PORTABLE_HOST=1 \
                timeout --kill-after=5s 60s \
                unshare --user --map-root-user --net sh -c '
                    set -eu
                    ip link set lo up
                    exec "$1"
                ' sh "$binary" >"$log" 2>&1 || packet_status=$?
            if [ "$packet_status" -eq 4 ]; then
                printf '%s\n' \
                    'host-differential: AF_PACKET capability unavailable inside isolated namespaces' \
                    >>"$log"
                return 1
            fi
            return "$packet_status"
            ;;
        io-uring-directio-differential)
            # Keep FIEMAP and O_DIRECT on the repository filesystem.  /tmp is
            # commonly tmpfs in CI and can turn these checks into observations
            # of filesystem fallback rather than Linux direct-I/O semantics.
            THEKERNEL_PORTABLE_HOST=1 \
            THEKERNEL_DIRECTIO_PATH="$workdir/io-uring-directio-fixture" \
                timeout --kill-after=5s 60s "$binary" >"$log" 2>&1
            ;;
        vfork-smoke)
            local exec_target
            local iteration
            exec_target=$(command -v busybox 2>/dev/null ||
                command -v sleep 2>/dev/null || true)
            if [ -z "$exec_target" ]; then
                printf '%s\n' \
                    'host-differential: busybox or sleep is required for the vfork exec oracle' \
                    >"$log"
                return 1
            fi
            : >"$log"
            for ((iteration = 1; iteration <= 200; ++iteration)); do
                if ! THEKERNEL_PORTABLE_HOST=1 \
                    THEKERNEL_VFORK_EXEC_TARGET="$exec_target" \
                    timeout --kill-after=5s 60s "$binary" >>"$log" 2>&1; then
                    printf 'host-differential: vfork iteration %d failed\n' \
                        "$iteration" >>"$log"
                    return 1
                fi
            done
            ;;
        *)
            THEKERNEL_PORTABLE_HOST=1 \
                timeout --kill-after=5s 60s "$binary" >"$log" 2>&1
            ;;
    esac
}

for index in "${!sources[@]}"; do
    source=${sources[$index]}
    name=${source##*/}
    name=${name%.c}
    binary="$workdir/$name"
    log="$workdir/$name.log"

    if ! "$CC" -O2 -std=c11 -Wall -Wextra -Werror -pthread "$source" \
        -o "$binary" >"$log" 2>&1; then
        failures=$((failures + 1))
        printf 'not ok %d - %s # compile failed\n' "$((index + 1))" "$name"
        emit_diagnostics "$name" "$log"
        continue
    fi

    status=0
    run_portable "$name" "$binary" "$log" || status=$?
    case $status in
        0)
            passes=$((passes + 1))
            printf 'ok %d - %s\n' "$((index + 1))" "$name"
            emit_diagnostics "$name" "$log"
            ;;
        4)
            skips=$((skips + 1))
            printf 'ok %d - %s # SKIP\n' "$((index + 1))" "$name"
            emit_diagnostics "$name" "$log"
            ;;
        *)
            failures=$((failures + 1))
            printf 'not ok %d - %s\n' "$((index + 1))" "$name"
            emit_diagnostics "$name" "$log"
            ;;
    esac
done

printf '# pass=%d fail=%d skip=%d\n' "$passes" "$failures" "$skips"
if ((failures != 0)); then
    exit 1
fi
if ((passes == 0)); then
    exit 4
fi
