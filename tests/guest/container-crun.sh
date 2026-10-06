#!/bin/sh
# Optional Level3 real OCI and enforced pids regression. Guest only.
set -eu
crun=/opt/thekernel-tools/bin/crun
base=/var/tmp/thekernel-crun.$$
state=$base/state
basic=tk-basic-$$
pids=tk-pids-$$
memory=tk-memory-$$
oom=tk-oom-$$
cleanup() {
    result=$?
    if [ "$result" != 0 ]; then cat "$base/basic.log" "$base/pids.log" "$base/memory.log" "$base/oom.log" 2>/dev/null || :; fi
    for name in "$basic" "$pids" "$memory" "$oom"; do
        if [ -d "$state/$name" ]; then "$crun" --root "$state" delete -f "$name" || result=1; fi
    done
    rm -rf "$base"
    exit "$result"
}
trap cleanup EXIT HUP INT TERM
mkdir -p /sys/fs/cgroup "$base/bundle"
if ! grep -q ' /sys/fs/cgroup cgroup2 ' /proc/mounts; then
    /opt/thekernel-tools/bin/mount -t cgroup2 none /sys/fs/cgroup
fi
echo +pids +memory > /sys/fs/cgroup/cgroup.subtree_control
cp -a /opt/thekernel-containers/busybox-root "$base/bundle/rootfs"
cp /opt/thekernel-tests/bin/thekernel-container-limit-probe "$base/bundle/rootfs/bin/limit-probe"
mkdir "$base/bundle/rootfs/cg"
cd "$base/bundle"
write_config() {
    name=$1
    args=$2
    resources=$3
    mkdir -p "/sys/fs/cgroup/$name"
    printf '%s\n' "{\"ociVersion\":\"1.0.2\",\"process\":{\"terminal\":false,\"user\":{\"uid\":0,\"gid\":0},\"args\":$args,\"env\":[\"PATH=/bin:/usr/bin\"],\"cwd\":\"/\",\"noNewPrivileges\":true},\"root\":{\"path\":\"rootfs\",\"readonly\":false},\"hostname\":\"tk-crun\",\"mounts\":[{\"destination\":\"/proc\",\"type\":\"proc\",\"source\":\"proc\"},{\"destination\":\"/dev\",\"type\":\"bind\",\"source\":\"/dev\",\"options\":[\"rbind\",\"nosuid\"]},{\"destination\":\"/cg\",\"type\":\"bind\",\"source\":\"/sys/fs/cgroup/$name\",\"options\":[\"rbind\",\"ro\"]}],\"linux\":{\"namespaces\":[{\"type\":\"pid\"},{\"type\":\"mount\"},{\"type\":\"uts\"},{\"type\":\"ipc\"},{\"type\":\"network\"},{\"type\":\"cgroup\"}],\"cgroupsPath\":\"/$name\",\"resources\":$resources}}" > config.json
}
echo 'KTAP version 1'
echo '1..4'
write_config "$basic" '["/bin/sh","-c","set -eu; [ \"$$\" = 1 ]; [ \"$(id -u)\" = 0 ]; [ \"$(/bin/busybox hostname)\" = tk-crun ]; cat /proc/self/cgroup; echo CRUN_OCI_COMMAND_EXIT_OK"]' '{}'
"$crun" --root "$state" run --keep "$basic" > "$base/basic.log" 2>&1
cat "$base/basic.log"
grep -q '^CRUN_OCI_COMMAND_EXIT_OK$' "$base/basic.log"
"$crun" --root "$state" delete "$basic"
echo 'ok 1 - signed crun real OCI command namespaces and exit'
write_config "$pids" '["/bin/limit-probe","pids"]' '{"pids":{"limit":8}}'
"$crun" --root "$state" run --keep "$pids" > "$base/pids.log" 2>&1
cat "$base/pids.log"
grep -q '^CRUN_PIDS_FORK_EAGAIN_OK children=7 limit=8$' "$base/pids.log"
[ "$(cat "/sys/fs/cgroup/$pids/pids.max")" = 8 ]
[ "$(cat "/sys/fs/cgroup/$pids/pids.current")" = 0 ]
limit_events=$(awk '$1 == "max" { print $2 }' "/sys/fs/cgroup/$pids/pids.events")
[ "$limit_events" -ge 1 ]
"$crun" --root "$state" delete "$pids"
echo 'ok 2 - real cgroup pids budget denies fork and refunds exited children'
echo THEKERNEL_CONTAINER_CRUN_PIDS_OK
write_config "$memory" '["/bin/limit-probe","memory"]' '{"memory":{"limit":33554432}}'
"$crun" --root "$state" run --keep "$memory" > "$base/memory.log" 2>&1
cat "$base/memory.log"
grep -q '^CRUN_MEMORY_REAL_CHARGE_REFUND_OK ' "$base/memory.log"
[ "$(cat "/sys/fs/cgroup/$memory/memory.max")" = 33554432 ]
[ "$(cat "/sys/fs/cgroup/$memory/memory.peak")" -ge 4194304 ]
"$crun" --root "$state" delete "$memory"
echo 'ok 3 - resident memory budget admits sparse VM and refunds real freed frames'
write_config "$oom" '["/bin/limit-probe","oom"]' '{"memory":{"limit":33554432}}'
result=0
"$crun" --root "$state" run --keep "$oom" > "$base/oom.log" 2>&1 || result=$?
cat "$base/oom.log"
[ "$result" = 137 ]
grep -q '^CRUN_MEMORY_TOUCH_BEGIN$' "$base/oom.log"
! grep -q CRUN_MEMORY_LIMIT_FAILED_TO_KILL "$base/oom.log"
[ "$(cat "/sys/fs/cgroup/$oom/memory.current")" -le 33554432 ]
for event in max oom oom_kill; do
    count=$(awk -v event="$event" '$1 == event { print $2 }' "/sys/fs/cgroup/$oom/memory.events")
    [ "$count" -ge 1 ]
done
"$crun" --root "$state" delete "$oom"
echo 'ok 4 - actual OCI memory overrun receives OOM SIGKILL with real events'
echo THEKERNEL_CONTAINER_CRUN_OK
