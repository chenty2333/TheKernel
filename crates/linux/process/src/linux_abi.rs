//! Pure clone, rusage and pidfd ABI policy.
#![allow(missing_docs)]
use alloc::vec::Vec;

use crate::Pid;

/// Linux `TASK_COMM_LEN` from include/linux/sched.h.
pub const TASK_COMM_LEN: usize = 16;

/// The raw image Linux keeps in `task_struct::comm`.
///
/// `task_struct::comm` is a plain byte array, not a string: `PR_SET_NAME`
/// copies bytes straight out of userspace and `/proc/<pid>/stat` prints them
/// back verbatim, so a task name is allowed to be non-UTF-8 and must survive a
/// round trip unchanged. The image is always NUL-terminated within
/// `TASK_COMM_LEN` bytes, which is what `strscpy_pad()` in
/// `__set_task_comm()` (kernel/fork.c) guarantees.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskComm {
    bytes: [u8; TASK_COMM_LEN],
}

impl TaskComm {
    /// The empty name, as `strscpy_pad()` produces for a zero-length source.
    pub const EMPTY: Self = Self {
        bytes: [0; TASK_COMM_LEN],
    };

    /// Builds the `comm` image from an already-copied prefix.
    ///
    /// Matches `set_task_comm()` -> `__set_task_comm()` ->
    /// `strscpy_pad(tsk->comm, from, TASK_COMM_LEN)` in kernel/fork.c: at most
    /// `TASK_COMM_LEN - 1` bytes are kept, the copy stops at the first NUL, and
    /// the remainder of the array stays zero.
    pub const fn from_prefix(prefix: &[u8]) -> Self {
        let mut bytes = [0; TASK_COMM_LEN];
        let mut index = 0;
        while index < TASK_COMM_LEN - 1 && index < prefix.len() {
            if prefix[index] == 0 {
                break;
            }
            bytes[index] = prefix[index];
            index += 1;
        }
        Self { bytes }
    }

    /// Wraps an on-the-wire `comm` image read back from a task.
    pub const fn from_raw(bytes: [u8; TASK_COMM_LEN]) -> Self {
        Self { bytes }
    }

    /// The full NUL-padded image, which is what `PR_GET_NAME` copies out.
    pub const fn raw(&self) -> [u8; TASK_COMM_LEN] {
        self.bytes
    }

    /// The bytes before the first NUL, or every byte when the array is full.
    pub fn as_bytes(&self) -> &[u8] {
        let len = self
            .bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(TASK_COMM_LEN);
        &self.bytes[..len]
    }
}

/// `AT_VECTOR_SIZE_BASE` from include/linux/auxvec.h.
pub const AT_VECTOR_SIZE_BASE: usize = 24;

/// `AT_VECTOR_SIZE_ARCH` for x86-64 without `CONFIG_IA32_EMULATION`.
///
/// arch/x86/include/uapi/asm/auxvec.h selects 3 entries for a compat kernel
/// and 2 otherwise; this kernel has no 32-bit personality, so it is 2.
pub const AT_VECTOR_SIZE_ARCH: usize = 2;

/// `AT_VECTOR_SIZE` from include/linux/mm_types.h.
pub const AT_VECTOR_SIZE: usize = 2 * (AT_VECTOR_SIZE_ARCH + AT_VECTOR_SIZE_BASE + 1);

/// `sizeof(mm->saved_auxv)`, the image `PR_GET_AUXV` reports.
pub const SAVED_AUXV_BYTES: usize = AT_VECTOR_SIZE * size_of::<usize>();

/// Serializes an exec-time auxiliary vector the way Linux stores it.
///
/// `create_elf_tables()` writes the pairs straight into
/// `mm_struct::saved_auxv`, a `SAVED_AUXV_BYTES` array that the mm allocator
/// zeroed first, so a vector shorter than the array leaves a zero tail that
/// `PR_GET_AUXV` copies out verbatim. Serializing the same way keeps that tail
/// byte-identical, and an oversized vector is truncated the way the fixed
/// array would be.
pub fn saved_auxv_image(entries: impl IntoIterator<Item = (usize, usize)>) -> Vec<u8> {
    let mut image = Vec::new();
    for (kind, value) in entries {
        if image.len() + 2 * size_of::<usize>() > SAVED_AUXV_BYTES {
            break;
        }
        image.extend_from_slice(&(kind as u64).to_ne_bytes());
        image.extend_from_slice(&(value as u64).to_ne_bytes());
    }
    image
}

/// How many bytes `PR_SET_NAME` copies out of userspace.
///
/// `SYSCALL_DEFINE5(prctl, ...)` in kernel/sys.c clears `comm[TASK_COMM_LEN-1]`
/// and then calls `strncpy_from_user(comm, arg2, TASK_COMM_LEN - 1)`, so the
/// read stops after 15 bytes even when the user string is longer.
pub const fn prctl_set_name_read_bound() -> usize {
    TASK_COMM_LEN - 1
}

