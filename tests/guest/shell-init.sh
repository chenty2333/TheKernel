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

# Emit readiness from the interactive prompt, after the shell has configured
# its terminal. Start a fresh line even when firmware or a command left a
# partial line; the runner intentionally accepts only standalone markers.
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
