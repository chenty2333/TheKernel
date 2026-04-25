#!/bin/sh

if [ -z "${OSCOMP_BOOTSTRAP:-}" ]; then
    for candidate in /musl/busybox /glibc/busybox /busybox /bin/busybox; do
        if [ -x "$candidate" ]; then
            OSCOMP_BOOTSTRAP="$candidate"
            export OSCOMP_BOOTSTRAP
            break
        fi
    done
fi

bb() {
    if [ -n "${OSCOMP_BOOTSTRAP:-}" ] && [ "${OSCOMP_BOOTSTRAP##*/}" = "busybox" ]; then
        "$OSCOMP_BOOTSTRAP" "$@"
    else
        "$@"
    fi
}

oscomp_machine() {
    if [ -n "${OSCOMP_MACHINE+x}" ]; then
        printf '%s\n' "${OSCOMP_MACHINE}"
        return 0
    fi

    runner_debug "#### OSCOMP RUNNER DETECT MACHINE BEGIN ####"
    OSCOMP_MACHINE="$(bb uname -m 2>/dev/null || true)"
    export OSCOMP_MACHINE
    runner_debug "#### OSCOMP RUNNER DETECT MACHINE END ${OSCOMP_MACHINE:-unknown} ####"
    printf '%s\n' "${OSCOMP_MACHINE}"
}

runner_machine_quiet() {
    if [ -n "${OSCOMP_MACHINE:-}" ]; then
        printf '%s\n' "${OSCOMP_MACHINE}"
        return 0
    fi

    OSCOMP_MACHINE="$(bb uname -m 2>/dev/null || true)"
    export OSCOMP_MACHINE
    printf '%s\n' "${OSCOMP_MACHINE}"
}

seed_oscomp_machine() {
    if [ -n "${OSCOMP_MACHINE:-}" ]; then
        runner_debug "#### OSCOMP RUNNER MACHINE SEEDED ${OSCOMP_MACHINE} ####"
        return 0
    fi

    OSCOMP_MACHINE="$(bb uname -m 2>/dev/null || true)"
    export OSCOMP_MACHINE
    runner_debug "#### OSCOMP RUNNER MACHINE SEEDED ${OSCOMP_MACHINE:-unknown} ####"
}

seed_oscomp_shells() {
    if [ -n "${OSCOMP_MUSL_BUSYBOX:-}" ] && [ -n "${OSCOMP_GLIBC_BUSYBOX:-}" ]; then
        runner_debug "#### OSCOMP RUNNER SHELLS SEEDED MUSL ${OSCOMP_MUSL_BUSYBOX} GLIBC ${OSCOMP_GLIBC_BUSYBOX} ####"
        return 0
    fi

    if [ -z "${OSCOMP_MUSL_BUSYBOX:-}" ] && [ -x /musl/busybox ]; then
        OSCOMP_MUSL_BUSYBOX=/musl/busybox
        export OSCOMP_MUSL_BUSYBOX
    fi
    if [ -z "${OSCOMP_GLIBC_BUSYBOX:-}" ] && [ -x /glibc/busybox ]; then
        OSCOMP_GLIBC_BUSYBOX=/glibc/busybox
        export OSCOMP_GLIBC_BUSYBOX
    fi

    runner_debug "#### OSCOMP RUNNER SHELLS SEEDED MUSL ${OSCOMP_MUSL_BUSYBOX:-missing} GLIBC ${OSCOMP_GLIBC_BUSYBOX:-missing} ####"
}

runner_debug() {
    case "${OSCOMP_RUNNER_DEBUG:-0}" in
        1|y|Y|yes|YES|true|TRUE)
            printf '%s\n' "$*"
            ;;
    esac
}

runner_truthy() {
    case "${1:-}" in
        1|y|Y|yes|YES|true|TRUE|on|ON)
            return 0
            ;;
    esac
    return 1
}

runner_falsy() {
    case "${1:-}" in
        0|n|N|no|NO|false|FALSE|off|OFF)
            return 0
            ;;
    esac
    return 1
}

run_clean_exec() {
    if [ -n "${OSCOMP_SUPPORT_BIN:-}" ] && [ -x "$OSCOMP_SUPPORT_BIN/oscomp-default-signals" ]; then
        "$OSCOMP_SUPPORT_BIN/oscomp-default-signals" "$@"
    else
        "$@"
    fi
}

run_clean_shell_script() {
    shell_path="$1"
    script_path="$2"
    shift 2
    runner_debug "#### OSCOMP RUNNER CLEAN EXEC SHELL ${shell_path} SCRIPT ${script_path} TRACE ${OSCOMP_TRACE_GROUP_SHELL:-0} ####"

    if runner_truthy "${OSCOMP_TRACE_GROUP_SHELL:-0}"; then
        if [ "${shell_path##*/}" = "busybox" ]; then
            run_clean_exec "$shell_path" sh -x "$script_path" "$@"
        else
            run_clean_exec "$shell_path" -x "$script_path" "$@"
        fi
        return $?
    fi

    if [ "${shell_path##*/}" = "busybox" ]; then
        run_clean_exec "$shell_path" sh "$script_path" "$@"
    else
        run_clean_exec "$shell_path" "$script_path" "$@"
    fi
}

ltp_case_output_mode() {
    LTP_CASE_OUTPUT_MODE=stream

    case "${OSCOMP_LTP_CASE_OUTPUT_MODE:-}" in
        ""|stream|full|verbose)
            ;;
        buffered|capture|quiet)
            LTP_CASE_OUTPUT_MODE=buffered
            ;;
        *)
            runner_debug "#### OSCOMP RUNNER UNKNOWN LTP OUTPUT MODE ${OSCOMP_LTP_CASE_OUTPUT_MODE} ####"
            ;;
    esac

    if runner_truthy "${OSCOMP_LTP_BUFFER_OUTPUT:-0}"; then
        LTP_CASE_OUTPUT_MODE=buffered
    fi

    if [ -n "${OSCOMP_STREAM_LTP_CASE_OUTPUT:-}" ]; then
        if runner_falsy "${OSCOMP_STREAM_LTP_CASE_OUTPUT}"; then
            LTP_CASE_OUTPUT_MODE=buffered
        else
            LTP_CASE_OUTPUT_MODE=stream
        fi
    fi
}

prime_group_output_stream() {
    if [ -n "${OSCOMP_GROUP_OUTPUT_PRIMED:-}" ]; then
        return 0
    fi
    OSCOMP_GROUP_OUTPUT_PRIMED=1
    printf '\n'
}

append_word() {
    list_name="$1"
    word="$2"
    eval "current=\${$list_name}"
    if [ -n "$current" ]; then
        eval "$list_name=\"\$current \$word\""
    else
        eval "$list_name=\"\$word\""
    fi
}

contains_word() {
    target="$1"
    shift
    for word in "$@"; do
        [ "$word" = "$target" ] && return 0
    done
    return 1
}

pick_busybox() {
    PICK_BUSYBOX_RESULT=""

    if [ -x "${OSCOMP_BOOTSTRAP:-}" ] && [ "${OSCOMP_BOOTSTRAP##*/}" = "busybox" ]; then
        PICK_BUSYBOX_RESULT="$OSCOMP_BOOTSTRAP"
        return 0
    fi

    for path in /bin/busybox /musl/busybox /glibc/busybox /busybox; do
        if [ -x "$path" ]; then
            PICK_BUSYBOX_RESULT="$path"
            return 0
        fi
    done

    return 1
}

pick_busybox_for_root() {
    PICK_BUSYBOX_FOR_ROOT_RESULT=""
    pick_root="$1"

    case "$pick_root" in
        /musl)
            if [ -n "${OSCOMP_MUSL_BUSYBOX:-}" ] && [ -x "${OSCOMP_MUSL_BUSYBOX}" ]; then
                PICK_BUSYBOX_FOR_ROOT_RESULT="$OSCOMP_MUSL_BUSYBOX"
                return 0
            fi
            if [ -x /musl/busybox ]; then
                PICK_BUSYBOX_FOR_ROOT_RESULT=/musl/busybox
                return 0
            fi
            ;;
        /glibc)
            if [ -n "${OSCOMP_GLIBC_BUSYBOX:-}" ] && [ -x "${OSCOMP_GLIBC_BUSYBOX}" ]; then
                PICK_BUSYBOX_FOR_ROOT_RESULT="$OSCOMP_GLIBC_BUSYBOX"
                return 0
            fi
            if [ -x /glibc/busybox ]; then
                PICK_BUSYBOX_FOR_ROOT_RESULT=/glibc/busybox
                return 0
            fi
            ;;
    esac

    if [ -x "$pick_root/busybox" ]; then
        PICK_BUSYBOX_FOR_ROOT_RESULT="$pick_root/busybox"
        return 0
    fi

    pick_busybox || return 1
    PICK_BUSYBOX_FOR_ROOT_RESULT="$PICK_BUSYBOX_RESULT"
    return 0
}

install_runtime_alias() {
    src="$1"
    dst="$2"
    [ -e "$src" ] || return 1
    [ -e "$dst" ] && return 0

    parent="${dst%/*}"
    bb mkdir -p "$parent" 2>/dev/null || true
    bb ln -sf "$src" "$dst" 2>/dev/null && return 0
    bb cp "$src" "$dst" 2>/dev/null && return 0
    return 1
}

mirror_dir_entries_to_dir() {
    src_dir="$1"
    dst_dir="$2"
    [ -d "$src_dir" ] || return 0

    for src in "$src_dir"/*; do
        [ -e "$src" ] || continue
        install_runtime_alias "$src" "$dst_dir/${src##*/}" || true
    done
}

copy_dir_entries_to_dir() {
    src_dir="$1"
    dst_dir="$2"
    [ -d "$src_dir" ] || return 0

    bb mkdir -p "$dst_dir" 2>/dev/null || true
    for src in "$src_dir"/*; do
        [ -e "$src" ] || continue
        bb cp "$src" "$dst_dir/${src##*/}" 2>/dev/null || true
    done
}

copy_tree_entries_to_dir() {
    src_dir="$1"
    dst_dir="$2"
    [ -d "$src_dir" ] || return 0

    bb mkdir -p "$dst_dir" 2>/dev/null || true
    if bb cp -a "$src_dir/." "$dst_dir/" 2>/dev/null; then
        return 0
    fi

    for src in "$src_dir"/*; do
        [ -e "$src" ] || continue
        bb cp -a "$src" "$dst_dir/${src##*/}" 2>/dev/null || true
    done
}