/// The bytes `/proc/<pid>/stat` prints between its field-1 parentheses.
///
/// Linux v7.2.3, `fs/proc/array.c`, inside `do_task_stat()`:
///
/// ```c
/// 	seq_puts(m, " (");
/// 	proc_task_name(m, task, false);
/// 	seq_puts(m, ") ");
/// ```
///
/// and `proc_task_name()` with `escape = false`:
///
/// ```c
/// void proc_task_name(struct seq_file *m, struct task_struct *p, bool escape)
/// {
/// 	char tcomm[64];
/// 	...
/// 	else
/// 		get_task_comm(tcomm, p);
///
/// 	if (escape)
/// 		seq_escape_str(m, tcomm, ESCAPE_SPACE | ESCAPE_SPECIAL, "\n\\");
/// 	else
/// 		seq_printf(m, "%.64s", tcomm);
/// }
/// ```
///
/// `get_task_comm()` is `strscpy_pad()` out of `task_struct::comm` and `%.64s`
/// stops at the first NUL byte, so the field is the task's raw byte image up to
/// `TASK_COMM_LEN`. `PR_SET_NAME` copies bytes out of userspace without any
/// UTF-8 validation, so a name is a byte string: converting it to a `str` here
/// would fabricate bytes Linux never produces.
///
/// `comm` may be the NUL-trimmed name (what [`TaskComm::as_bytes`] produces) or
/// the complete `TASK_COMM_LEN` image ([`TaskComm::raw`]); the padding is
/// dropped either way, and the `min(TASK_COMM_LEN)` bound mirrors the fixed
/// size of the image `strscpy_pad()` writes.
pub fn proc_stat_comm_field(comm: &[u8]) -> &[u8] {
    let bounded = &comm[..comm.len().min(TASK_COMM_LEN)];
    let len = bounded
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bounded.len());
    &bounded[..len]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessAbiError {
    InvalidFlags,
    InvalidExitSignal,
    InvalidStack,
    NonzeroTail,
    AddressOverflow,
    InvalidPidfdFlags,
    InvalidSize,
    TooLarge,
    InvalidSetTid,
    PermissionDenied,
    InvalidCgroup,
    InvalidRusageSelector,
}
pub mod clone_flags {
    pub const VM: u64 = 0x100;
    pub const FS: u64 = 0x200;
    pub const FILES: u64 = 0x400;
    pub const SIGHAND: u64 = 0x800;
    pub const PIDFD: u64 = 0x1000;
    pub const PTRACE: u64 = 0x2000;
    pub const VFORK: u64 = 0x4000;
    pub const PARENT: u64 = 0x8000;
    pub const THREAD: u64 = 0x10000;
    pub const PARENT_SETTID: u64 = 0x100000;
    pub const CHILD_CLEARTID: u64 = 0x200000;
    pub const CHILD_SETTID: u64 = 0x1000000;
    pub const NEWCGROUP: u64 = 0x2000000;
    pub const NEWNS: u64 = 0x0002_0000;
    pub const NEWUTS: u64 = 0x4000000;
    pub const NEWIPC: u64 = 0x8000000;
    pub const NEWUSER: u64 = 0x10000000;
    pub const NEWPID: u64 = 0x20000000;
    pub const NEWNET: u64 = 0x40000000;
    pub const IO: u64 = 0x80000000;
    pub const SYSVSEM: u64 = 0x40000;
    pub const SETTLS: u64 = 0x80000;
    pub const UNTRACED: u64 = 0x800000;
    pub const CLEAR_SIGHAND: u64 = 0x1_0000_0000;
    pub const INTO_CGROUP: u64 = 0x2_0000_0000;
    pub const DETACHED: u64 = 0x0040_0000;
    /// `CLONE_NEWTIME`. Occupies bit 7, the only `CSIGNAL` bit that `clone3`
    /// reuses; `clone(2)` cannot express it because the low byte is the exit
    /// signal there.
    pub const NEWTIME: u64 = 0x0000_0080;
    /// `CLONE_AUTOREAP`: the child is reaped as it exits and sends nothing.
    pub const AUTOREAP: u64 = 1 << 34;
    /// `CLONE_NNP`: the child starts with `no_new_privs` set.
    pub const NNP: u64 = 1 << 35;
    /// `CLONE_PIDFD_AUTOKILL`: the child dies with its pidfd.
    pub const PIDFD_AUTOKILL: u64 = 1 << 36;
    /// `CLONE_EMPTY_MNTNS`: the child receives an empty mount namespace.
    pub const EMPTY_MNTNS: u64 = 1 << 37;
    pub const KNOWN: u64 = VM
        | FS
        | FILES
        | SIGHAND
        | PIDFD
        | VFORK
        | THREAD
        | PARENT_SETTID
        | CHILD_CLEARTID
        | CHILD_SETTID
        | PTRACE
        | PARENT
        | NEWCGROUP
        | NEWNS
        | NEWUTS
        | NEWIPC
        | NEWUSER
        | NEWPID
        | NEWNET
        | IO
        | SYSVSEM
        | SETTLS
        | UNTRACED
        | CLEAR_SIGHAND
        | INTO_CGROUP
        | DETACHED
        | NEWTIME
        | AUTOREAP
        | NNP
        | PIDFD_AUTOKILL
        | EMPTY_MNTNS;
}

/// `PTRACE_O_*` option bits and the shared admission helper the two request
/// spellings that accept them must run.
///
/// `include/uapi/linux/ptrace.h` defines the bits, `kernel/ptrace.c`
/// `check_ptrace_options()` decides them, and *both* `ptrace_attach()`
/// (`PTRACE_SEIZE`) and `ptrace_setoptions()` (`PTRACE_SETOPTIONS`) call that
/// one helper -- with the single documented difference that unknown bits are
/// `-EIO` from `SEIZE` and `-EINVAL` from `SETOPTIONS`, which Linux implements
/// by duplicating the mask test at the `SEIZE` call site.
pub mod ptrace_options {
    /// `PTRACE_O_TRACESYSGOOD` (bit 0).
    pub const TRACESYSGOOD: u32 = 1;
    /// `PTRACE_O_EXITKILL`: `(1 << 20)`.
    pub const EXITKILL: u32 = 1 << 20;
    /// `PTRACE_O_SUSPEND_SECCOMP`: `(1 << 21)`.
    pub const SUSPEND_SECCOMP: u32 = 1 << 21;

    /// `PTRACE_O_MASK`:
    ///
    /// ```c
    /// #define PTRACE_O_MASK		(\
    /// 	0x000000ff | PTRACE_O_EXITKILL | PTRACE_O_SUSPEND_SECCOMP)
    /// ```
    ///
    /// `0x000000ff` covers `TRACESYSGOOD`, `TRACEFORK`, `TRACEVFORK`,
    /// `TRACECLONE`, `TRACEEXEC`, `TRACEVFORKDONE`, `TRACEEXIT` and
    /// `TRACESECCOMP`, so the whole word is `0x003000ff`.
    pub const MASK: u32 = 0x0000_00ff | EXITKILL | SUSPEND_SECCOMP;

