#!/bin/sh
# Real, unmodified B1 programs. Nonzero results remain failures, not skips.
export LC_ALL=C TERM=xterm
T=/opt/thekernel-tools/bin
failures=0
probe() {
    name=$1
    shift
    echo "# INSPECT_BEGIN $name"
    /bin/busybox timeout -k 2 15 "$@"
    result=$?
    echo "# INSPECT_END $name result=$result"
    [ "$result" = 0 ] || failures=$((failures+1))
}
probe ps "$T/ps" aux
probe top "$T/top" -b -n 1
probe free "$T/free" -w
probe vmstat "$T/vmstat" -s
probe uptime "$T/uptime"
probe pmap "$T/pmap" -x $$
probe pidstat "$T/pidstat" -p $$
probe htop /opt/thekernel-tests/bin/thekernel-inspect-pty-smoke "$T/htop" --readonly
probe lsblk "$T/lsblk" -a -o NAME,SIZE,TYPE,RO,RM,PKNAME
probe findmnt "$T/findmnt"
probe mount "$T/mount"
probe df "$T/df" -h
probe lscpu "$T/lscpu"
probe lsns "$T/lsns"
probe lspci-vvv "$T/lspci" -vvv
probe lspci-k "$T/lspci" -k
probe lspci-tree "$T/lspci" -t
probe usb-hwdb /bin/busybox sh -c '[ "$(/bin/busybox dd if=/etc/udev/hwdb.bin bs=8 count=1 2>/dev/null)" = KSLPHHRH ]'
probe lsusb "$T/lsusb"
probe iostat "$T/iostat" -x
probe mpstat "$T/mpstat" -P ALL
probe ip "$T/ip" -s link
probe ss "$T/ss" -tanp
probe netstat "$T/netstat" -tunap
for file in dev route tcp tcp6 udp udp6 unix snmp; do
    probe "net-$file" /bin/busybox cat "/proc/net/$file"
done
echo "INSPECT_TOOLS_COMPLETE failures=$failures"
[ "$failures" = 0 ]
