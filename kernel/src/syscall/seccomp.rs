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
use syscalls::Sysno;
use thekernel_linux_seccomp::{
    ActionClass, BPF_MAXINSNS, ClassicBpfInstruction, FilterInstallError, FilterMetadata,
    ProgramError, SECCOMP_GET_ACTION_AVAIL, SECCOMP_GET_NOTIF_SIZES, SECCOMP_RET_ALLOW,
    SECCOMP_RET_ERRNO, SECCOMP_RET_KILL_PROCESS, SECCOMP_RET_KILL_THREAD, SECCOMP_RET_LOG,
    SECCOMP_RET_TRAP, SECCOMP_SET_MODE_FILTER, SECCOMP_SET_MODE_STRICT, SeccompData, SeccompMode,
    VerifiedProgram,
};
use thekernel_linux_signal::{SignalInfo, Signo};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmPtr};

use crate::{
    mm::map_usercopy_error,
    task::{
        AsThread, SeccompPublicationError, do_exit, fail_closed_exit, force_signal_current_thread,
        ns_capable, seccomp_filter_budget,
    },
};

/// Audit architecture used by the x86_64 syscall ABI.
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
#[derive(Clone, Copy, bytemuck::AnyBitPattern)]
struct RawSockFprog {
    length: u16,
    padding: [u8; 6],
    filter: u64,
}

const _: [(); 16] = [(); mem::size_of::<RawSockFprog>()];
const _: [(); 8] = [(); mem::align_of::<RawSockFprog>()];
const _: [(); 8] = [(); mem::offset_of!(RawSockFprog, filter)];
const _: [(); 8] = [(); mem::size_of::<ClassicBpfInstruction>()];
const _: [(); 4] = [(); mem::align_of::<ClassicBpfInstruction>()];
const _: [(); 0] = [(); mem::offset_of!(ClassicBpfInstruction, code)];
const _: [(); 2] = [(); mem::offset_of!(ClassicBpfInstruction, jt)];
const _: [(); 3] = [(); mem::offset_of!(ClassicBpfInstruction, jf)];
const _: [(); 4] = [(); mem::offset_of!(ClassicBpfInstruction, k)];

impl RawSockFprog {
    fn read_from_user<M: UserMemory + ?Sized>(
        memory: &mut UserMemoryContext<'_, M>,
        pointer: usize,
    ) -> AxResult<Self> {
        // Linux `copy_from_user` accepts unaligned fprog pointers. Copy as
        // bytes so the Rust adapter does not accidentally impose a stronger
        // typed-pointer alignment contract. `RawSockFprog` is an explicit
        // integer/padding mirror of the 64-bit Linux UAPI object, so reading
        // it through the byte-addressed context is valid even when `pointer`
        // is unaligned.
        (pointer as *const Self)
            .vm_read(memory)
            .map_err(map_usercopy_error)
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

fn map_seccomp_publication_error(error: SeccompPublicationError) -> AxError {
    error.into_ax_error()
}

fn copy_filter_program<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    header: RawSockFprog,
) -> AxResult<Vec<ClassicBpfInstruction>> {
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
    // SAFETY: `ClassicBpfInstruction` is the eight-byte integer-only Linux
    // `sock_filter` representation (layout asserted below). The provider
    // initializes every byte or returns an error; no reference to the buffer
    // escapes during the copy.
    let destination = unsafe {
        core::slice::from_raw_parts_mut(
            instructions
                .as_mut_ptr()
                .cast::<MaybeUninit<ClassicBpfInstruction>>(),
            length,
        )
    };
    memory
        .read_slice(
            header.filter as usize as *const ClassicBpfInstruction,
            destination,
        )
        .map_err(map_usercopy_error)?;
    Ok(instructions)
}

fn filter_install_permitted() -> bool {
    let curr = current();
    let thread = curr.as_thread();
    let credential = thread.current_cred();
    credential.no_new_privs() || ns_capable(&credential, credential.user_ns(), CAP_SYS_ADMIN)
}

fn install_filter<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    flags: u32,
    args: *const (),
) -> AxResult<isize> {
    // Phase one intentionally supports task-local installation only. TSYNC,
    // NEW_LISTENER, LOG-at-install and speculative flags require their full
    // transactions/lifecycles and are rejected rather than partially faked.
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    // Linux copies the fprog header and validates its length before checking
    // no_new_privs/CAP_SYS_ADMIN, but performs that admission before copying
    // and verifying the instruction array.
    let header = RawSockFprog::read_from_user(memory, args as usize)?;
    let length = usize::from(header.length);
    if length == 0 || length > BPF_MAXINSNS {
        return Err(AxError::InvalidInput);
    }
    if !filter_install_permitted() {
        return Err(LinuxError::EACCES.into());
    }

    let instructions = copy_filter_program(memory, header)?;
    let program = VerifiedProgram::try_from_vec(instructions).map_err(map_program_error)?;
    // Verification is authoritative. Native translation is selected once at
    // admission and retained by the immutable filter node. Auto may use the
    // interpreter after a bounded native failure; force-jit returns an
    // explicit error and is never silently downgraded.
    let executor = crate::seccomp_jit::try_compile(&program)
        .map_err(crate::seccomp_jit::JitError::into_ax_error)?;

    let curr = current();
    let thread = curr.as_thread();
    let snapshot = thread.seccomp_snapshot();
    let expected = snapshot.filters();
    let prepared = expected
        .try_append_with_executor(
            program,
            FilterMetadata::default(),
            seccomp_filter_budget(),
            executor,
        )
        .map_err(map_install_error)?;
    // Reserve the publication accounting before the immutable task pointer
    // can become visible. Any stale/retire failure drops this guard and
    // removes the reservation without exposing a failed program.
    let publication = crate::seccomp_jit::try_reserve_published().ok_or(AxError::NoMemory)?;
    thread
        .try_publish_seccomp_filter(&snapshot, &prepared)
        .map_err(map_seccomp_publication_error)?;
    publication.commit();
    Ok(0)
}

