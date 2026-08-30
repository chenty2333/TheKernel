use alloc::sync::Arc;

use axerrno::{AxError, AxResult};

use crate::file::{SecretMemFile, add_file_like_with_flags};
use linux_raw_sys::general::{MFD_CLOEXEC, O_LARGEFILE, O_RDWR};

pub(crate) fn sys_memfd_secret(flags: u32) -> AxResult<isize> {
    if flags & !MFD_CLOEXEC != 0 {
        return Err(AxError::InvalidInput);
    }
    let file: Arc<dyn crate::file::FileLike> =
        Arc::try_new(SecretMemFile::new()).map_err(|_| AxError::NoMemory)?;
    // Linux unconditionally exposes O_LARGEFILE in this anonymous file's
    // status flags, including through F_GETFL.
    Ok(add_file_like_with_flags(file, flags & MFD_CLOEXEC != 0, O_RDWR | O_LARGEFILE)? as isize)
}
