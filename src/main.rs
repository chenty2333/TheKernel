#![no_std]
#![no_main]
#![doc = include_str!("../README.md")]

extern crate alloc;

use alloc::{borrow::ToOwned, vec::Vec};

const INIT_BOOTSTRAP: &str = r#"
set +e
BUSYBOX=/musl/busybox
[ -x "$BUSYBOX" ] || BUSYBOX=/glibc/busybox
"$BUSYBOX" mkdir -p /support /tmp /dev 2>/dev/null
attempt=0
while [ "$attempt" -lt 30 ]; do
    for dev in /dev/vdb /dev/sdb /dev/vdc /dev/sdc /dev/vda /dev/sda; do
        [ -e "$dev" ] || continue
        for fs in ext4 ext2; do
            "$BUSYBOX" mount -t "$fs" -o ro "$dev" /support >/dev/null 2>&1 || continue
            if [ -f /support/meta/init.sh ]; then
                exec "$BUSYBOX" sh /support/meta/init.sh
            fi
            "$BUSYBOX" umount /support >/dev/null 2>&1 || true
        done
    done
    attempt=$((attempt + 1))
    "$BUSYBOX" sleep 1
done
echo "missing oscomp support init"
"$BUSYBOX" poweroff -f >/dev/null 2>&1 || true
"$BUSYBOX" reboot -f >/dev/null 2>&1 || true
"$BUSYBOX" halt -f >/dev/null 2>&1 || true
exit 1
"#;

pub const CMDLINE: &[&str] = &["/musl/busybox", "sh", "-c", INIT_BOOTSTRAP];

#[unsafe(no_mangle)]
fn main() {
    let args = CMDLINE
        .iter()
        .copied()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let envs = [];

    starry_kernel::entry::init(&args, &envs);
}

#[cfg(feature = "vf2")]
extern crate axplat_riscv64_visionfive2;
