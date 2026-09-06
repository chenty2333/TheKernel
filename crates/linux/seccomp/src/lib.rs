//! Bounded Linux seccomp policy and classic-BPF profile contracts.
//!
//! This crate owns Linux-profile instruction plans, filter stacking, action
//! precedence, accounting, and explicit task-state transitions. Generic
//! classic-BPF verification and execution are supplied through the neutral
//! executor port by the embedding kernel. It deliberately does not
//! dereference userspace, own task locks, deliver signals, implement ptrace
//! stops, allocate listener file descriptors, or perform audit logging.

#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod action;
mod bpf;
mod budget;
mod chain;
mod state;
mod uapi;

pub use action::{Action, ActionClass, MAX_ERRNO};
pub use bpf::{
    ClassicBpfInstruction, ProgramError, SeccompData, SeccompExecutor, VerifiedProgram,
    opcode as classic_bpf_opcode,
};
pub use budget::{FilterBudget, FilterBudgetCreateError};
pub use chain::{FilterChain, FilterDecision, FilterInstallError, FilterMetadata};
pub use state::{SeccompMode, SeccompState, StateTransitionError, SyncEligibility};
pub use uapi::*;
