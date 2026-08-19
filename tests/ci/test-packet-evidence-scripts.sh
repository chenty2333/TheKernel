#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
CI_DIR="$REPO_ROOT/scripts/ci"
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT

commit_fixture() {
    local repo=$1
    git -C "$repo" init --quiet
    git -C "$repo" add .
    git -C "$repo" -c user.name=CI -c user.email=ci@example.invalid \
        commit --quiet -m fixture
}

# Exercise the host oracle without requiring namespace support in this script
# test. The fake compiler emits the exact bounded marker protocol, while the
# fake unshare distinguishes the preflight from helper runs.
host_repo="$tmp/host-repo"
mkdir -p "$host_repo/scripts/ci" "$host_repo/tests/guest/tools" "$tmp/host-bin"
cp "$CI_DIR/packet-host-differential.sh" "$host_repo/scripts/ci/"
mkdir -p "$host_repo/scripts/ci/differential/manifests"
cp "$CI_DIR/differential/lib.sh" "$host_repo/scripts/ci/differential/"
cp "$CI_DIR/differential/manifests/packet.markers" \
    "$host_repo/scripts/ci/differential/manifests/"
printf '%s\n' 'int main(void) { return 0; }' \
    >"$host_repo/tests/guest/tools/packet-socket-smoke.c"
commit_fixture "$host_repo"
cat >"$tmp/host-bin/cc" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = --version ]; then
    printf '%s\n' 'fixture cc 1.0'
    exit 0
fi
output=
while (($#)); do
    case "$1" in
        -o) output=${2:-}; shift 2 ;;
        *) shift ;;
    esac
done
[ -n "$output" ]
cat >"$output" <<'PROGRAM'
#!/usr/bin/env sh
set -eu
[ -z "${MUTATE_SOURCE:-}" ] || printf '%s\n' mutation >>"$MUTATE_SOURCE"
printf '%s\n' \
    THEKERNEL_PACKET_UDP_PRECONDITION_OK \
    THEKERNEL_PACKET_CREATE_OK \
    THEKERNEL_PACKET_RECEIVE_OK \
    THEKERNEL_PACKET_FAULT_OWNERSHIP_OK \
    'THEKERNEL_PACKET_SEND_BOUNDARY case=one' \
    'THEKERNEL_PACKET_SEND_BOUNDARY case=two' \
    'THEKERNEL_PACKET_SEND_BOUNDARY case=three' \
    'THEKERNEL_PACKET_SEND_FLAGS_BOUNDARY accepted=OOB,MORE,DONTROUTE,EOR,CONFIRM,NOSIGNAL' \
    THEKERNEL_PACKET_SEND_FLAGS_OK \
    THEKERNEL_PACKET_SEND_OK \
    'THEKERNEL_PACKET_NAME_BOUNDARY fixture' \
    'THEKERNEL_PACKET_OPTION_BOUNDARY fixture' \
    'THEKERNEL_PACKET_SOL_SOCKET_BOUNDARY fixture' \
    'THEKERNEL_PACKET_ZERO_LENGTH_BOUNDARY fixture' \
    'THEKERNEL_PACKET_CONTROL_BOUNDARY fixture' \
    THEKERNEL_PACKET_OPTIONS_OK \
    THEKERNEL_PACKET_CBPF_METADATA_OK \
    THEKERNEL_PACKET_CBPF_OK \
    THEKERNEL_PACKET_OK