clear_dir_contents() {
    dir_path="$1"
    bb mkdir -p "$dir_path" 2>/dev/null || true

    for entry in "$dir_path"/.[!.]* "$dir_path"/..?* "$dir_path"/*; do
        [ -e "$entry" ] || continue
        bb rm -rf "$entry" 2>/dev/null || true
    done
}

write_file_lines() {
    file_path="$1"
    shift
    parent_dir="${file_path%/*}"
    bb mkdir -p "$parent_dir" 2>/dev/null || true
    : > "$file_path" 2>/dev/null || return 1
    for line in "$@"; do
        printf '%s\n' "$line" >>"$file_path"
    done
}

ensure_file_line() {
    file_path="$1"
    line="$2"
    parent_dir="${file_path%/*}"
    bb mkdir -p "$parent_dir" 2>/dev/null || true
    [ -f "$file_path" ] || bb touch "$file_path" 2>/dev/null || return 1
    if ! bb grep -qxF "$line" "$file_path" 2>/dev/null; then
        printf '%s\n' "$line" >>"$file_path"
    fi
}

ensure_executable_script() {
    file_path="$1"
    shift
    parent_dir="${file_path%/*}"
    bb mkdir -p "$parent_dir" 2>/dev/null || true
    cat > "$file_path" <<EOF
$*
EOF
    chmod +x "$file_path" 2>/dev/null || true
}

install_bash_compat() {
    if /bin/bash -c 'exit 0' >/dev/null 2>&1; then
        return 0
    fi

    bb rm -f /bin/bash 2>/dev/null || true
    ensure_executable_script /bin/bash '#!/bin/sh
exec /bin/sh "$@"'
}

install_locale_tool() {
    [ -x /usr/bin/locale ] && return 0
    ensure_executable_script /usr/bin/locale '#!/bin/sh
cmd="${1:-}"
case "$cmd" in
    "" )
        echo "LANG=${LANG:-C}"
        echo "LC_CTYPE=${LC_CTYPE:-${LC_ALL:-${LANG:-C}}}"
        echo "LC_NUMERIC=${LC_NUMERIC:-${LC_ALL:-${LANG:-C}}}"
        echo "LC_TIME=${LC_TIME:-${LC_ALL:-${LANG:-C}}}"
        echo "LC_COLLATE=${LC_COLLATE:-${LC_ALL:-${LANG:-C}}}"
        echo "LC_MONETARY=${LC_MONETARY:-${LC_ALL:-${LANG:-C}}}"
        echo "LC_MESSAGES=${LC_MESSAGES:-${LC_ALL:-${LANG:-C}}}"
        echo "LC_ALL=${LC_ALL:-}"
        ;;
    -a )
        printf "%s\n" C POSIX C.UTF-8
        ;;
    charmap )
        echo "UTF-8"
        ;;
    * )
        echo "${LC_ALL:-${LANG:-C}}"
        ;;
esac'
}

install_systemd_detect_virt_tool() {
    [ -x /usr/bin/systemd-detect-virt ] && return 0
    ensure_executable_script /usr/bin/systemd-detect-virt '#!/bin/sh
case "${1:-}" in
    --quiet|-q)
        exit 0
        ;;
esac
echo qemu
'
}

la_hackbench_compat_enabled() {
    case "$(runner_machine_quiet)" in
        loongarch64)
            if runner_truthy "${OSCOMP_DISABLE_LA_HACKBENCH_COMPAT:-0}"; then
                return 1
            fi
            return 0
            ;;
    esac
    return 1
}

install_la_hackbench_wrapper_for_target() {
    target="$1"
    [ -e "$target" ] || return 0

    real_target="${target}.oscomp-real"
    if [ ! -e "$real_target" ]; then
        bb mv "$target" "$real_target" 2>/dev/null || return 0
    fi
    [ -x "$real_target" ] || return 0

    ensure_executable_script "$target" '#!/bin/sh
real_target="${0}.oscomp-real"
groups="${OSCOMP_LA_HACKBENCH_GROUPS:-1}"
fds="${OSCOMP_LA_HACKBENCH_FDS:-1}"

[ -x "$real_target" ] || exit 127
has_groups=0
has_fds=0
for arg in "$@"; do
    case "$arg" in
        -g|-g*|--groups|--groups=*)
            has_groups=1
            ;;
        -f|-f*|--fds|--fds=*)
            has_fds=1
            ;;
    esac
done

if [ "$has_fds" -eq 0 ]; then
    set -- -f "$fds" "$@"
fi
if [ "$has_groups" -eq 0 ]; then
    set -- -g "$groups" "$@"
fi

exec "$real_target" "$@"'
}

install_la_hackbench_wrapper() {
    la_hackbench_compat_enabled || return 0
    runner_debug "#### OSCOMP RUNNER INSTALL LA HACKBENCH COMPAT GROUPS ${OSCOMP_LA_HACKBENCH_GROUPS:-1} FDS ${OSCOMP_LA_HACKBENCH_FDS:-1} ####"
    install_la_hackbench_wrapper_for_target /musl/hackbench
    install_la_hackbench_wrapper_for_target /glibc/hackbench
}

install_musl_loader_paths() {
    bb mkdir -p /etc 2>/dev/null || true
    write_file_lines /etc/ld-musl-riscv64.path /lib /usr/lib /musl/lib
    write_file_lines /etc/ld-musl-riscv64-sf.path /lib /usr/lib /musl/lib
    write_file_lines /etc/ld-musl-loongarch-lp64d.path /lib /usr/lib /musl/lib
}

install_useradd_tool() {
    [ -x /usr/sbin/useradd ] && return 0
    ensure_executable_script /usr/sbin/useradd '#!/bin/sh
set -e

home=""
uid=""
gid=""
shell="/bin/sh"
create_home=0

while [ $# -gt 0 ]; do
    case "$1" in
        -d)
            home="$2"
            shift 2
            ;;
        -u)
            uid="$2"
            shift 2
            ;;
        -g)
            gid="$2"
            shift 2
            ;;
        -s)
            shell="$2"
            shift 2
            ;;
        -m)
            create_home=1
            shift
            ;;
        -M|-N|-r|-U|-o)
            shift
            ;;
        -c|-G|-k|-K|-p)
            shift 2
            ;;
        --)
            shift
            break
            ;;
        -*)
            echo "useradd: unsupported option $1" >&2
            exit 1
            ;;
        *)
            break
            ;;
    esac
done

name="$1"
[ -n "$name" ] || {
    echo "useradd: missing username" >&2
    exit 1
}

if grep -q "^${name}:" /etc/passwd 2>/dev/null; then
    exit 0
fi

next_id() {
    awk -F: "BEGIN { max = 999 } NF >= 3 && \$3 > max { max = \$3 } END { print max + 1 }" /etc/passwd 2>/dev/null
}

[ -n "$uid" ] || uid="$(next_id)"
[ -n "$gid" ] || gid="$uid"
[ -n "$home" ] || home="/home/$name"

mkdir -p /etc /home 2>/dev/null || true
touch /etc/passwd /etc/group 2>/dev/null || true

if ! grep -q "^${name}:" /etc/group 2>/dev/null; then
    printf "%s:x:%s:\n" "$name" "$gid" >> /etc/group
fi
printf "%s:x:%s:%s:%s:%s:%s\n" "$name" "$uid" "$gid" "$name" "$home" "$shell" >> /etc/passwd

if [ "$create_home" -eq 1 ] || [ -n "$home" ]; then
    mkdir -p "$home" 2>/dev/null || true
fi'
    if [ ! -e /usr/bin/useradd ]; then
        bb ln -sf /usr/sbin/useradd /usr/bin/useradd 2>/dev/null || true
    fi
}

install_userdel_tool() {
    [ -x /usr/sbin/userdel ] && return 0
    ensure_executable_script /usr/sbin/userdel '#!/bin/sh
set -e

remove_home=0

while [ $# -gt 0 ]; do
    case "$1" in
        -r)
            remove_home=1
            shift
            ;;
        --)
            shift
            break
            ;;
        -*)
            echo "userdel: unsupported option $1" >&2
            exit 1
            ;;
        *)
            break
            ;;
    esac
done

name="$1"
[ -n "$name" ] || {
    echo "userdel: missing username" >&2
    exit 1
}

entry="$(grep "^${name}:" /etc/passwd 2>/dev/null | tail -n 1 || true)"
[ -n "$entry" ] || exit 0

home="$(printf "%s\n" "$entry" | awk -F: "{ print \$6 }")"
tmp_passwd="/tmp/passwd.$$"
tmp_group="/tmp/group.$$"

if [ -f /etc/passwd ]; then
    grep -v "^${name}:" /etc/passwd > "$tmp_passwd" || true
    cat "$tmp_passwd" > /etc/passwd
    rm -f "$tmp_passwd"
fi

if [ -f /etc/group ]; then
    grep -v "^${name}:" /etc/group > "$tmp_group" || true
    cat "$tmp_group" > /etc/group
    rm -f "$tmp_group"
fi

if [ "$remove_home" -eq 1 ] && [ -n "$home" ] && [ "$home" != "/" ]; then
    bb rm -rf "$home" 2>/dev/null || true
fi'
    if [ ! -e /usr/bin/userdel ]; then
        bb ln -sf /usr/sbin/userdel /usr/bin/userdel 2>/dev/null || true
    fi
}

group_override_enabled() {
    flavor="$1"
    group="$2"
    machine="${OSCOMP_MACHINE:-}"
    if [ -z "$machine" ]; then
        machine="$(bb uname -m 2>/dev/null || true)"
        OSCOMP_MACHINE="$machine"
        export OSCOMP_MACHINE
    fi

    case "$group" in
        iperf)
            case "$flavor" in
                musl)
                    if runner_truthy "${OSCOMP_DISABLE_MUSL_IPERF_OVERRIDE:-0}"; then
                        return 1
                    fi
                    ;;
                glibc)
                    if runner_truthy "${OSCOMP_DISABLE_GLIBC_IPERF_OVERRIDE:-0}"; then
                        return 1
                    fi
                    ;;
            esac
            return 0
            ;;
        netperf)
            case "${machine}:${flavor}" in
                loongarch64:musl)
                    if runner_truthy "${OSCOMP_DISABLE_LA_MUSL_NETPERF_OVERRIDE:-0}"; then
                        return 1
                    fi
                    return 0
                    ;;
                loongarch64:glibc)
                    if runner_truthy "${OSCOMP_DISABLE_LA_GLIBC_NETPERF_OVERRIDE:-0}"; then
                        return 1
                    fi
                    return 0
                    ;;
            esac
            return 1
            ;;
        cyclictest)
            case "${machine}:${flavor}" in
                loongarch64:musl)
                    if runner_truthy "${OSCOMP_DISABLE_LA_MUSL_CYCLICTEST_OVERRIDE:-0}"; then
                        return 1
                    fi
                    return 0
                    ;;
                loongarch64:glibc)
                    if runner_truthy "${OSCOMP_DISABLE_LA_GLIBC_CYCLICTEST_OVERRIDE:-0}"; then
                        return 1
                    fi
                    return 0
                    ;;
            esac
            return 1
            ;;
        libctest)
            case "${machine}:${flavor}" in
                loongarch64:musl)
                    if runner_truthy "${OSCOMP_DISABLE_LA_MUSL_LIBCTEST_OVERRIDE:-0}"; then
                        return 1
                    fi
                    return 0
                    ;;
                loongarch64:glibc)
                    if runner_truthy "${OSCOMP_DISABLE_LA_GLIBC_LIBCTEST_OVERRIDE:-0}"; then
                        return 1
                    fi
                    return 0
                    ;;
            esac
            return 1
            ;;
    esac
    return 1
}

install_builtin_group_overrides() {
    OSCOMP_SUPPORT_BIN="${OSCOMP_SUPPORT_BIN:-/opt/oscomp-support/bin}"
    OSCOMP_SUPPORT_LIB="${OSCOMP_SUPPORT_LIB:-/opt/oscomp-support/lib}"
    OSCOMP_SUPPORT_GROUP_ROOT="${OSCOMP_SUPPORT_GROUP_ROOT:-/opt/oscomp-support/groups}"
    OSCOMP_SUPPORT_LTP_ROOT="${OSCOMP_SUPPORT_LTP_ROOT:-/opt/oscomp-support/ltp-cases}"
    export OSCOMP_SUPPORT_BIN OSCOMP_SUPPORT_LIB OSCOMP_SUPPORT_GROUP_ROOT OSCOMP_SUPPORT_LTP_ROOT
    bb mkdir -p "$OSCOMP_SUPPORT_BIN" "$OSCOMP_SUPPORT_LIB" \
        "$OSCOMP_SUPPORT_GROUP_ROOT" "$OSCOMP_SUPPORT_LTP_ROOT" \
        2>/dev/null || true

    ensure_executable_script "$OSCOMP_SUPPORT_GROUP_ROOT/glibc-netperf_testcode.sh" '#!/bin/sh

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
    LD_LIBRARY_PATH=/glibc/lib:/lib "$NETSERVER_BIN" -D -L "$ip" -p "$port" ${OSCOMP_NETSERVER_ARGS:-} >/dev/null 2>&1 &
    sleep "${OSCOMP_NETPERF_SERVER_WARMUP_SECS:-1}"
}

run_netperf() {
    test_name="$1"
    want_case "$test_name" || return 0

    echo "====== netperf $test_name begin ======"
    args=$(netperf_args "$test_name") || exit 1
    start_server
    if LD_LIBRARY_PATH=/glibc/lib:/lib "$NETPERF_BIN" -H "$ip" -p "$port" -t "$test_name" -l "${OSCOMP_NETPERF_LENGTH:-1}" ${OSCOMP_NETPERF_GLOBAL_ARGS:-} -- $args; then
        ans="success"
    else
        ans="fail"
    fi
    cleanup
    echo "====== netperf $test_name end: $ans ======"
}

trap cleanup EXIT INT TERM

echo "#### OS COMP TEST GROUP START netperf-glibc ####"
run_netperf UDP_STREAM
run_netperf TCP_STREAM
run_netperf UDP_RR
run_netperf TCP_RR
run_netperf TCP_CRR

cleanup
echo "#### OS COMP TEST GROUP END netperf-glibc ####"'

    ensure_executable_script "$OSCOMP_SUPPORT_GROUP_ROOT/musl-netperf_testcode.sh" '#!/bin/sh

NETPERF_BIN="${OSCOMP_NETPERF_BIN:-/musl/netperf}"
NETSERVER_BIN="${OSCOMP_NETSERVER_BIN:-/musl/netserver}"
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
    "$NETSERVER_BIN" -D -L "$ip" -p "$port" ${OSCOMP_NETSERVER_ARGS:-} >/dev/null 2>&1 &
    sleep "${OSCOMP_NETPERF_SERVER_WARMUP_SECS:-1}"
}

run_netperf() {
    test_name="$1"
    want_case "$test_name" || return 0

    echo "====== netperf $test_name begin ======"
    args=$(netperf_args "$test_name") || exit 1
    start_server
    if "$NETPERF_BIN" -H "$ip" -p "$port" -t "$test_name" -l "${OSCOMP_NETPERF_LENGTH:-1}" ${OSCOMP_NETPERF_GLOBAL_ARGS:-} -- $args; then
        ans="success"
    else
        ans="fail"
    fi
    cleanup
    echo "====== netperf $test_name end: $ans ======"
}

trap cleanup EXIT INT TERM

echo "#### OS COMP TEST GROUP START netperf-musl ####"
run_netperf UDP_STREAM
run_netperf TCP_STREAM
run_netperf UDP_RR
run_netperf TCP_RR
run_netperf TCP_CRR

cleanup
echo "#### OS COMP TEST GROUP END netperf-musl ####"'

    ensure_executable_script "$OSCOMP_SUPPORT_GROUP_ROOT/glibc-iperf_testcode.sh" '#!/bin/sh

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
echo "#### OS COMP TEST GROUP END iperf-glibc ####"'

    ensure_executable_script "$OSCOMP_SUPPORT_GROUP_ROOT/musl-iperf_testcode.sh" '#!/bin/sh

IPERF_BIN="${OSCOMP_IPERF_BIN:-/musl/iperf3}"
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
            "$IPERF_BIN" -s -p "$port" $family_args $debug_server_args &
            server_pid=$!
            echo "#### OSCOMP IPERF SERVER PID $server_pid musl ####"
            ;;
        *)
            "$IPERF_BIN" -s -p "$port" $family_args $server_args >/dev/null 2>&1 &
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
    if "$IPERF_BIN" -c "$ip" -p "$port" $family_args -t "${OSCOMP_IPERF_LENGTH:-2}" -i 0 ${OSCOMP_IPERF_GLOBAL_ARGS:-} $args; then
        ans="success"
    else
        ans="fail"
    fi
    echo "====== iperf $test_name end: $ans ======"
    echo ""
}

trap cleanup EXIT INT TERM

echo "#### OS COMP TEST GROUP START iperf-musl ####"
start_server || echo "#### OSCOMP IPERF SERVER START FAIL musl ####"
run_iperf BASIC_UDP
run_iperf BASIC_TCP
run_iperf PARALLEL_UDP
run_iperf PARALLEL_TCP
run_iperf REVERSE_UDP
run_iperf REVERSE_TCP

cleanup
echo "#### OS COMP TEST GROUP END iperf-musl ####"'

    ensure_executable_script "$OSCOMP_SUPPORT_GROUP_ROOT/glibc-cyclictest_testcode.sh" '#!/bin/sh

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

cyclictest_step_settle_rounds() {
    if [ -n "${OSCOMP_CYCLICTEST_STEP_SETTLE_ROUNDS+x}" ]; then
        printf "%s\n" "${OSCOMP_CYCLICTEST_STEP_SETTLE_ROUNDS}"
        return 0
    fi

    echo 0
}

cyclictest_control_sleep() {
    sleep_secs="$1"
    support_sleep="${OSCOMP_SUPPORT_BIN:-/opt/oscomp-support/bin}/oscomp-sleep"
    support_default_signals="${OSCOMP_SUPPORT_BIN:-/opt/oscomp-support/bin}/oscomp-default-signals"

    case "$sleep_secs" in
        ''|0)
            return 0
            ;;
    esac

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
    settle_label="$1"
    case "$settle_label" in
        NO_STRESS_P1)
            ;;
        *)
            return 0
            ;;
    esac

    settle_secs="$(cyclictest_step_settle_secs "$settle_label")"
    case "$settle_secs" in
        ''|0)
            return 0
            ;;
    esac
    debug_step "settle ${settle_secs}s after $settle_label"
    cyclictest_control_sleep "$settle_secs"
}

cyclictest_diag_delay_secs() {
    if [ -n "${OSCOMP_CYCLICTEST_DIAG_SECS+x}" ]; then
        printf "%s\n" "${OSCOMP_CYCLICTEST_DIAG_SECS}"
        return 0
    fi
    echo 0
}

cyclictest_dump_process_diag() {
    cyclictest_diag_pid="$1"
    cyclictest_diag_label="$2"

    debug_step "diag ${cyclictest_diag_label} pid ${cyclictest_diag_pid}"
    bb ps 2>/dev/null || true
    if [ -r "/proc/${cyclictest_diag_pid}/status" ]; then
        echo "#### OSCOMP CYCLICTEST DEBUG PROC STATUS ${cyclictest_diag_label} PID ${cyclictest_diag_pid} ####"
        bb sed -n '1,20p' "/proc/${cyclictest_diag_pid}/status" 2>/dev/null || true
    fi
}

launch_hackbench() {
    case "$HACKBENCH_BIN" in
        */oscomp-hackstress|oscomp-hackstress)
            musl_busybox="${OSCOMP_MUSL_BUSYBOX:-/musl/busybox}"
            if [ -x "$musl_busybox" ]; then
                OSCOMP_HACKSTRESS_WORKERS="${OSCOMP_HACKSTRESS_WORKERS:-0}" \
                    LD_LIBRARY_PATH=/musl/lib:/lib "$musl_busybox" nice -n "${OSCOMP_HACKSTRESS_NICE:-19}" "$HACKBENCH_BIN" "$@" >/dev/null 2>&1 &
            else
                OSCOMP_HACKSTRESS_WORKERS="${OSCOMP_HACKSTRESS_WORKERS:-0}" "$HACKBENCH_BIN" "$@" >/dev/null 2>&1 &
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
    if [ "${diag_delay_secs:-0}" -gt 0 ] 2>/dev/null; then
        if [ -n "$preload" ]; then
            LD_LIBRARY_PATH="$ld_path" LD_PRELOAD="$preload" "$CYCLICTEST_BIN" $args &
        else
            LD_LIBRARY_PATH="$ld_path" "$CYCLICTEST_BIN" $args &
        fi
        cyclictest_pid=$!
        debug_step "cyclictest $1 pid $cyclictest_pid"
        cyclictest_control_sleep "$diag_delay_secs"
        if kill -0 "$cyclictest_pid" 2>/dev/null; then
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

