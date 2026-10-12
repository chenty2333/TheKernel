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
gpu_smoke=0
for option in $(cat /proc/cmdline); do
    case "$option" in
    n305.net=dhcp)
        ip link set eth0 up 2>/dev/null || true
        if ! udhcpc -i eth0 -n -q -t 4 -T 3 -s /etc/thekernel/n305-dhcp.script; then
            echo "N305_DHCP_FAILED: keep screen console; inspect PCI inventory and link diagnostics"
        fi
        ;;
    n305.netconsole=*) netconsole=${option#n305.netconsole=} ;;
    n305.gpu_smoke=1) gpu_smoke=1 ;;
    esac
done
if [ -n "$netconsole" ]; then
    /opt/thekernel-tests/bin/thekernel-netconsole "${netconsole%:*}" "${netconsole##*:}" &
fi

# The existing Mesa client checks the PCI ID, rejects software renderers, and
# verifies shader pixels. Run only on explicit hardware-validation boots and
# keep the shell available even if a client stalls. Results also reach the
# existing UDP relay without writing /dev/kmsg; no storage device is opened.
if [ "$gpu_smoke" = 1 ]; then
    (
      mkdir -p /run
      (
        client=/usr/local/bin/intel-mesa-smoke
        node=/dev/dri/renderD128
        if [ ! -x "$client" ] || [ ! -c "$node" ]; then
            echo 'N305_GPU_SMOKE state=NOT_RUN reason=client_or_render_node_missing'
            exit 0
        fi
        echo 'N305_GPU_SMOKE state=START'
        LD_BIND_NOW=1 MESA_LOADER_DRIVER_OVERRIDE=iris "$client" --execute "$node" &
        client_pid=$!
        (
            sleep 45 &
            timer_pid=$!
            trap 'kill "$timer_pid" 2>/dev/null || true' EXIT
            trap 'exit 0' HUP INT TERM
            wait "$timer_pid"
            trap - EXIT
            echo 'N305_GPU_SMOKE state=DEADLINE'
            kill -KILL "$client_pid" 2>/dev/null || true
        ) &
        watchdog_pid=$!
        status=0
        wait "$client_pid" || status=$?
        kill "$watchdog_pid" 2>/dev/null || true
        wait "$watchdog_pid" 2>/dev/null || true
        echo "N305_GPU_SMOKE state=EXIT status=$status"
      ) >/run/n305-gpu-smoke.log 2>&1
      cat /run/n305-gpu-smoke.log
      if [ -n "$netconsole" ]; then
          /opt/thekernel-tests/bin/thekernel-netconsole "${netconsole%:*}" "${netconsole##*:}" --stdin \
              </run/n305-gpu-smoke.log || true
      fi
    ) &
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
