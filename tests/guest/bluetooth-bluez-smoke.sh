#!/bin/sh
set -eu

mkdir -p /run/dbus /var/lib/dbus /var/lib/bluetooth
/usr/bin/dbus-uuidgen --ensure=/var/lib/dbus/machine-id
/usr/bin/dbus-daemon --system --fork
/usr/lib/bluetooth/bluetoothd -n >/tmp/thekernel-bluetoothd.log 2>&1 &
bluetoothd_pid=$!
cleanup() {
    kill "$bluetoothd_pid" 2>/dev/null || true
    if [ -f /run/dbus/pid ]; then
        kill "$(cat /run/dbus/pid)" 2>/dev/null || true
    fi
}
trap cleanup EXIT HUP INT TERM

sleep 2
kill -0 "$bluetoothd_pid"
grep -q 'Bluetooth management interface 1.0 initialized' /tmp/thekernel-bluetoothd.log
/usr/bin/btmgmt info >/tmp/thekernel-btmgmt-info.log 2>&1
grep -Fx 'Index list with 0 items' /tmp/thekernel-btmgmt-info.log
/usr/bin/bluetoothctl list >/tmp/thekernel-bluetoothctl-list.log 2>&1
[ ! -s /tmp/thekernel-bluetoothctl-list.log ]
echo BLUETOOTH_BLUEZ_NO_CONTROLLER_ACCEPTANCE_DONE