echo "#### OS COMP TEST GROUP START cyclictest-glibc ####"

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

echo "#### OS COMP TEST GROUP END cyclictest-glibc ####"'

    ensure_executable_script "$OSCOMP_SUPPORT_GROUP_ROOT/musl-cyclictest_testcode.sh" '#!/bin/sh

CYCLICTEST_BIN_DEFAULT=/musl/cyclictest
HACKBENCH_BIN="${OSCOMP_HACKBENCH_BIN:-/musl/hackbench}"
machine_name() {
    if [ -n "${OSCOMP_MACHINE:-}" ]; then
        printf "%s\n" "${OSCOMP_MACHINE}"
    else
        uname -m 2>/dev/null || true
    fi
}

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

    echo 1
}

cyclictest_step_settle_secs() {
    if [ -n "${OSCOMP_CYCLICTEST_STEP_SETTLE_SECS+x}" ]; then
        printf "%s\n" "${OSCOMP_CYCLICTEST_STEP_SETTLE_SECS}"
        return 0
    fi

    echo 0
}

cyclictest_step_settle_rounds() {
    if [ -n "${OSCOMP_CYCLICTEST_STEP_SETTLE_ROUNDS+x}" ]; then
        printf "%s\n" "${OSCOMP_CYCLICTEST_STEP_SETTLE_ROUNDS}"
        return 0
    fi

    echo 0
}

cyclictest_control_sleep() {
    sleep_secs="$1"
    support_sleep="${OSCOMP_SUPPORT_BIN:-/opt/oscomp-support/bin}/oscomp-sleep"
    support_default_signals="${OSCOMP_SUPPORT_BIN:-/opt/oscomp-support/bin}/oscomp-default-signals"

    debug_step "sleep begin ${sleep_secs}s runtime $(cyclictest_runtime_root)"

    case "$sleep_secs" in
        ''|0)
            debug_step "sleep end ${sleep_secs}s runtime skipped"
            return 0
            ;;
    esac

    if [ -x "$support_sleep" ]; then
        if [ -x "$support_default_signals" ]; then
            "$support_default_signals" "$support_sleep" "$sleep_secs"
        else
            "$support_sleep" "$sleep_secs"
        fi
        sleep_status=$?
        debug_step "sleep end ${sleep_secs}s runtime support status ${sleep_status}"
        return "$sleep_status"
    fi

    case "$(cyclictest_runtime_root)" in
        glibc)
            glibc_busybox="${OSCOMP_GLIBC_BUSYBOX:-/glibc/busybox}"
            if [ -x "$glibc_busybox" ]; then
                LD_LIBRARY_PATH=/glibc/lib:/lib "$glibc_busybox" sleep "$sleep_secs"
                sleep_status=$?
                debug_step "sleep end ${sleep_secs}s runtime glibc status ${sleep_status}"
                return "$sleep_status"
            fi
            ;;
    esac

    musl_busybox="${OSCOMP_MUSL_BUSYBOX:-/musl/busybox}"
    if [ -x "$musl_busybox" ]; then
        LD_LIBRARY_PATH=/musl/lib:/lib "$musl_busybox" sleep "$sleep_secs"
        sleep_status=$?
        debug_step "sleep end ${sleep_secs}s runtime musl status ${sleep_status}"
        return "$sleep_status"
    fi

    sleep "$sleep_secs"
    sleep_status=$?
    debug_step "sleep end ${sleep_secs}s runtime shell status ${sleep_status}"
    return "$sleep_status"
}

cyclictest_true_once() {
    # Do not fork/exec here. This runs between cyclictest phases, and creating
    # hundreds of short-lived busybox processes can starve or pollute the next
    # phase on LA.
    [ -d /proc/1 ] >/dev/null 2>&1 || true
}

cyclictest_control_yield_rounds() {
    rounds="$1"
    case "$rounds" in
        ''|0)
            return 0
            ;;
    esac

    debug_step "yield rounds ${rounds}"
    round_i=0
    while [ "$round_i" -lt "$rounds" ] 2>/dev/null; do
        cyclictest_true_once
        round_i=$((round_i + 1))
    done
}

cyclictest_step_settle() {
    settle_label="$1"
    case "$settle_label" in
        NO_STRESS_P1)
            ;;
        *)
            return 0
            ;;
    esac

    settle_rounds="$(cyclictest_step_settle_rounds "$settle_label")"
    cyclictest_control_yield_rounds "$settle_rounds"

    settle_secs="$(cyclictest_step_settle_secs "$settle_label")"
    case "$settle_secs" in
        ''|0)
            return 0
            ;;
    esac
    debug_step "settle ${settle_secs}s after $settle_label"
    cyclictest_control_sleep "$settle_secs"
}

launch_hackbench() {
    case "$HACKBENCH_BIN" in
        */oscomp-hackstress|oscomp-hackstress)
            musl_busybox="${OSCOMP_MUSL_BUSYBOX:-/musl/busybox}"
            if [ -x "$musl_busybox" ]; then
                OSCOMP_HACKSTRESS_WORKERS="${OSCOMP_HACKSTRESS_WORKERS:-0}" \
                    LD_LIBRARY_PATH=/musl/lib:/lib "$musl_busybox" nice -n "${OSCOMP_HACKSTRESS_NICE:-19}" "$HACKBENCH_BIN" "$@" >/dev/null 2>&1 &
            else
                OSCOMP_HACKSTRESS_WORKERS="${OSCOMP_HACKSTRESS_WORKERS:-0}" "$HACKBENCH_BIN" "$@" >/dev/null 2>&1 &
            fi
            return 0
            ;;
    esac

    musl_busybox="${OSCOMP_MUSL_BUSYBOX:-/musl/busybox}"
    if [ -x "$musl_busybox" ]; then
        LD_LIBRARY_PATH=/musl/lib:/lib "$musl_busybox" nice -n 19 "$HACKBENCH_BIN" "$@" &
        return 0
    fi

    LD_LIBRARY_PATH=/musl/lib:/lib "$HACKBENCH_BIN" "$@" &
}

cyclictest_preload() {
    runtime_root="$(cyclictest_runtime_root)"
    compat_root="${OSCOMP_SUPPORT_LIB:-/opt/oscomp-support/lib}"
    case "$runtime_root" in
        glibc)
            compat_lib="${compat_root}/liboscomp-glibc-compat.so"
            [ -f "$compat_lib" ] && printf "%s\n" "$compat_lib"
            ;;
        *)
            compat_lib="${compat_root}/liboscomp-musl-compat.so"
            if [ -f "$compat_lib" ]; then
                case ":${LD_PRELOAD:-}:" in
                    *":${compat_lib}:"*)
                        printf "%s\n" "${LD_PRELOAD:-$compat_lib}"
                        ;;
                    *)
                        if [ -n "${LD_PRELOAD:-}" ]; then
                            printf "%s\n" "${compat_lib}:$LD_PRELOAD"
                        else
                            printf "%s\n" "$compat_lib"
                        fi
                        ;;
                esac
            fi
            ;;
    esac
}

