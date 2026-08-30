use alloc::sync::Arc;

use axerrno::{AxError, AxResult};

use crate::file::{SecretMemFile, add_file_like_with_flags};
use linux_raw_sys::general::{O_CLOEXEC, O_LARGEFILE, O_RDWR};

const SECRETMEM_ALLOWED_FLAGS: u32 = O_CLOEXEC;

pub(crate) fn sys_memfd_secret(flags: u32) -> AxResult<isize> {
    if flags & !SECRETMEM_ALLOWED_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    let file: Arc<dyn crate::file::FileLike> =
        Arc::try_new(SecretMemFile::new()).map_err(|_| AxError::NoMemory)?;
    // Linux unconditionally exposes O_LARGEFILE in this anonymous file's
    // status flags, including through F_GETFL.
    Ok(add_file_like_with_flags(file, flags & O_CLOEXEC != 0, O_RDWR | O_LARGEFILE)? as isize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_raw_sys::general::MFD_CLOEXEC;

    #[test]
    fn secretmem_uses_open_cloexec_not_memfd_cloexec() {
        assert_eq!(O_CLOEXEC, 0x80000);
        assert_eq!(MFD_CLOEXEC, 1);
        assert_eq!(MFD_CLOEXEC & !SECRETMEM_ALLOWED_FLAGS, MFD_CLOEXEC);
    }
}
