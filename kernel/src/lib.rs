//! The core functionality of a monolithic kernel, including loading user
//! programs and managing processes.

#![no_std]
// The bpf/perf object graph is mutually referential through `Arc` (perf event
// files hold programs, programs hold maps, maps hold perf event files), so the
// default trait-solver depth overflows on the auto-trait checks for those
// cycles.  Give the solver more room instead of scattering manual impls.
#![recursion_limit = "256"]
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
#[cfg(feature = "bpf")]
mod bpf_security;

mod async_operation;
mod config;
mod deferred_work;
pub mod drm;
mod file;
mod jit_memory;
mod keyring;
mod mm;
mod mounts;
mod nfs_gss;
mod nfs_transport;
mod packet_cbpf;
mod perf_records;
mod perf_security;
mod perf_sources;
#[cfg(feature = "pmu-diagnostics")]
mod pmu;
mod pmu_registry;
mod pseudofs;
mod random;
mod rcu;
mod readiness;
mod seccomp_jit;
mod syscall;
mod task;
#[cfg(test)]
mod test_support;
mod text_patch;
mod time;
mod uprobe;
