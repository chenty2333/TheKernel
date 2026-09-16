use axerrno::{AxError, AxResult, LinuxError};
use axtask::{
    TaskState, current, replace_inactive_task_user_cet_state,
    snapshot_inactive_task_user_cet_state, yield_now,
};
use tk_linux_arch_x86_64::{ARCH_SHSTK_UNLOCK, NT_X86_SHSTK, X86ShstkRegset};
use tk_linux_process_adapter::Pid;
/// `PTRACE_O_MASK`, the two eventless option bits and the shared
/// `check_ptrace_options()` ladder live in `tk_linux_process` because both the
/// `PTRACE_SEIZE` and the `PTRACE_SETOPTIONS` spelling of the request word must
/// run the *same* admission.
use tk_linux_process::ptrace_options::{PtraceOptionReject, SuspendSeccompAdmission};
// Only the request-decoding test spells the two eventless bits out by name; the
// syscall path takes the whole mask from the same crate.
#[cfg(test)]
use tk_linux_process::ptrace_options::{
    EXITKILL as PTRACE_O_EXITKILL, SUSPEND_SECCOMP as PTRACE_O_SUSPEND_SECCOMP,
};
use tk_linux_seccomp::SeccompMode;
use tk_linux_signal::{SignalInfo, Signo};

use crate::{
    mm::{IoVec, UserMemoryCapability, map_usercopy_error},
    task::{
        AsThread, ProcessData, PtraceAccessMode, PtraceRelationshipOrigin,
        PtraceRelationshipSnapshot, PtraceReverseLink, PtraceSession, TaskParentCredentialPin,
        Thread, check_thread_ptrace_image_access_with_actor, get_task, get_visible_task,
        notify_ptrace_attach_stop, reinject_ptrace_signal,
        security::{ProcessImageSecurityRef, PtraceTracemeContext, dispatch_ptrace_traceme},
        send_signal_to_process,
    },
};

const PTRACE_TRACEME: u32 = 0;
const PTRACE_PEEKTEXT: u32 = 1;
const PTRACE_PEEKDATA: u32 = 2;
const PTRACE_PEEKUSER: u32 = 3;
const PTRACE_POKETEXT: u32 = 4;
const PTRACE_POKEDATA: u32 = 5;
const PTRACE_POKEUSER: u32 = 6;
const PTRACE_CONT: u32 = 7;
const PTRACE_KILL: u32 = 8;
const PTRACE_SINGLESTEP: u32 = 9;
const PTRACE_ATTACH: u32 = 16;
const PTRACE_DETACH: u32 = 17;
const PTRACE_SYSCALL: u32 = 24;
const PTRACE_ARCH_PRCTL: u32 = 30;
// include/uapi/linux/ptrace.h: "PTRACE_OLDSETOPTIONS is an alias for
// PTRACE_SETOPTIONS" -- `#define PTRACE_OLDSETOPTIONS 21` and
// `#define PTRACE_SETOPTIONS 0x4200` feed the same `ptrace_setoptions()` in
// kernel/ptrace.c (`case PTRACE_SETOPTIONS: ...`, which lists both).
// TheKernel accepted neither spelling before, because 21 was not decoded at
// all and therefore fell through to the "unknown request" arm.
const PTRACE_OLDSETOPTIONS: u32 = 21;
// kernel/ptrace.c `ptrace_resume()` handles all six resume spellings:
// PTRACE_CONT, PTRACE_SYSCALL, PTRACE_SINGLESTEP, PTRACE_SYSEMU,
// PTRACE_SYSEMU_SINGLESTEP and PTRACE_SINGLEBLOCK.
const PTRACE_SYSEMU: u32 = 31;
const PTRACE_SYSEMU_SINGLESTEP: u32 = 32;
const PTRACE_SINGLEBLOCK: u32 = 33;
const PTRACE_SETOPTIONS: u32 = 0x4200;
const PTRACE_GETEVENTMSG: u32 = 0x4201;
const PTRACE_GETSIGINFO: u32 = 0x4202;
const PTRACE_SETSIGINFO: u32 = 0x4203;
const PTRACE_GETREGSET: u32 = 0x4204;
const PTRACE_SETREGSET: u32 = 0x4205;
const PTRACE_SEIZE: u32 = 0x4206;
const PTRACE_INTERRUPT: u32 = 0x4207;
const PTRACE_LISTEN: u32 = 0x4208;
const PTRACE_GET_SYSCALL_INFO: u32 = 0x420e;

// kernel/ptrace.c: `#define PTRACE_O_MASK (0x000000ff | PTRACE_O_EXITKILL |
// PTRACE_O_SUSPEND_SECCOMP)`, with `PTRACE_O_EXITKILL (1 << 20)` and
// `PTRACE_O_SUSPEND_SECCOMP (1 << 21)` from include/uapi/linux/ptrace.h.
// 0x000000ff covers TRACESYSGOOD, TRACEFORK, TRACEVFORK, TRACECLONE,
// TRACEEXEC, TRACEVFORKDONE, TRACEEXIT and TRACESECCOMP.
// The previous value (0x2f_ffff) admitted bits 16..21 -- none of which Linux
// defines -- and rejected PTRACE_O_EXITKILL (1 << 20).
const PTRACE_O_MASK: usize = tk_linux_process::ptrace_options::MASK as usize;

/// `AUDIT_ARCH_X86_64` (include/uapi/linux/audit.h) =
/// `EM_X86_64(62) | __AUDIT_ARCH_64BIT(0x8000_0000) | __AUDIT_ARCH_LE(0x4000_0000)`.
/// `ptrace_get_syscall_info()` fills `info.arch` from `syscall_get_arch()`,
/// which is `AUDIT_ARCH_X86_64` on this target (arch/x86/include/asm/syscall.h).
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

/// `struct ptrace_syscall_info` op codes (include/uapi/linux/ptrace.h).
const PTRACE_SYSCALL_INFO_NONE: u8 = 0;
const PTRACE_SYSCALL_INFO_ENTRY: u8 = 1;
const PTRACE_SYSCALL_INFO_EXIT: u8 = 2;
const PTRACE_SYSCALL_INFO_SECCOMP: u8 = 3;

/// Architected `ptrace(PTRACE_GETREGSET/SETREGSET, ..., type, ...)` record
/// selectors present in the x86_64 `user_regset_view` (arch/x86/kernel/ptrace.c
/// `x86_64_regsets`). `find_regset()` matches on `regset->core_note_type`, so a
/// type outside this table is `-EINVAL` (kernel/ptrace.c `ptrace_regset()`:
/// "if (!regset || (kiov->iov_len % regset->size) != 0) return -EINVAL;").
const NT_PRSTATUS: usize = 1;
const NT_PRFPREG: usize = 2;
const NT_386_IOPERM: usize = 0x201;
const NT_X86_XSTATE: usize = 0x202;