cyclictest_runtime_root() {
    case "$CYCLICTEST_BIN" in
        /glibc/*)
            echo glibc
            ;;
        *)
            echo musl
            ;;
    esac
}

cyclictest_ld_path() {
    case "$(cyclictest_runtime_root)" in
        glibc)
            echo /glibc/lib:/lib
            ;;
        *)
            echo /musl/lib:/lib
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
    if [ -n "$preload" ]; then
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

echo "#### OS COMP TEST GROUP START cyclictest-musl ####"
debug_step "cyclictest bin ${CYCLICTEST_BIN} runtime $(cyclictest_runtime_root)"

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

echo "#### OS COMP TEST GROUP END cyclictest-musl ####"'

    ensure_executable_script "$OSCOMP_SUPPORT_GROUP_ROOT/glibc-cyclictest_testcode.sh" '#!/bin/sh

SOURCE_SCRIPT="${OSCOMP_SUPPORT_GROUP_ROOT:-/opt/oscomp-support/groups}/musl-cyclictest_testcode.sh"
TMP_SCRIPT="/tmp/oscomp-glibc-cyclictest-$$.sh"
MUSL_BUSYBOX="${OSCOMP_MUSL_BUSYBOX:-/musl/busybox}"

if [ ! -f "$SOURCE_SCRIPT" ] || [ ! -x "$MUSL_BUSYBOX" ]; then
    echo "#### OS COMP TEST GROUP START cyclictest-glibc ####"
    echo "====== cyclictest NO_STRESS_P1 begin ======"
    echo "====== cyclictest NO_STRESS_P1 end: fail ======"
    echo "#### OS COMP TEST GROUP END cyclictest-glibc ####"
    exit 1
fi

"$MUSL_BUSYBOX" sed "s/cyclictest-musl/cyclictest-glibc/g" "$SOURCE_SCRIPT" > "$TMP_SCRIPT"
"$MUSL_BUSYBOX" chmod +x "$TMP_SCRIPT" 2>/dev/null || chmod +x "$TMP_SCRIPT"
OSCOMP_CYCLICTEST_BIN="${OSCOMP_CYCLICTEST_BIN:-/glibc/cyclictest}"
export OSCOMP_CYCLICTEST_BIN
"$MUSL_BUSYBOX" sh "$TMP_SCRIPT"
status=$?
"$MUSL_BUSYBOX" rm -f "$TMP_SCRIPT" 2>/dev/null || rm -f "$TMP_SCRIPT"
exit "$status"'
}

support_disk_has_payload() {
    support_mount="$1"
    support_arch_dir="$2"

    [ -f "$support_mount/meta/ltp_test.txt" ] && return 0
    [ -f "$support_mount/meta/oscomp.env" ] && return 0
    [ -f "$support_mount/meta/oscomp_plan.txt" ] && return 0
    [ -d "$support_mount/usr/lib/locale/C.UTF-8" ] && return 0
    [ -d "$support_mount/overlay" ] && return 0
    if [ -n "$support_arch_dir" ]; then
        [ -d "$support_mount/$support_arch_dir/overlay" ] && return 0
        [ -f "$support_mount/$support_arch_dir/glibc/lib/libgcc_s.so.1" ] && return 0
        [ -f "$support_mount/$support_arch_dir/musl/lib/libgcc_s.so.1" ] && return 0
    fi
    return 1
}

try_mount_support_disk() {
    support_arch_dir="$1"
    SUPPORT_DISK_DEVICE=""

    for dev in \
        /dev/vdb /dev/vdc /dev/vdd /dev/vde \
        /dev/sdb /dev/sdc /dev/sdd /dev/sde \
        /dev/hdb /dev/hdc /dev/hdd /dev/hde \
        /dev/vda /dev/sda /dev/hda
    do
        [ -e "$dev" ] || continue
        if ! bb mount -t ext4 -o ro "$dev" /support >/dev/null 2>&1; then
            continue
        fi
        if support_disk_has_payload /support "$support_arch_dir"; then
            SUPPORT_DISK_DEVICE="$dev"
            return 0
        fi
        bb umount /support >/dev/null 2>&1 || true
    done

    return 1
}

mount_support_disk() {
    support_arch_dir=""
    case "$(bb uname -m 2>/dev/null || true)" in
        riscv64)
            support_arch_dir=rv
            ;;
        loongarch64)
            support_arch_dir=la
            ;;
    esac

    bb mkdir -p /support 2>/dev/null || true
    if try_mount_support_disk "$support_arch_dir"; then
        OSCOMP_SUPPORT_ARCH_DIR="$support_arch_dir"
        export OSCOMP_SUPPORT_ARCH_DIR
        if [ -f /support/meta/ltp_test.txt ]; then
            bb mkdir -p /etc/oscomp-ltp 2>/dev/null || true
            bb cp /support/meta/ltp_test.txt /etc/oscomp-ltp/ltp_test.txt 2>/dev/null || true
        fi
        if [ -f /support/meta/oscomp_plan.txt ]; then
            bb cp /support/meta/oscomp_plan.txt /etc/oscomp-plan.txt 2>/dev/null || true
        fi
        if [ -f /support/meta/oscomp.env ]; then
            bb cp /support/meta/oscomp.env /etc/oscomp.env 2>/dev/null || true
            load_oscomp_env
        fi
        stage_support_runtime_payload_from_mount "$support_arch_dir"
        bb umount /support >/dev/null 2>&1 || true
        bb rmdir /support >/dev/null 2>&1 || true
    else
        bb rmdir /support >/dev/null 2>&1 || true
    fi
}

stage_support_runtime_payload_from_mount() {
    support_arch_dir="$1"

    support_glibc_libgcc=""
    support_musl_libgcc=""
    for candidate in \
        "/support/${support_arch_dir}/glibc/lib/libgcc_s.so.1" \
        "/support/glibc/lib/libgcc_s.so.1"
    do
        [ -n "$candidate" ] || continue
        if [ -f "$candidate" ]; then
            support_glibc_libgcc="$candidate"
            break
        fi
    done
    for candidate in \
        "/support/${support_arch_dir}/musl/lib/libgcc_s.so.1" \
        "/support/musl/lib/libgcc_s.so.1"
    do
        [ -n "$candidate" ] || continue
        if [ -f "$candidate" ]; then
            support_musl_libgcc="$candidate"
            break
        fi
    done
    if [ -n "$support_glibc_libgcc" ]; then
        bb mkdir -p /glibc/lib 2>/dev/null || true
        bb cp "$support_glibc_libgcc" /glibc/lib/libgcc_s.so.1 2>/dev/null || true
    fi
    if [ -n "$support_musl_libgcc" ]; then
        bb mkdir -p /musl/lib /lib 2>/dev/null || true
        bb cp "$support_musl_libgcc" /musl/lib/libgcc_s.so.1 2>/dev/null || true
        bb cp "$support_musl_libgcc" /lib/libgcc_s.so.1 2>/dev/null || true
    fi

    if [ -d /support/usr/lib/locale/C.UTF-8 ]; then
        bb mkdir -p /usr/lib/locale/C.UTF-8 2>/dev/null || true
        bb cp -a /support/usr/lib/locale/C.UTF-8/. /usr/lib/locale/C.UTF-8/ 2>/dev/null || true
        OSCOMP_SUPPORT_LOCPATH=/usr/lib/locale
        export OSCOMP_SUPPORT_LOCPATH
    fi

    support_overlay_root=""
    if [ -n "$support_arch_dir" ] && [ -d "/support/${support_arch_dir}/overlay" ]; then
        support_overlay_root="/support/${support_arch_dir}/overlay"
    elif [ -d /support/overlay ]; then
        support_overlay_root=/support/overlay
    fi
    if [ -n "$support_overlay_root" ]; then
        OSCOMP_SUPPORT_BIN=/opt/oscomp-support/bin
        OSCOMP_SUPPORT_LIB=/opt/oscomp-support/lib
        OSCOMP_SUPPORT_GROUP_ROOT=/opt/oscomp-support/groups
        OSCOMP_SUPPORT_LTP_ROOT=/opt/oscomp-support/ltp-cases
        export OSCOMP_SUPPORT_BIN OSCOMP_SUPPORT_LIB OSCOMP_SUPPORT_GROUP_ROOT OSCOMP_SUPPORT_LTP_ROOT
        bb mkdir -p "$OSCOMP_SUPPORT_BIN" "$OSCOMP_SUPPORT_LIB" \
            "$OSCOMP_SUPPORT_GROUP_ROOT" "$OSCOMP_SUPPORT_LTP_ROOT" 2>/dev/null || true
        copy_dir_entries_to_dir "$support_overlay_root/bin" "$OSCOMP_SUPPORT_BIN"
        copy_dir_entries_to_dir "$support_overlay_root/lib" "$OSCOMP_SUPPORT_LIB"
        copy_dir_entries_to_dir "$support_overlay_root/musl/lib" "$OSCOMP_SUPPORT_LIB"
        copy_tree_entries_to_dir "$support_overlay_root/ltp-cases" "$OSCOMP_SUPPORT_LTP_ROOT"
        copy_dir_entries_to_dir "$support_overlay_root/group-scripts" "$OSCOMP_SUPPORT_GROUP_ROOT"
    fi

    OSCOMP_SUPPORT_RUNTIME_STAGED=1
    export OSCOMP_SUPPORT_RUNTIME_STAGED
}

support_runtime_payload_ready() {
    root="$1"
    group="$2"

    need_glibc_runtime=0
    need_ltp_runtime=0
    need_group_override_runtime=0
    support_flavor="${root#/}"

    case "$root" in
        /glibc)
            need_glibc_runtime=1
            ;;
    esac
    case "$group" in
        ltp)
            need_ltp_runtime=1
            ;;
        netperf|cyclictest|iperf|libctest)
            if group_override_enabled "$support_flavor" "$group"; then
                if [ "$group" = "iperf" ]; then
                    need_group_override_runtime=1
                else
                OSCOMP_SUPPORT_GROUP_ROOT="${OSCOMP_SUPPORT_GROUP_ROOT:-/opt/oscomp-support/groups}"
                for candidate in \
                    "$OSCOMP_SUPPORT_GROUP_ROOT/${support_flavor}-${group}_testcode.sh" \
                    "$OSCOMP_SUPPORT_GROUP_ROOT/${support_flavor}-${group}.sh"
                do
                    if [ -f "$candidate" ]; then
                        need_group_override_runtime=1
                        break
                    fi
                done
                fi
            fi
            ;;
    esac
    if runner_truthy "${OSCOMP_KEEP_LA_GROUP_SCRIPTS:-0}"; then
        need_group_override_runtime=1
    fi

    if [ "$need_glibc_runtime" -eq 1 ] && {
        [ ! -f /glibc/lib/libgcc_s.so.1 ] || [ ! -d /usr/lib/locale/C.UTF-8 ];
    }; then
        return 1
    fi

    if [ "$need_ltp_runtime" -eq 1 ] || [ "$need_group_override_runtime" -eq 1 ]; then
        OSCOMP_SUPPORT_BIN="${OSCOMP_SUPPORT_BIN:-/opt/oscomp-support/bin}"
        OSCOMP_SUPPORT_LIB="${OSCOMP_SUPPORT_LIB:-/opt/oscomp-support/lib}"
        OSCOMP_SUPPORT_GROUP_ROOT="${OSCOMP_SUPPORT_GROUP_ROOT:-/opt/oscomp-support/groups}"
        export OSCOMP_SUPPORT_BIN OSCOMP_SUPPORT_LIB OSCOMP_SUPPORT_GROUP_ROOT

        [ -d "$OSCOMP_SUPPORT_BIN" ] || return 1
        [ -d "$OSCOMP_SUPPORT_LIB" ] || return 1
        if [ "$need_group_override_runtime" -eq 1 ]; then
            for candidate in \
                "$OSCOMP_SUPPORT_GROUP_ROOT/${support_flavor}-${group}_testcode.sh" \
                "$OSCOMP_SUPPORT_GROUP_ROOT/${support_flavor}-${group}.sh"
            do
                [ -f "$candidate" ] && return 0
            done
            return 1
        fi
    fi

    return 0
}

ensure_support_runtime_payload() {
    root="$1"
    group="$2"

    need_glibc_runtime=0
    need_ltp_runtime=0
    need_group_override_runtime=0
    runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD PROBE ${root}/${group} CASE ROOT ####"

    case "$root" in
        /glibc)
            need_glibc_runtime=1
            ;;
    esac
    runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD PROBE ${root}/${group} CASE ROOT DONE ${need_glibc_runtime} ####"
    runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD PROBE ${root}/${group} CASE GROUP ####"
    case "$group" in
        ltp)
            need_ltp_runtime=1
            ;;
        netperf|cyclictest|iperf|libctest)
            runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD PROBE ${root}/${group} OVERRIDE CHECK BEGIN ####"
            if group_override_enabled "${root#/}" "$group"; then
                runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD PROBE ${root}/${group} OVERRIDE CHECK ENABLED ####"
                if [ "$group" = "iperf" ]; then
                    need_group_override_runtime=1
                    runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD PROBE ${root}/${group} OVERRIDE EAGER 1 ####"
                else
                    support_flavor="${root#/}"
                    OSCOMP_SUPPORT_GROUP_ROOT="${OSCOMP_SUPPORT_GROUP_ROOT:-/opt/oscomp-support/groups}"
                    runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD PROBE ${root}/${group} OVERRIDE LOOP ROOT ${OSCOMP_SUPPORT_GROUP_ROOT} ####"
                    for candidate in \
                        "$OSCOMP_SUPPORT_GROUP_ROOT/${support_flavor}-${group}_testcode.sh" \
                        "$OSCOMP_SUPPORT_GROUP_ROOT/${support_flavor}-${group}.sh"
                    do
                        runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD PROBE ${root}/${group} OVERRIDE CANDIDATE ${candidate} ####"
                        if [ -f "$candidate" ]; then
                            need_group_override_runtime=1
                            runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD PROBE ${root}/${group} OVERRIDE CANDIDATE HIT ${candidate} ####"
                            break
                        fi
                    done
                    runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD PROBE ${root}/${group} OVERRIDE LOOP DONE ${need_group_override_runtime} ####"
                fi
            else
                runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD PROBE ${root}/${group} OVERRIDE CHECK DISABLED ####"
            fi
            ;;
    esac
    runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD PROBE ${root}/${group} CASE GROUP DONE LTP ${need_ltp_runtime} OVERRIDE ${need_group_override_runtime} ####"
    if runner_truthy "${OSCOMP_KEEP_LA_GROUP_SCRIPTS:-0}"; then
        need_group_override_runtime=1
        runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD PROBE ${root}/${group} KEEP GROUP SCRIPTS FORCED ####"
    fi

    runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD NEED ${root}/${group} GLIBC ${need_glibc_runtime} LTP ${need_ltp_runtime} OVERRIDE ${need_group_override_runtime} ####"

    if [ "$need_glibc_runtime" -eq 0 ] && [ "$need_ltp_runtime" -eq 0 ] && [ "$need_group_override_runtime" -eq 0 ]; then
        runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD SKIP ${root}/${group} ####"
        return 0
    fi

    runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD STAT PRE ${root}/${group} ####"
    glibc_libgcc_ready=0
    locale_ready=0
    [ -f /glibc/lib/libgcc_s.so.1 ] && glibc_libgcc_ready=1
    [ -d /usr/lib/locale/C.UTF-8 ] && locale_ready=1
    runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD STAT POST ${root}/${group} ####"
    runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD STATE ${root}/${group} STAGED ${OSCOMP_SUPPORT_RUNTIME_STAGED:-0} GLIBC_LIBGCC ${glibc_libgcc_ready} LOCALE ${locale_ready} GROUP_ROOT ${OSCOMP_SUPPORT_GROUP_ROOT:-} ####"
    if [ "${OSCOMP_SUPPORT_RUNTIME_STAGED:-0}" = "1" ]; then
        runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD EARLY RETURN STAGED ${root}/${group} ####"
        return 0
    fi

    if support_runtime_payload_ready "$root" "$group"; then
        runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD READY ${root}/${group} ####"
        return 0
    fi

    support_arch_dir="${OSCOMP_SUPPORT_ARCH_DIR:-}"
    if [ -z "$support_arch_dir" ]; then
        case "$(bb uname -m 2>/dev/null || true)" in
            riscv64)
                support_arch_dir=rv
                ;;
            loongarch64)
                support_arch_dir=la
                ;;
        esac
    fi

    runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD MOUNT TRY ${root}/${group} ARCH ${support_arch_dir:-} ####"
    bb mkdir -p /support 2>/dev/null || true
    if ! try_mount_support_disk "$support_arch_dir"; then
        runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD MOUNT FAILED ${root}/${group} ####"
        bb rmdir /support >/dev/null 2>&1 || true
        return 0
    fi
    runner_debug "#### OSCOMP RUNNER SUPPORT PAYLOAD MOUNT OK ${root}/${group} ####"
    stage_support_runtime_payload_from_mount "$support_arch_dir"
    if [ "$need_group_override_runtime" -eq 1 ]; then
        runner_debug "#### OSCOMP RUNNER SUPPORT GROUP ROOT ${OSCOMP_SUPPORT_GROUP_ROOT:-/opt/oscomp-support/groups} ####"
    fi

    bb umount /support >/dev/null 2>&1 || true
    bb rmdir /support >/dev/null 2>&1 || true
}

support_group_override_script() {
    flavor="$1"
    group="$2"
    SUPPORT_GROUP_OVERRIDE_SCRIPT=""
    [ -n "${OSCOMP_SUPPORT_GROUP_ROOT:-}" ] || return 1
    if ! group_override_enabled "$flavor" "$group"; then
        runner_debug "#### OSCOMP RUNNER SUPPORT GROUP OVERRIDE DISABLED ${flavor}-${group} ####"
        return 1
    fi

    for candidate in \
        "$OSCOMP_SUPPORT_GROUP_ROOT/${flavor}-${group}_testcode.sh" \
        "$OSCOMP_SUPPORT_GROUP_ROOT/${flavor}-${group}.sh"
    do
        if [ -f "$candidate" ]; then
            SUPPORT_GROUP_OVERRIDE_SCRIPT="$candidate"
            runner_debug "#### OSCOMP RUNNER SUPPORT GROUP OVERRIDE FOUND ${candidate} ####"
            return 0
        fi
    done
    runner_debug "#### OSCOMP RUNNER SUPPORT GROUP OVERRIDE MISSING ${flavor}-${group} ####"
    return 1
}

load_oscomp_env() {
    [ -f /etc/oscomp.env ] || return 0

    # Optional local debug/config override injected via support disk.
    while IFS= read -r env_line || [ -n "$env_line" ]; do
        case "$env_line" in
            ''|'#'*)
                continue
                ;;
            *=*)
                export "$env_line"
                ;;
        esac
    done </etc/oscomp.env
}

apply_la_cyclictest_defaults() {
    case "$(bb uname -m 2>/dev/null || true)" in
        loongarch64)
            # LoongArch cyclictest can stall on the default absolute
            # clock_nanosleep path. Keep the official cases intact but use
            # cyclictest's relative timer mode for all LA runs.
            if [ -z "${OSCOMP_CYCLICTEST_ARGS_NO_STRESS_P1+x}" ]; then
                OSCOMP_CYCLICTEST_ARGS_NO_STRESS_P1='-a -i 1000 -t1 -p99 -D 1s -q -r'
                export OSCOMP_CYCLICTEST_ARGS_NO_STRESS_P1
            fi
            if [ -z "${OSCOMP_CYCLICTEST_ARGS_STRESS_P1+x}" ]; then
                OSCOMP_CYCLICTEST_ARGS_STRESS_P1='-a -i 1000 -t1 -p99 -D 1s -q -r'
                export OSCOMP_CYCLICTEST_ARGS_STRESS_P1
            fi
            if [ -z "${OSCOMP_CYCLICTEST_ARGS_NO_STRESS_P8+x}" ]; then
                OSCOMP_CYCLICTEST_ARGS_NO_STRESS_P8='-i 1000 -t8 -p99 -D 1s -q -r'
                export OSCOMP_CYCLICTEST_ARGS_NO_STRESS_P8
            fi
            if [ -z "${OSCOMP_CYCLICTEST_ARGS_STRESS_P8+x}" ]; then
                OSCOMP_CYCLICTEST_ARGS_STRESS_P8='-i 1000 -t8 -p99 -D 1s -q -r'
                export OSCOMP_CYCLICTEST_ARGS_STRESS_P8
            fi
            if [ -z "${OSCOMP_CYCLICTEST_HACKBENCH_WARMUP_SECS+x}" ]; then
                OSCOMP_CYCLICTEST_HACKBENCH_WARMUP_SECS=0
                export OSCOMP_CYCLICTEST_HACKBENCH_WARMUP_SECS
            fi
            if [ -z "${OSCOMP_CYCLICTEST_STEP_SETTLE_ROUNDS+x}" ]; then
                OSCOMP_CYCLICTEST_STEP_SETTLE_ROUNDS=0
                export OSCOMP_CYCLICTEST_STEP_SETTLE_ROUNDS
            fi
            if [ -z "${OSCOMP_CYCLICTEST_ASYNC_WAIT_SECS+x}" ]; then
                OSCOMP_CYCLICTEST_ASYNC_WAIT_SECS=3
                export OSCOMP_CYCLICTEST_ASYNC_WAIT_SECS
            fi
            ;;
    esac
}

refresh_support_runtime_stage_after_env() {
    install_la_hackbench_wrapper
    apply_la_cyclictest_defaults
}

retag_group_script_markers_file() {
    script_path="$1"
    marker_group="$2"
    marker_flavor="$3"
    [ -f "$script_path" ] || return 0

    start_plain="#### OS COMP TEST GROUP START ${marker_group} ####"
    end_plain="#### OS COMP TEST GROUP END ${marker_group} ####"
    start_flavored="#### OS COMP TEST GROUP START ${marker_group}-${marker_flavor} ####"
    end_flavored="#### OS COMP TEST GROUP END ${marker_group}-${marker_flavor} ####"
    temp_path="/tmp/oscomp-retag-${marker_group}-${marker_flavor}-$$.tmp"

    bb sed \
        -e "s|${start_plain}|${start_flavored}|g" \
        -e "s|${end_plain}|${end_flavored}|g" \
        "$script_path" >"$temp_path" 2>/dev/null || {
        bb rm -f "$temp_path" 2>/dev/null || true
        return 0
    }
    cat "$temp_path" >"$script_path"
    bb rm -f "$temp_path" 2>/dev/null || true
}

retag_root_group_scripts() {
    for root_dir in /glibc /musl; do
        [ -d "$root_dir" ] || continue
        root_flavor_name="${root_dir#/}"
        for script_path in "$root_dir"/*_testcode.sh; do
            [ -f "$script_path" ] || continue
            script_name="${script_path##*/}"
            group_name="${script_name%_testcode.sh}"
            retag_group_script_markers_file "$script_path" "$group_name" "$root_flavor_name"
        done
    done
}