fn enter_strict(flags: u32, args: *const ()) -> AxResult<isize> {
    if flags != 0 || !args.is_null() {
        return Err(AxError::InvalidInput);
    }
    let curr = current();
    curr.as_thread()
        .try_enter_seccomp_strict()
        .map_err(map_seccomp_publication_error)?;
    Ok(0)
}

fn get_action_available<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    flags: u32,
    args: *const (),
) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    // As with the fprog header, Linux accepts an unaligned query pointer.
    let action = (args as usize as *const u32)
        .vm_read(memory)
        .map_err(map_usercopy_error)?;
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
pub fn sys_seccomp<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    op: u32,
    flags: u32,
    args: *const (),
) -> AxResult<isize> {
    match op {
        SECCOMP_SET_MODE_STRICT => enter_strict(flags, args),
        SECCOMP_SET_MODE_FILTER => install_filter(memory, flags, args),
        SECCOMP_GET_ACTION_AVAIL => get_action_available(memory, flags, args),
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
pub(crate) fn sys_prctl_set_seccomp<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    mode: usize,
    args: *const (),
) -> AxResult<isize> {
    match mode {
        value if value == SeccompMode::Strict as usize => {
            // Linux has always ignored prctl's optional filter argument for
            // strict mode, even though seccomp(2) requires a NULL `uargs`.
            sys_seccomp(memory, SECCOMP_SET_MODE_STRICT, 0, core::ptr::null())
        }
        value if value == SeccompMode::Filter as usize => {
            sys_seccomp(memory, SECCOMP_SET_MODE_FILTER, 0, args)
        }
        _ => Err(AxError::InvalidInput),
    }
}

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
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
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
    // Linux restores the syscall frame before TRAP and every terminal
    // filter-action KILL path. Keep the x86_64 syscall-number register
    // available to the signal/core handling path.
    let _ = data;
    uctx.set_retval(_raw_syscall);
}

/// Applies the calling task's exact immutable seccomp snapshot before syscall
/// decoding or any getter/time fast path. Returns `true` only when normal
/// dispatch may continue.
pub(super) fn enforce_syscall_seccomp(uctx: &mut UserContext) -> bool {
    let curr = current();
    let thread = curr.as_thread();
    // Permanently disabled tasks do not enter the RCU domain at all. The
    // pointer load is only a fast-bit hint; the optional RCU read below is
    // authoritative and closes publication-versus-evaluation races.
    if !thread.seccomp_active() {
        return true;
    }

    let raw_syscall = uctx.sysno();
    let data = seccomp_data(uctx);
    // The active path pins the immutable state only for this evaluation. It
    // neither clones the `SeccompState`/`FilterChain` Arc nor takes a spinlock.
    let Some((mode, decision)) = thread.with_seccomp_current(|state| {
        let mode = state.mode();
        let decision = (mode == SeccompMode::Filter).then(|| state.evaluate(&data));
        (mode, decision)
    }) else {
        return true;
    };
    match mode {
        SeccompMode::Disabled => return true,
        SeccompMode::Strict => {
            if strict_allows(raw_syscall) {
                return true;
            }
            terminate_for_seccomp(Signo::SIGKILL, false);
            return false;
        }
        SeccompMode::Filter => {}
    }
    let decision = decision.expect("active seccomp filter state has no evaluation");
    crate::seccomp_jit::record_interpreter_executed_many(decision.interpreter_executions);
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
            map_seccomp_publication_error(SeccompPublicationError::Stale),
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

        assert_eq!(context.retval(), Sysno::getppid as usize);
    }
}
