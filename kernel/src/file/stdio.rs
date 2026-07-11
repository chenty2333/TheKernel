use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use axfs::{FS_CONTEXT, OpenOptions};
use flatten_objects::FlattenObjects;

use super::{
    desc::{FileDescription, FileDescriptor},
    fs::File,
};
use crate::task::AX_FILE_LIMIT;

pub fn add_stdio(fd_table: &mut FlattenObjects<FileDescriptor, AX_FILE_LIMIT>) -> AxResult<()> {
    assert_eq!(fd_table.count(), 0);
    let cx = FS_CONTEXT.lock();
    let open = |options: &mut OpenOptions| {
        AxResult::Ok(Arc::new(File::new(
            options.open(&cx, "/dev/console")?.into_file()?,
        )))
    };

    let tty_in = open(OpenOptions::new().read(true).write(false))?;
    let tty_out = open(OpenOptions::new().read(false).write(true))?;
    let stdin = FileDescription::new(tty_in)?;
    fd_table
        .add(FileDescriptor {
            description: stdin.clone(),
            cloexec: false,
        })
        .map_err(|_| AxError::TooManyOpenFiles)?;
    stdin.mark_open_committed();
    let stdout = FileDescription::new(tty_out.clone())?;
    fd_table
        .add(FileDescriptor {
            description: stdout.clone(),
            cloexec: false,
        })
        .map_err(|_| AxError::TooManyOpenFiles)?;
    stdout.mark_open_committed();
    let stderr = FileDescription::new(tty_out)?;
    fd_table
        .add(FileDescriptor {
            description: stderr.clone(),
            cloexec: false,
        })
        .map_err(|_| AxError::TooManyOpenFiles)?;
    stderr.mark_open_committed();

    Ok(())
}