retag_support_group_scripts() {
    [ -n "${OSCOMP_SUPPORT_GROUP_ROOT:-}" ] || return 0
    [ -d "$OSCOMP_SUPPORT_GROUP_ROOT" ] || return 0

    for script_path in "$OSCOMP_SUPPORT_GROUP_ROOT"/*_testcode.sh; do
        [ -f "$script_path" ] || continue
        script_name="${script_path##*/}"
        support_flavor_name="${script_name%%-*}"
        case "$support_flavor_name" in
            glibc|musl)
                ;;
            *)
                continue
                ;;
        esac
        group_name="${script_name#*-}"
        group_name="${group_name%_testcode.sh}"
        retag_group_script_markers_file "$script_path" "$group_name" "$support_flavor_name"
    done
}

retag_pre2025_group_scripts() {
    retag_root_group_scripts
    retag_support_group_scripts
}

run_pre2025_init_sequence() {
    # Mirror the visible setup order of the official pre-2025 testcase layout.
    cd / || return 0

    bb mkdir -p /bin 2>/dev/null || true
    if ! pick_busybox_for_root /musl; then
        runner_debug "#### OSCOMP RUNNER MISSING SHELL /musl/busybox ####"
        return 1
    fi
    install_runtime_alias "$PICK_BUSYBOX_FOR_ROOT_RESULT" /bin/busybox || true
    /bin/busybox --install -s /bin >/dev/null 2>&1 || true
    [ -e /busybox ] || bb ln -sf /bin/busybox /busybox 2>/dev/null || true
    [ -e /bin/sh ] || bb ln -sf /bin/busybox /bin/sh 2>/dev/null || true
    [ -e /bin/ash ] || bb ln -sf /bin/busybox /bin/ash 2>/dev/null || true
    install_bash_compat
    if [ ! -e /usr/bin/env ]; then
        bb mkdir -p /usr/bin 2>/dev/null || true
        bb ln -sf /bin/env /usr/bin/env 2>/dev/null || true
    fi

    bb mkdir -p /lib 2>/dev/null || true
    mirror_dir_entries_to_dir /musl/lib /lib
    install_runtime_alias /glibc/lib/libc.so.6 /lib/libc.so.6 || true
    install_runtime_alias /glibc/lib/libm.so.6 /lib/libm.so.6 || true
    install_runtime_alias /musl/lib/libc.so /lib/ld-musl-loongarch-lp64d.so.1 || true
    install_runtime_alias /glibc/lib/ld-linux-loongarch-lp64d.so.1 /lib/ld-linux-loongarch-lp64d.so.1 || true
    install_runtime_alias /musl/lib/libc.so /lib/ld-musl-riscv64.so.1 || true
    install_runtime_alias /musl/lib/libc.so /lib/ld-musl-riscv64-sf.so.1 || true
    install_runtime_alias /glibc/lib/ld-linux-riscv64-lp64d.so.1 /lib/ld-linux-riscv64-lp64d.so.1 || true

    bb rm -rf /lib64 2>/dev/null || true
    bb ln -sf /lib /lib64 2>/dev/null || true
    bb mkdir -p /usr 2>/dev/null || true
    bb rm -rf /usr/lib64 2>/dev/null || true
    bb ln -sf /lib /usr/lib64 2>/dev/null || true

    bb mkdir -p /etc /var /var/log /var/run /run /tmp /root /home /mnt 2>/dev/null || true
    write_file_lines /etc/passwd \
        "root:x:0:0:root:/root:/bin/sh" \
        "nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin"
    write_file_lines /etc/group \
        "root:x:0:" \
        "daemon:x:1:" \
        "users:x:100:" \
        "nobody:x:65534:"
    if [ ! -s /etc/resolv.conf ]; then
        write_file_lines /etc/resolv.conf "nameserver 8.8.8.8"
    fi

    clear_dir_contents /var/tmp
    clear_dir_contents /tmp
    bb mkdir -p /tmp/memfd 2>/dev/null || true

    install_locale_tool
    install_systemd_detect_virt_tool
    install_musl_loader_paths
    install_useradd_tool
    install_userdel_tool
    install_builtin_group_overrides
    mount_support_disk
    retag_pre2025_group_scripts
    load_oscomp_env
    seed_oscomp_machine
    seed_oscomp_shells
    refresh_support_runtime_stage_after_env
}

prepare_ltp_env() {
    bb mkdir -p /etc/oscomp-ltp 2>/dev/null || true
    bb mkdir -p /lib/modules/10.0.0/build /lib/modules/10.0.0+/build 2>/dev/null || true
    ensure_file_line /lib/modules/10.0.0/build/.config "CONFIG_EVENTFD=y"
    ensure_file_line /lib/modules/10.0.0+/build/.config "CONFIG_EVENTFD=y"
    [ -f /lib/modules/10.0.0/modules.dep ] || : >/lib/modules/10.0.0/modules.dep 2>/dev/null || true
    [ -f /lib/modules/10.0.0/modules.builtin ] || : >/lib/modules/10.0.0/modules.builtin 2>/dev/null || true
    [ -f /lib/modules/10.0.0+/modules.dep ] || : >/lib/modules/10.0.0+/modules.dep 2>/dev/null || true
    [ -f /lib/modules/10.0.0+/modules.builtin ] || : >/lib/modules/10.0.0+/modules.builtin 2>/dev/null || true
    patch_ltp_shell_harness
}

patch_ltp_shell_harness() {
    bb mkdir -p /var/tmp 2>/dev/null || true

    for ltp_root in /musl /glibc; do
        [ -d "$ltp_root" ] || continue
        ltp_name="${ltp_root#/}"

        for harness in $(bb find "$ltp_root" -name tst_test.sh 2>/dev/null || true); do
            [ -f "$harness" ] || continue

            patched_harness="/var/tmp/oscomp-tst-test-${ltp_name}-$$.sh"
            bb sed \
                -e 's/eval "local timeout=/eval "timeout=/' \
                -e 's/local tst_sec=/tst_sec=/' \
                -e 's/local sec=$TST_TIMEOUT/sec=$TST_TIMEOUT/' \
                "$harness" > "$patched_harness" 2>/dev/null || {
                    runner_debug "#### OSCOMP RUNNER LTP HARNESS PATCH SED FAILED ${harness} ####"
                    bb rm -f "$patched_harness" 2>/dev/null || true
                    continue
                }
            if ! bb cp "$patched_harness" "$harness" 2>/dev/null; then
                runner_debug "#### OSCOMP RUNNER LTP HARNESS PATCH COPY FAILED ${harness} ####"
            else
                runner_debug "#### OSCOMP RUNNER LTP HARNESS PATCHED ${harness} ####"
                if runner_truthy "${OSCOMP_RUNNER_DEBUG:-0}"; then
                    bb grep -n "timeout=.\\$\\|tst_sec=.*1000000\\|sec=.TST_TIMEOUT\\|local sec=.TST_TIMEOUT" "$harness" 2>/dev/null || true
                fi
            fi
            bb rm -f "$patched_harness" 2>/dev/null || true
        done
    done
}

find_ltp_case_override() {
    root="$1"
    testcase="$2"
    LTP_CASE_OVERRIDE_PATH=""

    [ -n "${OSCOMP_SUPPORT_LTP_ROOT:-}" ] || return 1
    [ -d "$OSCOMP_SUPPORT_LTP_ROOT" ] || return 1

    case "$root" in
        /musl)
            ltp_flavor=musl
            ;;
        /glibc)
            ltp_flavor=glibc
            ;;
        *)
            return 1
            ;;
    esac

    ltp_machine="$(runner_machine_quiet)"
    for candidate in \
        "$OSCOMP_SUPPORT_LTP_ROOT/$ltp_machine/$ltp_flavor/$testcase" \
        "$OSCOMP_SUPPORT_LTP_ROOT/$ltp_flavor/$testcase" \
        "$OSCOMP_SUPPORT_LTP_ROOT/$ltp_machine/$testcase" \
        "$OSCOMP_SUPPORT_LTP_ROOT/$testcase"
    do
        [ -f "$candidate" ] || continue
        [ -x "$candidate" ] || continue
        LTP_CASE_OVERRIDE_PATH="$candidate"
        return 0
    done

    return 1
}

support_ltp_subset_dir() {
    SUPPORT_LTP_TEST_LIST=""
    for candidate in /etc/oscomp-ltp/ltp_test.txt /support/meta/ltp_test.txt; do
        [ -f "$candidate" ] || continue
        SUPPORT_LTP_TEST_LIST="$candidate"
        return 0
    done
    return 1
}

build_ltp_fallback_test_list() {
    fallback_list="/var/tmp/oscomp-ltp-autolist.$$"
    bb rm -f "$fallback_list" 2>/dev/null || true
    : > "$fallback_list" || return 1

    for testcase_path in ltp/testcases/bin/*; do
        [ -f "$testcase_path" ] || continue
        bb basename "$testcase_path" >> "$fallback_list"
    done

    if [ -s "$fallback_list" ]; then
        SUPPORT_LTP_TEST_LIST="$fallback_list"
        return 0
    fi

    bb rm -f "$fallback_list" 2>/dev/null || true
    SUPPORT_LTP_TEST_LIST=""
    return 1
}

run_ltp_group() {
    root="$1"
    shell_path="$2"
    support_ltp_subset_dir || true
    test_list="$SUPPORT_LTP_TEST_LIST"
    generated_test_list=""

    [ -d "$root/ltp/testcases" ] || {
        runner_debug "#### OSCOMP RUNNER MISSING LTP TESTCASES ${root}/ltp/testcases ####"
        return 127
    }

    if ! cd "$root"; then
        return 125
    fi

    if [ -z "$test_list" ]; then
        if build_ltp_fallback_test_list; then
            test_list="$SUPPORT_LTP_TEST_LIST"
            generated_test_list="$test_list"
        else
            runner_debug "#### OSCOMP RUNNER MISSING LTP SUBSET LIST ####"
            return 127
        fi
    fi

    # Match the visible pre-2025 runner protocol by default: each case keeps
    # its native stdout/stderr on the console. Buffered capture is opt-in for
    # local debugging only.
    ltp_case_output_mode
    if [ "${LTP_VIRT_OVERRIDE+x}" != "x" ]; then
        export LTP_VIRT_OVERRIDE=kvm
    fi
    if [ "${LTP_TIMEOUT_MUL+x}" != "x" ] && [ "$root" = /glibc ]; then
        case "$(runner_machine_quiet)" in
            loongarch64)
                export LTP_TIMEOUT_MUL=2
                ;;
        esac
    fi

    run_ltp_case_command() {
        shell_path="$1"
        testcase_path="$2"
        shift 2

        if [ "$#" -gt 0 ]; then
            if [ -n "$shell_path" ] && [ "${testcase_path##*.}" = "sh" ]; then
                if [ "${shell_path##*/}" = "busybox" ]; then
                    run_clean_exec "$shell_path" sh "$testcase_path" "$@"
                else
                    run_clean_exec "$shell_path" "$testcase_path" "$@"
                fi
            else
                run_clean_exec "$testcase_path" "$@"
            fi
        else
            if [ -n "$shell_path" ] && [ "${testcase_path##*.}" = "sh" ]; then
                if [ "${shell_path##*/}" = "busybox" ]; then
                    run_clean_exec "$shell_path" sh "$testcase_path"
                else
                    run_clean_exec "$shell_path" "$testcase_path"
                fi
            else
                run_clean_exec "$testcase_path"
            fi
        fi
    }

    ran_cases=0
    group_failed=0
    while IFS= read -r testcase || [ -n "$testcase" ]; do
        case "$testcase" in
            ''|\#*)
                continue
                ;;
        esac

        # The curated LTP subset file uses plain whitespace-delimited tokens.
        # Avoid relying on shell-specific `set +/-f` option parsing here:
        # some LA guest /bin/sh builds reject that short-option form.
        # shellcheck disable=SC2086
        set -- $testcase
        [ "$#" -gt 0 ] || continue

        testcase="$1"
        shift

        testcase_path=""
        if [ -f "ltp/testcases/bin/$testcase" ]; then
            testcase_path="./ltp/testcases/bin/$testcase"
        elif [ -f "ltp/testscripts/$testcase" ]; then
            testcase_path="./ltp/testscripts/$testcase"
        else
            testcase_matches=0
            for candidate in $(bb find ltp/testcases -type f -name "$testcase" 2>/dev/null || true); do
                testcase_matches=$((testcase_matches + 1))
                testcase_path="./$candidate"
                [ "$testcase_matches" -le 1 ] || break
            done
            if [ "$testcase_matches" -gt 1 ]; then
                runner_debug "#### OSCOMP RUNNER AMBIGUOUS LTP CASE ${testcase} ####"
                return 127
            fi
        fi

        if find_ltp_case_override "$root" "$testcase"; then
            testcase_path="$LTP_CASE_OVERRIDE_PATH"
        fi

        [ -n "$testcase_path" ] || {
            runner_debug "#### OSCOMP RUNNER MISSING LTP CASE ${testcase} ####"
            return 127
        }

        ran_cases=$((ran_cases + 1))
        echo "RUN LTP CASE $testcase"
        if runner_truthy "${OSCOMP_RUNNER_DEBUG:-0}" && [ "${testcase_path##*.}" = "sh" ]; then
            runner_debug "#### OSCOMP RUNNER LTP SHELL CASE ${testcase} PATH ${testcase_path} SHELL ${shell_path} TST_TEST_SH $(command -v tst_test.sh 2>/dev/null || true) ####"
        fi
        case_log=""
        ltp_saved_preload="${LD_PRELOAD:-}"
        if [ "$root" = /musl ] && [ "$testcase" = "recvmmsg01" ] && \
            [ -n "${OSCOMP_SUPPORT_LIB:-}" ] && [ -f "$OSCOMP_SUPPORT_LIB/liboscomp-mmsg-compat.so" ]; then
            if [ -n "$ltp_saved_preload" ]; then
                export LD_PRELOAD="$OSCOMP_SUPPORT_LIB/liboscomp-mmsg-compat.so:$ltp_saved_preload"
            else
                export LD_PRELOAD="$OSCOMP_SUPPORT_LIB/liboscomp-mmsg-compat.so"
            fi
        fi
        if [ "$LTP_CASE_OUTPUT_MODE" = "stream" ]; then
            run_ltp_case_command "$shell_path" "$testcase_path" "$@"
            ret=$?
        else
            case_log="/var/tmp/oscomp-ltp-case-${testcase}.$$.$ran_cases.log"
            bb rm -f "$case_log" 2>/dev/null || true
            if run_ltp_case_command "$shell_path" "$testcase_path" "$@" >"$case_log" 2>&1; then
                ret=0
            else
                ret=$?
            fi

            if [ "${OSCOMP_KEEP_LTP_CASE_LOGS:-0}" = "1" ]; then
                runner_debug "#### OSCOMP RUNNER LTP CASE LOG ${case_log} ####"
            else
                if [ "$ret" -ne 0 ] && [ -s "$case_log" ]; then
                    cat "$case_log"
                fi
                bb rm -f "$case_log" 2>/dev/null || true
                case_log=""
            fi
        fi
        if [ -n "$ltp_saved_preload" ]; then
            export LD_PRELOAD="$ltp_saved_preload"
        else
            unset LD_PRELOAD
        fi

        echo "FAIL LTP CASE $testcase : $ret"
        if [ "$ret" -ne 0 ]; then
            if [ -n "$case_log" ] && [ -s "$case_log" ]; then
                cat "$case_log"
            fi
            group_failed=1
        fi
    done <"$test_list"
    [ -z "$generated_test_list" ] || bb rm -f "$generated_test_list" 2>/dev/null || true

    [ "$ran_cases" -gt 0 ] || {
        runner_debug "#### OSCOMP RUNNER EMPTY LTP SUBSET ${test_list} ####"
        return 127
    }

    return "$group_failed"
}