    /// Why `check_ptrace_options()` refused an option word.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum PtraceOptionReject {
        /// A bit outside [`MASK`].  `SEIZE` reports this as `-EIO` and
        /// `SETOPTIONS` as `-EINVAL`.
        Unknown,
        /// `PTRACE_O_SUSPEND_SECCOMP` on a kernel built without
        /// `CONFIG_CHECKPOINT_RESTORE` or without `CONFIG_SECCOMP`.
        Unsupported,
        /// `PTRACE_O_SUSPEND_SECCOMP` from a tracer that does not hold
        /// `CAP_SYS_ADMIN`, that is already filtered, or that already carries
        /// `PT_SUSPEND_SECCOMP`.
        NotPermitted,
    }

    /// The tracer-side facts `check_ptrace_options()` observes.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct SuspendSeccompAdmission {
        /// `IS_ENABLED(CONFIG_CHECKPOINT_RESTORE) && IS_ENABLED(CONFIG_SECCOMP)`.
        pub configured: bool,
        /// `capable(CAP_SYS_ADMIN)`, i.e. `ns_capable(&init_user_ns, ...)`.
        pub cap_sys_admin: bool,
        /// `seccomp_mode(&current->seccomp) != SECCOMP_MODE_DISABLED` for the
        /// *tracer*.
        pub filtered: bool,
        /// `current->ptrace & PT_SUSPEND_SECCOMP` for the *tracer*.
        pub already_suspended: bool,
    }

    /// Linux `check_ptrace_options()` (`kernel/ptrace.c`):
    ///
    /// ```c
    /// 	if (data & ~(unsigned long)PTRACE_O_MASK)
    /// 		return -EINVAL;
    ///
    /// 	if (unlikely(data & PTRACE_O_SUSPEND_SECCOMP)) {
    /// 		if (!IS_ENABLED(CONFIG_CHECKPOINT_RESTORE) ||
    /// 		    !IS_ENABLED(CONFIG_SECCOMP))
    /// 			return -EINVAL;
    ///
    /// 		if (!capable(CAP_SYS_ADMIN))
    /// 			return -EPERM;
    ///
    /// 		if (seccomp_mode(&current->seccomp) != SECCOMP_MODE_DISABLED ||
    /// 		    current->ptrace & PT_SUSPEND_SECCOMP)
    /// 			return -EPERM;
    /// 	}
    /// 	return 0;
    /// ```
    ///
    /// `current` is the *tracer*: the option only means anything when the
    /// tracer itself runs unfiltered, and `PT_SUSPEND_SECCOMP` is the tracer's
    /// own flag bit.
    pub const fn check(
        data: u32,
        suspend_seccomp: SuspendSeccompAdmission,
    ) -> Result<(), PtraceOptionReject> {
        if data & !MASK != 0 {
            return Err(PtraceOptionReject::Unknown);
        }
        if data & SUSPEND_SECCOMP != 0 {
            if !suspend_seccomp.configured {
                return Err(PtraceOptionReject::Unsupported);
            }
            if !suspend_seccomp.cap_sys_admin {
                return Err(PtraceOptionReject::NotPermitted);
            }
            if suspend_seccomp.filtered || suspend_seccomp.already_suspended {
                return Err(PtraceOptionReject::NotPermitted);
            }
        }
        Ok(())
    }
}

/// Flags `clone3` accepts that `clone(2)` cannot express.
///
/// `clone3_args_valid()` in kernel/fork.c allows `CLONE_LEGACY_FLAGS` plus
/// these four; everything else is an unknown flag and reports `EINVAL`.
pub const CLONE3_ONLY_FLAGS: u64 = clone_flags::CLEAR_SIGHAND
    | clone_flags::INTO_CGROUP
    | clone_flags::AUTOREAP
    | clone_flags::NNP
    | clone_flags::PIDFD_AUTOKILL
    | clone_flags::EMPTY_MNTNS;

