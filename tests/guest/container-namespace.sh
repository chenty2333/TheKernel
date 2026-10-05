#!/bin/sh
# Optional signed util-linux acceptance, never run namespaces on the host.
set -eu
TOOLS=/opt/thekernel-tools/bin
base=/var/tmp/thekernel-container-namespace.$$
launcher=
outside=
cleanup() {
    result=$?
    if [ "$result" != 0 ]; then
        cat "$base/inside.log" 2>/dev/null || :
        echo 'not ok 1 - util-linux namespace isolation or nsenter'
    fi
    # Release the namespace init's normal exit path before waiting. Merely
    # TERMing its wrapper may leave PID1 ignoring the forwarded signal.
    [ -z "$launcher" ] || echo finish > "$base/finish"
    [ -z "$launcher" ] || wait "$launcher" 2>/dev/null || :
    [ -z "$outside" ] || kill "$outside" 2>/dev/null || :
    [ -z "$outside" ] || wait "$outside" 2>/dev/null || :
    rm -rf "$base"
}

trap cleanup EXIT HUP INT TERM
mkdir -p "$base/mnt"
original=$(readlink /proc/self/ns/mnt)
"$TOOLS/top" -b -d 60 > "$base/outside.log" 2>&1 &
outside=$!
"$TOOLS/unshare" -mpfUr --mount-proc /bin/sh -c '
    set -eu
    base=$1
    tools=/opt/thekernel-tools/bin
    [ "$$" = 1 ]
    [ "$(id -u)" = 0 ] && [ "$(id -g)" = 0 ]
    "$tools/ps" -eo pid,ppid,comm > "$base/inside-ps"
    ! grep -w top "$base/inside-ps"
    "$tools/mount" -t tmpfs -o mode=755 tmpfs "$base/mnt"
    echo namespace-private > "$base/mnt/private"
    "$tools/mount" > "$base/inside-mounts"
    grep -F " on $base/mnt " "$base/inside-mounts"
    readlink /proc/self/ns/mnt > "$base/namespace"
    echo ready > "$base/ready"
    while [ ! -e "$base/finish" ]; do sleep 0.1; done
' sh "$base" > "$base/inside.log" 2>&1 &
launcher=$!
tries=0
while [ ! -f "$base/ready" ]; do
    if ! kill -0 "$launcher" 2>/dev/null || [ "$tries" -ge 200 ]; then
        cat "$base/inside.log"
        echo 'not ok 1 - util-linux unshare setup'
        exit 1
    fi
    tries=$((tries + 1))
    sleep 0.1
done
[ "$original" != "$(cat "$base/namespace")" ]
[ "$original" = "$(readlink /proc/self/ns/mnt)" ]
[ ! -e "$base/mnt/private" ]
"$TOOLS/mount" > "$base/parent-mounts"
! grep -F " on $base/mnt " "$base/parent-mounts"
kill -0 "$outside"
"$TOOLS/nsenter" --user="/proc/$launcher/ns/user" \
    --mount="/proc/$launcher/ns/mnt" --pid="/proc/$launcher/ns/pid_for_children" \
    /bin/sh -c '
        set -eu
        [ "$(id -u)" = 0 ]
        [ "$(cat "$1/mnt/private")" = namespace-private ]
        /opt/thekernel-tools/bin/ps -eo pid,ppid,comm > "$1/entered-ps"
        ! grep -w top "$1/entered-ps"
        echo NSENTER_REAL_NAMESPACE_OK
    ' sh "$base"
echo finish > "$base/finish"
wait "$launcher"
launcher=
cat "$base/inside-ps" "$base/entered-ps"
echo 'KTAP version 1'
echo '1..1'
echo 'ok 1 - signed util-linux user mount PID isolation and nsenter'
echo THEKERNEL_CONTAINER_NAMESPACE_OK
