#!/bin/sh
# Run with a QEMU e1000-family NIC and a host-forwarded loopback TCP port.
set -eu
mac=52:54:00:00:00:03
iface=
for address_file in /sys/class/net/*/address; do
    [ "$(cat "$address_file")" = "$mac" ] || continue
    iface=${address_file%/address}
    iface=${iface##*/}
    break
done
[ -n "$iface" ] || { echo "E1000_NIC_NOT_FOUND mac=$mac"; exit 10; }
ip link set "$iface" up
udhcpc -i "$iface" -n -q -t 4 -T 2 -s /etc/thekernel/n305-dhcp.script
ping -c 1 -W 5 10.0.2.2
nc -l -p 8080 > /tmp/e1000-tcp-payload &
server=$!
echo E1000_TCP_READY
wait "$server"
[ "$(cat /tmp/e1000-tcp-payload)" = E1000_TCP_PAYLOAD ]
echo E1000_NETWORK_OK
/bin/busybox poweroff -f
