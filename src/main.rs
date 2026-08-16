#![no_std]
#![no_main]
#![doc = include_str!("../README.md")]

extern crate alloc;

use alloc::{borrow::ToOwned, vec::Vec};

pub const SYSTEM_CMDLINE: &[&str] = &["/sbin/init"];
pub const SHELL_CMDLINE: &[&str] = &["/bin/busybox", "sh", "/etc/thekernel/shell-init.sh"];

#[cfg(not(feature = "boot-shell"))]
pub const CMDLINE: &[&str] = SYSTEM_CMDLINE;

#[cfg(feature = "boot-shell")]
pub const CMDLINE: &[&str] = SHELL_CMDLINE;

#[cfg(feature = "boot-shell")]
pub const ENVS: &[&str] = &[
    "HOME=/root",
    "PATH=/opt/thekernel-tests/bin:/sbin:/bin:/usr/sbin:/usr/bin",
    "TERM=vt100",
];

#[cfg(not(feature = "boot-shell"))]
pub const ENVS: &[&str] = &[
    "HOME=/",
    "PATH=/opt/thekernel-tests/bin:/sbin:/bin:/usr/sbin:/usr/bin",
    "TERM=vt100",
];

#[unsafe(no_mangle)]
fn main() {
    let args = CMDLINE
        .iter()
        .copied()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let envs = ENVS.iter().copied().map(str::to_owned).collect::<Vec<_>>();

    thekernel_kernel::entry::init(&args, &envs);
}
