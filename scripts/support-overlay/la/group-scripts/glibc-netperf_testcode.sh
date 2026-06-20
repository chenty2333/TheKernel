#!/bin/sh

NETPERF_BIN="${OSCOMP_NETPERF_BIN:-/glibc/netperf}"
NETSERVER_BIN="${OSCOMP_NETSERVER_BIN:-/glibc/netserver}"
ip="${OSCOMP_NETPERF_IP:-127.0.0.1}"
port="${OSCOMP_NETPERF_PORT:-12865}"
case_filter="${OSCOMP_NETPERF_CASE_FILTER:-}"

want_case() {
    [ -z "$case_filter" ] && return 0
    case " $case_filter " in
        *" $1 "*) return 0 ;;
    esac
    return 1
}

netperf_args() {
    case "$1" in
        UDP_STREAM)
            echo "${OSCOMP_NETPERF_ARGS_UDP_STREAM:--s 16k -S 16k -m 1k -M 1k}"
            ;;
        TCP_STREAM)
            echo "${OSCOMP_NETPERF_ARGS_TCP_STREAM:--s 16k -S 16k -m 1k -M 1k}"
            ;;
        UDP_RR)
            echo "${OSCOMP_NETPERF_ARGS_UDP_RR:--s 16k -S 16k -m 1k -M 1k -r 64,64 -R 1}"
            ;;
        TCP_RR)
            echo "${OSCOMP_NETPERF_ARGS_TCP_RR:--s 16k -S 16k -m 1k -M 1k -r 64,64 -R 1}"
            ;;
        TCP_CRR)
            echo "${OSCOMP_NETPERF_ARGS_TCP_CRR:--s 16k -S 16k -m 1k -M 1k -r 64,64 -R 1}"
            ;;
        *)
            return 1
            ;;
    esac
}

cleanup() {
    ./busybox killall -9 netserver >/dev/null 2>&1 || true
}

start_server() {
    cleanup
    LD_LIBRARY_PATH=/glibc/lib:/lib "$NETSERVER_BIN" -D -L "$ip" -p "$port" \
        ${OSCOMP_NETSERVER_ARGS:-} >/dev/null 2>&1 &
    sleep "${OSCOMP_NETPERF_SERVER_WARMUP_SECS:-1}"
}

run_netperf() {
    test_name="$1"
    want_case "$test_name" || return 0

    echo "====== netperf $test_name begin ======"
    args=$(netperf_args "$test_name") || exit 1
    start_server
    if LD_LIBRARY_PATH=/glibc/lib:/lib "$NETPERF_BIN" -H "$ip" -p "$port" -t "$test_name" \
        -l "${OSCOMP_NETPERF_LENGTH:-1}" ${OSCOMP_NETPERF_GLOBAL_ARGS:-} -- $args; then
        ans="success"
    else
        ans="fail"
    fi
    cleanup
    echo "====== netperf $test_name end: $ans ======"
}

trap cleanup EXIT INT TERM

echo "#### OS COMP TEST GROUP START netperf ####"
run_netperf UDP_STREAM
run_netperf TCP_STREAM
run_netperf UDP_RR
run_netperf TCP_RR
run_netperf TCP_CRR

cleanup
echo "#### OS COMP TEST GROUP END netperf ####"
