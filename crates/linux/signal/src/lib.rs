#![no_std]
#![feature(allocator_api)]

#[cfg(not(target_pointer_width = "64"))]
compile_error!("thekernel-linux-signal 0.1.0 supports only 64-bit Linux ABIs");

#[macro_use]
extern crate log;
extern crate alloc;

pub mod api;
pub mod arch;

mod action;
pub use action::*;

mod pending;
pub use pending::*;

mod types;
pub use types::*;

mod signalfd;
pub use signalfd::*;
