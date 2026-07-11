#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
CI_DIR="$REPO_ROOT/scripts/ci"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

while IFS= read -r script; do
    bash -n "$script"
done < <(find "$CI_DIR" -type f -name '*.sh' -print | sort)
bash -n "$0"

python3 "$REPO_ROOT/tests/ci/test_vendor_provenance.py"

cat >"$tmp/pass.log" <<'EOF'
CI_BOOT_GATE_START
CI_BOOT_GATE_ROOTFS_OK
CI_BOOT_GATE_TMPFS_OK
CI_BOOT_GATE_PROCFS_OK
CI_BOOT_GATE_BIND_OK
CI_BOOT_GATE_PASS
System is shutting down
EOF
"$CI_DIR/validate-boot-log.sh" rv "$tmp/pass.log" >/dev/null

cp "$tmp/pass.log" "$tmp/fail.log"
printf 'CI_BOOT_GATE_FAIL injected\n' >>"$tmp/fail.log"
if "$CI_DIR/validate-boot-log.sh" la "$tmp/fail.log" >/dev/null 2>&1; then
    printf 'test-ci-scripts: failure marker was accepted\n' >&2
    exit 1
fi

cp "$tmp/pass.log" "$tmp/missing.log"
sed -i '/CI_BOOT_GATE_BIND_OK/d' "$tmp/missing.log"
if "$CI_DIR/validate-boot-log.sh" rv "$tmp/missing.log" >/dev/null 2>&1; then
    printf 'test-ci-scripts: missing marker was accepted\n' >&2
    exit 1
fi

for category in ltp pressure oom-failpoint fs-powercut nonloopback-network; do
    "$CI_DIR/nightly-gate.sh" --list | grep -q "^${category}"
done

# The host-test linker must add -no-pie only to executables. Shared objects
# include proc macros and would fail to link if the C runtime expected main.
cat >"$tmp/fake-cc" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$FAKE_CC_LOG"
EOF
chmod +x "$tmp/fake-cc"
FAKE_CC_LOG="$tmp/shared-link.args" THEKERNEL_HOST_CC="$tmp/fake-cc" \
    "$CI_DIR/host-test-linker.sh" -shared input.o -o output.so
if grep -qx -- -no-pie "$tmp/shared-link.args"; then
    printf 'test-ci-scripts: shared link received -no-pie\n' >&2
    exit 1
fi
FAKE_CC_LOG="$tmp/executable-link.args" THEKERNEL_HOST_CC="$tmp/fake-cc" \
    "$CI_DIR/host-test-linker.sh" input.o -o output
grep -qx -- -no-pie "$tmp/executable-link.args"

env \
    THEKERNEL_NIGHTLY_LTP_ENABLED=0 \
    THEKERNEL_NIGHTLY_PRESSURE_ENABLED=0 \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED=0 \
    THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED=0 \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED=0 \
    "$CI_DIR/nightly-gate.sh" --log-dir "$tmp/nightly" >/dev/null

[ "$(awk -F '\t' '$2 == "skip" { count += 1 } END { print count + 0 }' "$tmp/nightly/nightly-status.tsv")" -eq 5 ]

# Configured adapters retain the same three-state contract as repository
# adapters. In particular, exit 78 must remain unsupported and make the whole
# gate return 78; it must never be rewritten to pass.
set +e
env \
    THEKERNEL_NIGHTLY_LTP_ENABLED=0 \
    THEKERNEL_NIGHTLY_PRESSURE_COMMAND='exit 78' \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED=0 \
    THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED=0 \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED=0 \
    "$CI_DIR/nightly-gate.sh" --log-dir "$tmp/nightly-unsupported" >/dev/null
status=$?
set -e
[ "$status" -eq 78 ] || {
    printf 'test-ci-scripts: unsupported adapter returned %s, expected 78\n' "$status" >&2
    exit 1
}
grep -q $'^pressure\tunsupported\t' "$tmp/nightly-unsupported/nightly-status.tsv"

set +e
env \
    THEKERNEL_NIGHTLY_LTP_ENABLED=0 \
    THEKERNEL_NIGHTLY_PRESSURE_COMMAND='exit 9' \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED=0 \
    THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED=0 \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED=0 \
    "$CI_DIR/nightly-gate.sh" --log-dir "$tmp/nightly-fail" >/dev/null
status=$?
set -e
[ "$status" -eq 1 ] || {
    printf 'test-ci-scripts: failed adapter returned %s, expected 1\n' "$status" >&2
    exit 1
}
grep -q $'^pressure\tfail\t' "$tmp/nightly-fail/nightly-status.tsv"

env \
    THEKERNEL_NIGHTLY_LTP_ENABLED=0 \
    THEKERNEL_NIGHTLY_PRESSURE_COMMAND='exit 0' \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED=0 \
    THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED=0 \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED=0 \
    "$CI_DIR/nightly-gate.sh" --log-dir "$tmp/nightly-pass" >/dev/null
grep -q $'^pressure\tpass\t' "$tmp/nightly-pass/nightly-status.tsv"

printf 'support\n' >"$tmp/fake-support.img"
set +e
env \
    THEKERNEL_NIGHTLY_ARCHES=invalid \
    THEKERNEL_NIGHTLY_SUPPORT_IMAGE="$tmp/fake-support.img" \
    "$CI_DIR/nightly/pressure.sh" >"$tmp/invalid-arch.log" 2>&1