PROGRAM
chmod +x "$output"
EOF
cat >"$tmp/host-bin/unshare" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if (($# >= 7)); then
    helper=${!#}
    exec "$helper"
fi
printf '%s\n' 'uid=0' 'lo_ifindex=1' $'CapEff:\t0000000000002000'
EOF
cat >"$tmp/host-bin/ip" <<'EOF'
#!/usr/bin/env sh
exit 0
EOF
chmod +x "$tmp/host-bin/cc" "$tmp/host-bin/unshare" "$tmp/host-bin/ip"

PATH="$tmp/host-bin:$PATH" "$host_repo/scripts/ci/packet-host-differential.sh" \
    --workdir "$tmp/host-pass" >/dev/null
grep -Fqx $'status\tPASS' "$tmp/host-pass/receipt.tsv"
grep -Fqx $'source_set_revalidated\tPASS' "$tmp/host-pass/receipt.tsv"
python3 "$CI_DIR/differential/validate-receipt.py" \
    --receipt "$tmp/host-pass/receipt.json" \
    --case packet \
    --manifest "$CI_DIR/differential/manifests/packet.markers" \
    --require-empty-allowlist --require-pass >/dev/null
(cd "$tmp/host-pass" && sha256sum -c artifacts.sha256 >/dev/null)
(cd "$tmp/host-pass" && sha256sum -c bundle.sha256 >/dev/null)

set +e
PATH="$tmp/host-bin:$PATH" \
    MUTATE_SOURCE="$host_repo/tests/guest/tools/packet-socket-smoke.c" \
    "$host_repo/scripts/ci/packet-host-differential.sh" \
    --workdir "$tmp/host-mutation" >"$tmp/host-mutation.out" 2>&1
status=$?
set -e
[ "$status" -eq 1 ]
grep -Fq 'source identity or cleanliness changed during execution' \
    "$tmp/host-mutation.out"
[ ! -e "$tmp/host-mutation/receipt.tsv" ]
[ ! -e "$tmp/host-mutation/receipt.json" ]

# The broker harness is driven with deterministic schema-2 output. Its second
# run can mutate one maintained sibling, proving the final three-repository
# HEAD/tree/clean check is active rather than decorative receipt text.
broker_repo="$tmp/broker-repo"
ax_repo="$tmp/ax-repo"
abi_repo="$tmp/abi-repo"
mkdir -p "$broker_repo/scripts/ci" "$ax_repo" "$abi_repo"
cp "$CI_DIR/packet-broker-performance.sh" "$CI_DIR/exact-source-lib.sh" \
    "$broker_repo/scripts/ci/"
printf '%s\n' main >"$broker_repo/source"
printf '%s\n' ax >"$ax_repo/source"
printf '%s\n' abi >"$abi_repo/source"
cat >"$broker_repo/scripts/ci/focused-cargo-test.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
run=${THEKERNEL_PACKET_BROKER_PERF_RUN:?}
for spec in \
    'zero_subscriber 0 0' \
    'single_subscriber 1 10' \
    'multi_subscriber 8 80' \
    'saturation_accounting 1 10' \
    'concurrent_pipeline 1 10'
do
    read -r case_name subscribers events <<<"$spec"
    printf 'THEKERNEL_PACKET_BROKER_PERF schema=2 run=%s case=%s subscribers=%s count=10 elapsed_ns=100 throughput_per_sec=100000 latency_scope=fixture p50_ns=1 p99_ns=2 p999_ns=3 expected_events=%s packet_events=%s received=%s stage_errors=0 drops=0 unattributed_drops=0 charged_shared_bytes=0 invariant=ok\n' \
        "$run" "$case_name" "$subscribers" "$events" "$events" "$events"
done
printf '%s\n' 'THEKERNEL_PACKET_BROKER_PERF_OK schema=2 cases=5'
case "${BROKER_BAD_OUTPUT:-}" in
    duplicate-ok)
        printf '%s\n' 'THEKERNEL_PACKET_BROKER_PERF_OK schema=2 cases=5'
        ;;
    failure)
        printf '%s\n' 'test result: FAILED. 4 passed; 1 failed'
        ;;
    panic)
        printf '%s\n' "thread 'fixture' panicked at fixture"
        ;;
esac
if [ "$run" = 2 ] && [ -n "${MUTATE_RESTORE_FILE:-}" ]; then
    backup=$(mktemp)
    cp -- "$MUTATE_RESTORE_FILE" "$backup"
    printf '%s\n' mutation >"$MUTATE_RESTORE_FILE"
    execution_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
    [ "$(cat "$execution_root/source")" = main ]
    cp -- "$backup" "$MUTATE_RESTORE_FILE"
    rm -f -- "$backup"
