//! ArceOS platform implementation for the Raspberry Pi Zero 2W.
//!
//! The Zero 2W is built around the RP3A0 system-in-package (BCM2710A1 die, the
//! same BCM2837 die as the Raspberry Pi 3): quad Cortex-A53 at 1 GHz, 512 MB
//! LPDDR2, and a VideoCore IV GPU. This crate implements the platform
//! contract:
//!
//! * default (tgoskits): the [`ax_plat`] interface used by `ax-hal` /
//!   `axruntime` (select with `AX_PLATFORM_CRATE=axplat_bcm2837_zero2w`);
//! * `legacy`: the crates.io `axplat` 0.3 interface used by TheKernel.
//!
//! The two interfaces are mutually exclusive; enable at most one.

#![no_std]
#![allow(static_mut_refs)]

extern crate alloc;

#[cfg(not(feature = "legacy"))]
#[macro_use]
extern crate ax_plat;

mod boot;
mod config;
mod console;
#[cfg(not(feature = "legacy"))]
mod cpu;
#[cfg(not(feature = "legacy"))]
mod init;
mod irq;
mod mem;
#[cfg(not(feature = "legacy"))]
mod platform;
mod power;
mod time;

#[cfg(feature = "legacy")]
mod legacy;

#[cfg(feature = "legacy")]
use axplat_old as _;
