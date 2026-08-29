#!/bin/sh
# Minimal direct-kernel Linux init for the q35 ABI oracle.  The case list is
# intentionally carried in the command line so the image is reusable.
set -u

mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
mount -t proc proc /proc 2>/dev/null || true
mount -t sysfs sysfs /sys 2>/dev/null || true

cases=
for word in $(cat /proc/cmdline); do
    case "$word" in
        thekernel_abi_cases=*) cases=${word#thekernel_abi_cases=} ;;
    esac
done

status=0
old_ifs=$IFS
IFS=,
for case_id in $cases; do
    case "$case_id" in
        eventfd.portable-differential)
            /opt/thekernel-tests/portable/eventfd-differential || status=1
            ;;
        creat.raw-differential)
            /opt/thekernel-tests/portable/creat-differential || status=1
            ;;
        native-ni.fixed-slots)
            /opt/thekernel-tests/portable/native-ni-differential || status=1
            ;;
        '') status=1 ;;
        *)
            echo "THEKERNEL_ABI_INIT_FAIL unmapped-case=$case_id" >&2
            status=1
            ;;
    esac
done
IFS=$old_ifs

if [ "$status" -eq 0 ]; then
    echo THEKERNEL_ABI_INIT_COMPLETE
else
    echo THEKERNEL_ABI_INIT_FAIL >&2
fi
sync
/bin/busybox poweroff -f
exit "$status"
