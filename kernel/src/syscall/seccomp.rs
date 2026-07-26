//! Linux seccomp syscall adapter and syscall-entry enforcement.

use alloc::vec::Vec;
use core::{
    mem::{self, MaybeUninit},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::uspace::UserContext;
use axtask::current;
use linux_raw_sys::general::CAP_SYS_ADMIN;
use starry_signal::{SignalInfo, Signo};
use starry_vm::vm_read_slice;
use syscalls::Sysno;
use thekernel_linux_seccomp::{
    ActionClass, BPF_MAXINSNS, ClassicBpfInstruction, FilterInstallError, FilterMetadata,
    ProgramError, SECCOMP_GET_ACTION_AVAIL, SECCOMP_GET_NOTIF_SIZES, SECCOMP_RET_ALLOW,
    SECCOMP_RET_ERRNO, SECCOMP_RET_KILL_PROCESS, SECCOMP_RET_KILL_THREAD, SECCOMP_RET_LOG,
    SECCOMP_RET_TRAP, SECCOMP_SET_MODE_FILTER, SECCOMP_SET_MODE_STRICT, SeccompData, SeccompMode,
    StateTransitionError, VerifiedProgram,
};

use crate::task::{
    AsThread, do_exit, fail_closed_exit, force_signal_current_thread, ns_capable,
    seccomp_filter_budget,
};

/// Host-only audit architecture used by kernel unit builds. The released bare
/// metal consumers use the RV64 and LoongArch64 constants from Layer 2.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

/// Global bound for seccomp diagnostic records emitted from syscall entry.
///
/// This prevents an untrusted filter from turning `SECCOMP_RET_LOG` into an
/// unbounded synchronous logging workload. It is deliberately a fixed boot
/// budget rather than an implied Linux audit subsystem.
const SECCOMP_LOG_RECORD_LIMIT: usize = 1024;

static SECCOMP_LOG_RECORDS: AtomicUsize = AtomicUsize::new(0);
static SECCOMP_LOG_SUPPRESSION_REPORTED: AtomicBool = AtomicBool::new(false);

/// All-integer mirror of Linux `struct sock_fprog` on the supported 64-bit
/// architectures. Arbitrary userspace bytes are valid for every field.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawSockFprog {
    length: u16,
    padding: [u8; 6],
    filter: usize,
}

const _: [(); 16] = [(); mem::size_of::<RawSockFprog>()];
const _: [(); 8] = [(); mem::align_of::<RawSockFprog>()];
const _: [(); 8] = [(); mem::offset_of!(RawSockFprog, filter)];

impl RawSockFprog {
    fn read_from_user(pointer: *const u8) -> AxResult<Self> {
        let mut bytes = [0u8; mem::size_of::<Self>()];
        // Linux `copy_from_user` accepts unaligned fprog pointers. Copy as
        // bytes so the Rust adapter does not accidentally impose a stronger
        // typed-pointer alignment contract.
        // SAFETY: `MaybeUninit<u8>` and `u8` have identical layout, and the
        // destination covers the complete local byte array.
        vm_read_slice(pointer, unsafe {
            core::slice::from_raw_parts_mut(
                bytes.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                bytes.len(),
            )
        })?;
        let length = u16::from_ne_bytes([bytes[0], bytes[1]]);
        let filter = usize::from_ne_bytes(
            bytes[8..16]
                .try_into()
                .expect("64-bit fprog pointer occupies eight bytes"),
        );
        Ok(Self {
            length,
            padding: bytes[2..8]
                .try_into()
                .expect("fprog padding occupies six bytes"),
            filter,
        })
    }
}

fn map_program_error(error: ProgramError) -> AxError {
    if error.is_no_memory() {
        AxError::NoMemory
    } else {
        AxError::InvalidInput
    }
}

fn map_install_error(error: FilterInstallError) -> AxError {
    match error {
        FilterInstallError::NoMemory
        | FilterInstallError::PathTooLong
        | FilterInstallError::BudgetExceeded => AxError::NoMemory,
        FilterInstallError::BudgetMismatch => AxError::BadState,
    }
}

fn map_transition_error(error: StateTransitionError) -> AxError {
    match error {
        StateTransitionError::ModeConflict => AxError::InvalidInput,
        StateTransitionError::Stale => LinuxError::EAGAIN.into(),
        StateTransitionError::InvalidPreparedState => AxError::BadState,
    }
}

