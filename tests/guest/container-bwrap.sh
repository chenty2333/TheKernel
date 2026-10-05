#!/bin/sh
# Level 2 optional signed bubblewrap acceptance.
set -eu
base=/var/tmp/thekernel-bwrap.$$
root=/opt/thekernel-containers/busybox-root
trap 'rm -rf "$base"' EXIT HUP INT TERM
mkdir -p "$base/fixture"
echo outside-input > "$base/fixture/input"
for kind in user mnt pid net uts ipc; do
    readlink "/proc/self/ns/$kind" > "$base/fixture/$kind-outside"
done
/opt/thekernel-tools/bin/bwrap --unshare-user --uid 0 --gid 0 \
    --unshare-pid --unshare-net --unshare-uts --unshare-ipc \
    --die-with-parent --new-session --hostname tk-bwrap \
    --ro-bind "$root" / --dev-bind /dev /dev --proc /proc --tmpfs /tmp \
    --ro-bind "$base/fixture" /fixture --chdir / \
    /bin/sh -c '
        set -eu
        [ "$(cat /fixture/input)" = outside-input ]
        if (echo changed > /fixture/input) 2>/dev/null; then exit 10; fi
        if (echo writable > /root/unexpected) 2>/dev/null; then exit 11; fi
        echo private-tmpfs > /tmp/private
        [ "$(cat /tmp/private)" = private-tmpfs ]
        [ "$(/bin/busybox hostname)" = tk-bwrap ]
        for kind in user mnt pid net uts ipc; do
            current=$(/bin/busybox readlink /proc/self/ns/$kind)
            [ "$current" != "$(cat /fixture/$kind-outside)" ]
        done
        [ -e /proc/1/status ]
        /bin/busybox ps
        echo BWRAP_READONLY_TMPFS_ISOLATION_OK
    '
[ "$(cat "$base/fixture/input")" = outside-input ]
[ ! -e "$base/fixture/private" ]
echo 'KTAP version 1'
echo '1..1'
echo 'ok 1 - signed bubblewrap readonly root fixture tmpfs and namespaces'
echo THEKERNEL_CONTAINER_BWRAP_OK
