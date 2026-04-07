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
    root="$1"

    case "$root" in
        /musl)
            if [ -x /musl/busybox ]; then
                PICK_BUSYBOX_FOR_ROOT_RESULT=/musl/busybox
                return 0
            fi
            ;;
        /glibc)
            if [ -x /glibc/busybox ]; then
                PICK_BUSYBOX_FOR_ROOT_RESULT=/glibc/busybox
                return 0
            fi
            ;;
    esac

    if [ -x "$root/busybox" ]; then
        PICK_BUSYBOX_FOR_ROOT_RESULT="$root/busybox"
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
    if bb mount -t ext4 -o ro /dev/vdb /support >/dev/null 2>&1; then
        support_libgcc=""
        for candidate in \
            "/support/${support_arch_dir}/glibc/lib/libgcc_s.so.1" \
            "/support/glibc/lib/libgcc_s.so.1"
        do
            [ -n "$candidate" ] || continue
            if [ -f "$candidate" ]; then
                support_libgcc="$candidate"
                break
            fi
        done
        if [ -n "$support_libgcc" ]; then
            install_runtime_alias "$support_libgcc" /glibc/lib/libgcc_s.so.1 || true
            install_runtime_alias "$support_libgcc" /lib/libgcc_s.so.1 || true
        fi
        if [ -d /support/usr/lib/locale/C.UTF-8 ]; then
            OSCOMP_SUPPORT_LOCPATH=/support/usr/lib/locale
            export OSCOMP_SUPPORT_LOCPATH
        fi
    else
        bb rmdir /support >/dev/null 2>&1 || true
    fi
}

run_pre2025_init_sequence() {
    # Mirror the visible setup order of the official pre-2025 testcase layout.
    cd / || return 0

    bb mkdir -p /bin 2>/dev/null || true
    if ! pick_busybox_for_root /musl; then
        echo "#### OSCOMP RUNNER MISSING SHELL /musl/busybox ####"
        return 1
    fi
    install_runtime_alias "$PICK_BUSYBOX_FOR_ROOT_RESULT" /bin/busybox || true
    /bin/busybox --install -s /bin >/dev/null 2>&1 || true
    [ -e /busybox ] || bb ln -sf /bin/busybox /busybox 2>/dev/null || true
    [ -e /bin/sh ] || bb ln -sf /bin/busybox /bin/sh 2>/dev/null || true
    [ -e /bin/ash ] || bb ln -sf /bin/busybox /bin/ash 2>/dev/null || true
    [ -e /bin/bash ] || bb ln -sf /bin/sh /bin/bash 2>/dev/null || true
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
        "nobody:x:65534:"
    if [ ! -s /etc/resolv.conf ]; then
        write_file_lines /etc/resolv.conf "nameserver 8.8.8.8"
    fi

    clear_dir_contents /var/tmp
    clear_dir_contents /tmp
    bb mkdir -p /tmp/memfd 2>/dev/null || true

    install_locale_tool
    install_useradd_tool
    mount_support_disk
}

prepare_ltp_env() {
    bb mkdir -p /etc/oscomp-ltp 2>/dev/null || true
    bb mkdir -p /lib/modules/10.0.0/build /lib/modules/10.0.0+/build 2>/dev/null || true
    ensure_file_line /lib/modules/10.0.0/build/.config "CONFIG_EVENTFD=y"
    ensure_file_line /lib/modules/10.0.0+/build/.config "CONFIG_EVENTFD=y"
}

prepare_lmbench_env() {
    root="$1"
    [ -n "$root" ] || return 0
    [ "$root" = / ] && return 0
    [ -x "$root/lmbench_all" ] || return 0
    install_runtime_alias "$root/lmbench_all" /code/lmbench_src/bin/build/lmbench_all || true
}

read_runner_config() {
    value="$1"
    file_path="$2"
    RUNNER_CONFIG_RESULT="$value"
    if [ -z "$RUNNER_CONFIG_RESULT" ] && [ -f "$file_path" ]; then
        RUNNER_CONFIG_RESULT="$(bb cat "$file_path" 2>/dev/null || true)"
    fi
}

