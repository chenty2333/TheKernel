use axerrno::{AxError, AxResult};
use axfs::{FsContext, OpenOptions};

use super::FdTable;

pub fn add_stdio(fd_table: &FdTable, cx: &FsContext) -> AxResult<()> {
    let open = |options: &mut OpenOptions, status_flags| {
        crate::syscall::open_init_description(cx, options, "/dev/console", status_flags)
    };

    let stdin = open(OpenOptions::new().read(true).write(false), 0)?;
    let tty_out = open(OpenOptions::new().read(false).write(true), 1)?;
    let stdout = tty_out.clone();
    let stderr = tty_out;

    if fd_table.add_at_least(stdin, 0, 1, false)? != 0
        || fd_table.add_at_least(stdout, 1, 2, false)? != 1
        || fd_table.add_at_least(stderr, 2, 3, false)? != 2
    {
        return Err(AxError::BadState);
    }

    Ok(())
}
