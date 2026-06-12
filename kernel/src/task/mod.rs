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

// Re-exports from split sub-modules — keep the old `crate::task::*` paths unchanged.
pub use self::{
    accounting::*,
    futex::*,
    ops::*,
    process::{Mempolicy, ProcessData},
    resources::*,
    signal::*,
    stat::*,
    thread::{AsThread, AssumeSync, Thread},
    timer::*,
    user::*,
};
pub(crate) use self::{
    creds::{CapabilityState, Credentials},
    jobctl::{ContinueResult, StopReport},
    process::{
        CgroupNamespace, PidNamespace, TimeNamespace, UTS_FIELD_LEN, UserNamespace, UtsNamespace,
    },
    restart::*,
    thread::ProcStateHint,
};