/// Rejects the flag and exit-signal combinations `copy_process()` refuses.
///
/// This is the part of `copy_process()` that depends only on the request:
/// the `CLONE_PIDFD_AUTOKILL` capability rule is folded in as
/// `has_cap_sys_admin` so the whole precedence order stays testable.
/// `clone3_args_valid()` has already run, so these checks see admitted flags
/// only. Order matters: Linux evaluates every `CLONE_AUTOREAP` rule before
/// the `CLONE_NNP` and `CLONE_PIDFD_AUTOKILL` ones.
pub const fn clone_flag_admission(
    flags: u64,
    exit_signal: u8,
    caller_autoreap: bool,
    has_cap_sys_admin: bool,
) -> Result<(), ProcessAbiError> {
    use clone_flags::*;
    if flags & AUTOREAP != 0 {
        if flags & THREAD != 0 {
            return Err(ProcessAbiError::InvalidFlags);
        }
        if flags & PARENT != 0 {
            return Err(ProcessAbiError::InvalidFlags);
        }
        if exit_signal != 0 {
            return Err(ProcessAbiError::InvalidExitSignal);
        }
    }
    if flags & PARENT != 0 && caller_autoreap {
        return Err(ProcessAbiError::InvalidFlags);
    }
    if flags & NNP != 0 && flags & THREAD != 0 {
        return Err(ProcessAbiError::InvalidFlags);
    }
    if flags & PIDFD_AUTOKILL != 0 {
        if flags & PIDFD == 0 {
            return Err(ProcessAbiError::InvalidFlags);
        }
        if flags & AUTOREAP == 0 {
            return Err(ProcessAbiError::InvalidFlags);
        }
        if flags & THREAD != 0 {
            return Err(ProcessAbiError::InvalidFlags);
        }
        if flags & NNP == 0 && !has_cap_sys_admin {
            return Err(ProcessAbiError::PermissionDenied);
        }
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClonePlan {
    pub flags: u64,
    pub exit_signal: u8,
    pub stack_top: u64,
    /// Explicit `clone3.stack_size`; zero for the legacy `clone(2)` ABI.
    pub stack_size: u64,
    pub tls: u64,
    pub parent_tid: u64,
    pub child_tid: u64,
    pub pidfd: u64,
}
impl ClonePlan {
    pub const fn from_clone(
        raw_flags: u64,
        stack_top: u64,
        tls: u64,
        parent_tid: u64,
        child_tid: u64,
    ) -> Result<Self, ProcessAbiError> {
        let flags = raw_flags & !255;
        // clone(2) overloads its parent_tid argument as the pidfd destination.
        // Unlike clone3, the two destinations cannot be supplied separately.
        if flags & (clone_flags::PIDFD | clone_flags::PARENT_SETTID)
            == (clone_flags::PIDFD | clone_flags::PARENT_SETTID)
        {
            return Err(ProcessAbiError::InvalidFlags);
        }
        Self::new(
            flags,
            raw_flags as u8,
            stack_top,
            0,
            tls,
            parent_tid,
            child_tid,
            0,
        )
    }
    pub const fn new(
        flags: u64,
        exit_signal: u8,
        stack_top: u64,
        stack_size: u64,
        tls: u64,
        parent_tid: u64,
        child_tid: u64,
        pidfd: u64,
    ) -> Result<Self, ProcessAbiError> {
        if flags & !clone_flags::KNOWN != 0 {
            return Err(ProcessAbiError::InvalidFlags);
        }
        // `kernel_clone()` rewrites the flag word before anything validates it:
        //
        // 	/*
        // 	 * Creating an empty mount namespace implies creating a new mount
        // 	 * namespace.  Set this before copy_process() so that the
        // 	 * CLONE_NEWNS|CLONE_FS mutual exclusion check works correctly.
        // 	 */
        // 	if (clone_flags & CLONE_EMPTY_MNTNS) {
        // 		clone_flags |= CLONE_NEWNS;
        // 		args->flags = clone_flags;
        // 	}
        //
        // (`kernel/fork.c`:2703-2711).  The implication has to be visible to
        // the `CLONE_NEWNS|CLONE_FS` exclusion, which `copy_process()` applies
        // to the rewritten word (`kernel/fork.c`:2011-2012), so it belongs to
        // the plan rather than to the namespace construction site.
        let flags = if flags & clone_flags::EMPTY_MNTNS != 0 {
            flags | clone_flags::NEWNS
        } else {
            flags
        };
        if exit_signal > 64 || exit_signal != 0 && flags & clone_flags::THREAD != 0 {
            return Err(ProcessAbiError::InvalidExitSignal);
        }
        Ok(Self {
            flags,
            exit_signal,
            stack_top,
            stack_size,
            tls,
            parent_tid,
            child_tid,
            pidfd,
        })
    }
}
/// The x86-64 Linux `struct clone_args` wire layout passed to `clone3`.
///
/// Short, supported prefixes are zero-extended before decoding.  A caller
/// must separately copy any extension bytes and pass them to
/// [`Self::normalize`], which rejects non-zero extensions.
#[repr(C)]
#[derive(bytemuck::AnyBitPattern, Clone, Copy, Debug, Default, Eq, PartialEq)]
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

/// Bounded, copied `clone3.set_tid` vector. Values are innermost-to-outermost
/// namespace PIDs, and the embedding kernel supplies namespace authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetTidPlan {
    address: u64,
    count: u8,
}

impl SetTidPlan {
    pub const MAX_ENTRIES: usize = 32;

    pub const fn new(address: u64, count: u64) -> Result<Self, ProcessAbiError> {
        // Linux ignores `set_tid` when its count is zero, so an otherwise
        // unused non-null pointer must not require a valid user mapping.
        if count == 0 {
            return Ok(Self {
                address: 0,
                count: 0,
            });
        }
        if address == 0 || count == 0 || count > Self::MAX_ENTRIES as u64 {
            return Err(ProcessAbiError::InvalidSetTid);
        }
        Ok(Self {
            address,
            count: count as u8,
        })
    }

    pub const fn address(self) -> u64 {
        self.address
    }
    pub const fn count(self) -> usize {
        self.count as usize
    }

    /// Check the copied PID vector before namespace reservation/publication.
    ///
    /// Authorization is intentionally left to the embedding kernel: every
    /// requested element is authorized against a different PID namespace.
    pub fn validate_values(self, values: &[Pid]) -> Result<(), ProcessAbiError> {
        if values.len() != self.count() || values.contains(&0) {
            return Err(ProcessAbiError::InvalidSetTid);
        }
        Ok(())
    }
}

/// Decoded `clone3` policy, with usercopy-dependent requests kept typed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Clone3Plan {
    pub clone: ClonePlan,
    /// Base of the explicit user stack.  The embedding kernel validates this
    /// normalized range against its own virtual-address layout.
    pub stack_base: u64,
    pub set_tid: SetTidPlan,
    pub cgroup_fd: Option<i32>,
}
impl Clone3Args {
    pub const MIN_SIZE: usize = 64;
    pub const KNOWN_SIZE: usize = core::mem::size_of::<Self>();
    pub const MAX_SIZE: usize = 4096;

    /// Returns the number of wire bytes an embedding kernel must copy before
    /// decoding the known `clone_args` fields.
    pub const fn known_prefix_size(supplied_size: usize) -> Result<usize, ProcessAbiError> {
        if supplied_size < Self::MIN_SIZE {
            return Err(ProcessAbiError::InvalidSize);
        }
        if supplied_size > Self::MAX_SIZE {
            return Err(ProcessAbiError::TooLarge);
        }
        Ok(if supplied_size < Self::KNOWN_SIZE {
            supplied_size
        } else {
            Self::KNOWN_SIZE
        })
    }

