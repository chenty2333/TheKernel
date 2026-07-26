//! The core functionality of a monolithic kernel, including loading user
//! programs and managing processes.

#![no_std]
#![feature(allocator_api)]
#![cfg_attr(feature = "dev-log", feature(bstr))]
#![feature(likely_unlikely)]
#![allow(missing_docs)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

extern crate alloc;
extern crate axruntime;

#[macro_use]
extern crate axlog;

pub mod entry;

#[cfg(feature = "bpf")]
pub mod bpf;

mod config;
mod deferred_work;
mod file;
mod keyring;
mod mm;
mod mounts;
#[cfg(feature = "pmu-diagnostics")]
mod pmu;
mod pseudofs;
mod random;
mod readiness;
mod syscall;
mod task;
#[cfg(test)]
mod test_support;
mod time;
