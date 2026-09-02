//! x86-64 internal uprobe trampoline syscalls.
//!
//! Numbers 335 and 336 are intentionally not general-purpose user APIs.  The
//! common dispatcher reaches them without taking a user-memory snapshot; the
//! uprobe core performs the exact trampoline provenance check first.

use axerrno::AxResult;
use axhal::uspace::UserContext;

pub(crate) fn sys_uretprobe(context: &mut UserContext) -> AxResult<isize> {
    crate::uprobe::syscall_uretprobe(context)
}

pub(crate) fn sys_uprobe(context: &mut UserContext) -> AxResult<isize> {
    crate::uprobe::syscall_uprobe(context)
}
