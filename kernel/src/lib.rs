//! The core functionality of a monolithic kernel, including loading user
//! programs and managing processes.

#![no_std]
#![feature(allocator_api)]
#![cfg_attr(feature = "dev-log", feature(bstr))]
#![feature(likely_unlikely)]
#![allow(missing_docs)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

#[cfg(any(
    all(feature = "eevdf-balanced", feature = "eevdf-latency"),
    all(feature = "eevdf-balanced", feature = "eevdf-throughput"),
    all(feature = "eevdf-latency", feature = "eevdf-throughput"),
))]
compile_error!(
    "select at most one EEVDF profile: eevdf-balanced, eevdf-latency, or eevdf-throughput"
);

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
#[cfg(feature = "bpf")]
mod jit_memory;
mod keyring;
mod mm;
mod mounts;
mod packet_cbpf;
#[cfg(feature = "pmu-diagnostics")]
mod pmu;
mod pseudofs;
mod random;
mod rcu;
mod readiness;
mod seccomp_jit;
mod syscall;
mod task;
#[cfg(test)]
mod test_support;
mod time;
mod world;