/// Linux `struct ptrace_syscall_info` (include/uapi/linux/ptrace.h).
///
/// ```c
/// struct ptrace_syscall_info {
/// 	__u8 op;	/* PTRACE_SYSCALL_INFO_* */
/// 	__u8 reserved;
/// 	__u16 flags;
/// 	__u32 arch;
/// 	__u64 instruction_pointer;
/// 	__u64 stack_pointer;
/// 	union {
/// 		struct {
/// 			__u64 nr;
/// 			__u64 args[6];
/// 		} entry;
/// 		struct {
/// 			__s64 rval;
/// 			__u8 is_error;
/// 		} exit;
/// 		struct {
/// 			__u64 nr;
/// 			__u64 args[6];
/// 			__u32 ret_data;
/// 		} seccomp;
/// 	};
/// };
/// ```
///
/// The payload is a union, so it is modelled as `[u64; 8]`-sized storage plus
/// typed accessors; the only field TheKernel can currently populate is `op`,
/// because it has no syscall-entry/exit stop generation.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PtraceSyscallInfo {
    pub op: u8,
    pub reserved: u8,
    pub flags: u16,
    pub arch: u32,
    pub instruction_pointer: u64,
    pub stack_pointer: u64,
    pub payload: PtraceSyscallInfoPayload,
}

