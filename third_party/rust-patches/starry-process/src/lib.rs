//! Process Management

#![no_std]
#![feature(allocator_api)]
#![warn(missing_docs)]

extern crate alloc;

mod process;
mod process_group;
mod session;

/// A process ID, also used as session ID, process group ID, and thread ID.
pub type Pid = u32;

pub use process::{
    Process, ProcessAdmission, ProcessError, ProcessUsage, ThreadAdmission, ThreadIds,
    ZombieSnapshot, init_proc,
};
pub use process_group::ProcessGroup;
pub use session::Session;
