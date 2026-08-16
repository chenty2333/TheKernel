use core::{mem::MaybeUninit, slice};

use axerrno::{AxError, AxResult};
use axhal::uspace::UserContext;
use bytemuck::AnyBitPattern;

use super::clone::{CloneApi, CloneArgs, CloneFlags};
use crate::mm::{UserMemoryCapability, map_usercopy_error};

/// Structure passed to clone3() system call.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, AnyBitPattern)]
pub struct Clone3Args {
    pub flags: u64,
    pub pidfd: u64,
    pub child_tid: u64,
    pub parent_tid: u64,
    pub exit_signal: u64,
    pub stack: u64,
    pub stack_size: u64,
    pub tls: u64,
    pub set_tid: u64,
    pub set_tid_size: u64,
    pub cgroup: u64,
}

const MIN_CLONE_ARGS_SIZE: usize = core::mem::size_of::<u64>() * 8;
const CLONE3_ARGS_SIZE: usize = core::mem::size_of::<Clone3Args>();
const CLONE3_ARGS_SIZE_VER2: usize = core::mem::size_of::<Clone3Args>();

fn validate_extra_bytes(bytes: &[u8]) -> AxResult<()> {
    if bytes.iter().any(|byte| *byte != 0) {
        // `AxError` distinguishes its own kinds from Linux-encoded ones by
        // sign, so `LinuxError::E2BIG.into()` and
        // `AxError::ArgumentListTooLong` compare unequal despite mapping to
        // the same errno. Keep the semantic kind here and let the Linux
        // adapter perform the errno mapping at the Linux ABI boundary.
        Err(AxError::ArgumentListTooLong)
    } else {
        Ok(())
    }
}

impl TryFrom<Clone3Args> for CloneArgs {
    type Error = axerrno::AxError;

    fn try_from(args: Clone3Args) -> AxResult<Self> {
        if args.set_tid_size != 0 {
            return Err(AxError::OperationNotSupported);
        }
        let flags = CloneFlags::from_bits(args.flags).ok_or(AxError::InvalidInput)?;

        if args.exit_signal > 0 && flags.intersects(CloneFlags::THREAD | CloneFlags::PARENT) {
            return Err(AxError::InvalidInput);
        }
        if args.stack == 0 && args.stack_size != 0 {
            return Err(AxError::InvalidInput);
        }
        if args.stack != 0 && args.stack_size == 0 {
            return Err(AxError::InvalidInput);
        }

        let stack = if args.stack > 0 {
            (args.stack + args.stack_size) as usize
        } else {
            0
        };

        Ok(CloneArgs {
            flags,
            exit_signal: args.exit_signal,
            stack,
            tls: args.tls as usize,
            parent_tid: args.parent_tid as usize,
            child_tid: args.child_tid as usize,
            pidfd: args.pidfd as usize,
            cgroup_fd: flags
                .contains(CloneFlags::INTO_CGROUP)
                .then_some(args.cgroup as i32),
        })
    }
}

