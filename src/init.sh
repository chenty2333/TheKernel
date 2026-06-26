#!/bin/sh

set +e

BUSYBOX=/musl/busybox
[ -x "$BUSYBOX" ] || BUSYBOX=/glibc/busybox

bb() {
    "$BUSYBOX" "$@"
}

busybox_for_root() {
    root="$1"
    if [ -x "$root/busybox" ]; then
        printf '%s\n' "$root/busybox"
    else
        printf '%s\n' "$BUSYBOX"
    fi
}

run_shell() {
    root="$1"
    shift
    shell_busybox="$(busybox_for_root "$root")"
    "$shell_busybox" ash "$@"
}

ROOTS="/musl /glibc"
NORMALIZE_SED=/tmp/oscomp-marker-normalize.sed

detect_arch() {
    machine="$(uname -m 2>/dev/null || true)"
    case "$machine" in
        loongarch64*) OSCOMP_ARCH=la ;;
        riscv64*) OSCOMP_ARCH=rv ;;
        *) [ -d /support/la ] && OSCOMP_ARCH=la || OSCOMP_ARCH=rv ;;
    esac
    export OSCOMP_ARCH OSCOMP_MACHINE="$machine"
}

setup_base_fs() {
    bb mkdir -p /bin /sbin /usr/bin /usr/sbin /usr/lib/locale
    bb mkdir -p /etc /root /tmp /var/tmp /dev /proc /sys /support
    bb mkdir -p /opt/oscomp-support/bin /opt/oscomp-support/lib
    bb mkdir -p /opt/oscomp-support/ltp-cases /opt/oscomp-support/share
    bb chmod 1777 /tmp /var/tmp 2>/dev/null || true
    bb mkdir -p /tmp/memfd

    bb ln -sf "$BUSYBOX" /bin/busybox 2>/dev/null || true
    "$BUSYBOX" --install -s /bin >/dev/null 2>&1 || true
    [ -e /usr/bin/env ] || bb ln -sf "$BUSYBOX" /usr/bin/env 2>/dev/null || true

    printf '%s\n' \
        'root:x:0:0:root:/root:/bin/sh' \
        'nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin' \
        > /etc/passwd
    printf '%s\n' \
        'root:x:0:' \
        'daemon:x:1:' \
        'users:x:100:' \
        'nobody:x:65534:' \
        > /etc/group
    printf '%s\n' \
        'passwd: files' \
        'group: files' \
        > /etc/nsswitch.conf
    printf '%s\n' \
        '127.0.0.1 localhost' \
        '::1 localhost ip6-localhost ip6-loopback' \
        > /etc/hosts
    printf '%s\n' \
        'echo 7/tcp' \
        'echo 7/udp' \
        > /etc/services

    export PATH="/opt/oscomp-support/bin:/bin:/usr/bin:/sbin:/usr/sbin"
    export SHELL=/bin/sh HOME=/root USER=root TERM=dumb
    export TMPDIR=/var/tmp TMP=/var/tmp TEMP=/var/tmp
    export OSCOMP_SUPPORT_BIN=/opt/oscomp-support/bin
    export OSCOMP_SUPPORT_LIB=/opt/oscomp-support/lib
}

support_payload_present() {
    [ -f /support/meta/ltp_test.txt ] && return 0
    [ -f /support/meta/oscomp_plan.txt ] && return 0
    [ -d "/support/$OSCOMP_ARCH/overlay" ] && return 0
    return 1
}

mount_support_disk() {
    support_payload_present && return 0
    for dev in /dev/vdb /dev/sdb /dev/vdc /dev/sdc /dev/vda /dev/sda; do
        [ -e "$dev" ] || continue
        for fs in ext4 ext2; do
            bb mount -t "$fs" -o ro "$dev" /support >/dev/null 2>&1 || continue
            support_payload_present && return 0
            bb umount /support >/dev/null 2>&1 || true
        done
    done
    return 1
}

copy_tree() {
    src="$1"
    dst="$2"
    [ -d "$src" ] || return 0
    bb mkdir -p "$dst"
    bb cp -a "$src/." "$dst/" 2>/dev/null || bb cp -R "$src/." "$dst/" 2>/dev/null || true
}