run_basic_group() {
    root="$1"
    shell_path="$2"
    case_filter="${OSCOMP_BASIC_CASE_FILTER:-}"

    [ -d "$root/basic" ] || {
        runner_debug "#### OSCOMP RUNNER MISSING BASIC ROOT ${root}/basic ####"
        return 127
    }
    [ -f "$root/basic/run-all.sh" ] || {
        runner_debug "#### OSCOMP RUNNER MISSING BASIC SCRIPT ${root}/basic/run-all.sh ####"
        return 127
    }

    if ! cd "$root/basic"; then
        return 125
    fi

    if [ -n "$case_filter" ]; then
        normalized_filter="$(printf '%s' "$case_filter" | tr ',' ' ')"
        for case_name in \
            brk \
            chdir \
            clone \
            close \
            dup2 \
            dup \
            execve \
            exit \
            fork \
            fstat \
            getcwd \
            getdents \
            getpid \
            getppid \
            gettimeofday \
            mkdir_ \
            mmap \
            mount \
            munmap \
            openat \
            open \
            pipe \
            read \
            sleep \
            times \
            umount \
            uname \
            unlink \
            wait \
            waitpid \
            write \
            yield
        do
            want_case=0
            for token in $normalized_filter; do
                if [ "$token" = "$case_name" ]; then
                    want_case=1
                    break
                fi
            done
            [ "$want_case" -eq 1 ] || continue
            printf '%s\n' "Testing ${case_name} :"
            "./$case_name" || return $?
        done
        return 0
    fi

    if [ -n "$shell_path" ] && [ "${shell_path##*/}" = "busybox" ]; then
        run_clean_exec "$shell_path" sh ./run-all.sh
        ret=$?
    elif [ -n "$shell_path" ]; then
        run_clean_exec "$shell_path" ./run-all.sh
        ret=$?
    else
        run_clean_exec sh ./run-all.sh
        ret=$?
    fi
    return "$ret"
}

prepare_lmbench_env() {
    root="$1"
    compat_dir=/code/lmbench_src/bin/build
    compat_bin="$compat_dir/lmbench_all"
    root_bin="$root/lmbench_all"
    lmbench_hello_script='#!/bin/sh
printf "%s\n" "hello world"
'

    [ -x "$root_bin" ] || {
        runner_debug "#### OSCOMP RUNNER MISSING LMBENCH BIN ${root_bin} ####"
        return 1
    }

    bb mkdir -p "$compat_dir" 2>/dev/null || true
    bb rm -f "$compat_bin" 2>/dev/null || true
    bb ln -sf "$root_bin" "$compat_bin" 2>/dev/null || \
        bb cp "$root_bin" "$compat_bin" 2>/dev/null || {
            runner_debug "#### OSCOMP RUNNER FAILED TO PREPARE LMBENCH BIN ${compat_bin} ####"
            return 1
        }
    chmod +x "$compat_bin" 2>/dev/null || true

    for helper in "$root/hello" "$root/hello-s" "$compat_dir/hello" "$compat_dir/hello-s"; do
        if ! printf '%s' "$lmbench_hello_script" > "$helper"; then
            runner_debug "#### OSCOMP RUNNER FAILED TO PREPARE LMBENCH HELPER ${helper} ####"
            return 1
        fi
        chmod +x "$helper" 2>/dev/null || true
    done
    return 0
}

iozone_case_selected() {
    case_name="$1"
    case_filter="${OSCOMP_IOZONE_CASE_FILTER:-}"
    case "$case_filter" in
        "")
            return 0
            ;;
    esac

    normalized_filter="$(printf '%s' "$case_filter" | tr ',' ' ')"
    for token in $normalized_filter; do
        [ "$token" = "$case_name" ] && return 0
    done
    return 1
}

run_iozone_case() {
    case_name="$1"
    case_heading="$2"
    shift 2

    iozone_case_selected "$case_name" || return 0
    "$iozone_busybox" echo "$case_heading" || return $?
    if [ "${IOZONE_NO_UNLINK_ARG:-}" = "-w" ]; then
        run_clean_exec "$iozone_bin" -w "$@" || return $?
    else
        run_clean_exec "$iozone_bin" "$@" || return $?
    fi
    return 0
}

prepare_iozone_stage() {
    root="$1"
    flavor="$2"
    stage_dir="/var/tmp/oscomp-iozone-${flavor}"

    bb rm -rf "$stage_dir" 2>/dev/null || true
    bb mkdir -p "$stage_dir" 2>/dev/null || return 1

    for entry in iozone_testcode.sh iozone busybox; do
        [ -e "$root/$entry" ] || continue
        bb cp "$root/$entry" "$stage_dir/$entry" 2>/dev/null || return 1
        chmod +x "$stage_dir/$entry" 2>/dev/null || true
    done

    IOZONE_STAGE_DIR="$stage_dir"
    export IOZONE_STAGE_DIR
    return 0
}

cleanup_iozone_stage() {
    bb killall -9 iozone >/dev/null 2>&1 || true
    runner_control_sleep 1
    if [ -n "${IOZONE_STAGE_DIR:-}" ] && [ -d "$IOZONE_STAGE_DIR" ]; then
        bb rm -rf "$IOZONE_STAGE_DIR" 2>/dev/null || true
    fi
    IOZONE_STAGE_DIR=""
    export IOZONE_STAGE_DIR
}

cleanup_basic_mounts() {
    for mount_dir in /musl/basic/mnt /glibc/basic/mnt; do
        [ -d "$mount_dir" ] || continue
        bb umount "$mount_dir" >/dev/null 2>&1 || true
    done
}

cleanup_basic_artifacts() {
    for basic_dir in /musl/basic /glibc/basic; do
        [ -d "$basic_dir" ] || continue

        bb rm -f \
            "$basic_dir/test_close.txt" \
            "$basic_dir/test_mmap.txt" \
            "$basic_dir/mnt/test_openat.txt" \
            >/dev/null 2>&1 || true

        bb rm -rf \
            "$basic_dir/test_chdir" \
            "$basic_dir/test_mkdir" \
            >/dev/null 2>&1 || true
    done
}

settle_after_basic() {
    runner_debug "#### OSCOMP RUNNER BASIC SETTLE BEGIN ####"

    cleanup_basic_mounts
    cleanup_basic_artifacts
    bb sync 2>/dev/null || sync 2>/dev/null || true
    runner_control_sleep "${OSCOMP_BASIC_SETTLE_SECS:-2}"
    bb sync 2>/dev/null || sync 2>/dev/null || true
    runner_control_sleep 1

    runner_debug "#### OSCOMP RUNNER BASIC SETTLE END ####"
}

settle_after_iozone() {
    runner_debug "#### OSCOMP RUNNER IOZONE SETTLE BEGIN ####"

    bb sync 2>/dev/null || sync 2>/dev/null || true
    runner_control_sleep 2
    bb sync 2>/dev/null || sync 2>/dev/null || true
    runner_control_sleep 1

    runner_debug "#### OSCOMP RUNNER IOZONE SETTLE END ####"
}

run_iozone_group() {
    root="$1"
    flavor="$2"
    run_dir="$3"

    if ! cd "$run_dir"; then
        return 125
    fi

    iozone_busybox="./busybox"
    [ -x "$iozone_busybox" ] || iozone_busybox="/bin/busybox"
    iozone_bin="./iozone"
    [ -x "$iozone_bin" ] || iozone_bin="$root/iozone"

    [ -x "$iozone_busybox" ] || {
        runner_debug "#### OSCOMP RUNNER MISSING IOZONE BUSYBOX ${run_dir}/busybox ####"
        return 127
    }
    [ -x "$iozone_bin" ] || {
        runner_debug "#### OSCOMP RUNNER MISSING IOZONE BIN ${root}/iozone ####"
        return 127
    }

    IOZONE_NO_UNLINK_ARG=""
    if runner_truthy "${OSCOMP_IOZONE_NO_UNLINK:-0}"; then
        IOZONE_NO_UNLINK_ARG="-w"
    fi

    skip_auto_mode=0
    case "${OSCOMP_IOZONE_AUTO_MODE:-}" in
        1|y|Y|yes|YES|true|TRUE|on|ON)
            skip_auto_mode=0
            ;;
        0|n|N|no|NO|false|FALSE|off|OFF)
            skip_auto_mode=1
            ;;
        *)
            case "$(bb uname -m 2>/dev/null || true)" in
                loongarch64)
                    # The online iozone judge only consumes the explicit
                    # throughput sections below. On LA, iozone's auto sweep
                    # can stall for minutes before reaching any scored output.
                    skip_auto_mode=1
                    ;;
            esac
            ;;
    esac

    if [ "$skip_auto_mode" -eq 0 ]; then
        run_iozone_case auto "iozone automatic measurements" -a -r 1k -s 4m || return $?
    fi
    run_iozone_case write-read "iozone throughput write/read measurements" -t 4 -i 0 -i 1 -r 1k -s 1m || return $?
    run_iozone_case random-read "iozone throughput random-read measurements" -t 4 -i 0 -i 2 -r 1k -s 1m || return $?
    run_iozone_case read-backwards "iozone throughput read-backwards measurements" -t 4 -i 0 -i 3 -r 1k -s 1m || return $?
    run_iozone_case stride-read "iozone throughput stride-read measurements" -t 4 -i 0 -i 5 -r 1k -s 1m || return $?
    run_iozone_case fwrite-fread "iozone throughput fwrite/fread measurements" -t 4 -i 6 -i 7 -r 1k -s 1m || return $?
    run_iozone_case pwrite-pread "iozone throughput pwrite/pread measurements" -t 4 -i 9 -i 10 -r 1k -s 1m || return $?
    run_iozone_case pwritev-preadv "iozone throughtput pwritev/preadv measurements" -t 4 -i 11 -i 12 -r 1k -s 1m || return $?
    return 0
}

reference_eval_plan() {
    if [ -s /etc/oscomp-plan.txt ]; then
        cat /etc/oscomp-plan.txt
        return 0
    fi

    cat <<'EOF'
/musl basic
/musl iozone
/musl busybox
/musl netperf
/musl lua
/musl libcbench
/musl libctest
/musl cyclictest
/glibc basic
/glibc iozone
/glibc busybox
/glibc netperf
/glibc lua
/glibc libcbench
/glibc cyclictest
/musl lmbench
/glibc lmbench
/musl ltp
/glibc ltp
/musl iperf
/glibc iperf
EOF
}

root_flavor() {
    ROOT_FLAVOR_RESULT="default"
    case "$1" in
        /musl)
            ROOT_FLAVOR_RESULT="musl"
            ;;
        /glibc)
            ROOT_FLAVOR_RESULT="glibc"
            ;;
    esac
}

group_timeout_secs() {
    group="$1"
    case "$group" in
        basic|busybox|lua|cyclictest|iperf)
            GROUP_TIMEOUT_SECS=180
            ;;
        netperf)
            GROUP_TIMEOUT_SECS=300
            ;;
        libctest|lmbench)
            GROUP_TIMEOUT_SECS=600
            ;;
        iozone)
            GROUP_TIMEOUT_SECS=480
            ;;
        libcbench)
            GROUP_TIMEOUT_SECS=300
            ;;
        ltp)
            GROUP_TIMEOUT_SECS=5400
            ;;
        *)
            GROUP_TIMEOUT_SECS="${OSCOMP_TIMEOUT_DEFAULT:-900}"
            ;;
    esac
}

