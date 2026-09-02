use core::{mem::MaybeUninit, slice};

use axerrno::{AxError, AxResult};
use axhal::uspace::UserContext;
use thekernel_linux_process::{
    Clone3Args as LinuxClone3Args, Clone3Plan, ProcessAbiError, SetTidPlan,
};
use thekernel_linux_signal::Signo;

use super::clone::{CloneApi, CloneArgs, CloneFlags};
use crate::{
    config::{USER_SPACE_BASE, USER_SPACE_SIZE},
    mm::{UserMemoryCapability, map_usercopy_error},
};

fn map_clone3_abi_error(error: ProcessAbiError) -> AxError {
    match error {
        ProcessAbiError::PermissionDenied => axerrno::LinuxError::EPERM.into(),
        ProcessAbiError::NonzeroTail | ProcessAbiError::TooLarge => AxError::ArgumentListTooLong,
        _ => AxError::InvalidInput,
    }
}

/// Linux's clone3-only scalar validation, kept outside the shared ABI adapter
/// because the adapter is also used by non-syscall callers.
fn validate_clone3_wire_args(args: &LinuxClone3Args) -> AxResult<()> {
    if args.set_tid_size > SetTidPlan::MAX_ENTRIES as u64
        || (args.set_tid == 0) != (args.set_tid_size == 0)
    {
        return Err(AxError::InvalidInput);
    }
    const CSIGNAL: u64 = 0x7f;
    if args.exit_signal & !CSIGNAL != 0
        || (args.exit_signal != 0 && Signo::from_repr(args.exit_signal as u8).is_none())
    {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

/// Scalar checks performed by Linux's `copy_clone_args_from_user()` after the
/// clone_args object has been copied but before it dereferences `set_tid`.
fn validate_clone3_pre_set_tid_args(args: &LinuxClone3Args, size: usize) -> AxResult<()> {
    let into_cgroup = linux_raw_sys::general::CLONE_INTO_CGROUP as u64;
    if args.flags & into_cgroup != 0
        && (size < LinuxClone3Args::KNOWN_SIZE || args.cgroup > i32::MAX as u64)
    {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

/// Validates the ABI-normalized clone3 stack top against this kernel's user
/// address layout.
fn clone3_stack_top(stack_base: u64, stack_top: u64) -> AxResult<usize> {
    let stack_base = usize::try_from(stack_base).map_err(|_| AxError::InvalidInput)?;
    let stack_top = usize::try_from(stack_top).map_err(|_| AxError::InvalidInput)?;
    let user_space_end = USER_SPACE_BASE
        .checked_add(USER_SPACE_SIZE)
        .ok_or(AxError::BadState)?;

    if stack_top != 0 && (stack_base < USER_SPACE_BASE || stack_top > user_space_end) {
        return Err(AxError::InvalidInput);
    }

    Ok(stack_top)
}

impl TryFrom<Clone3Plan> for CloneArgs {
    type Error = axerrno::AxError;

    fn try_from(plan: Clone3Plan) -> AxResult<Self> {
        let flags = CloneFlags::from_bits(plan.clone.flags).ok_or(AxError::InvalidInput)?;
        let stack = clone3_stack_top(plan.stack_base, plan.clone.stack_top)?;
        let stack_size =
            usize::try_from(plan.clone.stack_size).map_err(|_| AxError::InvalidInput)?;

        Ok(CloneArgs {
            flags,
            exit_signal: plan.clone.exit_signal as u64,
            stack,
            stack_size,
            tls: plan.clone.tls as usize,
            parent_tid: plan.clone.parent_tid as usize,
            child_tid: plan.clone.child_tid as usize,
            pidfd: plan.clone.pidfd as usize,
            cgroup_fd: plan.cgroup_fd,
            set_tid: [0; SetTidPlan::MAX_ENTRIES],
            set_tid_size: 0,
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

    let known_size = LinuxClone3Args::known_prefix_size(size).map_err(map_clone3_abi_error)?;
    if size > LinuxClone3Args::KNOWN_SIZE {
        debug!("sys_clone3: size {size} larger than expected, using known fields only");
    }

    let mut buffer = [0u8; LinuxClone3Args::KNOWN_SIZE];
    // SAFETY: MaybeUninit<T> is compatible with T, and we're filling in the
    // buffer with bytes read from the user
    let buffer_bytes = unsafe {
        slice::from_raw_parts_mut(buffer.as_mut_ptr().cast::<MaybeUninit<u8>>(), known_size)
    };
    memory
        .read_bytes(args as usize, buffer_bytes)
        .map_err(map_usercopy_error)?;
    let tail_size = size - known_size;
    let mut tail = alloc::vec![0_u8; tail_size];
    let tail_address = (args as usize)
        .checked_add(known_size)
        .ok_or(AxError::BadAddress)?;
    if !tail.is_empty() {
        let tail_bytes = unsafe {
            slice::from_raw_parts_mut(tail.as_mut_ptr().cast::<MaybeUninit<u8>>(), tail.len())
        };
        memory
            .read_bytes(tail_address, tail_bytes)
            .map_err(map_usercopy_error)?;
    }
    let wire_args = LinuxClone3Args::decode_prefix(size, &buffer[..known_size])
        .map_err(map_clone3_abi_error)?;
    // copy_struct_from_user() verifies an extension tail while copying the
    // clone_args object, before inspecting its scalar fields or dereferencing
    // set_tid. Preserve that E2BIG-before-set_tid-usercopy boundary here.
    LinuxClone3Args::validate_tail(&tail).map_err(map_clone3_abi_error)?;
    // copy_clone_args_from_user() rejects the pointer/count shape and the
    // untruncated exit_signal first.  It then copies the set_tid vector
    // *before* clone3_args_valid() examines flag combinations and the stack:
    // an unreadable vector therefore wins over a later EINVAL.
    validate_clone3_wire_args(&wire_args)?;
    validate_clone3_pre_set_tid_args(&wire_args, size)?;
    let set_tid_count = wire_args.set_tid_size as usize;
    let mut tids = [0u32; thekernel_linux_process::SetTidPlan::MAX_ENTRIES];
    if set_tid_count != 0 {
        let byte_count = set_tid_count
            .checked_mul(core::mem::size_of::<u32>())
            .ok_or(AxError::InvalidInput)?;
        // SAFETY: `tids` is initialized and the byte view is bounded by its size.
        let bytes = unsafe {
            slice::from_raw_parts_mut(tids.as_mut_ptr().cast::<MaybeUninit<u8>>(), byte_count)
        };
        memory
            .read_bytes(wire_args.set_tid as usize, bytes)
            .map_err(map_usercopy_error)?;
    }
    let plan = wire_args
        .normalize(size, &tail)
        .map_err(map_clone3_abi_error)?;
    plan.set_tid
        .validate_values(&tids[..set_tid_count])
        .map_err(map_clone3_abi_error)?;
    let mut clone_args = CloneArgs::try_from(plan)?;
    clone_args.set_tid[..set_tid_count].copy_from_slice(&tids[..set_tid_count]);
    clone_args.set_tid_size = set_tid_count;
    clone_args.do_clone(uctx, CloneApi::Clone3, &memory)
}

#[cfg(test)]
mod tests {
    use axerrno::AxError;
    use linux_raw_sys::general::{
        CLONE_DETACHED, CLONE_FS, CLONE_NEWNS, CLONE_NEWPID, CLONE_PIDFD,
    };

    use super::{
        CloneApi, CloneArgs, CloneFlags, LinuxClone3Args, clone3_stack_top,
        validate_clone3_wire_args,
    };
    use crate::config::{USER_SPACE_BASE, USER_SPACE_SIZE};

    #[test]
    fn clone3_accepts_abi_stack_ending_at_user_address_boundary() {
        let user_space_end = USER_SPACE_BASE + USER_SPACE_SIZE;
        assert_eq!(
            clone3_stack_top(USER_SPACE_BASE as u64, user_space_end as u64),
            Ok(user_space_end)
        );
    }

    #[test]
    fn clone3_rejects_stack_range_outside_user_address_space() {
        assert_eq!(
            clone3_stack_top((USER_SPACE_BASE - 1) as u64, (USER_SPACE_BASE + 1) as u64),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            clone3_stack_top(
                USER_SPACE_BASE as u64,
                (USER_SPACE_BASE + USER_SPACE_SIZE + 1) as u64
            ),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn clone3_rejects_stack_top_overflow_without_wrapping() {
        assert_eq!(clone3_stack_top(1, u64::MAX), Err(AxError::InvalidInput));
    }

    #[test]
    fn clone3_defers_nonempty_set_tid_to_the_abi_usercopy_admission() {
        let args = LinuxClone3Args {
            set_tid: 0x1000,
            set_tid_size: 1,
            ..Default::default()
        };
        assert!(args.normalize(LinuxClone3Args::KNOWN_SIZE, &[]).is_ok());
    }

    #[test]
    fn clone3_preserves_explicit_stack_size_for_cet_thread_setup() {
        let plan = LinuxClone3Args {
            stack: 0x4000,
            stack_size: 0x3000,
            ..Default::default()
        }
        .normalize(LinuxClone3Args::KNOWN_SIZE, &[])
        .unwrap();
        let args = CloneArgs::try_from(plan).unwrap();
        assert_eq!(args.stack, 0x7000);
        assert_eq!(args.stack_size, 0x3000);
    }

    #[test]
    fn clone3_wire_validation_rejects_mismatched_set_tid_shape_and_exit_signal_bits() {
        let args = LinuxClone3Args {
            set_tid: 0x1000,
            set_tid_size: 0,
            ..Default::default()
        };
        assert_eq!(validate_clone3_wire_args(&args), Err(AxError::InvalidInput));

        let args = LinuxClone3Args {
            set_tid_size: 1,
            ..Default::default()
        };
        assert_eq!(validate_clone3_wire_args(&args), Err(AxError::InvalidInput));

        let args = LinuxClone3Args {
            exit_signal: 1_u64 << 32 | linux_raw_sys::general::SIGCHLD as u64,
            ..Default::default()
        };
        assert_eq!(validate_clone3_wire_args(&args), Err(AxError::InvalidInput));

        let args = LinuxClone3Args {
            exit_signal: 0x7f,
            ..Default::default()
        };
        assert_eq!(validate_clone3_wire_args(&args), Err(AxError::InvalidInput));
    }

    #[test]
    fn clone_validate_rejects_shared_fs_struct_with_new_mount_namespace() {
        let args = CloneArgs {
            flags: CloneFlags::from_bits_retain((CLONE_FS | CLONE_NEWNS) as u64),
            ..Default::default()
        };
        assert_eq!(
            args.validate_for(CloneApi::Clone),
            Err(AxError::InvalidInput)
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