fn copy_filter_program(header: RawSockFprog) -> AxResult<Vec<ClassicBpfInstruction>> {
    let length = usize::from(header.length);
    // Linux's classic-BPF basic check rejects a NULL instruction pointer as
    // EINVAL before attempting copy_from_user. Non-NULL pointers remain byte
    // copied and may be unaligned.
    if length == 0 || length > BPF_MAXINSNS || header.filter == 0 {
        return Err(AxError::InvalidInput);
    }

    let mut instructions = Vec::new();
    instructions
        .try_reserve_exact(length)
        .map_err(|_| AxError::NoMemory)?;
    instructions.resize(length, ClassicBpfInstruction::default());
    let byte_length = length
        .checked_mul(mem::size_of::<ClassicBpfInstruction>())
        .ok_or(AxError::NoMemory)?;
    // SAFETY: the vector owns `length` initialized instructions, each raw
    // instruction consists exclusively of integer fields, and the byte slice
    // spans exactly that allocation. `vm_read_slice` overwrites every byte or
    // returns an error; no reference to the buffer escapes during the copy.
    let destination = unsafe {
        core::slice::from_raw_parts_mut(
            instructions.as_mut_ptr().cast::<MaybeUninit<u8>>(),
            byte_length,
        )
    };
    vm_read_slice(header.filter as *const u8, destination)?;
    Ok(instructions)
}

fn filter_install_permitted() -> bool {
    let curr = current();
    let thread = curr.as_thread();
    let credential = thread.current_cred();
    credential.no_new_privs() || ns_capable(&credential, credential.user_ns(), CAP_SYS_ADMIN)
}

fn install_filter(flags: u32, args: *const ()) -> AxResult<isize> {
    // Phase one intentionally supports task-local installation only. TSYNC,
    // NEW_LISTENER, LOG-at-install and speculative flags require their full
    // transactions/lifecycles and are rejected rather than partially faked.
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    // Linux copies the fprog header and validates its length before checking
    // no_new_privs/CAP_SYS_ADMIN, but performs that admission before copying
    // and verifying the instruction array.
    let header = RawSockFprog::read_from_user(args.cast())?;
    let length = usize::from(header.length);
    if length == 0 || length > BPF_MAXINSNS {
        return Err(AxError::InvalidInput);
    }
    if !filter_install_permitted() {
        return Err(LinuxError::EACCES.into());
    }

    let instructions = copy_filter_program(header)?;
    let program = VerifiedProgram::try_from_vec(instructions).map_err(map_program_error)?;

    let curr = current();
    let thread = curr.as_thread();
    let snapshot = thread.seccomp_snapshot();
    let expected = snapshot.filters();
    let prepared = expected
        .try_append(program, FilterMetadata::default(), seccomp_filter_budget())
        .map_err(map_install_error)?;
    thread
        .try_publish_seccomp_filter(&expected, &prepared)
        .map_err(map_transition_error)?;
    Ok(0)
}

fn enter_strict(flags: u32, args: *const ()) -> AxResult<isize> {
    if flags != 0 || !args.is_null() {
        return Err(AxError::InvalidInput);
    }
    let curr = current();
    curr.as_thread()
        .try_enter_seccomp_strict()
        .map_err(map_transition_error)?;
    Ok(0)
}

fn get_action_available(flags: u32, args: *const ()) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let mut bytes = [0u8; mem::size_of::<u32>()];
    // As with the fprog header, Linux accepts an unaligned query pointer.
    // SAFETY: the destination is the complete local byte array.
    vm_read_slice(args.cast(), unsafe {
        core::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<MaybeUninit<u8>>(), bytes.len())
    })?;
    let action = u32::from_ne_bytes(bytes);
    match action {
        SECCOMP_RET_KILL_PROCESS
        | SECCOMP_RET_KILL_THREAD
        | SECCOMP_RET_TRAP
        | SECCOMP_RET_ERRNO
        | SECCOMP_RET_LOG
        | SECCOMP_RET_ALLOW => Ok(0),
        _ => Err(LinuxError::EOPNOTSUPP.into()),
    }
}

/// Implements the Linux `seccomp(2)` operation boundary.
pub fn sys_seccomp(op: u32, flags: u32, args: *const ()) -> AxResult<isize> {
    match op {
        SECCOMP_SET_MODE_STRICT => enter_strict(flags, args),
        SECCOMP_SET_MODE_FILTER => install_filter(flags, args),
        SECCOMP_GET_ACTION_AVAIL => get_action_available(flags, args),
        SECCOMP_GET_NOTIF_SIZES => {
            if flags != 0 {
                Err(AxError::InvalidInput)
            } else {
                // No listener FD/request-ID/cancellation lifecycle is exposed.
                Err(LinuxError::EOPNOTSUPP.into())
            }
        }
        _ => Err(AxError::InvalidInput),
    }
}

