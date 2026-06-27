#!/bin/sh

./busybox echo "#### OS COMP TEST GROUP START cyclictest ####"

hackbench_pid=
cleanup_hackbench() {
    [ -n "$hackbench_pid" ] || return 0
    kill -2 "$hackbench_pid" >/dev/null 2>&1 || true
    ./busybox killall -15 hackbench >/dev/null 2>&1 || true
    ./busybox killall -9 hackbench >/dev/null 2>&1 || true
    wait "$hackbench_pid" >/dev/null 2>&1 || true
    hackbench_pid=
}

trap 'cleanup_hackbench' EXIT INT TERM

HACKBENCH_BIN=${OSCOMP_HACKBENCH_BIN:-}
if [ -z "$HACKBENCH_BIN" ]; then
    compat_hackbench="${OSCOMP_SUPPORT_BIN:-/opt/oscomp-support/bin}/oscomp-hackstress"
    if [ -x "$compat_hackbench" ]; then
        HACKBENCH_BIN="$compat_hackbench"
    else
        HACKBENCH_BIN=./hackbench
    fi
fi

run_cyclictest() {
    echo "====== cyclictest $1 begin ======"
    ./cyclictest $2
    if [ $? = 0 ]; then
        ans="success"
    else
        ans="fail"
    fi
    echo "====== cyclictest $1 end: $ans ======"
}

run_cyclictest NO_STRESS_P1 "-a -i 1000 -t1  -p99 -D 1s -q"
run_cyclictest NO_STRESS_P8 "-a -i 1000 -t8  -p99 -D 1s -q"

echo "====== start hackbench ======"
"$HACKBENCH_BIN" -l 100000000 &
hackbench_pid=$!

./busybox sleep 1

run_cyclictest STRESS_P1 "-a -i 1000 -t1  -p99 -D 1s -q"
run_cyclictest STRESS_P8 "-a -i 1000 -t8  -p99 -D 1s -q"

cleanup_hackbench
ans="success"
echo "====== kill hackbench: $ans ======"

./busybox echo "#### OS COMP TEST GROUP END cyclictest ####"
