#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

[ "$#" -eq 0 ] || nightly_fail 'nonloopback-network adapter takes no arguments'
command -v python3 >/dev/null 2>&1 || nightly_unsupported 'missing python3 host peer'
command -v sha256sum >/dev/null 2>&1 || nightly_unsupported 'missing sha256sum'
mkdir -p "$NIGHTLY_LOG_DIR"

peer_pid=
cleanup_peer() {
    if [ -n "$peer_pid" ] && kill -0 "$peer_pid" 2>/dev/null; then
        kill "$peer_pid" 2>/dev/null || true
        wait "$peer_pid" 2>/dev/null || true
    fi
}
trap cleanup_peer EXIT

selected_arches=$(nightly_selected_arches) || exit $?
while IFS= read -r arch; do
    commands="$NIGHTLY_LOG_DIR/$arch.commands"
    run_dir="$NIGHTLY_LOG_DIR/$arch"
    port_file="$NIGHTLY_LOG_DIR/$arch.peer-port"
    peer_log="$NIGHTLY_LOG_DIR/$arch.peer.log"
    nonce=$(printf '%s:%s:%s\n' "$arch" "$$" "$(date +%s)" | sha256sum | awk '{ print $1 }')
    rm -f "$port_file"

    python3 "$SCRIPT_DIR/network-peer.py" \
        --nonce "$nonce" \
        --port-file "$port_file" \
        --timeout "$((NIGHTLY_GUEST_TIMEOUT_SECS + 60))" \
        >"$peer_log" 2>&1 &
    peer_pid=$!

    for ((attempt = 0; attempt < 100; attempt += 1)); do
        [ -s "$port_file" ] && break
        kill -0 "$peer_pid" 2>/dev/null \
            || nightly_fail "host network peer exited before publishing its port: $peer_log"
        sleep 0.1
    done
    [ -s "$port_file" ] \
        || nightly_fail "host network peer did not publish its port: $peer_log"
    port=$(tr -d '\r\n' <"$port_file")
    case "$port" in
        ''|*[!0-9]*) nightly_fail "host network peer published invalid port: $port" ;;
    esac

    printf 'peer_port=%s\npeer_nonce=%s\n' "$port" "$nonce" >"$commands"
    cat >>"$commands" <<'EOF'
echo CI_NIGHTLY_NONLOOPBACK_NETWORK_START
rm -f /tmp/ci-network-reply
printf 'THEKERNEL_NETWORK_PROBE %s\n' "$peer_nonce" | /musl/busybox nc -w 30 10.0.2.2 "$peer_port" > /tmp/ci-network-reply
network_status=$?
test "$network_status" -eq 0 || { echo CI_NIGHTLY_NONLOOPBACK_NETWORK_FAIL connect; exit 1; }
test "$(cat /tmp/ci-network-reply)" = "THEKERNEL_NETWORK_REPLY $peer_nonce" || { echo CI_NIGHTLY_NONLOOPBACK_NETWORK_FAIL payload; exit 1; }
rm -f /tmp/ci-network-reply
echo CI_NIGHTLY_NONLOOPBACK_NETWORK_PASS
exit
EOF

    nightly_run_guest "$arch" "$commands" "$run_dir"
    if wait "$peer_pid"; then
        peer_pid=
    else
        status=$?
        peer_pid=
        nightly_fail "host network peer failed with exit $status: $peer_log"
    fi
    nightly_validate_guest_log \
        "$run_dir/qemu.log" clean \
        CI_NIGHTLY_NONLOOPBACK_NETWORK_START \
        CI_NIGHTLY_NONLOOPBACK_NETWORK_PASS
    grep -Fq 'network-peer: validated guest request' "$peer_log" \
        || nightly_fail "host peer did not validate guest traffic: $peer_log"
done <<<"$selected_arches"

printf 'nightly non-loopback network adapter: PASS\n'
