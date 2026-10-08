#!/bin/sh
# Run with a QEMU e1000-family NIC and a host-forwarded loopback TCP port.
set -eu
iface=eth0
ip link set "$iface" up
udhcpc -i "$iface" -n -q -t 4 -T 2 -s /etc/thekernel/n305-dhcp.script
ping -c 1 -W 5 10.0.2.2
nc -l -p 8080 > /tmp/e1000-tcp-payload &
server=$!
echo E1000_TCP_READY
sleep 3
kill "$server" 2>/dev/null || true
wait "$server" 2>/dev/null || true
[ "$(cat /tmp/e1000-tcp-payload)" = E1000_TCP_PAYLOAD ]
echo E1000_NETWORK_OK
/bin/busybox poweroff -f
