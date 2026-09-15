use axerrno::{AxError, AxResult};
use axfs::{FsContext, OpenOptions};
use axfs_ng_vfs::FsPath;

use super::FdTable;

pub fn add_stdio(fd_table: &FdTable, cx: &FsContext) -> AxResult<()> {
    let open = |options: &mut OpenOptions, status_flags| {
        crate::syscall::open_init_description(
            cx,
            options,
            FsPath::new(b"/dev/console"),
            status_flags,
        )
    };

    // One O_RDWR open-file description, duplicated onto the three standard fds.
    let stdin = open(OpenOptions::new().read(true).write(true), 2)?;
    let stdout = stdin.clone();
    let stderr = stdin.clone();

    if fd_table.add_at_least(stdin, 0, 1, false)? != 0
        || fd_table.add_at_least(stdout, 1, 2, false)? != 1
        || fd_table.add_at_least(stderr, 2, 3, false)? != 2
    {
        return Err(AxError::BadState);
    }

    Ok(())
}
