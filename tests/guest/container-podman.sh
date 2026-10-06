#!/bin/sh
# Optional Level4: genuine non-root user, delegated v2 and offline overlay.
set -eu
base=/var/tmp/thekernel-podman.$$
home=/home/tkcontainer
runtime=/run/user/1000
cg=/sys/fs/cgroup/tk-user-1000
mkdir -p "$base" "$home" "$runtime" /sys/fs/cgroup
if ! grep -q ' /sys/fs/cgroup cgroup2 ' /proc/mounts; then
    /opt/thekernel-tools/bin/mount -t cgroup2 none /sys/fs/cgroup
fi
if ! grep -q '^tkcontainer:' /etc/passwd; then
    echo 'tkcontainer:x:1000:1000:container guest:/home/tkcontainer:/bin/sh' >> /etc/passwd
    echo 'tkcontainer:x:1000:' >> /etc/group
fi
printf '%s\n' 'tkcontainer:100000:65536' > /etc/subuid
printf '%s\n' 'tkcontainer:100000:65536' > /etc/subgid
echo +pids +memory > /sys/fs/cgroup/cgroup.subtree_control
mkdir -p "$cg/manager"
echo +pids +memory > "$cg/cgroup.subtree_control"
chown -R 1000:1000 "$cg" "$home" "$runtime" "$base"
chmod 700 "$home" "$runtime"
cat > "$base/inside.sh" <<'INNER'
#!/bin/sh
set -eu
podman=/opt/thekernel-tools/bin/podman
base=$1
export HOME=/home/tkcontainer USER=tkcontainer LOGNAME=tkcontainer
export XDG_RUNTIME_DIR=/run/user/1000
export PATH=/opt/thekernel-tools/bin:/usr/bin:/bin:/sbin
"$podman" --log-level=debug --storage-driver=overlay --root "$HOME/storage" --runroot "$XDG_RUNTIME_DIR/storage" \
    load --input /opt/thekernel-containers/images/alpine-3.24.1.tar
"$podman" --log-level=debug --storage-driver=overlay --root "$HOME/storage" --runroot "$XDG_RUNTIME_DIR/storage" \
    run --rm --network=none --pull=never --cgroup-parent=/tk-user-1000 alpine echo hello
"$podman" --log-level=debug --storage-driver=overlay --root "$HOME/storage" --runroot "$XDG_RUNTIME_DIR/storage" \
    ps --all --quiet > "$base/containers-after-run"
[ ! -s "$base/containers-after-run" ]
echo THEKERNEL_PODMAN_REMOVE_OK
INNER
chmod 755 "$base/inside.sh"
/opt/thekernel-tests/bin/thekernel-container-rootless-run "$cg/manager/cgroup.procs" \
    /bin/sh "$base/inside.sh" "$base" > "$base/podman.log" 2>&1
cat "$base/podman.log"
grep -q '^THEKERNEL_ROOTLESS_UID=1000$' "$base/podman.log"
grep -q '^hello$' "$base/podman.log"
grep -q '^THEKERNEL_PODMAN_REMOVE_OK$' "$base/podman.log"
echo 'KTAP version 1'
echo '1..1'
echo 'ok 1 - rootless offline alpine overlay OCI hello'
echo THEKERNEL_CONTAINER_PODMAN_OK
