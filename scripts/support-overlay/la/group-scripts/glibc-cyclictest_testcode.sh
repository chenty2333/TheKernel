#!/bin/sh

HACKBENCH_BIN="${OSCOMP_HACKBENCH_BIN:-/musl/hackbench}"
machine_name() {
    if [ -n "${OSCOMP_MACHINE:-}" ]; then
        printf "%s\n" "${OSCOMP_MACHINE}"
    else
        uname -m 2>/dev/null || true
    fi
}

CYCLICTEST_BIN_DEFAULT=/glibc/cyclictest
case "$(machine_name)" in
    loongarch64)
        CYCLICTEST_BIN_DEFAULT=/musl/cyclictest
        compat_hackbench="${OSCOMP_SUPPORT_BIN:-/opt/oscomp-support/bin}/oscomp-hackstress"
        [ -x "$compat_hackbench" ] && HACKBENCH_BIN="$compat_hackbench"
        ;;
esac

CYCLICTEST_BIN="${OSCOMP_CYCLICTEST_BIN:-$CYCLICTEST_BIN_DEFAULT}"

debug_step() {
    case "${OSCOMP_CYCLICTEST_DEBUG_STEPS:-0}" in
        1|y|Y|yes|YES|true|TRUE|on|ON)
            echo "#### OSCOMP CYCLICTEST DEBUG $* ####"
            ;;
    esac
}

hackbench_args() {
    case "$(machine_name)" in
        loongarch64)
            echo "${OSCOMP_HACKBENCH_ARGS:--g 1 -f 1 -l 10000000}"
            ;;
        *)
            echo "${OSCOMP_HACKBENCH_ARGS:--l 100000000}"
            ;;
    esac
}

cyclictest_hackbench_warmup_secs() {
    if [ -n "${OSCOMP_CYCLICTEST_HACKBENCH_WARMUP_SECS+x}" ]; then
        printf "%s\n" "${OSCOMP_CYCLICTEST_HACKBENCH_WARMUP_SECS}"
        return 0
    fi

    case "$(machine_name):$(cyclictest_runtime_root)" in
        loongarch64:musl)
            echo 0
            ;;
        *)
            echo 1
            ;;
    esac
}

cyclictest_step_settle_secs() {
    if [ -n "${OSCOMP_CYCLICTEST_STEP_SETTLE_SECS+x}" ]; then
        printf "%s\n" "${OSCOMP_CYCLICTEST_STEP_SETTLE_SECS}"
        return 0
    fi

    echo 0
}

cyclictest_control_sleep() {
    sleep_secs="$1"
    support_sleep="${OSCOMP_SUPPORT_BIN:-/opt/oscomp-support/bin}/oscomp-sleep"
    support_default_signals="${OSCOMP_SUPPORT_BIN:-/opt/oscomp-support/bin}/oscomp-default-signals"

    if [ -x "$support_sleep" ]; then
        if [ -x "$support_default_signals" ]; then
            "$support_default_signals" "$support_sleep" "$sleep_secs"
        else
            "$support_sleep" "$sleep_secs"
        fi
        return $?
    fi

    case "$(cyclictest_runtime_root)" in
        musl)
            musl_busybox="${OSCOMP_MUSL_BUSYBOX:-/musl/busybox}"
            if [ -x "$musl_busybox" ]; then
                LD_LIBRARY_PATH=/musl/lib:/lib "$musl_busybox" sleep "$sleep_secs"
                return $?
            fi
            ;;
    esac

    sleep "$sleep_secs"
}

cyclictest_step_settle() {
    settle_secs="$(cyclictest_step_settle_secs "$1")"
    case "$settle_secs" in
        ''|0)
            return 0
            ;;
    esac
    debug_step "settle ${settle_secs}s after $1"
    cyclictest_control_sleep "$settle_secs"
}

