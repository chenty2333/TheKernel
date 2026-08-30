//! x86 `iopl(2)` and `ioperm(2)` syscall policy.

use axerrno::{AxError, AxResult};
use axtask::current;
use linux_raw_sys::general::CAP_SYS_RAWIO;

use crate::task::AsThread;

const IO_PORT_COUNT: usize = 65_536;

fn ioperm_end(from: usize, num: usize) -> AxResult<usize> {
    let end = from.checked_add(num).ok_or(AxError::InvalidInput)?;
    if end <= from || end > IO_PORT_COUNT {
        return Err(AxError::InvalidInput);
    }
    Ok(end)
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
    ioperm_end(from, num)?;
    let turn_on = turn_on != 0;
    if turn_on && !may_enable_ioports() {
        return Err(AxError::OperationNotPermitted);
    }
    current().as_thread().update_ioperm(from, num, turn_on)?;
    Ok(0)
}

/// Changes the calling thread's emulated I/O privilege level.
pub fn sys_iopl(level: u32) -> AxResult<isize> {
    if level > 3 {
        return Err(AxError::InvalidInput);
    }
    // Unlike ioperm(2), Linux requires CAP_SYS_RAWIO and the lockdown check
    // for every iopl(2) invocation, including a reduction or no-op request.
    if !may_enable_ioports() {
        return Err(AxError::OperationNotPermitted);
    }
    let task = current();
    let thread = task.as_thread();
    thread.set_iopl_level(level as u8);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::ioperm_end;
    use axerrno::AxError;

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