runner_root_mode() {
    read_runner_config "${OSCOMP_RUNNER_ROOT:-}" /etc/oscomp-runner/root
    set -- $RUNNER_CONFIG_RESULT
    mode="${1:-all}"
    case "$mode" in
        default|musl|glibc|all)
            RUNNER_ROOT_MODE_RESULT="$mode"
            ;;
        *)
            RUNNER_ROOT_MODE_RESULT="all"
            ;;
    esac
}

runner_group_filters() {
    read_runner_config "${OSCOMP_RUNNER_GROUPS:-}" /etc/oscomp-runner/groups
    filters="$(printf '%s' "$RUNNER_CONFIG_RESULT" | bb tr ',\t\r\n' '    ' 2>/dev/null || true)"
    RUNNER_GROUP_FILTERS=""
    for group_name in $filters; do
        append_word RUNNER_GROUP_FILTERS "$group_name"
    done
}

group_selected() {
    [ -z "$RUNNER_GROUP_FILTERS" ] && return 0
    contains_word "$1" $RUNNER_GROUP_FILTERS
}

official_group_list() {
    printf '%s\n' \
        basic \
        busybox \
        lua \
        libctest \
        iozone \
        unixbench \
        iperf \
        libcbench \
        lmbench \
        netperf \
        cyclictest \
        ltp
}

configure_runner_roots() {
    GROUP_ROOTS=""

    runner_root_mode
    case "$RUNNER_ROOT_MODE_RESULT" in
        default|all)
            append_word GROUP_ROOTS /glibc
            append_word GROUP_ROOTS /musl
            ;;
        musl)
            append_word GROUP_ROOTS /musl
            ;;
        glibc)
            append_word GROUP_ROOTS /glibc
            ;;
    esac
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
        basic|busybox|lua|cyclictest|iperf|netperf)
            GROUP_TIMEOUT_SECS=180
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
            GROUP_TIMEOUT_SECS=1500
            ;;
        unixbench)
            GROUP_TIMEOUT_SECS=900
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
        echo "#### OSCOMP RUNNER GLOBAL TIMEOUT ${RUNNER_GLOBAL_TIMEOUT_SECS}s BEFORE ${script} ####"
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

print_group_output_filtered() {
    output_file="$1"
    [ -f "$output_file" ] || return 0

    while IFS= read -r line || [ -n "$line" ]; do
        printf '%s\n' "$line"
    done < "$output_file"
}

run_group_script() {
    root="$1"
    group="$2"
    script_path_for_root "$root" "$group"
    script="$SCRIPT_PATH_RESULT"
    [ -f "$script" ] || return 0

    root_flavor "$root"
    flavor="$ROOT_FLAVOR_RESULT"

    echo "#### OSCOMP RUNNER START ${script} ####"

    if ! prepare_group_timeout_secs "$root" "$group" "$script"; then
        echo "#### OSCOMP RUNNER END ${script} STATUS 124 ####"
        return 124
    fi

    timeout_secs="$RUNNER_GROUP_TIMEOUT_SECS"
    output_file="/tmp/oscomp-${group}-${flavor}.log"
    : > "$output_file"

    if [ "$group" = "ltp" ]; then
        if [ ! -d "$root/ltp" ]; then
            echo "#### OSCOMP RUNNER MISSING LTP ROOT ${root}/ltp ####"
            echo "#### OSCOMP RUNNER END ${script} STATUS 127 ####"
            return 127
        fi
        prepare_ltp_env
    elif [ "$group" = "lmbench" ]; then
        prepare_lmbench_env "$root"
    fi

    (
        cd "$root" || exit 125
        build_group_path "$root"
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
            export PATH="$BUILT_GROUP_PATH"
        fi
        build_ld_library_path "$root"
        if [ -n "$BUILT_LD_LIBRARY_PATH" ]; then
            export LD_LIBRARY_PATH="$BUILT_LD_LIBRARY_PATH"
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

        script_name="${script##*/}"
        script_shell=""
        if pick_busybox_for_root "$root"; then
            script_shell="$PICK_BUSYBOX_FOR_ROOT_RESULT"
        elif [ -x /bin/sh ]; then
            script_shell=/bin/sh
        fi

        if [ -z "$script_shell" ]; then
            echo "#### OSCOMP RUNNER MISSING SHELL ${root}/busybox ####"
            exit 127
        fi

        if [ "${script_shell##*/}" = "busybox" ]; then
            exec "$script_shell" sh "./$script_name" </dev/null
        fi
        exec "$script_shell" "./$script_name" </dev/null
    ) >"$output_file" 2>&1 &
    runner_pid=$!
    timed_out=""
    elapsed=0
    while kill -0 "$runner_pid" 2>/dev/null; do
        if [ "$elapsed" -ge "$timeout_secs" ]; then
            timed_out=1
            echo "#### OSCOMP RUNNER TIMEOUT ${script} AFTER ${timeout_secs}s ####"
            kill_process_group TERM "$runner_pid"
            bb sleep 2
            kill_process_group KILL "$runner_pid"
            bb sleep 1
            kill_process_tree KILL "$runner_pid"
            break
        fi
        bb sleep 1
        elapsed=$((elapsed + 1))
    done

    wait "$runner_pid" 2>/dev/null
    status=$?
    if [ -n "$timed_out" ]; then
        status=124
    fi
    refresh_runner_timeout_state
    print_group_output_filtered "$output_file"
    echo "#### OSCOMP RUNNER END ${script} STATUS ${status} ####"
    bb rm -f "$output_file" 2>/dev/null || true
    return "$status"
}