pub fn sys_clone3(
    memory: UserMemoryCapability,
    uctx: &UserContext,
    args: *const u8,
    size: usize,
) -> AxResult<isize> {
    debug!("sys_clone3 <= args: {args:p}, size: {size}");

    if size < MIN_CLONE_ARGS_SIZE {
        warn!("sys_clone3: size {size} too small, minimum is {MIN_CLONE_ARGS_SIZE}");
        return Err(AxError::InvalidInput);
    }

    if size > CLONE3_ARGS_SIZE {
        debug!("sys_clone3: size {size} larger than expected, using known fields only");
    }

    let mut buffer = [0u8; CLONE3_ARGS_SIZE];
    let known_size = size.min(CLONE3_ARGS_SIZE);
    // SAFETY: MaybeUninit<T> is compatible with T, and we're filling in the
    // buffer with bytes read from the user
    let buffer_bytes = unsafe {
        slice::from_raw_parts_mut(buffer.as_mut_ptr().cast::<MaybeUninit<u8>>(), known_size)
    };
    memory
        .read_bytes(args as usize, buffer_bytes)
        .map_err(map_usercopy_error)?;
    let mut remaining = size - known_size;
    let mut extra_address = (args as usize)
        .checked_add(known_size)
        .ok_or(AxError::BadAddress)?;
    while remaining > 0 {
        let chunk_len = remaining.min(32);
        let mut chunk = [0u8; 32];
        let chunk_bytes = unsafe {
            slice::from_raw_parts_mut(chunk.as_mut_ptr().cast::<MaybeUninit<u8>>(), chunk_len)
        };
        memory
            .read_bytes(extra_address, chunk_bytes)
            .map_err(map_usercopy_error)?;
        validate_extra_bytes(&chunk[..chunk_len])?;
        extra_address = extra_address
            .checked_add(chunk_len)
            .ok_or(AxError::BadAddress)?;
        remaining -= chunk_len;
    }
    let clone3_args: Clone3Args =
        bytemuck::try_pod_read_unaligned(&buffer).map_err(|_| AxError::InvalidInput)?;
    if clone3_args.flags & CloneFlags::INTO_CGROUP.bits() != 0
        && (clone3_args.cgroup > i32::MAX as u64 || size < CLONE3_ARGS_SIZE_VER2)
    {
        return Err(AxError::InvalidInput);
    }

    let clone_args = CloneArgs::try_from(clone3_args)?;
    clone_args.do_clone(uctx, CloneApi::Clone3, &memory)
}

#[cfg(test)]
mod tests {
    use axerrno::AxError;
    use linux_raw_sys::general::{
        CLONE_DETACHED, CLONE_FS, CLONE_NEWNS, CLONE_NEWPID, CLONE_PIDFD,
    };

    use super::{Clone3Args, CloneApi, CloneArgs, CloneFlags, validate_extra_bytes};

    #[test]
    fn clone3_requires_matching_stack_and_stack_size() {
        let args = Clone3Args {
            stack: 0x1000,
            stack_size: 0,
            ..Default::default()
        };
        assert_eq!(CloneArgs::try_from(args), Err(AxError::InvalidInput));

        let args = Clone3Args {
            stack: 0,
            stack_size: 0x1000,
            ..Default::default()
        };
        assert_eq!(CloneArgs::try_from(args), Err(AxError::InvalidInput));
    }

    #[test]
    fn clone3_rejects_unsupported_set_tid_request() {
        let args = Clone3Args {
            set_tid: 0x1000,
            set_tid_size: 1,
            ..Default::default()
        };
        assert_eq!(
            CloneArgs::try_from(args),
            Err(AxError::OperationNotSupported)
        );
    }

    #[test]
    fn clone3_allows_unused_set_tid_pointer() {
        let args = Clone3Args {
            set_tid: 0x1000,
            set_tid_size: 0,
            ..Default::default()
        };
        assert!(CloneArgs::try_from(args).is_ok());
    }

    #[test]
    fn clone3_only_accepts_zeroed_unknown_fields() {
        assert_eq!(validate_extra_bytes(&[0; 32]), Ok(()));
        assert_eq!(
            validate_extra_bytes(&[0, 0, 1, 0]),
            Err(AxError::ArgumentListTooLong)
        );
    }

    #[test]
    fn clone_validate_rejects_unimplemented_mount_namespace() {
        let args = CloneArgs {
            flags: CloneFlags::from_bits_retain((CLONE_FS | CLONE_NEWNS) as u64),
            ..Default::default()
        };
        assert_eq!(
            args.validate_for(CloneApi::Clone),
            Err(AxError::OperationNotSupported)
        );
    }

    #[test]
    fn clone_validate_allows_newpid_and_pidfd() {
        let args = CloneArgs {
            flags: CloneFlags::from_bits_retain((CLONE_NEWPID | CLONE_PIDFD) as u64),
            pidfd: 0x1000,
            ..Default::default()
        };
        assert_eq!(args.validate_for(CloneApi::Clone), Ok(()));
    }

    #[test]
    fn clone3_validate_rejects_detached() {
        let args = CloneArgs {
            flags: CloneFlags::from_bits_retain(CLONE_DETACHED as u64),
            ..Default::default()
        };
        assert_eq!(
            args.validate_for(CloneApi::Clone3),
            Err(AxError::InvalidInput)
        );
    }
}
