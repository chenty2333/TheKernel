use alloc::vec::Vec;
use core::{mem::MaybeUninit, slice};

use axerrno::{AxError, AxResult};
use axhal::uspace::UserContext;
use thekernel_linux_process::{
    Clone3Args as LinuxClone3Args, Clone3Plan, ProcessAbiError, SetTidPlan,
};
use thekernel_linux_signal::Signo;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

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

/// Copies the versioned `clone_args` object with Linux's
/// `copy_struct_from_user()` ordering.
///
/// Unknown extension bytes are checked before the common prefix.  Keeping
/// this ordering here is necessary because the shared process ABI consumes a
/// decoded prefix and a kernel-owned tail separately, while Linux exposes the
/// tail-before-prefix fault precedence as part of clone3's syscall contract.
fn copy_clone3_wire_args<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    args: *const u8,
    size: usize,
) -> AxResult<(LinuxClone3Args, Vec<u8>)> {
    let known_size = LinuxClone3Args::known_prefix_size(size).map_err(map_clone3_abi_error)?;
    let tail_size = size - known_size;

    let mut offset = known_size;
    while offset < size {
        let count = (size - offset).min(32);
        let mut bytes = [MaybeUninit::<u8>::uninit(); 32];
        let address = (args as usize)
            .checked_add(offset)
            .ok_or(AxError::BadAddress)?;
        memory
            .read_bytes(address, &mut bytes[..count])
            .map_err(map_usercopy_error)?;
        if bytes[..count]
            .iter()
            .any(|byte| unsafe { byte.assume_init() } != 0)
        {
            return Err(AxError::ArgumentListTooLong);
        }
        offset += count;
    }

    let mut buffer = [0u8; LinuxClone3Args::KNOWN_SIZE];
    // SAFETY: the byte view covers initialized kernel-owned storage and the
    // provider initializes the requested prefix on success.
    let buffer_bytes = unsafe {
        slice::from_raw_parts_mut(buffer.as_mut_ptr().cast::<MaybeUninit<u8>>(), known_size)
    };
    memory
        .read_bytes(args as usize, buffer_bytes)
        .map_err(map_usercopy_error)?;
    let wire_args = LinuxClone3Args::decode_prefix(size, &buffer[..known_size])
        .map_err(map_clone3_abi_error)?;

    // `normalize` also verifies the supplied extension length.  The extension
    // was already copied and proved zero above, so retain only a fallibly
    // allocated zero snapshot instead of copying it a second time.
    let mut tail = Vec::new();
    tail.try_reserve_exact(tail_size)
        .map_err(|_| AxError::NoMemory)?;
    tail.resize(tail_size, 0);
    Ok((wire_args, tail))
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

    if size > LinuxClone3Args::KNOWN_SIZE {
        debug!("sys_clone3: size {size} larger than expected, using known fields only");
    }
    let (wire_args, tail) =
        memory.with_memory(|memory| copy_clone3_wire_args(memory, args, size))?;
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
    use alloc::{vec, vec::Vec};
    use core::mem::MaybeUninit;

    use axerrno::AxError;
    use linux_raw_sys::general::{
        CLONE_DETACHED, CLONE_FS, CLONE_NEWNS, CLONE_NEWPID, CLONE_PIDFD,
    };
    use thekernel_linux_usercopy::{UserCopyError, UserMemory, UserMemoryContext, VmResult};

    use super::{
        CloneApi, CloneArgs, CloneFlags, LinuxClone3Args, clone3_stack_top, copy_clone3_wire_args,
        validate_clone3_wire_args,
    };
    use crate::config::{USER_SPACE_BASE, USER_SPACE_SIZE};

    struct CopyProbe {
        bytes: Vec<u8>,
        fault_at: Option<usize>,
        reads: Vec<(usize, usize)>,
    }

    impl CopyProbe {
        fn new(bytes: Vec<u8>, fault_at: Option<usize>) -> Self {
            Self {
                bytes,
                fault_at,
                reads: Vec::new(),
            }
        }
    }

    // SAFETY: the probe reads only from its owned byte vector and initializes
    // every requested destination byte on success.
    unsafe impl UserMemory for CopyProbe {
        fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
            self.reads.push((start, dst.len()));
            let end = start
                .checked_add(dst.len())
                .ok_or(UserCopyError::BadAddress)?;
            if end > self.bytes.len()
                || self
                    .fault_at
                    .is_some_and(|fault| start <= fault && fault < end)
            {
                return Err(UserCopyError::BadAddress);
            }
            for (destination, source) in dst.iter_mut().zip(&self.bytes[start..end]) {
                destination.write(*source);
            }
            Ok(())
        }

        fn write(&mut self, _: usize, _: &[u8]) -> VmResult {
            Err(UserCopyError::BadAddress)
        }
    }

    #[test]
    fn clone3_copy_checks_extension_before_faulting_common_prefix() {
        let size = LinuxClone3Args::KNOWN_SIZE + 1;

        let mut nonzero = vec![0; size];
        nonzero[LinuxClone3Args::KNOWN_SIZE] = 1;
        let mut provider = CopyProbe::new(nonzero, Some(0));
        let result = copy_clone3_wire_args(
            &mut UserMemoryContext::new(&mut provider),
            core::ptr::null(),
            size,
        );
        assert_eq!(result, Err(AxError::ArgumentListTooLong));
        assert_eq!(provider.reads, [(LinuxClone3Args::KNOWN_SIZE, 1)]);

        let mut provider = CopyProbe::new(vec![0; size], Some(0));
        let result = copy_clone3_wire_args(
            &mut UserMemoryContext::new(&mut provider),
            core::ptr::null(),
            size,
        );
        assert_eq!(result, Err(AxError::BadAddress));
        assert_eq!(
            provider.reads,
            [
                (LinuxClone3Args::KNOWN_SIZE, 1),
                (0, LinuxClone3Args::KNOWN_SIZE)
            ]
        );
    }

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
