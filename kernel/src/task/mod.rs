//! User task management.

mod accounting;
pub(crate) mod coredump;
mod creds;
mod futex;
mod jobctl;
mod ops;
mod process;
mod resources;
mod restart;
mod signal;
mod stat;
mod thread;
mod timer;
mod user;

pub(crate) use self::restart::*;
pub use self::{
    accounting::*, futex::*, ops::*, resources::*, signal::*, stat::*, timer::*, user::*,
};

// Re-exports from split sub-modules — keep the old `crate::task::*` paths unchanged.
pub use self::thread::{AsThread, AssumeSync, Thread};
pub(crate) use self::thread::init_thread_cache;
pub(crate) use self::thread::ProcStateHint;
pub(crate) use self::creds::{CapabilityState, Credentials};
pub(crate) use self::jobctl::ContinueResult;
pub use self::process::ProcessData;
