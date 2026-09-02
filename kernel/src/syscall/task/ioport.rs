//! x86 `iopl(2)` and `ioperm(2)` syscall policy.

use axerrno::{AxError, AxResult};
use axtask::current;
use linux_raw_sys::general::CAP_SYS_RAWIO;
use thekernel_linux_arch_x86_64::{ArchPolicyError, IoPortPlan, IoplPlan};

use crate::task::AsThread;

const IO_PORT_COUNT: usize = 65_536;

fn map_arch_policy_error(_error: ArchPolicyError) -> AxError {
    AxError::InvalidInput
}

fn ioperm_end(from: usize, num: usize) -> AxResult<usize> {
    let plan = IoPortPlan::new(from, num, false).map_err(map_arch_policy_error)?;
    debug_assert!(plan.first < IO_PORT_COUNT);
    Ok(plan.first + plan.count)
}

/// Linux's `capable(CAP_SYS_RAWIO)` followed by
/// `security_locked_down(LOCKDOWN_IOPORT)` gate.
fn may_enable_ioports() -> bool {
    let task = current();
    let thread = task.as_thread();
    thread.has_effective_capability(CAP_SYS_RAWIO) && !thread.ioport_locked_down()
}

/// Sets port permissions for the calling thread.
pub fn sys_ioperm(from: usize, num: usize, turn_on: i32) -> AxResult<isize> {
    // Linux validates the unsigned-long range before CAP_SYS_RAWIO.
    let plan = IoPortPlan::new(from, num, turn_on != 0).map_err(map_arch_policy_error)?;
    if plan.enable && !may_enable_ioports() {
        return Err(AxError::OperationNotPermitted);
    }
    current()
        .as_thread()
        .update_ioperm(plan.first, plan.count, plan.enable)?;
    Ok(0)
}

/// Changes the calling thread's emulated I/O privilege level.
pub fn sys_iopl(level: u32) -> AxResult<isize> {
    let level = u8::try_from(level)
        .map_err(|_| AxError::InvalidInput)
        .and_then(|level| IoplPlan::new(level).map_err(map_arch_policy_error))?;
    // Unlike ioperm(2), Linux requires CAP_SYS_RAWIO and the lockdown check
    // for every iopl(2) invocation, including a reduction or no-op request.
    if !may_enable_ioports() {
        return Err(AxError::OperationNotPermitted);
    }
    let task = current();
    let thread = task.as_thread();
    thread.set_iopl_level(level.level());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use axerrno::AxError;

    use super::ioperm_end;

    #[test]
    fn ioperm_range_matches_linux_unsigned_long_checks() {
        assert_eq!(ioperm_end(0, 0), Err(AxError::InvalidInput));
        assert_eq!(ioperm_end(65_536, 0), Err(AxError::InvalidInput));
        assert_eq!(ioperm_end(65_535, 1), Ok(65_536));
        assert_eq!(ioperm_end(0, 65_536), Ok(65_536));
        assert_eq!(ioperm_end(65_535, 2), Err(AxError::InvalidInput));
        assert_eq!(ioperm_end(usize::MAX, 1), Err(AxError::InvalidInput));
    }
}