/// Implements the `PR_SET_SECCOMP` compatibility entry point.
pub(crate) fn sys_prctl_set_seccomp(mode: usize, args: *const ()) -> AxResult<isize> {
    match mode {
        value if value == SeccompMode::Strict as usize => {
            // Linux has always ignored prctl's optional filter argument for
            // strict mode, even though seccomp(2) requires a NULL `uargs`.
            sys_seccomp(SECCOMP_SET_MODE_STRICT, 0, core::ptr::null())
        }
        value if value == SeccompMode::Filter as usize => {
            sys_seccomp(SECCOMP_SET_MODE_FILTER, 0, args)
        }
        _ => Err(AxError::InvalidInput),
    }
}

#[cfg(target_arch = "riscv64")]
const fn current_audit_architecture() -> u32 {
    thekernel_linux_seccomp::AUDIT_ARCH_RISCV64
}

#[cfg(target_arch = "loongarch64")]
const fn current_audit_architecture() -> u32 {
    thekernel_linux_seccomp::AUDIT_ARCH_LOONGARCH64
}

#[cfg(target_arch = "x86_64")]
const fn current_audit_architecture() -> u32 {
    AUDIT_ARCH_X86_64
}

fn seccomp_data(uctx: &UserContext) -> SeccompData {
    SeccompData {
        // Linux's UAPI field is signed 32-bit even though the architecture
        // register and the generic context accessor are machine words.
        number: uctx.sysno() as i32,
        architecture: current_audit_architecture(),
        instruction_pointer: uctx.ip() as u64,
        arguments: [
            uctx.arg0() as u64,
            uctx.arg1() as u64,
            uctx.arg2() as u64,
            uctx.arg3() as u64,
            uctx.arg4() as u64,
            uctx.arg5() as u64,
        ],
    }
}

fn strict_allows(raw_syscall: usize) -> bool {
    matches!(
        raw_syscall,
        value if value == Sysno::read as usize
            || value == Sysno::write as usize
            || value == Sysno::exit as usize
            || value == Sysno::rt_sigreturn as usize
    )
}

fn bounded_log(data: &SeccompData, action: u32) {
    if SECCOMP_LOG_RECORDS
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            (used < SECCOMP_LOG_RECORD_LIMIT).then_some(used + 1)
        })
        .is_ok()
    {
        warn!(
            "seccomp decision: nr={} arch={:#x} ip={:#x} action={:#x}",
            data.number, data.architecture, data.instruction_pointer, action
        );
    } else if !SECCOMP_LOG_SUPPRESSION_REPORTED.swap(true, Ordering::Relaxed) {
        warn!(
            "seccomp diagnostic budget exhausted after {SECCOMP_LOG_RECORD_LIMIT} records; \
             suppressing further entries"
        );
    }
}

fn terminate_for_seccomp(signo: Signo, group_exit: bool) {
    if let Err(error) = do_exit(signo as i32, group_exit) {
        fail_closed_exit(error);
    }
}

fn rollback_seccomp_syscall_frame(uctx: &mut UserContext, data: &SeccompData, _raw_syscall: usize) {
    // TheKernel's stable RV64/LoongArch64 profile restores the architecture
    // syscall frame before TRAP and every terminal filter-action KILL path.
    // Linux restores TRAP and KILL branches that reach forced SIGSYS/core
    // handling, but has a non-final KILL_THREAD shortcut which exits without
    // rollback. Keeping one explicit TheKernel rule makes the observable frame
    // contract independent of that core-selection detail. RV64 and
    // LoongArch64 return the original first argument in a0; x86_64 exists only
    // for host unit builds and restores the raw syscall-number register in RAX.
    #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
    uctx.set_arg0(data.arguments[0] as usize);
    #[cfg(target_arch = "x86_64")]
    {
        let _ = data;
        uctx.set_retval(_raw_syscall);
    }
}