    /// Decodes a checked, zero-extended known wire prefix.
    pub fn decode_prefix(supplied_size: usize, prefix: &[u8]) -> Result<Self, ProcessAbiError> {
        let known_size = Self::known_prefix_size(supplied_size)?;
        if prefix.len() != known_size {
            return Err(ProcessAbiError::InvalidSize);
        }
        let mut bytes = [0_u8; Self::KNOWN_SIZE];
        bytes[..known_size].copy_from_slice(prefix);
        bytemuck::try_pod_read_unaligned(&bytes).map_err(|_| ProcessAbiError::InvalidSize)
    }

    /// Checks extension bytes beyond [`Self::KNOWN_SIZE`].
    pub fn validate_tail(tail: &[u8]) -> Result<(), ProcessAbiError> {
        if tail.iter().any(|byte| *byte != 0) {
            Err(ProcessAbiError::NonzeroTail)
        } else {
            Ok(())
        }
    }

    pub fn normalize(
        self,
        supplied_size: usize,
        tail: &[u8],
    ) -> Result<Clone3Plan, ProcessAbiError> {
        Self::known_prefix_size(supplied_size)?;
        let expected_tail_size = supplied_size.saturating_sub(Self::KNOWN_SIZE);
        if tail.len() != expected_tail_size {
            return Err(ProcessAbiError::InvalidSize);
        }
        Self::validate_tail(tail)?;
        if (self.stack == 0) != (self.stack_size == 0) {
            return Err(ProcessAbiError::InvalidStack);
        }
        if self.exit_signal > 255 {
            return Err(ProcessAbiError::InvalidExitSignal);
        }
        if self.exit_signal != 0 && self.flags & (clone_flags::THREAD | clone_flags::PARENT) != 0 {
            return Err(ProcessAbiError::InvalidExitSignal);
        }
        let top = self
            .stack
            .checked_add(self.stack_size)
            .ok_or(ProcessAbiError::AddressOverflow)?;
        let clone = ClonePlan::new(
            self.flags,
            self.exit_signal as u8,
            top,
            self.stack_size,
            self.tls,
            self.parent_tid,
            self.child_tid,
            self.pidfd,
        )?;
        if self.flags & (clone_flags::PIDFD | clone_flags::PARENT_SETTID)
            == (clone_flags::PIDFD | clone_flags::PARENT_SETTID)
            && self.pidfd == self.parent_tid
        {
            return Err(ProcessAbiError::InvalidFlags);
        }
        let set_tid = SetTidPlan::new(self.set_tid, self.set_tid_size)?;
        let cgroup_fd = if self.flags & clone_flags::INTO_CGROUP != 0 {
            if supplied_size < Self::KNOWN_SIZE || self.cgroup > i32::MAX as u64 {
                return Err(ProcessAbiError::InvalidCgroup);
            }
            Some(self.cgroup as i32)
        } else {
            None
        };
        Ok(Clone3Plan {
            clone,
            stack_base: self.stack,
            set_tid,
            cgroup_fd,
        })
    }
}