run_official_groups_in_root() {
    root="$1"
    while IFS= read -r group; do
        [ -n "$group" ] || continue
        group_selected "$group" || continue
        script_path_for_root "$root" "$group"
        [ -f "$SCRIPT_PATH_RESULT" ] || continue
        run_group_script "$root" "$group"
    done <<EOF
$(official_group_list)
EOF
}

run_official_group_across_roots() {
    group="$1"
    for root in $GROUP_ROOTS; do
        [ -n "$RUNNER_GLOBAL_TIMEOUT_REACHED" ] && break
        script_path_for_root "$root" "$group"
        [ -f "$SCRIPT_PATH_RESULT" ] || continue
        run_group_script "$root" "$group"
    done
}

has_any_group_scripts() {
    FOUND_ANY=""

    while IFS= read -r group; do
        [ -n "$group" ] || continue
        group_selected "$group" || continue
        for root in $GROUP_ROOTS; do
            script_path_for_root "$root" "$group"
            [ -f "$SCRIPT_PATH_RESULT" ] || continue
            FOUND_ANY=1
            return 0
        done
    done <<EOF
$(official_group_list)
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

    RUNNER_GLOBAL_TIMEOUT_SECS="${OSCOMP_TIMEOUT_GLOBAL:-6000}"
    RUNNER_GLOBAL_TIMEOUT_REACHED=""

    runner_now_epoch
    RUNNER_START_EPOCH="$RUNNER_NOW_EPOCH"
    case "$RUNNER_GLOBAL_TIMEOUT_SECS:$RUNNER_START_EPOCH" in
        *[!0-9:]*|:|''|*:)
            RUNNER_GLOBAL_TIMEOUT_SECS=""
            ;;
    esac

    run_pre2025_init_sequence || exit 1
    configure_runner_roots
    runner_group_filters

    echo "#### OSCOMP RUNNER BOOTSTRAP ${OSCOMP_BOOTSTRAP:-/bin/sh} ####"

    if [ -z "$GROUP_ROOTS" ]; then
        echo "#### OSCOMP RUNNER NO TEST SCRIPTS FOUND ####"
        exit 1
    fi

    has_any_group_scripts
    if [ -z "$FOUND_ANY" ]; then
        echo "#### OSCOMP RUNNER NO TEST SCRIPTS FOUND ####"
        exit 1
    fi

    while IFS= read -r group; do
        [ -n "$group" ] || continue
        [ -n "$RUNNER_GLOBAL_TIMEOUT_REACHED" ] && break
        group_selected "$group" || continue
        run_official_group_across_roots "$group"
    done <<EOF
$(official_group_list)
EOF

    oscomp_shutdown
    exit 0
}

oscomp_runner_main "$@"