cyclictest_diag_delay_secs() {
    if [ -n "${OSCOMP_CYCLICTEST_DIAG_SECS+x}" ]; then
        printf "%s\n" "${OSCOMP_CYCLICTEST_DIAG_SECS}"
        return 0
    fi
    echo 0
}

cyclictest_async_wait_secs() {
    if [ -n "${OSCOMP_CYCLICTEST_ASYNC_WAIT_SECS+x}" ]; then
        printf "%s\n" "${OSCOMP_CYCLICTEST_ASYNC_WAIT_SECS}"
        return 0
    fi

    case "$(machine_name)" in
        loongarch64)
            echo 2
            ;;
        *)
            echo 0
            ;;
    esac
}

cyclictest_dump_process_diag() {
    cyclictest_diag_pid="$1"
    cyclictest_diag_label="$2"

    debug_step "diag ${cyclictest_diag_label} pid ${cyclictest_diag_pid}"
    ps 2>/dev/null || true
    if [ -r "/proc/${cyclictest_diag_pid}/status" ]; then
        echo "#### OSCOMP CYCLICTEST DEBUG PROC STATUS ${cyclictest_diag_label} PID ${cyclictest_diag_pid} ####"
        sed -n '1,20p' "/proc/${cyclictest_diag_pid}/status" 2>/dev/null || true
    fi
}

launch_hackbench() {
    case "$HACKBENCH_BIN" in
        */oscomp-hackstress|oscomp-hackstress)
            support_default_signals="${OSCOMP_SUPPORT_BIN:-/opt/oscomp-support/bin}/oscomp-default-signals"
            if [ -x "$support_default_signals" ]; then
                "$support_default_signals" "$HACKBENCH_BIN" "$@" >/dev/null 2>&1 &
            else
                "$HACKBENCH_BIN" "$@" >/dev/null 2>&1 &
            fi
            return 0
            ;;
    esac

    case "$(cyclictest_runtime_root)" in
        musl)
            musl_busybox="${OSCOMP_MUSL_BUSYBOX:-/musl/busybox}"
            if [ -x "$musl_busybox" ]; then
                LD_LIBRARY_PATH=/musl/lib:/lib "$musl_busybox" nice -n 19 "$HACKBENCH_BIN" "$@" &
                return 0
            fi
            ;;
    esac

    LD_LIBRARY_PATH=/musl/lib:/lib "$HACKBENCH_BIN" "$@" &
}

