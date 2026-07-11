#![no_std]
#![no_main]
#![doc = include_str!("../README.md")]

extern crate alloc;

use alloc::{borrow::ToOwned, vec::Vec};

const INIT_SCRIPT: &str = include_str!("init.sh");

pub const CMDLINE: &[&str] = &["/musl/busybox", "sh", "-c", INIT_SCRIPT];

#[cfg(feature = "boot-shell")]
pub const ENVS: &[&str] = &["OSCOMP_BOOT_SHELL=1", "THEKERNEL_BOOT_MODE=shell"];

#[cfg(not(feature = "boot-shell"))]
pub const ENVS: &[&str] = &["THEKERNEL_BOOT_MODE=eval"];

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

#[cfg(feature = "vf2")]
extern crate axplat_riscv64_visionfive2;