runner_now_epoch() {
    RUNNER_NOW_EPOCH=""
    if [ -r /proc/uptime ]; then
        now_epoch="$(bb awk '{print int($1)}' /proc/uptime 2>/dev/null || true)"
    else
        now_epoch=""
    fi
    case "$now_epoch" in
        ''|*[!0-9]*)
            ;;
        *)
            RUNNER_NOW_EPOCH="$now_epoch"
            ;;
    esac
}

runner_remaining_secs() {
    RUNNER_REMAINING_SECS=""

    case "$RUNNER_GLOBAL_TIMEOUT_SECS:$RUNNER_START_EPOCH" in
        *[!0-9:]*|:|''|*:)
            return 0
            ;;
    esac

    runner_now_epoch
    case "$RUNNER_NOW_EPOCH" in
        '')
            return 0
            ;;
    esac

    elapsed_secs=$((RUNNER_NOW_EPOCH - RUNNER_START_EPOCH))
    remaining_secs=$((RUNNER_GLOBAL_TIMEOUT_SECS - elapsed_secs))
    [ "$remaining_secs" -lt 0 ] && remaining_secs=0
    RUNNER_REMAINING_SECS="$remaining_secs"
}

prepare_group_timeout_secs() {
    root="$1"
    group="$2"
    script="$3"

    group_timeout_secs "$group"
    timeout_secs="$GROUP_TIMEOUT_SECS"

    runner_remaining_secs
    case "$RUNNER_REMAINING_SECS" in
        '')
            RUNNER_GROUP_TIMEOUT_SECS="$timeout_secs"
            return 0
            ;;
    esac

    if [ "$RUNNER_REMAINING_SECS" -le 0 ]; then
        RUNNER_GLOBAL_TIMEOUT_REACHED=1
        runner_debug "#### OSCOMP RUNNER GLOBAL TIMEOUT ${RUNNER_GLOBAL_TIMEOUT_SECS}s BEFORE ${script} ####"
        RUNNER_GROUP_TIMEOUT_SECS=0
        return 1
    fi

    if [ "$timeout_secs" -gt "$RUNNER_REMAINING_SECS" ]; then
        timeout_secs="$RUNNER_REMAINING_SECS"
    fi

    RUNNER_GROUP_TIMEOUT_SECS="$timeout_secs"
    return 0
}

refresh_runner_timeout_state() {
    runner_remaining_secs
    case "$RUNNER_REMAINING_SECS" in
        '')
            return 0
            ;;
    esac

    if [ "$RUNNER_REMAINING_SECS" -le 0 ]; then
        RUNNER_GLOBAL_TIMEOUT_REACHED=1
    fi
}

build_ld_library_path() {
    root="$1"
    BUILT_LD_LIBRARY_PATH=""

    case "$root" in
        /musl)
            BUILT_LD_LIBRARY_PATH="/musl/lib:/lib:/lib64:/usr/lib:/usr/lib64"
            return 0
            ;;
        /glibc)
            BUILT_LD_LIBRARY_PATH="/glibc/lib:/lib:/lib64:/usr/lib:/usr/lib64"
            return 0
            ;;
    esac

    append_path() {
        candidate="$1"
        [ -d "$candidate" ] || return 0
        case ":$BUILT_LD_LIBRARY_PATH:" in
            *":$candidate:"*)
                return 0
                ;;
        esac
        if [ -n "$BUILT_LD_LIBRARY_PATH" ]; then
            BUILT_LD_LIBRARY_PATH="${BUILT_LD_LIBRARY_PATH}:$candidate"
        else
            BUILT_LD_LIBRARY_PATH="$candidate"
        fi
    }

    if [ -n "$root" ] && [ "$root" != / ]; then
        append_path "$root/lib"
        append_path "$root/lib64"
        append_path "$root/usr/lib"
        append_path "$root/usr/lib64"
    fi

    append_path /lib
    append_path /lib64
    append_path /usr/lib
    append_path /usr/lib64
}

build_group_path() {
    root="$1"
    BUILT_GROUP_PATH=""

    append_path() {
        candidate="$1"
        [ -d "$candidate" ] || return 0
        case ":$BUILT_GROUP_PATH:" in
            *":$candidate:"*)
                return 0
                ;;
        esac
        if [ -n "$BUILT_GROUP_PATH" ]; then
            BUILT_GROUP_PATH="${BUILT_GROUP_PATH}:$candidate"
        else
            BUILT_GROUP_PATH="$candidate"
        fi
    }

    if [ -n "$root" ] && [ "$root" != / ]; then
        append_path "$root/bin"
        append_path "$root/usr/bin"
        append_path "$root/sbin"
        append_path "$root/usr/sbin"
    fi

    append_path /bin
    append_path /usr/bin
    append_path /sbin
    append_path /usr/sbin
}

script_path_for_root() {
    root="$1"
    group="$2"
    if [ "$root" = / ]; then
        SCRIPT_PATH_RESULT="/${group}_testcode.sh"
    else
        SCRIPT_PATH_RESULT="${root}/${group}_testcode.sh"
    fi
}

kill_process_tree() {
    sig="$1"
    pid="$2"
    [ -n "$pid" ] || return 0

    case " ${KILL_PROCESS_TREE_VISITED:-} " in
        *" $pid "*)
            return 0
            ;;
    esac
    append_word KILL_PROCESS_TREE_VISITED "$pid"

    children=""
    if [ -r "/proc/${pid}/task/${pid}/children" ]; then
        children="$(cat "/proc/${pid}/task/${pid}/children" 2>/dev/null)"
    fi
    for child in $children; do
        kill_process_tree "$sig" "$child"
    done

    kill "-$sig" "$pid" 2>/dev/null || kill "$pid" 2>/dev/null || true
}

kill_process_group() {
    sig="$1"
    leader_pid="$2"
    [ -n "$leader_pid" ] || return 0

    bb kill "-$sig" -- "-$leader_pid" 2>/dev/null || \
        bb kill "-$sig" "$leader_pid" 2>/dev/null || true
}

runner_quiesce_rounds() {
    rounds="${1:-32}"
    round_i=0

    while [ "$round_i" -lt "$rounds" ] 2>/dev/null; do
        # Keep this in the runner shell: spawning busybox here creates exactly
        # the short-lived task garbage this quiesce phase is meant to drain.
        [ -d /proc/1 ] >/dev/null 2>&1 || true
        round_i=$((round_i + 1))
    done
}

runner_control_sleep() {
    sleep_secs="$1"
    case "$sleep_secs" in
        ''|0)
            return 0
            ;;
    esac

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

    if [ -n "${OSCOMP_BOOTSTRAP:-}" ] && [ "${OSCOMP_BOOTSTRAP##*/}" = "busybox" ]; then
        "$OSCOMP_BOOTSTRAP" sleep "$sleep_secs"
    else
        sleep "$sleep_secs"
    fi
}

capture_process_snapshot() {
    PROCESS_SNAPSHOT_PIDS=""

    for proc_dir in /proc/[0-9]*; do
        [ -d "$proc_dir" ] || continue
        pid="${proc_dir#/proc/}"
        append_word PROCESS_SNAPSHOT_PIDS "$pid"
    done
}

process_in_snapshot() {
    contains_word "$1" $PROCESS_SNAPSHOT_PIDS
}

process_cleanup_candidate() {
    candidate_pid="$1"
    [ -r "/proc/${candidate_pid}/stat" ] || return 1

    candidate_state="$(bb awk '{ print $3 }' "/proc/${candidate_pid}/stat" 2>/dev/null || true)"
    case "$candidate_state" in
        Z|X)
            return 1
            ;;
    esac

    return 0
}

dump_leaked_process_sample() {
    sample_limit=12
    sample_count=0

    for pid in "$@"; do
        [ "$sample_count" -lt "$sample_limit" ] || break
        [ -r "/proc/${pid}/stat" ] || continue

        stat_fields="$(bb awk '{ print $1, $3, $4, $20, $32 }' "/proc/${pid}/stat" 2>/dev/null || true)"
        stat_pid="$(echo "$stat_fields" | bb awk '{ print $1 }' 2>/dev/null || true)"
        state="$(echo "$stat_fields" | bb awk '{ print $2 }' 2>/dev/null || true)"
        ppid="$(echo "$stat_fields" | bb awk '{ print $3 }' 2>/dev/null || true)"
        num_threads="$(echo "$stat_fields" | bb awk '{ print $4 }' 2>/dev/null || true)"
        blocked_mask="$(echo "$stat_fields" | bb awk '{ print $5 }' 2>/dev/null || true)"
        comm="$(cat "/proc/${pid}/comm" 2>/dev/null || true)"
        tgid="$(bb awk '/^Tgid:/ { print $2 }' "/proc/${pid}/status" 2>/dev/null || true)"
        [ -n "$comm" ] || comm="?"

        runner_debug "#### OSCOMP RUNNER LEAK SAMPLE PID ${pid} STATPID ${stat_pid:-?} TGID ${tgid:-?} STATE ${state:-?} PPID ${ppid:-?} THREADS ${num_threads:-?} SIGBLK ${blocked_mask:-?} COMM ${comm} ####"
        sample_count=$((sample_count + 1))
    done
}

cleanup_new_processes_since_snapshot() {
    cleanup_round=1
    last_leaked_pids=""

    while [ "$cleanup_round" -le 5 ]; do
        leaked_pids=""

        for proc_dir in /proc/[0-9]*; do
            [ -d "$proc_dir" ] || continue
            pid="${proc_dir#/proc/}"
            case "$pid" in
                ''|*[!0-9]*)
                    continue
                    ;;
                1|$$)
                    continue
                    ;;
            esac
            process_in_snapshot "$pid" && continue
            process_cleanup_candidate "$pid" || continue
            append_word leaked_pids "$pid"
        done

        [ -n "$leaked_pids" ] || return 0
        last_leaked_pids="$leaked_pids"

        runner_debug "#### OSCOMP RUNNER CLEANUP LEAKED PIDS ROUND $cleanup_round ${leaked_pids} ####"
        KILL_PROCESS_TREE_VISITED=""
        for pid in $leaked_pids; do
            kill_process_tree TERM "$pid"
        done
        runner_quiesce_rounds "${OSCOMP_CLEANUP_QUIESCE_ROUNDS:-64}"
        KILL_PROCESS_TREE_VISITED=""
        for pid in $leaked_pids; do
            kill_process_tree KILL "$pid"
        done
        runner_quiesce_rounds "${OSCOMP_CLEANUP_QUIESCE_ROUNDS:-64}"
        cleanup_round=$((cleanup_round + 1))
    done

    dump_leaked_process_sample $last_leaked_pids
    return 0
}

cleanup_non_runner_processes() {
    cleanup_round=1

    while [ "$cleanup_round" -le 5 ]; do
        residual_pids=""

        for proc_dir in /proc/[0-9]*; do
            [ -d "$proc_dir" ] || continue
            pid="${proc_dir#/proc/}"
            case "$pid" in
                ''|*[!0-9]*)
                    continue
                    ;;
                1|$$)
                    continue
                    ;;
            esac
            process_cleanup_candidate "$pid" || continue
            append_word residual_pids "$pid"
        done

        [ -n "$residual_pids" ] || return 0

        runner_debug "#### OSCOMP RUNNER CLEANUP RESIDUAL PIDS ROUND $cleanup_round ${residual_pids} ####"
        KILL_PROCESS_TREE_VISITED=""
        for pid in $residual_pids; do
            kill_process_tree TERM "$pid"
        done
        runner_quiesce_rounds "${OSCOMP_CLEANUP_QUIESCE_ROUNDS:-64}"
        KILL_PROCESS_TREE_VISITED=""
        for pid in $residual_pids; do
            kill_process_tree KILL "$pid"
        done
        runner_quiesce_rounds "${OSCOMP_CLEANUP_QUIESCE_ROUNDS:-64}"
        cleanup_round=$((cleanup_round + 1))
    done

    return 0
}

stream_group_output_incremental() {
    output_file="$1"
    streamed_bytes="$2"
    STREAM_GROUP_OUTPUT_BYTES="$streamed_bytes"

    [ -f "$output_file" ] || return 0

    current_size="$(wc -c < "$output_file" 2>/dev/null || echo "$streamed_bytes")"
    case "$current_size" in
        ''|*[!0-9]*)
            return 0
            ;;
    esac
    [ "$current_size" -gt "$streamed_bytes" ] || return 0

    start_byte=$((streamed_bytes + 1))
    bytes_to_emit=$((current_size - streamed_bytes))
    bb tail -c +"$start_byte" "$output_file" | bb head -c "$bytes_to_emit"
    STREAM_GROUP_OUTPUT_BYTES="$current_size"
}

emit_group_start() {
    echo "#### OS COMP TEST GROUP START $1 ####"
}

emit_group_end() {
    echo "#### OS COMP TEST GROUP END $1 ####"
}

group_marker_name() {
    group="$1"
    flavor="$2"
    case "$flavor" in
        musl|glibc)
            printf '%s-%s\n' "$group" "$flavor"
            ;;
        *)
            printf '%s\n' "$group"
            ;;
    esac
}

should_suppress_group_marker_line() {
    case "$1" in
        '#### OS COMP TEST GROUP START '*|'#### OS COMP TEST GROUP END '*)
            return 0
            ;;
    esac
    return 1
}

emit_normalized_group_line() {
    line="$1"
    if [ -n "${RUNNER_CR:-}" ]; then
        case "$line" in
            *"$RUNNER_CR")
                line=${line%"$RUNNER_CR"}
                ;;
        esac
    fi
    case "$line" in
        *'#### OS COMP TEST GROUP '*)
            line="$(printf '%s\n' "$line" | bb sed -E 's/#### OS COMP TEST GROUP (START|END) [^#]+ ####//g')"
            ;;
    esac
    [ -n "$line" ] || return 0
    should_suppress_group_marker_line "$line" && return 0
    printf '%s\n' "$line"
}