status=$?
set -e
[ "$status" -eq 1 ] || {
    printf 'test-ci-scripts: invalid adapter architecture returned %s, expected 1\n' "$status" >&2
    exit 1
}
grep -Fq 'THEKERNEL_NIGHTLY_ARCHES must be rv, la, or both' "$tmp/invalid-arch.log"
if grep -Fq 'UNSUPPORTED' "$tmp/invalid-arch.log"; then
    printf 'test-ci-scripts: invalid adapter architecture was misclassified as unsupported\n' >&2
    exit 1
fi

# Verify the shared runner records and enforces a real wall-clock timeout.
# shellcheck source=../../scripts/ci/lib.sh
source "$CI_DIR/lib.sh"
export CI_LOG_DIR="$tmp/runner"
ci_prepare_log_dir "$CI_LOG_DIR"
ci_run_step quick-pass 5 bash -c 'printf ok' >/dev/null
if ci_run_step must-timeout 1 bash -c 'sleep 5' >/dev/null 2>&1; then
    printf 'test-ci-scripts: timeout step unexpectedly passed\n' >&2
    exit 1
else
    status=$?
    [ "$status" -eq 124 ] || {
        printf 'test-ci-scripts: timeout returned %s, expected 124\n' "$status" >&2
        exit 1
    }
fi
grep -q $'^must-timeout\ttimeout\t124\t' "$CI_LOG_DIR/status.tsv"

# A guest can close the serial pipe before the throttled producer finishes.
# The runner must preserve the replay status and leave panic/missing-marker
# classification to validate-boot-log.sh instead of surfacing SIGPIPE 141.
mkdir -p "$tmp/fake-bin" "$tmp/fake-work"
cat >"$tmp/fake-bin/python3" <<'EOF'
#!/usr/bin/env bash
[ -z "${FAKE_REPLAY_ARGS:-}" ] || printf '%s\n' "$@" >"$FAKE_REPLAY_ARGS"
exit "${FAKE_REPLAY_STATUS:-0}"
EOF
chmod +x "$tmp/fake-bin/python3"
for _ in $(seq 1 20000); do
    printf 'echo serial-input-padding-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n'
done >"$tmp/commands"
env PATH="$tmp/fake-bin:$PATH" FAKE_REPLAY_STATUS=0 \
    "$CI_DIR/boot-shell-runner.sh" rv /dev/null /dev/null \
    "$tmp/fake-work" "$tmp/commands" 1 0 0
if env PATH="$tmp/fake-bin:$PATH" FAKE_REPLAY_STATUS=23 \
    "$CI_DIR/boot-shell-runner.sh" rv /dev/null /dev/null \
    "$tmp/fake-work" "$tmp/commands" 1 0 0; then
    printf 'test-ci-scripts: replay failure was hidden by pipe handling\n' >&2
    exit 1
else
    status=$?
    [ "$status" -eq 23 ] || {
        printf 'test-ci-scripts: replay failure returned %s, expected 23\n' "$status" >&2
        exit 1
    }
fi

printf 'exit\n' >"$tmp/short-commands"
env PATH="$tmp/fake-bin:$PATH" FAKE_REPLAY_STATUS=75 \
    FAKE_REPLAY_ARGS="$tmp/replay.args" \
    "$CI_DIR/boot-shell-runner.sh" rv kernel image "$tmp/fake-work" \
    "$tmp/short-commands" 1 0 0 support.img extra.img STOP_MARKER || status=$?
[ "${status:-75}" -eq 75 ]
grep -Fxq -- '--support-image' "$tmp/replay.args"
grep -Fxq -- 'support.img' "$tmp/replay.args"
grep -Fxq -- '--extra-block-image' "$tmp/replay.args"
grep -Fxq -- 'extra.img' "$tmp/replay.args"
grep -Fxq -- '--stop-after-marker' "$tmp/replay.args"
grep -Fxq -- 'STOP_MARKER' "$tmp/replay.args"

# The one-shot host peer rejects unauthenticated traffic and returns the nonce
# only after receiving the exact guest probe.
peer_port_file="$tmp/peer.port"
python3 "$CI_DIR/nightly/network-peer.py" \
    --nonce ci-unit --port-file "$peer_port_file" --timeout 10 \
    >"$tmp/peer.log" 2>&1 &
peer_pid=$!
for ((attempt = 0; attempt < 100; attempt += 1)); do
    [ -s "$peer_port_file" ] && break
    sleep 0.01
done
[ -s "$peer_port_file" ]
python3 - "$(tr -d '\r\n' <"$peer_port_file")" <<'PY'
import socket
import sys

with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=5) as connection:
    connection.sendall(b"THEKERNEL_NETWORK_PROBE ci-unit\n")
    connection.shutdown(socket.SHUT_WR)
    assert connection.recv(4096) == b"THEKERNEL_NETWORK_REPLY ci-unit\n"
PY
wait "$peer_pid"
grep -Fq 'network-peer: validated guest request' "$tmp/peer.log"

# Compile both support-helper modes on the host. The guest gate supplies the
# strict overcommit policy that makes its finite request fail deterministically.
cc -O2 -std=c11 "$REPO_ROOT/scripts/support-tools/nightly-oom-admission.c" \
    -o "$tmp/nightly-oom-admission"
"$tmp/nightly-oom-admission" --expect-success 4096 >/dev/null
"$tmp/nightly-oom-admission" --expect-failure 18446744073709551615 >/dev/null

printf 'test-ci-scripts: PASS\n'