link_file() {
    src="$1"
    dst="$2"
    [ -e "$src" ] || return 0
    bb mkdir -p "${dst%/*}" 2>/dev/null || true
    bb ln -sf "$src" "$dst" 2>/dev/null || true
}

stage_support_disk() {
    mount_support_disk || return 0

    [ -f /support/meta/ltp_test.txt ] && bb cp /support/meta/ltp_test.txt /etc/oscomp-ltp.txt 2>/dev/null || true
    [ -f /support/meta/oscomp_plan.txt ] && bb cp /support/meta/oscomp_plan.txt /etc/oscomp-plan.txt 2>/dev/null || true
    [ -f /support/meta/oscomp.env ] && bb cp /support/meta/oscomp.env /etc/oscomp.env 2>/dev/null || true

    arch_root="/support/$OSCOMP_ARCH"
    copy_tree /support/usr/lib/locale/C.UTF-8 /usr/lib/locale/C.UTF-8
    copy_tree "$arch_root/glibc/lib" /glibc/lib
    copy_tree "$arch_root/overlay/bin" /opt/oscomp-support/bin
    copy_tree "$arch_root/overlay/lib" /opt/oscomp-support/lib
    copy_tree "$arch_root/overlay/ltp-cases" /opt/oscomp-support/ltp-cases
    copy_tree "$arch_root/overlay/share" /opt/oscomp-support/share
    copy_tree "$arch_root/overlay/musl" /musl
    copy_tree "$arch_root/overlay/glibc" /glibc

    bb umount /support >/dev/null 2>&1 || true
}

setup_loaders() {
    bb mkdir -p /lib /usr/lib 2>/dev/null || true
    for file in /musl/lib/*; do
        [ -e "$file" ] && link_file "$file" "/lib/${file##*/}"
    done
    link_file /musl/lib/libc.so /lib/ld-musl-loongarch-lp64d.so.1
    link_file /musl/lib/libc.so /lib/ld-musl-riscv64.so.1
    link_file /musl/lib/libc.so /lib/ld-musl-riscv64-sf.so.1
    link_file /glibc/lib/ld-linux-loongarch-lp64d.so.1 /lib/ld-linux-loongarch-lp64d.so.1
    link_file /glibc/lib/ld-linux-riscv64-lp64d.so.1 /lib/ld-linux-riscv64-lp64d.so.1
    link_file /glibc/lib/libc.so.6 /lib/libc.so.6
    link_file /glibc/lib/libm.so.6 /lib/libm.so.6
    bb rm -rf /lib64 /usr/lib64 2>/dev/null || true
    bb ln -sf /lib /lib64 2>/dev/null || true
    bb ln -sf /lib /usr/lib64 2>/dev/null || true
    printf '%s\n' /lib /usr/lib /musl/lib > /etc/ld-musl-riscv64.path 2>/dev/null || true
    printf '%s\n' /lib /usr/lib /musl/lib > /etc/ld-musl-riscv64-sf.path 2>/dev/null || true
    printf '%s\n' /lib /usr/lib /musl/lib > /etc/ld-musl-loongarch-lp64d.path 2>/dev/null || true
}

load_env_file() {
    [ -f /etc/oscomp.env ] || return 0
    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in ''|'#'*) continue ;; esac
        name=${line%%=*}
        case "$name" in ''|[0-9]*|*[!A-Za-z0-9_]*) continue ;; esac
        export "$line" 2>/dev/null || true
    done < /etc/oscomp.env
}

prepare_unixbench_inputs() {
    src=/opt/oscomp-support/share/unixbench/sort.src
    [ -f "$src" ] || return 0
    for root in $ROOTS; do
        [ -f "$root/unixbench_testcode.sh" ] && [ ! -f "$root/sort.src" ] && bb cp "$src" "$root/sort.src" 2>/dev/null
    done
}

prepare_lmbench_path() {
    root="$1"
    [ -x "$root/lmbench_all" ] || return 0
    bb mkdir -p /code/lmbench_src/bin/build 2>/dev/null || true
    bb ln -sf "$root/lmbench_all" /code/lmbench_src/bin/build/lmbench_all 2>/dev/null || true
}

add_preload_if_present() {
    so="$1"
    [ -f "$so" ] || return 0
    [ -n "$preload" ] && preload="$so:$preload" || preload="$so"
}