/// The `union` member of `struct ptrace_syscall_info`. `seccomp` is the largest
/// member (8 + 6*8 + 4 + 4 = 64 bytes), so the union -- and therefore the whole
/// structure, at 24 + 64 = 88 bytes -- is sized by it.
#[repr(C)]
#[derive(Clone, Copy)]
pub union PtraceSyscallInfoPayload {
    pub entry: PtraceSyscallInfoEntry,
    pub exit: PtraceSyscallInfoExit,
    pub seccomp: PtraceSyscallInfoSeccomp,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PtraceSyscallInfoEntry {
    pub nr: u64,
    pub args: [u64; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PtraceSyscallInfoExit {
    pub rval: i64,
    pub is_error: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PtraceSyscallInfoSeccomp {
    pub nr: u64,
    pub args: [u64; 6],
    pub ret_data: u32,
    pub reserved2: u32,
}

impl PtraceSyscallInfo {
    /// Bytes `ptrace_get_syscall_info()` reports as `actual_size`, i.e. the
    /// `offsetof`/`offsetofend` span of the union member the op selects:
    ///
    /// ```c
    /// 	unsigned long actual_size = offsetof(struct ptrace_syscall_info, entry);
    /// 	...
    /// 	if (info.op == PTRACE_SYSCALL_INFO_ENTRY || info.op == PTRACE_SYSCALL_INFO_SECCOMP) {
    /// 		...
    /// 		actual_size = offsetofend(struct ptrace_syscall_info, entry);
    /// 	} else if (info.op == PTRACE_SYSCALL_INFO_EXIT) {
    /// 		...
    /// 		actual_size = offsetofend(struct ptrace_syscall_info, exit.is_error);
    /// 	}
    /// ```
    ///
    /// NONE keeps the initialiser `offsetof(..., entry)`, which is just the
    /// 24-byte header. EXIT stops after the single `is_error` byte (33 bytes
    /// total), *not* at the end of the padded `struct ... exit` member.
    const fn active_size(op: u8) -> usize {
        /// `offsetof(struct ptrace_syscall_info, entry)`: op + reserved +
        /// flags + arch + instruction_pointer + stack_pointer.
        const HEADER: usize = 1 + 1 + 2 + 4 + 8 + 8;
        /// `offsetofend(struct ptrace_syscall_info, exit.is_error)`.
        const EXIT_END: usize = HEADER + 8 + 1;
        /// `offsetofend(struct ptrace_syscall_info, entry)`.
        const ENTRY_END: usize = HEADER + 8 + 6 * 8;
        match op {
            PTRACE_SYSCALL_INFO_EXIT => EXIT_END,
            PTRACE_SYSCALL_INFO_ENTRY | PTRACE_SYSCALL_INFO_SECCOMP => ENTRY_END,
            _ => HEADER,
        }
    }
}

#[cfg(target_arch = "x86_64")]
const ARCH_SHSTK_FEATURES: usize = 0b11;

fn ptrace_io_error() -> AxError {
    LinuxError::EIO.into()
}

fn current_pid() -> Pid {
    current().as_thread().proc_data.proc.pid()
}

fn current_kernel_tid() -> Pid {
    current().as_thread().kernel_tid()
}

fn check_tracee(target: &ProcessData) -> AxResult<PtraceSession> {
    target
        .ptrace_session_if_traced_by(current_pid(), current_kernel_tid())
        .ok_or(AxError::NoSuchProcess)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum InactiveScan {
    Inactive,
    Retry,
    Gone,
}

fn scan_tracee_task_states(
    states: impl IntoIterator<Item = AxResult<TaskState>>,
) -> AxResult<InactiveScan> {
    let mut saw_task = false;
    let mut scan = InactiveScan::Inactive;
    for state in states {
        saw_task = true;
        match state? {
            TaskState::Blocked => {}
            TaskState::Running | TaskState::Ready => scan = InactiveScan::Retry,
            TaskState::Exited => return Ok(InactiveScan::Gone),
        }
    }
    if saw_task {
        Ok(scan)
    } else {
        Ok(InactiveScan::Gone)
    }
}

fn check_inactive_tracee(target: &ProcessData) -> AxResult<PtraceSession> {
    loop {
        let session = target
            .ptrace_inactive_session_if_traced_by(current_pid(), current_kernel_tid())
            .ok_or(AxError::NoSuchProcess)?;
        let scan = scan_tracee_task_states(target.proc.thread_ids().map(|tid| {
            get_task(tid)
                .map(|task| task.state())
                .map_err(|_| AxError::NoSuchProcess)
        }))?;
        match scan {
            InactiveScan::Gone => return Err(AxError::NoSuchProcess),
            InactiveScan::Retry => {
                // The stop publisher has already interrupted every member.
                // Yielding here is the wait_task_inactive analogue: it gives
                // Ready tasks a chance to enter wait_if_stopped and does not
                // burn a CPU in a hidden polling loop.
                yield_now();
            }
            InactiveScan::Inactive => {
                // The outer ptrace action mutex prevents a sibling tracer
                // thread from resuming the group. Revalidate the exact stop
                // generation after observing every task blocked.
                if target.ptrace_inactive_session_if_traced_by(current_pid(), current_kernel_tid())
                    == Some(session)
                {
                    return Ok(session);
                }
                return Err(AxError::NoSuchProcess);
            }
        }
    }
}

/// Runs one remote-memory operation against the image that was current after
/// this tracer's session was verified.
///
/// The owned address-space handle deliberately stays in this scope so an exec
/// publication cannot make the operation re-sample a different image between
/// validation/population and the final transfer.
fn pinned_tracee_memory(
    target: &ProcessData,
    session: PtraceSession,
) -> AxResult<UserMemoryCapability> {
    let aspace_handle = target
        .ptrace_inactive_image_if_session(session)
        .ok_or(AxError::NoSuchProcess)?;
    Ok(UserMemoryCapability::new(aspace_handle))
}

/// Decode the `data` argument of the resume-family requests.
///
/// kernel/ptrace.c `ptrace_resume()`:
///
/// ```c
/// static int ptrace_resume(struct task_struct *child, long request,
/// 			 unsigned long data)
/// {
/// 	if (!valid_signal(data))
/// 		return -EIO;
/// ```
///
/// `valid_signal(sig)` is `((sig) <= _NSIG)`, so *every* out-of-range signal
/// number -- not just numbers above `u8` -- is -EIO.  `ptrace_detach()` starts
/// with the identical test.  TheKernel used to answer -EINVAL for both
/// `data > 64` and `data > 255`.
fn parse_signal(data: usize) -> AxResult<Option<SignalInfo>> {
    if data == 0 {
        return Ok(None);
    }
    let raw = u8::try_from(data).map_err(|_| ptrace_io_error())?;
    let signo = Signo::from_repr(raw).ok_or_else(ptrace_io_error)?;
    Ok(Some(SignalInfo::new_kernel(signo)))
}

fn interrupt_process_threads(target: &ProcessData) {
    for tid in target.proc.thread_ids() {
        if let Ok(task) = get_task(tid) {
            task.interrupt();
        }
    }
}

fn do_attach(target_thread: &Thread, seized: bool, initial_options: u32) -> AxResult<isize> {
    let curr = current();
    let ptracer = curr.as_thread();
    // Pin the exact actor before core/LSM authorization.  The same guard is
    // carried through relationship publication, so a concurrent credential
    // transition cannot turn an admitted actor into a different stored one.
    let ptracer_credential = ptracer.lock_credential_snapshot();
    let tracer_data = ptracer.proc_data.clone();
    let tracer = tracer_data.proc.pid();
    let tracer_kernel_tid = ptracer.kernel_tid();
    let target = &target_thread.proc_data;
    if target.proc.pid() == tracer {
        return Err(AxError::OperationNotPermitted);
    }
    if target.exec_in_progress() {
        return Err(AxError::OperationNotPermitted);
    }
    if !ptracer
        .landlock_domain()
        .is_ancestor_of(&target_thread.landlock_domain())
    {
        return Err(AxError::OperationNotPermitted);
    }
    let authorized_image = check_thread_ptrace_image_access_with_actor(
        ptracer,
        ptracer_credential.credential(),
        target_thread,
        PtraceAccessMode::AttachReal,
    )?;
    let reverse_link =
        tracer_data.try_prepare_ptrace_reverse_link(target.proc.pid(), tracer_kernel_tid)?;
    let publication = target.lock_ptrace_publication();
    let session = target.publish_ptrace_relationship(
        &publication,
        target_thread,
        ptracer,
        &ptracer_credential,
        PtraceRelationshipOrigin::Attach,
        ptracer_credential.credential(),
        seized,
        initial_options,
        &authorized_image,
        reverse_link,
    )?;
    if !seized && target.ptrace_stop(session, Signo::SIGSTOP as u8) {
        notify_ptrace_attach_stop(target);
        interrupt_process_threads(target);
    }
    drop(publication);
    Ok(0)
}

fn do_continue(
    target: &ProcessData,
    session: PtraceSession,
    data: usize,
    detach: bool,
) -> AxResult<PtraceContinueOutcome> {
    let curr = current();
    let tracer_data = curr.as_thread().proc_data.clone();
    let signal = parse_signal(data)?.map(|info| info.signo());
    let (resume_result, record, retired_relationship) = target
        .resume_ptrace(session, detach)
        .ok_or(AxError::NoSuchProcess)?;
    if detach {
        tracer_data.remove_ptrace_tracee(PtraceReverseLink::new(target.proc.pid(), session));
    }
    let reinjected = reinject_ptrace_signal(target, record, signal);
    target.finish_ptrace_resume(resume_result);
    Ok(PtraceContinueOutcome {
        result: reinjected.map(|()| 0),
        retired_relationship,
    })
}

/// Carries a detached relationship beyond the syscall's sleepable ptrace
/// action guard. Finishing earlier could run credential free hooks while that
/// guard is still held.
#[must_use = "detached relationship retirement must cross the ptrace action guard"]
struct PtraceContinueOutcome {
    result: AxResult<isize>,
    retired_relationship: Option<PtraceRelationshipSnapshot>,
}

impl PtraceContinueOutcome {
    fn finish(self) -> AxResult<isize> {
        let Self {
            result,
            retired_relationship,
        } = self;
        drop(retired_relationship);
        result
    }
}

fn peek_word(target: &ProcessData, session: PtraceSession, addr: usize) -> AxResult<isize> {
    let memory = pinned_tracee_memory(target, session)?;
    memory
        .read_value(addr as *const usize)
        .map(|word| word as isize)
        .map_err(|_| ptrace_io_error())
}

fn poke_word(
    target: &ProcessData,
    session: PtraceSession,
    addr: usize,
    data: usize,
) -> AxResult<isize> {
    let memory = pinned_tracee_memory(target, session)?;
    memory
        .write_value(addr as *mut usize, data)
        .map_err(|_| ptrace_io_error())?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
fn canonical_user_address(address: u64) -> bool {
    // The supported x86_64 product ABI uses canonical 48-bit user addresses.
    // Keep this check separate from VMA validation so malformed pointers never
    // reach address-space policy.
    ((address as i64) << 16 >> 16) as u64 == address
}

#[cfg(target_arch = "x86_64")]
fn ptrace_shstk_regset(
    tracer_memory: &UserMemoryCapability,
    target_task: &axtask::AxTaskRef,
    target: &ProcessData,
    session: PtraceSession,
    request: u32,
    note: usize,
    iov_address: usize,
) -> AxResult<isize> {
    // Linux checks tracee-stop authorization before interpreting the regset.
    check_inactive_tracee(target).and_then(|observed| {
        (observed == session)
            .then_some(())
            .ok_or(AxError::NoSuchProcess)
    })?;
    // arch/x86/kernel/ptrace.c decodes GETREGSET/SETREGSET as
    //
    // 	struct iovec __user *uiov = (struct iovec __user *)data;
    //
    // 	if (!access_ok(uiov, sizeof(*uiov)))
    // 		return -EFAULT;
    // 	if (__get_user(kiov.iov_base, &uiov->iov_base) ||
    // 	    __get_user(kiov.iov_len, &uiov->iov_len))
    // 		return -EFAULT;
    // 	ret = ptrace_regset(child, request, addr, &kiov);
    //
    // so the iovec is faulted in *before* the regset selector is interpreted:
    // a bad iovec pointer is -EFAULT even for an unknown record type.
    let mut iov = unsafe {
        tracer_memory
            .read_value_uninit(iov_address as *const IoVec)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    if note != NT_X86_SHSTK {
        // kernel/ptrace.c `ptrace_regset()`:
        //
        // 	const struct user_regset *regset = find_regset(view, type);
        //
        // 	if (!regset || (kiov->iov_len % regset->size) != 0)
        // 		return -EINVAL;
        //
        // `find_regset()` walks the x86_64 `user_regset_view`, whose members
        // are exactly NT_PRSTATUS, NT_PRFPREG, NT_X86_XSTATE, NT_386_IOPERM and
        // NT_X86_SHSTK, so an unknown type is -EINVAL.  For the architected
        // records TheKernel cannot produce (Linux answers with the tracee's
        // register file) fail closed with -EIO -- the errno Linux itself uses
        // for an inactive regset -- rather than fabricating registers.
        return Err(match note {
            NT_PRSTATUS | NT_PRFPREG | NT_X86_XSTATE | NT_386_IOPERM => ptrace_io_error(),
            _ => AxError::InvalidInput,
        });
    }
    if !axhal::asm::user_shadow_stack_enabled() {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    let required = core::mem::size_of::<X86ShstkRegset>();
    // `ptrace_regset()` rejects a length that is not a whole number of
    // records (-EINVAL); `__regset_get()` then clamps the count to the
    // record extent, so a zero length copies nothing and writes 0 back.
    if iov.iov_len < 0 || iov.iov_len as usize % required != 0 {
        return Err(AxError::InvalidInput);
    }
    iov.iov_len = (iov.iov_len as usize).min(required) as i64;
    match request {
        PTRACE_GETREGSET => {
            // With a zero count Linux never calls the regset's `get`
            // callback, so the empty request answers without consulting the
            // tracee's state.
            if iov.iov_len != 0 {
                let state = snapshot_inactive_task_user_cet_state(target_task)
                    .map_err(|_| AxError::NoSuchProcess)?;
                tracer_memory
                    .write_value(
                        iov.iov_base as *mut X86ShstkRegset,
                        X86ShstkRegset { ssp: state.pl3_ssp },
                    )
                    .map_err(map_usercopy_error)?;
            }
            tracer_memory
                .write_value(iov_address as *mut IoVec, iov)
                .map_err(map_usercopy_error)?;
            Ok(0)
        }
        PTRACE_SETREGSET => {
            if iov.iov_len != required as i64 {
                return Err(AxError::InvalidInput);
            }
            let regset = tracer_memory
                .read_value(iov.iov_base as *const X86ShstkRegset)
                .map_err(map_usercopy_error)?;
            if !canonical_user_address(regset.ssp) || regset.ssp & 7 != 0 {
                return Err(AxError::InvalidInput);
            }
            let mut state = snapshot_inactive_task_user_cet_state(target_task)
                .map_err(|_| AxError::NoSuchProcess)?;
            if !target
                .aspace()
                .lock()
                .cet_shadow_stack_pointer_valid(regset.ssp)
            {
                return Err(AxError::InvalidInput);
            }
            state.pl3_ssp = regset.ssp;
            replace_inactive_task_user_cet_state(target_task, state)
                .map_err(|_| AxError::NoSuchProcess)?;
            Ok(0)
        }
        _ => unreachable!(),
    }
}

/// Implement `PTRACE_GET_SYSCALL_INFO` (0x420e, include/uapi/linux/ptrace.h).
///
/// kernel/ptrace.c:
///
/// ```c
/// static int ptrace_get_syscall_info(struct task_struct *child, unsigned long user_size,
/// 				   void __user *datavp)
/// {
/// 	struct pt_regs *regs = task_pt_regs(child);
/// 	struct ptrace_syscall_info info = {
/// 		.op = PTRACE_SYSCALL_INFO_NONE,
/// 		.arch = syscall_get_arch(child),
/// 		.instruction_pointer = instruction_pointer(regs),
/// 		.stack_pointer = user_stack_pointer(regs),
/// 	};
/// 	unsigned long actual_size = offsetof(struct ptrace_syscall_info, entry);
/// 	unsigned long write_size;
/// 	...
/// 	if (info.op == PTRACE_SYSCALL_INFO_ENTRY || info.op == PTRACE_SYSCALL_INFO_SECCOMP) {
/// 		...
/// 		actual_size = offsetofend(struct ptrace_syscall_info, entry);
/// 	} else if (info.op == PTRACE_SYSCALL_INFO_EXIT) {
/// 		...
/// 		actual_size = offsetofend(struct ptrace_syscall_info, exit.is_error);
/// 	}
/// 	write_size = min(actual_size, user_size);
/// 	if (copy_to_user(datavp, &info, write_size))
/// 		return -EFAULT;
/// 	return actual_size;
/// }
/// ```
///
/// `user_size` arrives in `addr` and the destination pointer in `data`, because
/// kernel/ptrace.c routes the request as
/// `ptrace_get_syscall_info(child, addr, datap)`.
///
/// TheKernel never publishes a syscall-entry/exit stop, so no stop can be
/// classified as ENTRY, EXIT or SECCOMP, and `op` is always
/// `PTRACE_SYSCALL_INFO_NONE`.  That is the truthful answer a caller uses to
/// detect the absence of syscall stops; it is also what strace checks first.
/// `instruction_pointer`/`stack_pointer` would come from the stopped tracee's
/// saved register file, which TheKernel does not retain per stop, so they are
/// reported as zero instead of being fabricated.
#[cfg(target_arch = "x86_64")]
fn ptrace_get_syscall_info(
    tracer_memory: &UserMemoryCapability,
    user_size: usize,
    data: usize,
) -> AxResult<isize> {
    let info = PtraceSyscallInfo {
        op: PTRACE_SYSCALL_INFO_NONE,
        reserved: 0,
        flags: 0,
        arch: AUDIT_ARCH_X86_64,
        instruction_pointer: 0,
        stack_pointer: 0,
        // Initialise the largest union member so that every byte of the
        // structure is defined; `ptrace_get_syscall_info()` builds `info` with a
        // designated initialiser, which zero-fills the same bytes.
        payload: PtraceSyscallInfoPayload {
            seccomp: PtraceSyscallInfoSeccomp {
                nr: 0,
                args: [0; 6],
                ret_data: 0,
                reserved2: 0,
            },
        },
    };
    let actual_size = PtraceSyscallInfo::active_size(info.op);
    let write_size = actual_size.min(user_size);
    if write_size != 0 {
        // SAFETY: `PtraceSyscallInfo` is `#[repr(C)]`, every field (including
        // the whole union payload) is initialised above, it contains no
        // padding-invalid bytes, and `write_size <= size_of::<PtraceSyscallInfo>()`.
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (&info as *const PtraceSyscallInfo).cast::<u8>(),
                write_size,
            )
        };
        tracer_memory
            .write_bytes(data, bytes)
            .map_err(map_usercopy_error)?;
    }
    // Linux returns the *actual* size, not the number of bytes copied, so a
    // short (or even zero-length) user buffer still reports how much the kernel
    // had to offer.
    Ok(actual_size as isize)
}

/// Execute the one x86 arch_prctl operation Linux permits a tracer to apply
/// to a stopped tracee.  The normal `arch_prctl(ARCH_SHSTK_UNLOCK)` syscall
/// remains EPERM: permitting it here is deliberately tied to both a live
/// ptrace relationship and the stop generation checked by
/// `check_inactive_tracee`.
#[cfg(target_arch = "x86_64")]
fn ptrace_arch_prctl(
    target_task: &axtask::AxTaskRef,
    target: &ProcessData,
    session: PtraceSession,
    code: usize,
    requested_features: usize,
) -> AxResult<isize> {
    // PTRACE_ARCH_PRCTL is a stopped-tracee operation, just like the CET
    // regset.  Establish that boundary before interpreting the operation or
    // its feature mask.
    let observed = check_inactive_tracee(target)?;
    if observed != session {
        return Err(AxError::NoSuchProcess);
    }
    if i32::try_from(code).ok() != Some(ARCH_SHSTK_UNLOCK) {
        return Err(ptrace_io_error());
    }
    if !axhal::asm::user_shadow_stack_enabled() {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    if requested_features == 0 || requested_features & !ARCH_SHSTK_FEATURES != 0 {
        return Err(AxError::InvalidInput);
    }

    // The outer ptrace action guard prevents a sibling tracer from resuming
    // or detaching after the stop generation above was observed.
    let mut state =
        snapshot_inactive_task_user_cet_state(target_task).map_err(|_| AxError::NoSuchProcess)?;
    state.locked &= !(requested_features as u64);
    replace_inactive_task_user_cet_state(target_task, state).map_err(|_| AxError::NoSuchProcess)?;
    Ok(0)
}

fn sys_ptrace_traceme() -> AxResult<isize> {
    let curr = current();
    let child = curr.as_thread();
    let proc_data = &child.proc_data;
    let parent_snapshot = child
        .task_parent_snapshot()
        .ok_or(AxError::OperationNotPermitted)?;
    let resolve_exact_parent = || {
        let task =
            get_task(parent_snapshot.kernel_tid()).map_err(|_| AxError::OperationNotPermitted)?;
        let parent = task.try_as_thread().ok_or(AxError::OperationNotPermitted)?;
        if parent.kernel_tid() != parent_snapshot.kernel_tid()
            || !alloc::sync::Arc::ptr_eq(parent.task_parent_node(), parent_snapshot.parent_node())
            || !child.task_parent_security_snapshot_matches(&parent_snapshot)
        {
            return Err(AxError::OperationNotPermitted);
        }
        Ok::<_, AxError>(task)
    };
    let parent_task = resolve_exact_parent()?;
    if !parent_task
        .as_thread()
        .landlock_domain()
        .is_ancestor_of(&child.landlock_domain())
    {
        return Err(AxError::OperationNotPermitted);
    }
    let authorized_image = proc_data.thread_image_access_snapshot(child)?;

    let child_image_ref =
        ProcessImageSecurityRef::new(authorized_image.owner_user_ns(), authorized_image.aspace());
    let context = PtraceTracemeContext::new(
        parent_snapshot.credential(),
        authorized_image.credential(),
        child_image_ref.owner_user_ns(),
        &child_image_ref,
    );
    // All process-image locks used to create `authorized_image` have already
    // been released, and reverse-link/ptrace spin locks are acquired only
    // after the dedicated traceme hook stack admits this frozen context.
    dispatch_ptrace_traceme(&context)?;
    drop(parent_task);

    let parent_task = resolve_exact_parent()?;
    let parent_data = parent_task.as_thread().proc_data.clone();
    let mut reverse_link = Some(
        parent_data
            .try_prepare_ptrace_reverse_link(proc_data.proc.pid(), parent_snapshot.kernel_tid())?,
    );
    // Reservation is fallible and may sleep. Re-resolve the immutable parent
    // task and hook-actor credential, then separately pin the calling child's
    // current credential which Linux stores as ptracer_cred for TRACEME.
    drop(parent_task);
    loop {
        let publication = proc_data.lock_ptrace_traceme_publication(&parent_data)?;
        let graph = publication.task_parent_publication();
        let parent_task =
            get_task(parent_snapshot.kernel_tid()).map_err(|_| AxError::OperationNotPermitted)?;
        let parent = parent_task
            .try_as_thread()
            .ok_or(AxError::OperationNotPermitted)?;
        if parent.kernel_tid() != parent_snapshot.kernel_tid()
            || !alloc::sync::Arc::ptr_eq(parent.task_parent_node(), parent_snapshot.parent_node())
            || !alloc::sync::Arc::ptr_eq(&parent.proc_data, &parent_data)
            || !child.task_parent_security_snapshot_matches_locked(graph, &parent_snapshot)
        {
            return Err(AxError::OperationNotPermitted);
        }

        match child.try_lock_task_parent_security_snapshot(graph, &parent_snapshot) {
            TaskParentCredentialPin::Pinned(parent_credential) => {
                let Some(child_credential) = child.try_lock_credential_snapshot() else {
                    drop(parent_credential);
                    drop(parent_task);
                    drop(publication);
                    yield_now();
                    continue;
                };
                let result = proc_data.publish_ptrace_relationship(
                    &publication,
                    child,
                    parent,
                    &parent_credential,
                    PtraceRelationshipOrigin::Traceme,
                    child_credential.credential(),
                    false,
                    0,
                    &authorized_image,
                    reverse_link.take().expect("reserved ptrace reverse link"),
                );
                drop(child_credential);
                drop(parent_credential);
                drop(parent_task);
                drop(publication);
                result?;
                return Ok(0);
            }
            TaskParentCredentialPin::Stale => {
                return Err(AxError::OperationNotPermitted);
            }
            TaskParentCredentialPin::Busy => {
                drop(parent_task);
                drop(publication);
                yield_now();
            }
        }
    }
}

/// Linux `check_ptrace_options()` (`kernel/ptrace.c`), run by *both* request
/// spellings that carry an option word.
///
/// The option ladder itself -- the mask, and the `PTRACE_O_SUSPEND_SECCOMP`
/// configuration/capability/seccomp-state tests -- lives in
/// [`tk_linux_process::ptrace_options::check`]; this supplies the tracer-side
/// facts and Linux's errno mapping:
///
/// ```c
/// static int check_ptrace_options(unsigned long data)
/// {
/// 	if (data & ~(unsigned long)PTRACE_O_MASK)
/// 		return -EINVAL;
/// 	if (unlikely(data & PTRACE_O_SUSPEND_SECCOMP)) {
/// 		if (!IS_ENABLED(CONFIG_CHECKPOINT_RESTORE) ||
/// 		    !IS_ENABLED(CONFIG_SECCOMP))
/// 			return -EINVAL;
/// 		if (!capable(CAP_SYS_ADMIN))
/// 			return -EPERM;
/// 		if (seccomp_mode(&current->seccomp) != SECCOMP_MODE_DISABLED ||
/// 		    current->ptrace & PT_SUSPEND_SECCOMP)
/// 			return -EPERM;
/// 	}
/// 	return 0;
/// }
/// ```
///
/// TheKernel declares `CONFIG_CHECKPOINT_RESTORE=y` together with its real
/// `CONFIG_SECCOMP=y`, so the `-EINVAL` configuration branch is not taken here,
/// and the suspension itself is implemented: `PTRACE_O_SUSPEND_SECCOMP` is
/// stored in the relationship's option word, where
/// `ProcessData::ptrace_seccomp_suspended()` reads it for
/// `__secure_computing()`, and `current->ptrace & PT_SUSPEND_SECCOMP` becomes
/// the tracer's own process state.
fn check_ptrace_options(data: u32) -> AxResult<()> {
    let tracer = current();
    let tracer = tracer.as_thread();
    let admission = SuspendSeccompAdmission {
        configured: true,
        // `capable(CAP_SYS_ADMIN)` is the *initial* user namespace check,
        // matching Linux `capable()` -> `ns_capable(&init_user_ns, ...)`.
        cap_sys_admin: tracer.has_effective_capability(linux_raw_sys::general::CAP_SYS_ADMIN),
        // `seccomp_mode(&current->seccomp) != SECCOMP_MODE_DISABLED`.
        filtered: tracer.seccomp_mode() != SeccompMode::Disabled,
        // `current->ptrace & PT_SUSPEND_SECCOMP`: the tracer is itself a tracee
        // whose tracer suspended its policy.
        already_suspended: tracer.proc_data.ptrace_seccomp_suspended(),
    };
    match tk_linux_process::ptrace_options::check(data, admission) {
        Ok(()) => Ok(()),
        Err(PtraceOptionReject::Unknown | PtraceOptionReject::Unsupported) => {
            Err(AxError::InvalidInput)
        }
        Err(PtraceOptionReject::NotPermitted) => Err(AxError::OperationNotPermitted),
    }
}

fn sys_ptrace_for_target(
    tracer_memory: &UserMemoryCapability,
    request: u32,
    pid: Pid,
    addr: usize,
    data: usize,
) -> AxResult<isize> {
    let target_pid = current()
        .as_thread()
        .pid_ns()
        .resolve_visible_pid(pid)
        .ok_or(AxError::NoSuchProcess)?;
    let target_task = get_visible_task(target_pid)?;
    let target_thread = target_task.as_thread();
    let target = target_thread.proc_data.clone();
    match request {
        // Relationship publication has a stronger outer lock order:
        // process_lifecycle -> ptrace_actions -> exec/image/ptrace. Keep it
        // out of the ordinary action gate below; publication acquires the
        // composite guard in that order.
        PTRACE_ATTACH => return do_attach(target_thread, false, 0),
        PTRACE_SEIZE => {
            if addr != 0 {
                return Err(ptrace_io_error());
            }
            if data & !PTRACE_O_MASK != 0 {
                // kernel/ptrace.c `ptrace_attach()` rejects option bits it
                // does not know with -EIO, unlike PTRACE_SETOPTIONS which
                // reports -EINVAL for the same condition:
                //
                // 	} else if (request == PTRACE_SEIZE) {
                // 		if (addr)
                // 			return -EIO;
                // 		if (flags & ~(unsigned long)PTRACE_O_MASK)
                // 			return -EIO;
                // 		ret = check_ptrace_options(flags);
                return Err(ptrace_io_error());
            }
            // The very next statement in that branch is the *shared*
            // `check_ptrace_options()` call, so `PTRACE_SEIZE` must run exactly
            // the same option admission as `PTRACE_SETOPTIONS` rather than only
            // the unknown-bits duplicate above:
            //
            // 	retval = check_ptrace_options(flags);
            // 	if (retval)
            // 		return retval;
            check_ptrace_options(data as u32)?;
            return do_attach(target_thread, true, data as u32);
        }
        _ => {}
    }

    // Ordinary actions need only the sleepable per-target gate. Exact
    // ptrace/image/job-control spin checks remain short, while the relationship
    // cannot be resumed or detached by a sibling tracer thread during remote
    // memory or usercopy.
    let ptrace_action = target.lock_ptrace_actions();
    match request {
        PTRACE_CONT | PTRACE_SYSCALL | PTRACE_SINGLESTEP => {
            let session = check_inactive_tracee(&target)?;
            do_continue(&target, session, data, false)?.finish()
        }
        // kernel/ptrace.c `ptrace_resume()` also accepts PTRACE_SYSEMU,
        // PTRACE_SYSEMU_SINGLESTEP and PTRACE_SINGLEBLOCK:
        //
        // 	case PTRACE_SYSEMU:
        // 	case PTRACE_SYSEMU_SINGLESTEP:
        // 	case PTRACE_SINGLEBLOCK:
        // 		...
        // 		ret = ptrace_resume(child, request, data);
        //
        // All three need machinery TheKernel does not have -- syscall-entry
        // emulation (TIF_SYSCALL_EMU, i.e. a syscall stop whose return value is
        // forced to -ENOSYS with the syscall skipped) and hardware block-step
        // (arch_has_block_step()).  They are decoded here so that the *stop
        // generation* check runs first, which is what makes the difference
        // between -ESRCH (unrelated or running tracee) and -EIO; resuming
        // without the requested effect would be a silent ABI violation, so the
        // request still fails closed with -EIO.
        PTRACE_SYSEMU | PTRACE_SYSEMU_SINGLESTEP | PTRACE_SINGLEBLOCK => {
            check_inactive_tracee(&target)?;
            Err(ptrace_io_error())
        }
        PTRACE_DETACH => {
            let session = check_inactive_tracee(&target)?;
            let outcome = do_continue(&target, session, data, true)?;
            drop(ptrace_action);
            outcome.finish()
        }
        PTRACE_KILL => {
            check_tracee(&target)?;
            send_signal_to_process(
                target.proc.pid(),
                Some(SignalInfo::new_kernel(Signo::SIGKILL)),
            )?;
            Ok(0)
        }
        PTRACE_PEEKTEXT | PTRACE_PEEKDATA => {
            let session = check_inactive_tracee(&target)?;
            peek_word(&target, session, addr)
        }
        PTRACE_POKETEXT | PTRACE_POKEDATA => {
            let session = check_inactive_tracee(&target)?;
            poke_word(&target, session, addr, data)
        }
        PTRACE_PEEKUSER | PTRACE_POKEUSER => {
            check_inactive_tracee(&target)?;
            Err(ptrace_io_error())
        }
        // PTRACE_OLDSETOPTIONS (21) is the pre-0x4200 spelling of the same
        // request: kernel/ptrace.c decodes both in one arm.
        PTRACE_SETOPTIONS | PTRACE_OLDSETOPTIONS => {
            let session = check_inactive_tracee(&target)?;
            // kernel/ptrace.c `ptrace_setoptions()` is the whole body of this
            // arm:
            //
            // 	if (data & ~(unsigned long)PTRACE_O_MASK)
            // 		return -EINVAL;
            // 	ret = check_ptrace_options(data);
            // 	if (ret)
            // 		return ret;
            // 	child->ptrace = ... options ...;
            //
            // The option word itself is admitted by the same helper
            // `PTRACE_SEIZE` runs, so the two spellings cannot drift.
            if data & !PTRACE_O_MASK != 0 {
                return Err(AxError::InvalidInput);
            }
            check_ptrace_options(data as u32)?;
            if !target.ptrace_set_options(session, data as u32) {
                return Err(AxError::NoSuchProcess);
            }
            Ok(0)
        }
        PTRACE_GETEVENTMSG => {
            let session = check_inactive_tracee(&target)?;
            let event_message = target
                .ptrace_event_message(session)
                .ok_or(AxError::NoSuchProcess)?;
            tracer_memory
                .write_value(data as *mut usize, event_message)
                .map_err(map_usercopy_error)?;
            Ok(0)
        }
        PTRACE_INTERRUPT => {
            let session = check_tracee(&target)?;
            let stopped = target
                .ptrace_interrupt(session, Signo::SIGTRAP as u8)
                .ok_or_else(ptrace_io_error)?;
            if stopped {
                notify_ptrace_attach_stop(&target);
                interrupt_process_threads(&target);
            }
            Ok(0)
        }
        PTRACE_LISTEN => {
            check_inactive_tracee(&target)?;
            // LISTEN is not an ordinary resume: Linux retains a seized
            // group-stop in a distinct listening state until an event or
            // INTERRUPT re-traps it. Do not fake that state with CONT.
            Err(ptrace_io_error())
        }
        PTRACE_GETSIGINFO => {
            let session = check_inactive_tracee(&target)?;
            let info = target
                .ptrace_signal_info(session)
                .ok_or_else(ptrace_io_error)?;
            unsafe {
                tracer_memory
                    .write_value_unchecked(data as *mut SignalInfo, info)
                    .map_err(map_usercopy_error)?;
            }
            Ok(0)
        }
        PTRACE_SETSIGINFO => {
            let session = check_inactive_tracee(&target)?;
            let info = unsafe {
                tracer_memory
                    .read_value_uninit(data as *const SignalInfo)
                    .map_err(map_usercopy_error)?
                    .assume_init()
            };
            target.replace_ptrace_signal_info(session, info)?;
            Ok(0)
        }
        PTRACE_ARCH_PRCTL => {
            #[cfg(target_arch = "x86_64")]
            {
                let session = check_tracee(&target)?;
                return ptrace_arch_prctl(&target_task, &target, session, addr, data);
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                Err(ptrace_io_error())
            }
        }
        PTRACE_GETREGSET | PTRACE_SETREGSET => {
            #[cfg(target_arch = "x86_64")]
            {
                let session = check_inactive_tracee(&target)?;
                return ptrace_shstk_regset(
                    tracer_memory,
                    &target_task,
                    &target,
                    session,
                    request,
                    addr,
                    data,
                );
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                check_inactive_tracee(&target)?;
                Err(ptrace_io_error())
            }
        }
        PTRACE_GET_SYSCALL_INFO => {
            #[cfg(target_arch = "x86_64")]
            {
                check_inactive_tracee(&target)?;
                return ptrace_get_syscall_info(tracer_memory, addr, data);
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                check_inactive_tracee(&target)?;
                Err(ptrace_io_error())
            }
        }
        PTRACE_ATTACH | PTRACE_SEIZE => unreachable!(),
        // kernel/ptrace.c `ptrace_request()` opens with `int ret = -EIO;` and
        // every unrecognised request reaches `default: break;`, so an unknown
        // request number is -EIO -- *after* `ptrace_check_attach()` has already
        // established that the target is our stopped tracee (-ESRCH otherwise).
        _ => {
            check_inactive_tracee(&target)?;
            Err(ptrace_io_error())
        }
    }
}

pub fn sys_ptrace(
    tracer_memory: UserMemoryCapability,
    request: u32,
    pid: i32,
    addr: usize,
    data: usize,
) -> AxResult<isize> {
    match request {
        PTRACE_TRACEME => sys_ptrace_traceme(),
        _ => {
            if pid <= 0 {
                return Err(AxError::NoSuchProcess);
            }
            sys_ptrace_for_target(&tracer_memory, request, pid as Pid, addr, data)
        }
    }
}

#[cfg(test)]
mod tests {
    use axerrno::AxResult;
    use axtask::TaskState;

    use super::{InactiveScan, scan_tracee_task_states};

    fn scan(states: &[TaskState]) -> InactiveScan {
        scan_tracee_task_states(states.iter().copied().map(Ok::<_, axerrno::AxError>)).unwrap()
    }

    #[test]
    fn process_access_ptrace_scheduler_inactive_scan_waits_for_every_task() {
        assert_eq!(scan(&[]), InactiveScan::Gone);
        assert_eq!(scan(&[TaskState::Blocked]), InactiveScan::Inactive);
        assert_eq!(
            scan(&[TaskState::Blocked, TaskState::Ready]),
            InactiveScan::Retry
        );
        assert_eq!(
            scan(&[TaskState::Running, TaskState::Blocked]),
            InactiveScan::Retry
        );
        assert_eq!(
            scan(&[TaskState::Running, TaskState::Exited]),
            InactiveScan::Gone
        );
    }

    #[test]
    fn process_access_ptrace_scheduler_scan_propagates_lookup_failure() {
        let states: [AxResult<TaskState>; 1] = [Err(axerrno::AxError::NoSuchProcess)];
        assert_eq!(
            scan_tracee_task_states(states),
            Err(axerrno::AxError::NoSuchProcess)
        );
    }

    #[test]
    fn ptrace_option_mask_matches_linux() {
        // kernel/ptrace.c: `#define PTRACE_O_MASK (0x000000ff | PTRACE_O_EXITKILL |
        // PTRACE_O_SUSPEND_SECCOMP)`.  PTRACE_O_EXITKILL is (1 << 20) and
        // PTRACE_O_SUSPEND_SECCOMP is (1 << 21), so the mask is 0x003000ff.
        // The previous TheKernel value 0x2f_ffff rejected EXITKILL and admitted
        // six undefined bits.
        assert_eq!(super::PTRACE_O_MASK, 0x0030_00ff);
        assert_eq!(
            super::PTRACE_O_EXITKILL | super::PTRACE_O_SUSPEND_SECCOMP,
            0x0030_0000
        );
        // Both bits are accepted by PTRACE_SETOPTIONS...
        for option in [super::PTRACE_O_EXITKILL, super::PTRACE_O_SUSPEND_SECCOMP] {
            assert_eq!(super::PTRACE_O_MASK & option as usize, option as usize);
        }
        // ...and the undefined bits the old mask admitted are not.
        for bogus in [1 << 16, 1 << 17, 1 << 18, 1 << 19] {
            assert_eq!(super::PTRACE_O_MASK & bogus, 0);
        }
    }

    #[test]
    fn ptrace_request_numbers_match_linux_uapi() {
        // include/uapi/linux/ptrace.h.  PTRACE_OLDSETOPTIONS is an alias of
        // PTRACE_SETOPTIONS, and the three extra resume spellings sit between
        // PTRACE_ARCH_PRCTL (30) and the 0x42xx block.
        assert_eq!(super::PTRACE_OLDSETOPTIONS, 21);
        assert_eq!(super::PTRACE_SYSEMU, 31);
        assert_eq!(super::PTRACE_SYSEMU_SINGLESTEP, 32);
        assert_eq!(super::PTRACE_SINGLEBLOCK, 33);
        assert_eq!(super::PTRACE_GET_SYSCALL_INFO, 0x420e);
        // PTRACE_OLDSETOPTIONS is a plain number, not part of the 0x4200
        // request space, so the two spellings must not collide.
        assert_ne!(super::PTRACE_OLDSETOPTIONS, super::PTRACE_SETOPTIONS);
        assert!(super::PTRACE_OLDSETOPTIONS < 0x4200);
    }

    #[test]
    fn ptrace_syscall_info_layout_matches_linux() {
        use core::mem::{align_of, offset_of, size_of};

        use super::{
            PtraceSyscallInfo, PtraceSyscallInfoEntry, PtraceSyscallInfoSeccomp,
        };

        // `struct ptrace_syscall_info` (include/uapi/linux/ptrace.h) is 88 bytes
        // with 8-byte alignment on x86_64: a 24-byte header plus a union whose
        // largest member is `seccomp` at 64 bytes.
        assert_eq!(size_of::<PtraceSyscallInfo>(), 88);
        assert_eq!(align_of::<PtraceSyscallInfo>(), 8);
        assert_eq!(size_of::<PtraceSyscallInfoEntry>(), 56);
        assert_eq!(size_of::<PtraceSyscallInfoSeccomp>(), 64);
        assert_eq!(offset_of!(PtraceSyscallInfo, op), 0);
        assert_eq!(offset_of!(PtraceSyscallInfo, reserved), 1);
        assert_eq!(offset_of!(PtraceSyscallInfo, flags), 2);
        assert_eq!(offset_of!(PtraceSyscallInfo, arch), 4);
        assert_eq!(offset_of!(PtraceSyscallInfo, instruction_pointer), 8);
        assert_eq!(offset_of!(PtraceSyscallInfo, stack_pointer), 16);
        assert_eq!(offset_of!(PtraceSyscallInfo, payload), 24);
        assert_eq!(offset_of!(PtraceSyscallInfoSeccomp, ret_data), 56);
        assert_eq!(offset_of!(PtraceSyscallInfoSeccomp, reserved2), 60);
    }

    #[test]
    fn ptrace_syscall_info_active_size_matches_linux() {
        use super::{
            PTRACE_SYSCALL_INFO_ENTRY, PTRACE_SYSCALL_INFO_EXIT, PTRACE_SYSCALL_INFO_NONE,
            PTRACE_SYSCALL_INFO_SECCOMP, PtraceSyscallInfo,
        };

        // `actual_size` starts at `offsetof(struct ptrace_syscall_info, entry)`
        // (24), grows to `offsetofend(..., entry)` (80) for ENTRY/SECCOMP, and
        // to `offsetofend(..., exit.is_error)` (33) for EXIT -- note 33, not the
        // 40 bytes of the padded `struct ... exit` member.
        assert_eq!(PtraceSyscallInfo::active_size(PTRACE_SYSCALL_INFO_NONE), 24);
        assert_eq!(PtraceSyscallInfo::active_size(PTRACE_SYSCALL_INFO_ENTRY), 80);
        assert_eq!(PtraceSyscallInfo::active_size(PTRACE_SYSCALL_INFO_EXIT), 33);
        assert_eq!(
            PtraceSyscallInfo::active_size(PTRACE_SYSCALL_INFO_SECCOMP),
            80
        );
        // An unknown op code still yields the header size because the
        // initialiser is never revisited.
        assert_eq!(PtraceSyscallInfo::active_size(0xff), 24);
    }

    #[test]
    fn ptrace_x86_64_regset_note_types_match_the_arch_view() {
        // arch/x86/kernel/ptrace.c `x86_64_regsets` members, selected by
        // `find_regset()` on `core_note_type`.  NT_X86_SHSTK is the only one
        // TheKernel can serve; the rest must be classified as known-but-inert
        // rather than unknown, because `ptrace_regset()` answers them
        // differently (-EIO for an inactive regset, -EINVAL for a missing one).
        assert_eq!(super::NT_PRSTATUS, 1);
        assert_eq!(super::NT_PRFPREG, 2);
        assert_eq!(super::NT_386_IOPERM, 0x201);
        assert_eq!(super::NT_X86_XSTATE, 0x202);
        // Note the gap: NT_X86_SHSTK is 0x204 and 0x203 is unassigned.
        assert_eq!(tk_linux_arch_x86_64::NT_X86_SHSTK, 0x204);
        assert_eq!(super::AUDIT_ARCH_X86_64, 0xc000_003e);
    }
}
