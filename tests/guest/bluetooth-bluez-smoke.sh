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
/usr/bin/hciconfig >/tmp/thekernel-hciconfig-list.log 2>&1
[ ! -s /tmp/thekernel-hciconfig-list.log ]
if /usr/bin/hciconfig hci0 >/tmp/thekernel-hciconfig-info.log 2>&1; then
    echo 'hciconfig unexpectedly found hci0 without a controller' >&2
    exit 1
fi
grep -qi 'no such device' /tmp/thekernel-hciconfig-info.log || {
    cat /tmp/thekernel-hciconfig-info.log >&2
    exit 1
}
/usr/bin/btmon -i 0 >/tmp/thekernel-btmon-no-device.log 2>&1 &
btmon_pid=$!
sleep 1
kill -0 "$btmon_pid"
grep -q 'Bluetooth monitor ver 5.86' /tmp/thekernel-btmon-no-device.log
kill "$btmon_pid"
wait "$btmon_pid" 2>/dev/null || true
echo BLUETOOTH_BLUEZ_NO_CONTROLLER_ACCEPTANCE_DONE
