//! The core functionality of a monolithic kernel, including loading user
//! programs and managing processes.

#![no_std]
#![feature(likely_unlikely)]
#![feature(bstr)]
#![feature(allocator_api)]
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
mod pseudofs;
mod random;
mod readiness;
mod syscall;
mod task;
mod time;
