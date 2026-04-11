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

runner_debug() {
    case "${OSCOMP_RUNNER_DEBUG:-0}" in
        1|y|Y|yes|YES|true|TRUE)
            printf '%s\n' "$*"
            ;;
    esac
}

prime_group_output_stream() {
    if [ -z "${RUNNER_OUTPUT_PRIMED:-}" ]; then
        printf '\n'
        RUNNER_OUTPUT_PRIMED=1
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
        OSCOMP_SUPPORT_ARCH_DIR="$support_arch_dir"
        export OSCOMP_SUPPORT_ARCH_DIR
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
            bb mkdir -p /glibc/lib /lib 2>/dev/null || true
            bb cp "$support_libgcc" /glibc/lib/libgcc_s.so.1 2>/dev/null || true
            bb cp "$support_libgcc" /lib/libgcc_s.so.1 2>/dev/null || true
        fi
        if [ -d /support/usr/lib/locale/C.UTF-8 ]; then
            bb mkdir -p /usr/lib/locale/C.UTF-8 2>/dev/null || true
            bb cp -a /support/usr/lib/locale/C.UTF-8/. /usr/lib/locale/C.UTF-8/ 2>/dev/null || true
            OSCOMP_SUPPORT_LOCPATH=/usr/lib/locale
            export OSCOMP_SUPPORT_LOCPATH
        fi
        if [ -f /support/meta/ltp_test.txt ]; then
            bb mkdir -p /etc/oscomp-ltp 2>/dev/null || true
            bb cp /support/meta/ltp_test.txt /etc/oscomp-ltp/ltp_test.txt 2>/dev/null || true
        fi
        if [ -f /support/meta/oscomp_plan.txt ]; then
            bb cp /support/meta/oscomp_plan.txt /etc/oscomp-plan.txt 2>/dev/null || true
        fi
        if [ -n "$support_arch_dir" ] && [ -d "/support/${support_arch_dir}/overlay" ]; then
            bb cp -a "/support/${support_arch_dir}/overlay/." / 2>/dev/null || true
        elif [ -d /support/overlay ]; then
            bb cp -a /support/overlay/. / 2>/dev/null || true
        fi
        bb umount /support >/dev/null 2>&1 || true
        bb rmdir /support >/dev/null 2>&1 || true
    else
        bb rmdir /support >/dev/null 2>&1 || true
    fi
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
    mount_support_disk
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

run_ltp_group() {
    root="$1"
    shell_path="$2"
    support_ltp_subset_dir || {
        runner_debug "#### OSCOMP RUNNER MISSING LTP SUBSET LIST ####"
        return 127
    }
    test_list="$SUPPORT_LTP_TEST_LIST"

    [ -d "$root/ltp/testcases" ] || {
        runner_debug "#### OSCOMP RUNNER MISSING LTP TESTCASES ${root}/ltp/testcases ####"
        return 127
    }

    if ! cd "$root"; then
        return 125
    fi

    ran_cases=0
    group_failed=0
    while IFS= read -r testcase || [ -n "$testcase" ]; do
        case "$testcase" in
            ''|\#*)
                continue
                ;;
        esac

        set -f
        # shellcheck disable=SC2086
        set -- $testcase
        set +f
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

        [ -n "$testcase_path" ] || {
            runner_debug "#### OSCOMP RUNNER MISSING LTP CASE ${testcase} ####"
            return 127
        }

        ran_cases=$((ran_cases + 1))
        echo "RUN LTP CASE $testcase"
        if [ "$#" -gt 0 ]; then
            if [ -n "$shell_path" ] && [ "${testcase_path##*.}" = "sh" ]; then
                if [ "${shell_path##*/}" = "busybox" ]; then
                    "$shell_path" sh "$testcase_path" "$@"
                else
                    "$shell_path" "$testcase_path" "$@"
                fi
            else
                "$testcase_path" "$@"
            fi
        else
            if [ -n "$shell_path" ] && [ "${testcase_path##*.}" = "sh" ]; then
                if [ "${shell_path##*/}" = "busybox" ]; then
                    "$shell_path" sh "$testcase_path"
                else
                    "$shell_path" "$testcase_path"
                fi
            else
                "$testcase_path"
            fi
        fi
        ret=$?
        echo "FAIL LTP CASE $testcase : $ret"
        if [ "$ret" -ne 0 ]; then
            group_failed=1
        fi
    done <"$test_list"

    [ "$ran_cases" -gt 0 ] || {
        runner_debug "#### OSCOMP RUNNER EMPTY LTP SUBSET ${test_list} ####"
        return 127
    }

    return "$group_failed"
}

run_basic_group() {
    root="$1"
    shell_path="$2"

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

    if [ -n "$shell_path" ] && [ "${shell_path##*/}" = "busybox" ]; then
        "$shell_path" sh ./run-all.sh
        ret=$?
    elif [ -n "$shell_path" ]; then
        "$shell_path" ./run-all.sh
        ret=$?
    else
        sh ./run-all.sh
        ret=$?
    fi
    return "$ret"
}

prepare_lmbench_env() {
    root="$1"
    compat_dir=/code/lmbench_src/bin/build
    compat_bin="$compat_dir/lmbench_all"
    root_bin="$root/lmbench_all"

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
    if [ -n "${IOZONE_STAGE_DIR:-}" ] && [ -d "$IOZONE_STAGE_DIR" ]; then
        bb rm -rf "$IOZONE_STAGE_DIR" 2>/dev/null || true
    fi
    IOZONE_STAGE_DIR=""
    export IOZONE_STAGE_DIR
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

    "$iozone_busybox" echo iozone automatic measurements || return $?
    "$iozone_bin" -a -r 1k -s 4m || return $?
    "$iozone_busybox" echo iozone throughput write/read measurements || return $?
    "$iozone_bin" -t 4 -i 0 -i 1 -r 1k -s 1m || return $?
    "$iozone_busybox" echo iozone throughput random-read measurements || return $?
    "$iozone_bin" -t 4 -i 0 -i 2 -r 1k -s 1m || return $?
    "$iozone_busybox" echo iozone throughput read-backwards measurements || return $?
    "$iozone_bin" -t 4 -i 0 -i 3 -r 1k -s 1m || return $?
    "$iozone_busybox" echo iozone throughput stride-read measurements || return $?
    "$iozone_bin" -t 4 -i 0 -i 5 -r 1k -s 1m || return $?
    "$iozone_busybox" echo iozone throughput fwrite/fread measurements || return $?
    "$iozone_bin" -t 4 -i 6 -i 7 -r 1k -s 1m || return $?
    "$iozone_busybox" echo iozone throughput pwrite/pread measurements || return $?
    "$iozone_bin" -t 4 -i 9 -i 10 -r 1k -s 1m || return $?
    "$iozone_busybox" echo iozone throughtput pwritev/preadv measurements || return $?
    "$iozone_bin" -t 4 -i 11 -i 12 -r 1k -s 1m || return $?
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
            append_word leaked_pids "$pid"
        done

        [ -n "$leaked_pids" ] || return 0
        last_leaked_pids="$leaked_pids"

        runner_debug "#### OSCOMP RUNNER CLEANUP LEAKED PIDS ROUND $cleanup_round ${leaked_pids} ####"
        for pid in $leaked_pids; do
            kill_process_tree TERM "$pid"
        done
        bb sleep 1
        for pid in $leaked_pids; do
            kill_process_tree KILL "$pid"
        done
        # Stray grandchildren are reparented to pid 1 when the group shell exits.
        # Reap any adopted zombies here so they stop accumulating across groups.
        for pid in $leaked_pids; do
            wait "$pid" 2>/dev/null || true
        done
        bb sleep 1
        cleanup_round=$((cleanup_round + 1))
    done

    dump_leaked_process_sample $last_leaked_pids
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
    chunk_file="/tmp/oscomp-stream-${group:-group}-${flavor:-default}-$$.chunk"
    bb tail -c +"$start_byte" "$output_file" | bb head -c "$bytes_to_emit" >"$chunk_file"
    normalize_group_output_chunk <"$chunk_file"
    bb rm -f "$chunk_file" 2>/dev/null || true
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
    [ -n "${STREAM_GROUP_OUTPUT_FRAGMENT:-}" ] || return 0
    emit_normalized_group_line "$STREAM_GROUP_OUTPUT_FRAGMENT"
    STREAM_GROUP_OUTPUT_FRAGMENT=""
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
    emit_group_start "$group_marker"

    (
        cd "$run_dir" || exit 125
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
            if [ -n "${PATH:-}" ]; then
                export PATH="$BUILT_GROUP_PATH:/bin:/usr/bin:/sbin:/usr/sbin:$PATH"
            else
                export PATH="$BUILT_GROUP_PATH:/bin:/usr/bin:/sbin:/usr/sbin"
            fi
        fi
        build_ld_library_path "$root"
        if [ -n "$BUILT_LD_LIBRARY_PATH" ]; then
            export LD_LIBRARY_PATH="$BUILT_LD_LIBRARY_PATH"
        fi
        if [ "$root" = /musl ] && [ "$group" = "ltp" ] && [ -f /lib/liboscomp-musl-compat.so ]; then
            if [ -n "${LD_PRELOAD:-}" ]; then
                export LD_PRELOAD="liboscomp-musl-compat.so:$LD_PRELOAD"
            else
                export LD_PRELOAD="liboscomp-musl-compat.so"
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

        script_shell=""
        if [ "$group" = "iozone" ] && [ -x "$run_dir/busybox" ]; then
            script_shell="$run_dir/busybox"
        elif pick_busybox_for_root "$root"; then
            script_shell="$PICK_BUSYBOX_FOR_ROOT_RESULT"
        elif [ -x /bin/sh ]; then
            script_shell=/bin/sh
        fi

        if [ -z "$script_shell" ]; then
            runner_debug "#### OSCOMP RUNNER MISSING SHELL ${root}/busybox ####"
            exit 127
        fi

        if [ "$group" = "basic" ]; then
            run_basic_group "$root" "$script_shell"
            exit $?
        fi
        if [ "$group" = "iozone" ]; then
            run_iozone_group "$root" "$flavor" "$run_dir"
            exit $?
        fi
        if [ "$group" = "ltp" ]; then
            run_ltp_group "$root" "$script_shell"
            exit $?
        fi

        if [ "${script_shell##*/}" = "busybox" ]; then
            exec "$script_shell" sh "./$run_script_name" </dev/null
        fi
        exec "$script_shell" "./$run_script_name" </dev/null
    ) >"$output_file" 2>&1 &
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
    stream_group_output_incremental "$output_file" "$streamed_bytes"
    flush_group_output_fragment
    cleanup_new_processes_since_snapshot
    cleanup_iozone_stage
    runner_debug "#### OSCOMP RUNNER END ${script} STATUS ${status} ####"
    bb rm -f "$output_file" 2>/dev/null || true
    [ "$status" -eq 124 ] || emit_group_end "$group_marker"
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
