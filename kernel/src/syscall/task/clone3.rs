use core::mem::{self, MaybeUninit};

use axerrno::{AxError, AxResult};
use axhal::uspace::UserContext;
use bytemuck::AnyBitPattern;
use starry_vm::vm_read_slice;

use super::clone::{CloneApi, CloneArgs, CloneFlags};

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

impl TryFrom<Clone3Args> for CloneArgs {
    type Error = axerrno::AxError;

    fn try_from(args: Clone3Args) -> AxResult<Self> {
        if args.set_tid != 0 || args.set_tid_size != 0 {
            warn!("sys_clone3: set_tid/set_tid_size not supported, ignoring");
        }
        if args.cgroup != 0 {
            warn!("sys_clone3: cgroup parameter not supported, ignoring");
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
        })
    }
}

pub fn sys_clone3(uctx: &UserContext, args: *const u8, size: usize) -> AxResult<isize> {
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
    vm_read_slice(args, unsafe {
        mem::transmute::<&mut [u8], &mut [MaybeUninit<u8>]>(&mut buffer[..known_size])
    })?;
    let mut remaining = size - known_size;
    let mut extra_ptr = args.wrapping_add(known_size);
    while remaining > 0 {
        let chunk_len = remaining.min(32);
        let mut chunk = [0u8; 32];
        vm_read_slice(extra_ptr, unsafe {
            mem::transmute::<&mut [u8], &mut [MaybeUninit<u8>]>(&mut chunk[..chunk_len])
        })?;
        extra_ptr = extra_ptr.wrapping_add(chunk_len);
        remaining -= chunk_len;
    }
    let clone3_args: Clone3Args =
        bytemuck::try_pod_read_unaligned(&buffer).map_err(|_| AxError::InvalidInput)?;

    let clone_args = CloneArgs::try_from(clone3_args)?;
    clone_args.do_clone(uctx, CloneApi::Clone3)
}

#[cfg(test)]
mod tests {
    use axerrno::AxError;
    use linux_raw_sys::general::{CLONE_DETACHED, CLONE_FS, CLONE_NEWNS, CLONE_NEWPID, CLONE_PIDFD};

    use super::{Clone3Args, CloneApi, CloneArgs, CloneFlags};

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
    fn clone_validate_rejects_fs_newns_pair() {
        let args = CloneArgs {
            flags: CloneFlags::from_bits_retain((CLONE_FS | CLONE_NEWNS) as u64),
            ..Default::default()
        };
        assert_eq!(args.validate_for(CloneApi::Clone), Err(AxError::InvalidInput));
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