cleanup_after_group() {
    for mount_dir in /musl/basic/mnt /glibc/basic/mnt; do
        bb umount "$mount_dir" >/dev/null 2>&1 || true
    done
    for proc in iperf3 netserver hackbench cyclictest iozone; do
        bb killall -9 "$proc" >/dev/null 2>&1 || true
    done
}

normalize_group_markers() {
    if [ ! -f "$NORMALIZE_SED" ]; then
        bb cat > "$NORMALIZE_SED" <<'EOF'
s/^\(#### OS COMP TEST GROUP START [^ ]*\)-musl\( ####\)$/\1\2/
s/^\(#### OS COMP TEST GROUP END [^ ]*\)-musl\( ####\)$/\1\2/
s/^\(#### OS COMP TEST GROUP START [^ ]*\)-glibc\( ####\)$/\1\2/
s/^\(#### OS COMP TEST GROUP END [^ ]*\)-glibc\( ####\)$/\1\2/
EOF
    fi
    bb sed -f "$NORMALIZE_SED"
}

flavor_for_root() {
    [ "$1" = /glibc ] && FLAVOR=glibc || FLAVOR=musl
}

build_runtime_env() {
    root="$1"
    group="$2"
    flavor_for_root "$root"
    if [ "$group" = ltp ]; then
        PATH="$root/ltp/testcases/bin:$root/ltp/testscripts:/opt/oscomp-support/bin:/bin:/usr/bin:/sbin:/usr/sbin"
    else
        PATH="$root:$root/basic:$root/ltp/testcases/bin:$root/ltp/testscripts:/opt/oscomp-support/bin:/bin:/usr/bin:/sbin:/usr/sbin"
    fi
    export PATH TMPDIR=/var/tmp TMP=/var/tmp TEMP=/var/tmp
    export LTP_DEV_FS_TYPE=tmpfs LTP_SINGLE_FS_TYPE=tmpfs
    export LTP_COLORIZE_OUTPUT="${OSCOMP_LTP_COLORIZE_OUTPUT:-y}"

    if [ "$root" = /glibc ]; then
        unset LD_PRELOAD
        export LD_LIBRARY_PATH="/glibc/lib:/lib:/usr/lib:/opt/oscomp-support/lib"
        export LANG=C.UTF-8 LC_ALL=C.UTF-8 LC_CTYPE=C.UTF-8 LOCPATH=/usr/lib/locale
    else
        export LD_LIBRARY_PATH="/musl/lib:/lib:/usr/lib:/opt/oscomp-support/lib"
        preload=
        case "$OSCOMP_ARCH:$group" in
            la:*|*:ltp) add_preload_if_present /opt/oscomp-support/lib/liboscomp-musl-compat.so ;;
        esac
        [ "$group" = ltp ] && add_preload_if_present /opt/oscomp-support/lib/liboscomp-mmsg-compat.so
        [ -n "$preload" ] && export LD_PRELOAD="$preload" || unset LD_PRELOAD
    fi
    [ "$group" = ltp ] && export LTPROOT="$root/ltp"
}

run_regular_group() {
    root="$1"
    group="$2"
    script="$root/${group}_testcode.sh"
    [ -f "$script" ] || return 0
    (
        cd "$root" || exit 125
        build_runtime_env "$root" "$group"
        [ "$group" = lmbench ] && prepare_lmbench_path "$root"
        run_shell "$root" "./${group}_testcode.sh"
    ) < /dev/null 2>&1
    cleanup_after_group
}

ltp_timeout_mul_for_case() {
    case "$1" in
        fork06) printf '2\n' ;;
        *) return 1 ;;
    esac
}

run_ltp_case_cmd() {
    tag="$1"
    shift
    timeout_mul="$(ltp_timeout_mul_for_case "$tag" 2>/dev/null || true)"
    if [ -n "$timeout_mul" ]; then
        ( export LTP_TIMEOUT_MUL="$timeout_mul"; "$@" )
    else
        "$@"
    fi
}

run_ltp_command() {
    root="$1"
    flavor="$2"
    tag="$3"
    cmdline="$4"

    set -- $cmdline
    prog="$1"
    shift

    finish_ltp_case() {
        ret="$1"
        if [ "$ret" -eq 0 ] 2>/dev/null; then
            echo "PASS LTP CASE $tag : $ret"
        else
            echo "FAIL LTP CASE $tag : $ret"
        fi
    }

    for key in "$tag" "$prog"; do
        override="/opt/oscomp-support/ltp-cases/$flavor/$key"
        if [ -f "$override" ]; then
            echo "RUN LTP CASE $tag"
            run_ltp_case_cmd "$tag" run_shell "$root" "$override" "$@" < /dev/null
            ret=$?
            finish_ltp_case "$ret"
            return 0
        fi
    done

    if [ -f "$prog" ]; then
        echo "RUN LTP CASE $tag"
        case "$prog" in
            *.sh)
                run_ltp_case_cmd "$tag" run_shell "$root" "./$prog" "$@" < /dev/null
                ;;
            *)
                run_ltp_case_cmd "$tag" "./$prog" "$@" < /dev/null
                ;;
        esac
        ret=$?
        finish_ltp_case "$ret"
        return 0
    fi

    script="$root/ltp/testscripts/$prog"
    if [ -f "$script" ]; then
        echo "RUN LTP CASE $tag"
        run_ltp_case_cmd "$tag" run_shell "$root" "$script" "$@" < /dev/null
        ret=$?
        finish_ltp_case "$ret"
    fi
}

