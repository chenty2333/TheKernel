#!/bin/sh

IPERF_BIN="${OSCOMP_IPERF_BIN:-/glibc/iperf3}"
ip="${OSCOMP_IPERF_HOST:-127.0.0.1}"
port="${OSCOMP_IPERF_PORT:-5001}"
case_filter="${OSCOMP_IPERF_CASE_FILTER:-}"
family_args="${OSCOMP_IPERF_FAMILY_ARGS:--4}"

want_case() {
    [ -z "$case_filter" ] && return 0
    case " $case_filter " in
        *" $1 "*) return 0 ;;
    esac
    return 1
}

iperf_args() {
    case "$1" in
        BASIC_UDP)
            echo "${OSCOMP_IPERF_ARGS_BASIC_UDP:--u -b 1000G}"
            ;;
        BASIC_TCP)
            echo "${OSCOMP_IPERF_ARGS_BASIC_TCP:-}"
            ;;
        PARALLEL_UDP)
            echo "${OSCOMP_IPERF_ARGS_PARALLEL_UDP:--u -P 5 -b 1000G}"
            ;;
        PARALLEL_TCP)
            echo "${OSCOMP_IPERF_ARGS_PARALLEL_TCP:--P 5}"
            ;;
        REVERSE_UDP)
            echo "${OSCOMP_IPERF_ARGS_REVERSE_UDP:--u -R -b 1000G}"
            ;;
        REVERSE_TCP)
            echo "${OSCOMP_IPERF_ARGS_REVERSE_TCP:--R}"
            ;;
        *)
            return 1
            ;;
    esac
}

cleanup() {
    if [ -n "${server_pid:-}" ]; then
        kill "$server_pid" >/dev/null 2>&1 || true
        wait "$server_pid" 2>/dev/null || true
        server_pid=""
    fi
    ./busybox killall -9 iperf3 >/dev/null 2>&1 || true
}

start_server() {
    cleanup
    server_args="${OSCOMP_IPERF_SERVER_ARGS:-}"
    case "${OSCOMP_IPERF_DEBUG_SERVER:-0}" in
        1|y|Y|yes|YES|true|TRUE|on|ON)
            debug_server_args="${OSCOMP_IPERF_DEBUG_SERVER_ARGS:-}"
            LD_LIBRARY_PATH=/glibc/lib:/lib "$IPERF_BIN" -s -p "$port" $family_args $debug_server_args &
            server_pid=$!
            echo "#### OSCOMP IPERF SERVER PID $server_pid glibc ####"
            ;;
        *)
            LD_LIBRARY_PATH=/glibc/lib:/lib "$IPERF_BIN" -s -p "$port" $family_args $server_args >/dev/null 2>&1 &
            server_pid=$!
            ;;
    esac
    start_status=$?
    [ "$start_status" = 0 ] || return "$start_status"
    sleep "${OSCOMP_IPERF_SERVER_WARMUP_SECS:-1}"
    if [ -n "${server_pid:-}" ] && ! kill -0 "$server_pid" >/dev/null 2>&1; then
        return 1
    fi
    return 0
}

run_iperf() {
    test_name="$1"
    want_case "$test_name" || return 0

    echo "====== iperf $test_name begin ======"
    args=$(iperf_args "$test_name") || exit 1
    if LD_LIBRARY_PATH=/glibc/lib:/lib "$IPERF_BIN" -c "$ip" -p "$port" $family_args -t "${OSCOMP_IPERF_LENGTH:-2}" -i 0 ${OSCOMP_IPERF_GLOBAL_ARGS:-} $args; then
        ans="success"
    else
        ans="fail"
    fi
    echo "====== iperf $test_name end: $ans ======"
    echo ""
}

trap cleanup EXIT INT TERM

echo "#### OS COMP TEST GROUP START iperf-glibc ####"
start_server || echo "#### OSCOMP IPERF SERVER START FAIL glibc ####"
run_iperf BASIC_UDP
run_iperf BASIC_TCP
run_iperf PARALLEL_UDP
run_iperf PARALLEL_TCP
run_iperf REVERSE_UDP
run_iperf REVERSE_TCP

cleanup
echo "#### OS COMP TEST GROUP END iperf-glibc ####"