fi
if [ "$run" = 2 ] && [ -n "${MUTATE_REPO_FILE:-}" ]; then
    printf '%s\n' mutation >>"$MUTATE_REPO_FILE"
fi
EOF
chmod +x "$broker_repo/scripts/ci/focused-cargo-test.sh"
commit_fixture "$broker_repo"
commit_fixture "$ax_repo"
commit_fixture "$abi_repo"

THEKERNEL_AX_REPO="$ax_repo" THEKERNEL_LINUX_ABI_REPO="$abi_repo" \
    "$broker_repo/scripts/ci/packet-broker-performance.sh" \
    --workdir "$tmp/broker-pass" >/dev/null
grep -Fqx $'result\tPASS' "$tmp/broker-pass/receipt.tsv"
grep -Fqx $'source_set_revalidated\tPASS' "$tmp/broker-pass/receipt.tsv"
grep -Fqx $'result\tPASS' "$tmp/broker-pass/gate-envelope.tsv"
grep -Fqx $'origin_source_revalidated\tPASS' \
    "$tmp/broker-pass/gate-envelope.tsv"
[ "$(grep -c $'^final\t.*\tclean$' "$tmp/broker-pass/source-set.tsv")" -eq 3 ]
(cd "$tmp/broker-pass" && sha256sum -c checksums.sha256 >/dev/null)

# A mutate-read-restore race against the developer worktree cannot affect the
# committed clone consumed by the harness.
THEKERNEL_AX_REPO="$ax_repo" THEKERNEL_LINUX_ABI_REPO="$abi_repo" \
    MUTATE_RESTORE_FILE="$broker_repo/source" \
    "$broker_repo/scripts/ci/packet-broker-performance.sh" \
    --workdir "$tmp/broker-mutate-restore" >/dev/null
grep -Fqx $'result\tPASS' "$tmp/broker-mutate-restore/receipt.tsv"
[ "$(cat "$broker_repo/source")" = main ]

for bad_output in duplicate-ok failure panic; do
    set +e
    THEKERNEL_AX_REPO="$ax_repo" THEKERNEL_LINUX_ABI_REPO="$abi_repo" \
        BROKER_BAD_OUTPUT="$bad_output" \
        "$broker_repo/scripts/ci/packet-broker-performance.sh" \
        --workdir "$tmp/broker-$bad_output" >/dev/null 2>&1
    status=$?
    set -e
    [ "$status" -eq 1 ]
    [ ! -e "$tmp/broker-$bad_output/receipt.tsv" ]
done

set +e
THEKERNEL_AX_REPO="$ax_repo" THEKERNEL_LINUX_ABI_REPO="$abi_repo" \
    MUTATE_REPO_FILE="$abi_repo/source" \
    "$broker_repo/scripts/ci/packet-broker-performance.sh" \
    --workdir "$tmp/broker-mutation" >"$tmp/broker-mutation.out" 2>&1
status=$?
set -e
[ "$status" -eq 1 ]
grep -Fq 'dirty thekernel-linux-abi source' "$tmp/broker-mutation.out"
if grep -Eq '^packet-broker-performance: PASS( |$)' \
    "$tmp/broker-mutation.out"; then
    printf '%s\n' 'test-packet-evidence-scripts: inner broker gate published PASS' >&2
    exit 1
fi
grep -Fq 'packet-broker-performance: FAIL reason=origin-source-changed' \
    "$tmp/broker-mutation.out"
grep -Fqx $'result\tPASS' "$tmp/broker-mutation/receipt.tsv"
grep -Fqx $'result\tFAIL' "$tmp/broker-mutation/gate-envelope.tsv"
grep -Fqx $'child_exit_code\t0' "$tmp/broker-mutation/gate-envelope.tsv"
grep -Fqx $'origin_source_revalidated\tFAIL' \
    "$tmp/broker-mutation/gate-envelope.tsv"
(cd "$tmp/broker-mutation" && sha256sum -c checksums.sha256 >/dev/null)

printf '%s\n' 'test-packet-evidence-scripts: PASS'