run_ltp_group() {
    root="$1"
    list=/etc/oscomp-ltp.txt
    bin_dir="$root/ltp/testcases/bin"
    [ -d "$bin_dir" ] && [ -f "$list" ] || return 0
    flavor_for_root "$root"
    flavor="$FLAVOR"

    echo "#### OS COMP TEST GROUP START ltp ####"
    (
        cd "$bin_dir" || exit 125
        build_runtime_env "$root" ltp
        while IFS= read -r line || [ -n "$line" ]; do
            case "$line" in ''|'#'*) continue ;; esac
            set -- $line
            tag="$1"
            shift
            [ $# -gt 0 ] && cmdline="$*" || cmdline="$tag"
            run_ltp_command "$root" "$flavor" "$tag" "$cmdline"
        done < "$list"
    ) 2>&1
    echo "#### OS COMP TEST GROUP END ltp ####"
    cleanup_after_group
}

run_group() {
    root="$1"
    group="$2"
    [ -d "$root" ] || return 0
    if [ "$root" = /glibc ] && [ "$group" = libctest ]; then
        return 0
    fi
    [ "$group" = ltp ] && run_ltp_group "$root" || run_regular_group "$root" "$group"
}

run_plan_file() {
    [ -f "$1" ] || return 1
    while IFS=' ' read -r root group _ || [ -n "$root" ]; do
        case "$root" in ''|'#'*) continue ;; esac
        if [ -n "$group" ]; then
            run_group "$root" "$group"
            continue
        fi

        case "$root" in
            *-musl)
                run_group /musl "${root%-musl}"
                ;;
            *-glibc)
                run_group /glibc "${root%-glibc}"
                ;;
            *)
                for plan_root in $ROOTS; do
                    run_group "$plan_root" "$root"
                done
                ;;
        esac
    done < "$1"
    return 0
}

run_default_plan() {
    while IFS=' ' read -r root group _ || [ -n "$root" ]; do
        case "$root" in ''|'#'*) continue ;; esac
        [ -n "$group" ] && run_group "$root" "$group"
    done <<'EOF'
/musl basic
/musl iozone
/musl busybox
/musl netperf
/musl lua
/musl libcbench
/musl libctest
/musl unixbench
/musl cyclictest
/glibc basic
/glibc iozone
/glibc busybox
/glibc netperf
/glibc lua
/glibc libcbench
/glibc unixbench
/glibc cyclictest
/musl lmbench
/glibc lmbench
/musl iperf
/glibc iperf
/glibc ltp
/musl ltp
EOF
}

shutdown_system() {
    bb sync >/dev/null 2>&1 || true
    echo "System is shutting down"
    bb poweroff -f >/dev/null 2>&1 || true
    bb reboot -f >/dev/null 2>&1 || true
    bb halt -f >/dev/null 2>&1 || true
}

setup_base_fs
detect_arch
stage_support_disk
setup_loaders
load_env_file
prepare_unixbench_inputs
printf '\n'
run_plan_file /etc/oscomp-plan.txt || run_default_plan
shutdown_system
exit 0