/// Valid x86-64 Linux `getrusage` selectors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RusageSelector {
    SelfUsage,
    Children,
    Thread,
}
impl RusageSelector {
    pub const fn decode(value: i32) -> Result<Self, ProcessAbiError> {
        match value {
            0 => Ok(Self::SelfUsage),
            -1 => Ok(Self::Children),
            1 => Ok(Self::Thread),
            _ => Err(ProcessAbiError::InvalidRusageSelector),
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TimeVal {
    pub seconds: i64,
    pub microseconds: i64,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UsageSnapshot {
    pub user_ns: u64,
    pub system_ns: u64,
    pub max_rss_bytes: u64,
    pub minor_faults: u64,
    pub major_faults: u64,
    pub inblock: u64,
    pub oublock: u64,
    pub voluntary_switches: u64,
    pub involuntary_switches: u64,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rusage {
    pub utime: TimeVal,
    pub stime: TimeVal,
    pub maxrss_kib: i64,
    pub ixrss: i64,
    pub idrss: i64,
    pub isrss: i64,
    pub minflt: i64,
    pub majflt: i64,
    pub nswap: i64,
    pub inblock: i64,
    pub oublock: i64,
    pub msgsnd: i64,
    pub msgrcv: i64,
    pub nsignals: i64,
    pub nvcsw: i64,
    pub nivcsw: i64,
}
impl UsageSnapshot {
    pub fn project(self) -> Rusage {
        Rusage {
            utime: TimeVal {
                seconds: (self.user_ns / 1_000_000_000) as i64,
                microseconds: ((self.user_ns % 1_000_000_000) / 1000) as i64,
            },
            stime: TimeVal {
                seconds: (self.system_ns / 1_000_000_000) as i64,
                microseconds: ((self.system_ns % 1_000_000_000) / 1000) as i64,
            },
            maxrss_kib: (self.max_rss_bytes / 1024).min(i64::MAX as u64) as i64,
            ixrss: 0,
            idrss: 0,
            isrss: 0,
            minflt: self.minor_faults.min(i64::MAX as u64) as i64,
            majflt: self.major_faults.min(i64::MAX as u64) as i64,
            nswap: 0,
            inblock: self.inblock.min(i64::MAX as u64) as i64,
            oublock: self.oublock.min(i64::MAX as u64) as i64,
            msgsnd: 0,
            msgrcv: 0,
            nsignals: 0,
            nvcsw: self.voluntary_switches.min(i64::MAX as u64) as i64,
            nivcsw: self.involuntary_switches.min(i64::MAX as u64) as i64,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PidfdPlan {
    pub target: Pid,
    pub thread: bool,
    pub nonblocking: bool,
}
impl PidfdPlan {
    pub const NONBLOCK: u32 = 2048;
    pub const THREAD: u32 = 128;
    pub const FLAGS: u32 = Self::NONBLOCK | Self::THREAD;

    pub const fn open(target: Pid, flags: u32) -> Result<Self, ProcessAbiError> {
        if flags & !Self::FLAGS != 0 {
            Err(ProcessAbiError::InvalidPidfdFlags)
        } else {
            Ok(Self {
                target,
                thread: flags & Self::THREAD != 0,
                nonblocking: flags & Self::NONBLOCK != 0,
            })
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ptrace_option_mask_keeps_the_two_eventless_bits() {
        use ptrace_options::*;
        // include/uapi/linux/ptrace.h:
        // 	#define PTRACE_O_EXITKILL		(1 << 20)
        // 	#define PTRACE_O_SUSPEND_SECCOMP	(1 << 21)
        // 	#define PTRACE_O_MASK			(\
        // 		0x000000ff | PTRACE_O_EXITKILL | PTRACE_O_SUSPEND_SECCOMP)
        assert_eq!(EXITKILL, 1 << 20);
        assert_eq!(SUSPEND_SECCOMP, 1 << 21);
        assert_eq!(MASK, 0x0030_00ff);
        // The seven event options and TRACESYSGOOD live in the low byte; the
        // six bits the pre-fix kernel admitted (16..19, 22..23) are not in the
        // mask at all.
        assert_eq!(TRACESYSGOOD, 1);
        for bogus in [1 << 16, 1 << 17, 1 << 18, 1 << 19, 1 << 22, 1 << 23] {
            assert_eq!(MASK & bogus, 0, "0x{bogus:x} is not an option");
        }
        for option in [
            TRACESYSGOOD,
            EXITKILL,
            SUSPEND_SECCOMP,
            1 << 1,
            1 << 2,
            1 << 3,
            1 << 4,
            1 << 5,
            1 << 6,
            1 << 7,
        ] {
            assert_eq!(MASK & option, option, "0x{option:x} must be admitted");
        }
    }

    #[test]
    fn ptrace_option_check_follows_check_ptrace_options() {
        use ptrace_options::*;
        let declared = SuspendSeccompAdmission {
            configured: true,
            cap_sys_admin: true,
            filtered: false,
            already_suspended: false,
        };
        // Unknown bits are the first decision, ahead of SUSPEND_SECCOMP.
        assert_eq!(
            check(SUSPEND_SECCOMP | (1 << 16), declared),
            Err(PtraceOptionReject::Unknown)
        );
        assert_eq!(check(0, declared), Ok(()));
        assert_eq!(check(EXITKILL, declared), Ok(()));
        // PTRACE_O_EXITKILL needs no capability at all.
        assert_eq!(
            check(
                EXITKILL,
                SuspendSeccompAdmission {
                    configured: false,
                    cap_sys_admin: false,
                    ..declared
                }
            ),
            Ok(())
        );
        // Without CONFIG_CHECKPOINT_RESTORE (or CONFIG_SECCOMP) the bit is
        // -EINVAL for every caller, capability or not.
        let unconfigured = SuspendSeccompAdmission {
            configured: false,
            ..declared
        };
        assert_eq!(
            check(SUSPEND_SECCOMP, unconfigured),
            Err(PtraceOptionReject::Unsupported)
        );
        assert_eq!(
            check(
                SUSPEND_SECCOMP,
                SuspendSeccompAdmission {
                    cap_sys_admin: false,
                    ..unconfigured
                }
            ),
            Err(PtraceOptionReject::Unsupported)
        );
        // `capable(CAP_SYS_ADMIN)` precedes the tracer's own seccomp state.
        assert_eq!(
            check(
                SUSPEND_SECCOMP,
                SuspendSeccompAdmission {
                    cap_sys_admin: false,
                    ..declared
                }
            ),
            Err(PtraceOptionReject::NotPermitted)
        );
        // A filtered tracer, or one that already suspended its filters, is
        // -EPERM.
        assert_eq!(
            check(
                SUSPEND_SECCOMP,
                SuspendSeccompAdmission {
                    filtered: true,
                    ..declared
                }
            ),
            Err(PtraceOptionReject::NotPermitted)
        );
        assert_eq!(
            check(
                SUSPEND_SECCOMP,
                SuspendSeccompAdmission {
                    already_suspended: true,
                    ..declared
                }
            ),
            Err(PtraceOptionReject::NotPermitted)
        );
        assert_eq!(check(SUSPEND_SECCOMP, declared), Ok(()));
    }

    #[test]
    fn prctl_set_name_truncates_at_fifteen_bytes_and_keeps_raw_bytes() {
        assert_eq!(prctl_set_name_read_bound(), 15);

        // strscpy_pad() keeps at most TASK_COMM_LEN - 1 bytes and zero-fills
        // the rest, so a 15-byte name leaves the terminator in the last slot.
        let fifteen = b"abcdefghijklmno";
        let comm = TaskComm::from_prefix(fifteen);
        assert_eq!(comm.as_bytes(), fifteen);
        assert_eq!(comm.raw()[15], 0);
        assert_eq!(&comm.raw()[..15], fifteen);

        // A longer user string is truncated, not rejected.
        let comm = TaskComm::from_prefix(b"abcdefghijklmnopqrstuvwxyz");
        assert_eq!(comm.as_bytes(), fifteen);
        assert_eq!(comm.raw()[15], 0);

        // Non-UTF-8 bytes survive verbatim; Linux never validates the name.
        let raw = [0xff, 0xfe, 0x80, b'x'];
        let comm = TaskComm::from_prefix(&raw);
        assert_eq!(comm.as_bytes(), &raw);
        assert_eq!(&comm.raw()[..4], &raw);
        assert_eq!(&comm.raw()[4..], &[0; 12]);

        // The image always terminates inside the array, even when the source
        // is exactly TASK_COMM_LEN bytes long.
        let comm = TaskComm::from_prefix(&[b'z'; TASK_COMM_LEN]);
        assert_eq!(comm.as_bytes().len(), TASK_COMM_LEN - 1);
        assert_eq!(comm.raw()[TASK_COMM_LEN - 1], 0);

        assert_eq!(TaskComm::EMPTY.as_bytes(), b"");
        assert_eq!(TaskComm::EMPTY.raw(), [0; TASK_COMM_LEN]);
        assert_eq!(TaskComm::from_raw(comm.raw()), comm);
    }

    #[test]
    fn clone3_only_flags_are_admitted_but_keep_their_rules() {
        use clone_flags::*;

        // CLONE_NEWTIME is bit 7, which `clone(2)` spends on the exit signal.
        assert_eq!(NEWTIME, 0x80);
        // Every flag clone3 adds beyond the legacy mask is listed.
        for flag in [
            CLEAR_SIGHAND,
            INTO_CGROUP,
            AUTOREAP,
            NNP,
            PIDFD_AUTOKILL,
            EMPTY_MNTNS,
        ] {
            assert_eq!(CLONE3_ONLY_FLAGS & flag, flag);
            assert_eq!(KNOWN & flag, flag);
            assert_eq!(flag & !255, flag);
        }

        // An admitted flag alone is fine; the combination rules are what bite.
        assert_eq!(clone_flag_admission(NEWTIME, 0, false, false), Ok(()));
        assert_eq!(clone_flag_admission(NNP, 0, false, false), Ok(()));
        assert_eq!(clone_flag_admission(AUTOREAP, 0, false, false), Ok(()));
        assert_eq!(
            clone_flag_admission(EMPTY_MNTNS | NEWNS, 0, false, false),
            Ok(())
        );

        // An empty mount namespace is a new mount namespace, and the plan is
        // where `kernel_clone()` applies that implication -- before
        // `copy_process()` can reject CLONE_NEWNS|CLONE_FS
        // (`kernel/fork.c`:2011-2012, 2703-2711).
        for flags in [EMPTY_MNTNS, EMPTY_MNTNS | FS] {
            let plan = ClonePlan::new(flags, 0, 0, 0, 0, 0, 0, 0).unwrap();
            assert_eq!(plan.flags & EMPTY_MNTNS, EMPTY_MNTNS);
            assert_eq!(plan.flags & NEWNS, NEWNS);
        }
        assert_eq!(
            ClonePlan::new(NEWNS, 0, 0, 0, 0, 0, 0, 0).unwrap().flags & EMPTY_MNTNS,
            0
        );

        // CLONE_AUTOREAP owns a thread group's reaping, so it cannot ask for a
        // thread, a reparented child, or an exit signal.
        for flags in [AUTOREAP | THREAD, AUTOREAP | PARENT] {
            assert_eq!(
                clone_flag_admission(flags, 0, false, false),
                Err(ProcessAbiError::InvalidFlags)
            );
        }
        assert_eq!(
            clone_flag_admission(AUTOREAP, 17, false, false),
            Err(ProcessAbiError::InvalidExitSignal)
        );
        // A process that is itself auto-reaped cannot reparent to its parent.
        assert_eq!(
            clone_flag_admission(PARENT, 0, true, true),
            Err(ProcessAbiError::InvalidFlags)
        );

        assert_eq!(
            clone_flag_admission(NNP | THREAD, 0, false, false),
            Err(ProcessAbiError::InvalidFlags)
        );

        // CLONE_PIDFD_AUTOKILL needs a pidfd and an auto-reaped child, and
        // without CLONE_NNP it needs CAP_SYS_ADMIN rather than EINVAL.
        let autokill = PIDFD_AUTOKILL | PIDFD | AUTOREAP;
        assert_eq!(
            clone_flag_admission(PIDFD_AUTOKILL, 0, false, true),
            Err(ProcessAbiError::InvalidFlags)
        );
        assert_eq!(
            clone_flag_admission(PIDFD_AUTOKILL | PIDFD, 0, false, true),
            Err(ProcessAbiError::InvalidFlags)
        );
        assert_eq!(
            clone_flag_admission(autokill | THREAD, 0, false, true),
            Err(ProcessAbiError::InvalidFlags)
        );
        assert_eq!(
            clone_flag_admission(autokill, 0, false, false),
            Err(ProcessAbiError::PermissionDenied)
        );
        assert_eq!(clone_flag_admission(autokill, 0, false, true), Ok(()));
        assert_eq!(
            clone_flag_admission(autokill | NNP, 0, false, false),
            Ok(())
        );
    }

    #[test]
    fn saved_auxv_is_a_fixed_size_image_with_a_zero_tail() {
        // x86-64 without IA32 emulation: 2 * (2 + 24 + 1) entries of 8 bytes.
        assert_eq!(AT_VECTOR_SIZE, 54);
        assert_eq!(SAVED_AUXV_BYTES, 432);

        let image = saved_auxv_image([(6, 4096), (9, 0x401000), (0, 0)]);
        assert_eq!(image.len(), 48);
        assert_eq!(u64::from_ne_bytes(image[0..8].try_into().unwrap()), 6);
        assert_eq!(u64::from_ne_bytes(image[8..16].try_into().unwrap()), 4096);
        // Everything past the serialized entries reads back as zero, which is
        // what an mm-allocated `saved_auxv` array contains.
        assert!(image.len() < SAVED_AUXV_BYTES);

        // A vector that would not fit is truncated at the array bound.
        let oversized = saved_auxv_image((0..100).map(|index| (index, index)));
        assert_eq!(oversized.len(), SAVED_AUXV_BYTES);
    }

    #[test]
    fn edges() {
        assert_eq!(
            Clone3Args {
                stack: 1,
                ..Default::default()
            }
            .normalize(64, &[]),
            Err(ProcessAbiError::InvalidStack)
        );
        assert_eq!(
            Clone3Args::default().normalize(Clone3Args::KNOWN_SIZE + 1, &[1]),
            Err(ProcessAbiError::NonzeroTail)
        );
        assert_eq!(
            UsageSnapshot {
                max_rss_bytes: 2048,
                ..Default::default()
            }
            .project()
            .maxrss_kib,
            2
        );
    }

    #[test]
    fn clone3_size_is_checked_before_tail_and_tail_before_fields() {
        let invalid_stack = Clone3Args {
            stack: 1,
            ..Default::default()
        };
        assert_eq!(
            invalid_stack.normalize(Clone3Args::MIN_SIZE - 1, &[1]),
            Err(ProcessAbiError::InvalidSize)
        );
        assert_eq!(
            invalid_stack.normalize(Clone3Args::KNOWN_SIZE + 1, &[1]),
            Err(ProcessAbiError::NonzeroTail)
        );
        assert_eq!(
            Clone3Args::default().normalize(Clone3Args::MAX_SIZE + 1, &[]),
            Err(ProcessAbiError::TooLarge)
        );
        assert_eq!(
            Clone3Args::default().normalize(Clone3Args::KNOWN_SIZE + 1, &[]),
            Err(ProcessAbiError::InvalidSize)
        );
    }

    #[test]
    fn clone3_rejects_exit_signal_for_thread_or_parent() {
        for flag in [clone_flags::THREAD, clone_flags::PARENT] {
            assert_eq!(
                Clone3Args {
                    flags: flag,
                    exit_signal: 1,
                    ..Default::default()
                }
                .normalize(Clone3Args::KNOWN_SIZE, &[]),
                Err(ProcessAbiError::InvalidExitSignal)
            );
        }
    }

    #[test]
    fn clone_normalizes_legacy_flags_and_rejects_its_output_slot_collision() {
        let plan =
            ClonePlan::from_clone(clone_flags::VM | 17, 0x4000, 0x5000, 0x6000, 0x7000).unwrap();
        assert_eq!(plan.flags, clone_flags::VM);
        assert_eq!(plan.exit_signal, 17);
        assert_eq!(plan.stack_top, 0x4000);
        assert_eq!(plan.tls, 0x5000);
        assert_eq!(plan.parent_tid, 0x6000);
        assert_eq!(plan.child_tid, 0x7000);
        assert_eq!(
            ClonePlan::from_clone(clone_flags::PIDFD | clone_flags::PARENT_SETTID, 0, 0, 0, 0,),
            Err(ProcessAbiError::InvalidFlags)
        );
    }

    #[test]
    fn clone3_decodes_short_prefix_by_zero_extending_the_wire_layout() {
        let mut prefix = [0_u8; Clone3Args::MIN_SIZE];
        prefix[..8].copy_from_slice(&clone_flags::VM.to_ne_bytes());
        let decoded = Clone3Args::decode_prefix(Clone3Args::MIN_SIZE, &prefix).unwrap();
        assert_eq!(decoded.flags, clone_flags::VM);
        assert_eq!(decoded.set_tid, 0);
        assert_eq!(decoded.cgroup, 0);
        assert_eq!(
            Clone3Args::decode_prefix(Clone3Args::MIN_SIZE, &prefix[..63]),
            Err(ProcessAbiError::InvalidSize)
        );
    }

    #[test]
    fn clone3_set_tid_requires_a_complete_nonzero_vector() {
        let args = Clone3Args {
            set_tid: 0x1000,
            set_tid_size: 2,
            ..Default::default()
        };
        let plan = args.normalize(Clone3Args::KNOWN_SIZE, &[]).unwrap();
        assert_eq!(
            plan.set_tid.validate_values(&[7]),
            Err(ProcessAbiError::InvalidSetTid)
        );
        assert_eq!(
            plan.set_tid.validate_values(&[7, 0]),
            Err(ProcessAbiError::InvalidSetTid)
        );
        assert_eq!(plan.set_tid.validate_values(&[7, 8]), Ok(()));
    }

    #[test]
    fn clone3_ignores_set_tid_pointer_for_a_zero_length_request() {
        let plan = Clone3Args {
            set_tid: 0xdead_beef,
            set_tid_size: 0,
            ..Default::default()
        }
        .normalize(Clone3Args::KNOWN_SIZE, &[])
        .unwrap();
        assert_eq!(plan.set_tid.count(), 0);
        assert_eq!(plan.set_tid.address(), 0);
        assert_eq!(plan.set_tid.validate_values(&[]), Ok(()));
    }

    #[test]
    fn stat_comm_field_keeps_raw_bytes_and_never_grows() {
        // `PR_SET_NAME` copies at most `TASK_COMM_LEN - 1` bytes without
        // validating them, so a fifteen-byte name whose last byte starts a
        // UTF-8 sequence is a legal `comm`. `proc_task_name()` prints the image
        // through `%.64s`, which cannot change its length: the field is the
        // fifteen bytes that were stored, not fourteen plus U+FFFD.
        let partial = b"aaaaaaaaaaaaaa\xc3";
        assert_eq!(proc_stat_comm_field(partial), partial);
        assert_eq!(proc_stat_comm_field(partial).len(), TASK_COMM_LEN - 1);
        // A two-byte prefix of a three-byte sequence is the other shape of the
        // same rule.
        let truncated = b"aaaaaaaaaaaaa\xe2\x82";
        assert_eq!(proc_stat_comm_field(truncated), truncated);
        // The complete image is bounded at `TASK_COMM_LEN` and stops at the NUL
        // `strscpy_pad()` leaves behind, so the padding is not printed.
        let mut image = [0u8; TASK_COMM_LEN + 4];
        let stored = b"abc\x80\xffdefghijkl";
        image[..stored.len()].copy_from_slice(stored);
        assert_eq!(proc_stat_comm_field(&image), stored);
        // An image without a NUL is truncated to the field size, never copied
        // whole.
        let full = [b'x'; TASK_COMM_LEN + 1];
        assert_eq!(proc_stat_comm_field(&full).len(), TASK_COMM_LEN);
        // An empty name prints nothing.
        assert_eq!(proc_stat_comm_field(&[0; TASK_COMM_LEN]), b"");
    }
}