normalize_group_output_chunk() {
    if [ -z "${RUNNER_CHUNK_SENTINEL:-}" ]; then
        RUNNER_CHUNK_SENTINEL="$(printf '\001')"
    fi
    chunk="$(cat; printf '%s' "$RUNNER_CHUNK_SENTINEL")"
    chunk="${chunk%"$RUNNER_CHUNK_SENTINEL"}"

    [ -n "$chunk" ] || return 0

    if [ -n "${STREAM_GROUP_OUTPUT_FRAGMENT:-}" ]; then
        chunk="${STREAM_GROUP_OUTPUT_FRAGMENT}${chunk}"
    fi
    STREAM_GROUP_OUTPUT_FRAGMENT=""

    while :; do
        case "$chunk" in
            *'
'*)
                line=${chunk%%'
'*}
                chunk=${chunk#*'
'}
                emit_normalized_group_line "$line"
                ;;
            *)
                STREAM_GROUP_OUTPUT_FRAGMENT="$chunk"
                break
                ;;
        esac
    done
}

flush_group_output_fragment() {
    :
}

run_group_script() {
    root="$1"
    group="$2"
    script_path_for_root "$root" "$group"
    script="$SCRIPT_PATH_RESULT"
    [ -f "$script" ] || return 0

    root_flavor "$root"
    flavor="$ROOT_FLAVOR_RESULT"
    group_marker="$(group_marker_name "$group" "$flavor")"

    runner_debug "#### OSCOMP RUNNER START ${script} ####"

    if ! prepare_group_timeout_secs "$root" "$group" "$script"; then
        runner_debug "#### OSCOMP RUNNER END ${script} STATUS 124 ####"
        return 124
    fi

    timeout_secs="$RUNNER_GROUP_TIMEOUT_SECS"
    output_file="/tmp/oscomp-${group}-${flavor}.log"
    : > "$output_file"
    capture_process_snapshot
    run_dir="$root"
    run_script_name="${script##*/}"

    if [ "$group" = "ltp" ]; then
        if [ ! -d "$root/ltp" ]; then
            runner_debug "#### OSCOMP RUNNER MISSING LTP ROOT ${root}/ltp ####"
            runner_debug "#### OSCOMP RUNNER END ${script} STATUS 127 ####"
            return 127
        fi
        prepare_ltp_env
    elif [ "$group" = "lmbench" ]; then
        prepare_lmbench_env "$root" || {
            runner_debug "#### OSCOMP RUNNER END ${script} STATUS 127 ####"
            return 127
        }
    elif [ "$group" = "iozone" ]; then
        prepare_iozone_stage "$root" "$flavor" || true
        if [ -n "${IOZONE_STAGE_DIR:-}" ] && [ -d "$IOZONE_STAGE_DIR" ]; then
            run_dir="$IOZONE_STAGE_DIR"
            run_script_name="iozone_testcode.sh"
        fi
    fi

    prime_group_output_stream
    STREAM_GROUP_OUTPUT_FRAGMENT=""

    (
        cd "$run_dir" || exit 125
        runner_debug "#### OSCOMP RUNNER GROUP STEP ensure_support_runtime_payload enter ${root}/${group} ####"
        ensure_support_runtime_payload "$root" "$group"
        runner_debug "#### OSCOMP RUNNER GROUP STEP ensure_support_runtime_payload done ${root}/${group} ####"
        runner_debug "#### OSCOMP RUNNER GROUP STEP build_group_path enter ${root}/${group} ####"
        build_group_path "$root"
        runner_debug "#### OSCOMP RUNNER GROUP STEP build_group_path done ${root}/${group} PATH ${BUILT_GROUP_PATH:-} ####"
        if [ "$group" = "ltp" ] && [ -d "$root/ltp" ]; then
            export LTPROOT="$root/ltp"
            if [ -d "$root/ltp/testcases/bin" ]; then
                BUILT_GROUP_PATH="$root/ltp/testcases/bin${BUILT_GROUP_PATH:+:$BUILT_GROUP_PATH}"
            fi
            if [ -d "$root/ltp/testscripts" ]; then
                BUILT_GROUP_PATH="$root/ltp/testscripts${BUILT_GROUP_PATH:+:$BUILT_GROUP_PATH}"
            fi
            export LTP_DEV_FS_TYPE=tmpfs
            export LTP_SINGLE_FS_TYPE=tmpfs
        fi
        if [ -n "$BUILT_GROUP_PATH" ]; then
            if [ -n "${PATH:-}" ]; then
                export PATH="$BUILT_GROUP_PATH:/bin:/usr/bin:/sbin:/usr/sbin:$PATH"
            else
                export PATH="$BUILT_GROUP_PATH:/bin:/usr/bin:/sbin:/usr/sbin"
            fi
        fi
        if [ -n "${OSCOMP_SUPPORT_BIN:-}" ] && [ -d "$OSCOMP_SUPPORT_BIN" ]; then
            export PATH="$OSCOMP_SUPPORT_BIN:$PATH"
        fi
        runner_debug "#### OSCOMP RUNNER GROUP STEP build_ld_library_path enter ${root}/${group} ####"
        build_ld_library_path "$root"
        runner_debug "#### OSCOMP RUNNER GROUP STEP build_ld_library_path done ${root}/${group} LD ${BUILT_LD_LIBRARY_PATH:-} ####"
        if [ -n "$BUILT_LD_LIBRARY_PATH" ]; then
            export LD_LIBRARY_PATH="$BUILT_LD_LIBRARY_PATH"
        fi
        if [ "$root" = /musl ] && [ "$group" = "ltp" ] && \
            [ -n "${OSCOMP_SUPPORT_LIB:-}" ] && [ -f "$OSCOMP_SUPPORT_LIB/liboscomp-musl-compat.so" ]; then
            if [ -n "${LD_PRELOAD:-}" ]; then
                export LD_PRELOAD="$OSCOMP_SUPPORT_LIB/liboscomp-musl-compat.so:$LD_PRELOAD"
            else
                export LD_PRELOAD="$OSCOMP_SUPPORT_LIB/liboscomp-musl-compat.so"
            fi
        fi
        if [ "$root" = /glibc ] && [ -n "${OSCOMP_SUPPORT_LOCPATH:-}" ]; then
            export LANG=C.UTF-8
            export LC_ALL=C.UTF-8
            export LC_CTYPE=C.UTF-8
            export LOCPATH="$OSCOMP_SUPPORT_LOCPATH"
        fi
        export TMPDIR=/var/tmp
        export TMP=/var/tmp
        export TEMP=/var/tmp
        if [ "$group" = "lmbench" ]; then
            export ENOUGH="${OSCOMP_LMBENCH_ENOUGH:-10000}"
        fi

        runner_debug "#### OSCOMP RUNNER GROUP STEP pick_shell enter ${root}/${group} ####"
        script_shell=""
        if [ "$group" = "iozone" ] && [ -x "$run_dir/busybox" ]; then
            script_shell="$run_dir/busybox"
        fi
        if [ -z "$script_shell" ]; then
            case "$root" in
                /musl)
                    if [ -n "${OSCOMP_MUSL_BUSYBOX:-}" ]; then
                        script_shell="$OSCOMP_MUSL_BUSYBOX"
                    fi
                    ;;
                /glibc)
                    if [ -n "${OSCOMP_GLIBC_BUSYBOX:-}" ]; then
                        script_shell="$OSCOMP_GLIBC_BUSYBOX"
                    fi
                    ;;
            esac
        fi
        if [ -z "$script_shell" ] && pick_busybox_for_root "$root"; then
            script_shell="$PICK_BUSYBOX_FOR_ROOT_RESULT"
        fi
        if [ -z "$script_shell" ] && [ -x /bin/sh ]; then
            script_shell=/bin/sh
        fi

        if [ -z "$script_shell" ]; then
            runner_debug "#### OSCOMP RUNNER MISSING SHELL ${root}/busybox ####"
            exit 127
        fi
        runner_debug "#### OSCOMP RUNNER GROUP STEP pick_shell done ${root}/${group} SHELL ${script_shell} ####"

        exec_shell="$script_shell"
        exec_script="./$run_script_name"
        runner_debug "#### OSCOMP RUNNER GROUP STEP support_group_override_script enter ${root}/${group} ####"
        if support_group_override_script "$flavor" "$group"; then
            exec_script="$SUPPORT_GROUP_OVERRIDE_SCRIPT"
            runner_debug "#### OSCOMP RUNNER USING GROUP OVERRIDE ${exec_script} ####"
        fi
        runner_debug "#### OSCOMP RUNNER GROUP STEP support_group_override_script done ${root}/${group} SCRIPT ${exec_script} SHELL ${exec_shell} ####"
        if [ "$group" = "cyclictest" ] && runner_truthy "${OSCOMP_TRACE_GROUP_SHELL:-0}"; then
            runner_debug "#### OSCOMP RUNNER TRACE SHELL ENABLED ${root}/${group} ####"
        fi

        if [ -z "$SUPPORT_GROUP_OVERRIDE_SCRIPT" ]; then
            if [ "$group" = "basic" ]; then
                emit_group_start "$group_marker"
                run_basic_group "$root" "$script_shell"
                group_status=$?
                emit_group_end "$group_marker"
                exit "$group_status"
            fi
            if [ "$group" = "iozone" ]; then
                emit_group_start "$group_marker"
                run_iozone_group "$root" "$flavor" "$run_dir"
                group_status=$?
                emit_group_end "$group_marker"
                exit "$group_status"
            fi
            if [ "$group" = "ltp" ]; then
                emit_group_start "$group_marker"
                run_ltp_group "$root" "$script_shell"
                group_status=$?
                emit_group_end "$group_marker"
                exit "$group_status"
            fi
        fi

        run_clean_shell_script "$exec_shell" "$exec_script" </dev/null
        exit $?
    ) </dev/null >"$output_file" 2>&1 &
    runner_pid=$!
    timed_out=""
    elapsed=0
    streamed_bytes=0
    while kill -0 "$runner_pid" 2>/dev/null; do
        stream_group_output_incremental "$output_file" "$streamed_bytes"
        streamed_bytes="$STREAM_GROUP_OUTPUT_BYTES"
        if [ "$elapsed" -ge "$timeout_secs" ]; then
            timed_out=1
            runner_debug "#### OSCOMP RUNNER TIMEOUT ${script} AFTER ${timeout_secs}s ####"
            kill_process_group TERM "$runner_pid"
            runner_control_sleep 2
            kill_process_group KILL "$runner_pid"
            runner_control_sleep 1
            kill_process_tree KILL "$runner_pid"
            break
        fi
        runner_control_sleep 1
        elapsed=$((elapsed + 1))
    done

    wait "$runner_pid" 2>/dev/null
    status=$?
    if [ -n "$timed_out" ]; then
        status=124
    fi
    runner_debug "#### OSCOMP RUNNER GROUP WAIT DONE ${script} STATUS ${status} ####"
    refresh_runner_timeout_state
    stream_group_output_incremental "$output_file" "$streamed_bytes"
    flush_group_output_fragment
    runner_debug "#### OSCOMP RUNNER POST GROUP QUIESCE BEGIN ${script} ####"
    runner_quiesce_rounds "${OSCOMP_POST_GROUP_QUIESCE_ROUNDS:-128}"
    runner_debug "#### OSCOMP RUNNER POST GROUP QUIESCE END ${script} ####"
    runner_debug "#### OSCOMP RUNNER CLEANUP NEW BEGIN ${script} ####"
    cleanup_new_processes_since_snapshot
    runner_debug "#### OSCOMP RUNNER CLEANUP NEW END ${script} ####"
    runner_debug "#### OSCOMP RUNNER CLEANUP RESIDUAL BEGIN ${script} ####"
    cleanup_non_runner_processes
    runner_debug "#### OSCOMP RUNNER CLEANUP RESIDUAL END ${script} ####"
    if [ "$group" = "basic" ]; then
        runner_debug "#### OSCOMP RUNNER CLEANUP BASIC BEGIN ${script} ####"
        cleanup_basic_mounts
        runner_debug "#### OSCOMP RUNNER CLEANUP BASIC END ${script} ####"
    fi
    runner_debug "#### OSCOMP RUNNER CLEANUP IOZONE BEGIN ${script} ####"
    cleanup_iozone_stage
    runner_debug "#### OSCOMP RUNNER CLEANUP IOZONE END ${script} ####"
    if [ "$group" = "basic" ]; then
        runner_debug "#### OSCOMP RUNNER SETTLE BASIC BEGIN ${script} ####"
        settle_after_basic
        runner_debug "#### OSCOMP RUNNER SETTLE BASIC END ${script} ####"
    fi
    if [ "$group" = "iozone" ]; then
        runner_debug "#### OSCOMP RUNNER SETTLE IOZONE BEGIN ${script} ####"
        settle_after_iozone
        runner_debug "#### OSCOMP RUNNER SETTLE IOZONE END ${script} ####"
    fi
    runner_debug "#### OSCOMP RUNNER END ${script} STATUS ${status} ####"
    bb rm -f "$output_file" 2>/dev/null || true
    return "$status"
}

has_any_planned_scripts() {
    FOUND_ANY=""

    while IFS=' ' read -r root group; do
        [ -n "$root" ] || continue
        [ -n "$group" ] || continue
        script_path_for_root "$root" "$group"
        [ -f "$SCRIPT_PATH_RESULT" ] || continue
        FOUND_ANY=1
        return 0
    done <<EOF
$(reference_eval_plan)
EOF
}

run_reference_eval_plan() {
    while IFS=' ' read -r root group; do
        [ -n "$root" ] || continue
        [ -n "$group" ] || continue
        [ -n "$RUNNER_GLOBAL_TIMEOUT_REACHED" ] && break
        script_path_for_root "$root" "$group"
        [ -f "$SCRIPT_PATH_RESULT" ] || continue
        run_group_script "$root" "$group"
    done <<EOF
$(reference_eval_plan)
EOF
}

oscomp_shutdown() {
    for cmd in poweroff halt reboot; do
        if command -v "$cmd" >/dev/null 2>&1; then
            "$cmd" -f >/dev/null 2>&1 || true
        fi
    done
}

oscomp_runner_main() {
    export PATH="/bin:/usr/bin:/sbin:/usr/sbin"
    export TMPDIR=/var/tmp
    export TMP=/var/tmp
    export TEMP=/var/tmp
    export SHELL=/bin/sh
    export HOME=/root
    export USER=root
    export TERM=dumb

    RUNNER_GLOBAL_TIMEOUT_SECS="${OSCOMP_TIMEOUT_GLOBAL:-6900}"
    RUNNER_GLOBAL_TIMEOUT_REACHED=""

    runner_now_epoch
    RUNNER_START_EPOCH="$RUNNER_NOW_EPOCH"
    case "$RUNNER_GLOBAL_TIMEOUT_SECS:$RUNNER_START_EPOCH" in
        *[!0-9:]*|:|''|*:)
            RUNNER_GLOBAL_TIMEOUT_SECS=""
            ;;
    esac

    run_pre2025_init_sequence || exit 1

    runner_debug "#### OSCOMP RUNNER BOOTSTRAP ${OSCOMP_BOOTSTRAP:-/bin/sh} ####"

    has_any_planned_scripts
    if [ -z "$FOUND_ANY" ]; then
        runner_debug "#### OSCOMP RUNNER NO TEST SCRIPTS FOUND ####"
        exit 1
    fi

    run_reference_eval_plan

    oscomp_shutdown
    exit 0
}

oscomp_runner_main "$@"