/// Applies the calling task's exact immutable seccomp snapshot before syscall
/// decoding or any getter/time fast path. Returns `true` only when normal
/// dispatch may continue.
pub(super) fn enforce_syscall_seccomp(uctx: &mut UserContext) -> bool {
    let curr = current();
    let state = curr.as_thread().seccomp_snapshot();
    match state.mode() {
        SeccompMode::Disabled => return true,
        SeccompMode::Strict => {
            if strict_allows(uctx.sysno()) {
                return true;
            }
            terminate_for_seccomp(Signo::SIGKILL, false);
            return false;
        }
        SeccompMode::Filter => {}
    }

    // The only task publication lock was released by `seccomp_snapshot`.
    // Evaluation is allocation-free and touches only immutable chain nodes.
    let raw_syscall = uctx.sysno();
    let data = seccomp_data(uctx);
    let decision = state.evaluate(&data);
    let class = decision.action.classify();
    if decision.matched_filter.is_some_and(|metadata| metadata.log)
        && !matches!(class, ActionClass::Allow | ActionClass::Log)
    {
        bounded_log(&data, decision.action.raw());
    }

    match class {
        ActionClass::Allow => true,
        ActionClass::Log => {
            bounded_log(&data, decision.action.raw());
            true
        }
        ActionClass::Errno { errno } => {
            let result = if errno == 0 { 0 } else { -(errno as isize) };
            uctx.set_retval(result as usize);
            false
        }
        ActionClass::Trap { data: trap_data } => {
            rollback_seccomp_syscall_frame(uctx, &data, raw_syscall);
            force_signal_current_thread(SignalInfo::new_sigsys(
                i32::from(trap_data),
                data.instruction_pointer as usize,
                data.number,
                data.architecture,
            ));
            false
        }
        ActionClass::KillThread => {
            rollback_seccomp_syscall_frame(uctx, &data, raw_syscall);
            terminate_for_seccomp(Signo::SIGSYS, false);
            false
        }
        ActionClass::KillProcess | ActionClass::Unknown { .. } => {
            rollback_seccomp_syscall_frame(uctx, &data, raw_syscall);
            terminate_for_seccomp(Signo::SIGSYS, true);
            false
        }
        ActionClass::Trace { .. } | ActionClass::UserNotification { .. } => {
            // Linux skips the syscall with ENOSYS when no tracer/listener owns
            // the request. We do not advertise either action until their
            // complete external ownership lifecycle exists.
            uctx.set_retval((-LinuxError::ENOSYS.code() as isize) as usize);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use memory_addr::VirtAddr;

    use super::*;

    #[test]
    fn raw_sock_fprog_layout_matches_linux_64_bit_abi() {
        assert_eq!(mem::size_of::<RawSockFprog>(), 16);
        assert_eq!(mem::align_of::<RawSockFprog>(), 8);
        assert_eq!(mem::offset_of!(RawSockFprog, length), 0);
        assert_eq!(mem::offset_of!(RawSockFprog, filter), 8);
    }

    #[test]
    fn strict_allowlist_is_exact() {
        assert!(strict_allows(Sysno::read as usize));
        assert!(strict_allows(Sysno::write as usize));
        assert!(strict_allows(Sysno::exit as usize));
        assert!(strict_allows(Sysno::rt_sigreturn as usize));
        assert!(!strict_allows(Sysno::exit_group as usize));
        assert!(!strict_allows(Sysno::getpid as usize));
    }

    #[test]
    fn adapter_error_mapping_preserves_resource_and_stale_boundaries() {
        assert_eq!(map_program_error(ProgramError::NoMemory), AxError::NoMemory);
        assert_eq!(
            map_program_error(ProgramError::MissingReturn),
            AxError::InvalidInput
        );
        assert_eq!(
            map_install_error(FilterInstallError::PathTooLong),
            AxError::NoMemory
        );
        assert_eq!(
            map_transition_error(StateTransitionError::Stale),
            LinuxError::EAGAIN.into()
        );
    }

    #[test]
    fn syscall_data_preserves_raw_registers_and_post_syscall_ip() {
        let mut context = UserContext::new(0x1234_5678, VirtAddr::from_usize(0x8000), 11);
        context.set_sysno(usize::MAX);
        context.set_arg0(1);
        context.set_arg1(2);
        context.set_arg2(3);
        context.set_arg3(4);
        context.set_arg4(5);
        context.set_arg5(usize::MAX);
        context.set_ip(0xfeed_beef);

        let data = seccomp_data(&context);
        assert_eq!(data.number, -1);
        assert_eq!(data.architecture, current_audit_architecture());
        assert_eq!(data.instruction_pointer, 0xfeed_beef);
        assert_eq!(data.arguments, [1, 2, 3, 4, 5, usize::MAX as u64]);
    }

    #[test]
    fn stable_profile_shares_architecture_frame_rollback_across_terminal_and_trap_paths() {
        let mut context = UserContext::new(0x1234_5678, VirtAddr::from_usize(0x8000), 11);
        context.set_sysno(Sysno::getppid as usize);
        context.set_arg0(0xfeed_beef);
        let data = seccomp_data(&context);
        let raw_syscall = context.sysno();
        context.set_retval(usize::MAX);

        rollback_seccomp_syscall_frame(&mut context, &data, raw_syscall);

        #[cfg(target_arch = "x86_64")]
        assert_eq!(context.retval(), Sysno::getppid as usize);
        #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
        assert_eq!(context.arg0(), 0xfeed_beef);
    }
}
