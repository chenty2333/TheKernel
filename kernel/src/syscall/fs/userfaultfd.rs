use axerrno::{AxError, AxResult};
use axtask::current;
use linux_raw_sys::general::{O_CLOEXEC, O_NONBLOCK};

use crate::{file::{add_file_like, userfaultfd::UserfaultFile}, task::AsThread};

pub fn sys_userfaultfd(flags: i32) -> AxResult<isize> {
    let allowed = (O_CLOEXEC | O_NONBLOCK) as i32;
    if flags & !allowed != 0 {
        return Err(AxError::InvalidInput);
    }

    let cloexec = flags & O_CLOEXEC as i32 != 0;
    let nonblocking = flags & O_NONBLOCK as i32 != 0;
    let pid = current().as_thread().proc_data.proc.pid();
    let uffd = UserfaultFile::new(pid, nonblocking);
    add_file_like(uffd, cloexec).map(|fd| fd as isize)
}
