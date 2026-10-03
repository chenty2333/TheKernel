#!/bin/sh
set -eu

export HOME=${HOME:-/root}
export PATH=${PATH:-/opt/thekernel-tests/bin:/sbin:/bin:/usr/sbin:/usr/bin}
export TERM=${TERM:-vt100}

mkdir -p /dev /proc /sys /tmp /var/tmp /root
chmod 1777 /tmp /var/tmp
mountpoint -q /proc || mount -t proc proc /proc
mountpoint -q /sys || mount -t sysfs sysfs /sys
mountpoint -q /dev || mount -t devtmpfs devtmpfs /dev

# Hardware PXE opts in explicitly; ordinary QEMU shell boots stay unchanged.
netconsole=""
for option in $(cat /proc/cmdline); do
    case "$option" in
    n305.net=dhcp)
        ip link set eth0 up 2>/dev/null || true
        if ! udhcpc -i eth0 -n -q -t 4 -T 3 -s /etc/thekernel/n305-dhcp.script; then
            echo "N305_DHCP_FAILED: keep screen console; inspect PCI inventory and link diagnostics"
        fi
        ;;
    n305.netconsole=*) netconsole=${option#n305.netconsole=} ;;
    esac
done
if [ -n "$netconsole" ]; then
    /opt/thekernel-tests/bin/thekernel-netconsole "${netconsole%:*}" "${netconsole##*:}" &
fi

# Emit readiness from the interactive prompt, after the shell has configured
# its terminal. Start a fresh line even when firmware or a command left a
# partial line; the runner intentionally accepts only standalone markers.
# The runner retains this handshake in its log but hides it from the live console.
export PS1='
THEKERNEL_SHELL_READY
# '
cd /root
set +e
/bin/sh -i
shell_status=$?
set -e
if [ "$shell_status" -ne 0 ]; then
    echo "THEKERNEL_SHELL_FAIL status=${shell_status}" >&2
fi
exec /bin/busybox poweroff -f
