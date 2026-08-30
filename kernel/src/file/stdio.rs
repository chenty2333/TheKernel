use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use axfs::{FsContext, OpenOptions};

use super::{FdTable, desc::FileDescription, fs::File};

pub fn add_stdio(fd_table: &FdTable, cx: &FsContext) -> AxResult<()> {
    let open = |options: &mut OpenOptions| {
        AxResult::Ok(Arc::new(File::new(
            options.open(&cx, "/dev/console")?.into_file()?,
        )))
    };

    let tty_in = open(OpenOptions::new().read(true).write(false))?;
    let tty_out = open(OpenOptions::new().read(false).write(true))?;
    let stdin = FileDescription::new(tty_in)?;
    let stdout = FileDescription::new(tty_out.clone())?;
    let stderr = FileDescription::new(tty_out)?;

    if fd_table.add_at_least(stdin, 0, 1, false)? != 0
        || fd_table.add_at_least(stdout, 1, 2, false)? != 1
        || fd_table.add_at_least(stderr, 2, 3, false)? != 2
    {
        return Err(AxError::BadState);
    }

    Ok(())
}