cyclictest_runtime_root() {
    case "$CYCLICTEST_BIN" in
        /musl/*)
            echo musl
            ;;
        *)
            echo glibc
            ;;
    esac
}

cyclictest_ld_path() {
    case "$(cyclictest_runtime_root)" in
        musl)
            echo /musl/lib:/lib
            ;;
        *)
            echo /glibc/lib:/lib
            ;;
    esac
}

cyclictest_preload() {
    compat_root="${OSCOMP_SUPPORT_LIB:-/opt/oscomp-support/lib}"
    case "$(cyclictest_runtime_root)" in
        musl)
            compat_lib="${compat_root}/liboscomp-musl-compat.so"
            if [ -f "$compat_lib" ]; then
                if [ -n "${LD_PRELOAD:-}" ]; then
                    printf "%s\n" "${compat_lib}:$LD_PRELOAD"
                else
                    printf "%s\n" "$compat_lib"
                fi
            fi
            ;;
        *)
            compat_lib="${compat_root}/liboscomp-glibc-compat.so"
            if [ -f "$compat_lib" ]; then
                if [ -n "${LD_PRELOAD:-}" ]; then
                    printf "%s\n" "${compat_lib}:$LD_PRELOAD"
                else
                    printf "%s\n" "$compat_lib"
                fi
            fi
            ;;
    esac
}

prepare_cyclictest_env() {
    case "$(cyclictest_runtime_root)" in
        musl)
            unset LANG LC_ALL LC_CTYPE LOCPATH
            ;;
    esac
}

cyclictest_args() {
    case "$1" in
        NO_STRESS_P1)
            echo "${OSCOMP_CYCLICTEST_ARGS_NO_STRESS_P1:--a -i 1000 -t1 -p99 -D 1s -q}"
            ;;
        NO_STRESS_P8)
            echo "${OSCOMP_CYCLICTEST_ARGS_NO_STRESS_P8:--a -i 1000 -t8 -p99 -D 1s -q}"
            ;;
        STRESS_P1)
            echo "${OSCOMP_CYCLICTEST_ARGS_STRESS_P1:--a -i 1000 -t1 -p99 -D 1s -q}"
            ;;
        STRESS_P8)
            echo "${OSCOMP_CYCLICTEST_ARGS_STRESS_P8:--a -i 1000 -t8 -p99 -D 1s -q}"
            ;;
        *)
            return 1
            ;;
    esac
}

run_cyclictest() {
    echo "====== cyclictest $1 begin ======"
    args=$(cyclictest_args "$1") || exit 1
    ld_path="$(cyclictest_ld_path)"
    preload="$(cyclictest_preload)"
    diag_delay_secs="$(cyclictest_diag_delay_secs)"
    async_wait_secs="$(cyclictest_async_wait_secs)"
    if [ "${diag_delay_secs:-0}" -gt 0 ] 2>/dev/null || [ "${async_wait_secs:-0}" -gt 0 ] 2>/dev/null; then
        if [ -n "$preload" ]; then
            LD_LIBRARY_PATH="$ld_path" LD_PRELOAD="$preload" "$CYCLICTEST_BIN" $args &
        else
            LD_LIBRARY_PATH="$ld_path" "$CYCLICTEST_BIN" $args &
        fi
        cyclictest_pid=$!
        debug_step "cyclictest $1 pid $cyclictest_pid"
        if [ "${diag_delay_secs:-0}" -gt 0 ] 2>/dev/null; then
            cyclictest_control_sleep "$diag_delay_secs"
        else
            cyclictest_control_sleep "$async_wait_secs"
        fi
        if [ "${diag_delay_secs:-0}" -gt 0 ] 2>/dev/null && kill -0 "$cyclictest_pid" 2>/dev/null; then
            cyclictest_dump_process_diag "$cyclictest_pid" "$1"
        fi
        wait "$cyclictest_pid"
        if [ $? = 0 ]; then
            ans="success"
        else
            ans="fail"
        fi
    elif [ -n "$preload" ]; then
        if LD_LIBRARY_PATH="$ld_path" LD_PRELOAD="$preload" "$CYCLICTEST_BIN" $args; then
            ans="success"
        else
            ans="fail"
        fi
    elif LD_LIBRARY_PATH="$ld_path" "$CYCLICTEST_BIN" $args; then
        ans="success"
    else
        ans="fail"
    fi
    echo "====== cyclictest $1 end: $ans ======"
    cyclictest_step_settle "$1"
}

echo "#### OS COMP TEST GROUP START cyclictest ####"

prepare_cyclictest_env

run_cyclictest NO_STRESS_P1
run_cyclictest NO_STRESS_P8

echo "====== start hackbench ======"
hackbench_args="$(hackbench_args)"
launch_hackbench $hackbench_args
hackbench_pid=$!
debug_step "hackbench pid $hackbench_pid"

cyclictest_control_sleep "$(cyclictest_hackbench_warmup_secs)"
debug_step "after hackbench warmup"

run_cyclictest STRESS_P1
run_cyclictest STRESS_P8

kill -2 "$hackbench_pid"
kill_status=$?
debug_step "kill hackbench ret $kill_status"
if [ "$kill_status" = 0 ]; then
    ans="success"
else
    ans="fail, ignore STRESS result"
fi
echo "====== kill hackbench: $ans ======"

echo "#### OS COMP TEST GROUP END cyclictest ####"
