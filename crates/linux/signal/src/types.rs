use core::{fmt, mem};

use derive_more::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not};
use linux_raw_sys::{
    ctypes::c_void,
    general::{
        SI_KERNEL, SI_SIGIO, SI_TIMER, SS_DISABLE, SS_ONSTACK, SYS_SECCOMP, kernel_sigset_t,
        siginfo_t, sigval_t,
    },
};
use strum::{EnumIter, FromRepr, IntoEnumIterator};

use crate::DefaultSignalAction;

/// Signal number.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, FromRepr, EnumIter)]
pub enum Signo {
    SIGHUP    = 1,
    SIGINT    = 2,
    SIGQUIT   = 3,
    SIGILL    = 4,
    SIGTRAP   = 5,
    SIGABRT   = 6,
    SIGBUS    = 7,
    SIGFPE    = 8,
    SIGKILL   = 9,
    SIGUSR1   = 10,
    SIGSEGV   = 11,
    SIGUSR2   = 12,
    SIGPIPE   = 13,
    SIGALRM   = 14,
    SIGTERM   = 15,
    SIGSTKFLT = 16,
    SIGCHLD   = 17,
    SIGCONT   = 18,
    SIGSTOP   = 19,
    SIGTSTP   = 20,
    SIGTTIN   = 21,
    SIGTTOU   = 22,
    SIGURG    = 23,
    SIGXCPU   = 24,
    SIGXFSZ   = 25,
    SIGVTALRM = 26,
    SIGPROF   = 27,
    SIGWINCH  = 28,
    SIGIO     = 29,
    SIGPWR    = 30,
    SIGSYS    = 31,
    SIGRTMIN  = 32,
    SIGRT1    = 33,
    SIGRT2    = 34,
    SIGRT3    = 35,
    SIGRT4    = 36,
    SIGRT5    = 37,
    SIGRT6    = 38,
    SIGRT7    = 39,
    SIGRT8    = 40,
    SIGRT9    = 41,
    SIGRT10   = 42,
    SIGRT11   = 43,
    SIGRT12   = 44,
    SIGRT13   = 45,
    SIGRT14   = 46,
    SIGRT15   = 47,
    SIGRT16   = 48,
    SIGRT17   = 49,
    SIGRT18   = 50,
    SIGRT19   = 51,
    SIGRT20   = 52,
    SIGRT21   = 53,
    SIGRT22   = 54,
    SIGRT23   = 55,
    SIGRT24   = 56,
    SIGRT25   = 57,
    SIGRT26   = 58,
    SIGRT27   = 59,
    SIGRT28   = 60,
    SIGRT29   = 61,
    SIGRT30   = 62,
    SIGRT31   = 63,
    SIGRT32   = 64,
}

impl Signo {
    pub fn is_realtime(&self) -> bool {
        *self >= Signo::SIGRTMIN
    }

    pub fn default_action(&self) -> DefaultSignalAction {
        match self {
            Signo::SIGHUP => DefaultSignalAction::Terminate,
            Signo::SIGINT => DefaultSignalAction::Terminate,
            Signo::SIGQUIT => DefaultSignalAction::CoreDump,
            Signo::SIGILL => DefaultSignalAction::CoreDump,
            Signo::SIGTRAP => DefaultSignalAction::CoreDump,
            Signo::SIGABRT => DefaultSignalAction::CoreDump,
            Signo::SIGBUS => DefaultSignalAction::CoreDump,
            Signo::SIGFPE => DefaultSignalAction::CoreDump,
            Signo::SIGKILL => DefaultSignalAction::Terminate,
            Signo::SIGUSR1 => DefaultSignalAction::Terminate,
            Signo::SIGSEGV => DefaultSignalAction::CoreDump,
            Signo::SIGUSR2 => DefaultSignalAction::Terminate,
            Signo::SIGPIPE => DefaultSignalAction::Terminate,
            Signo::SIGALRM => DefaultSignalAction::Terminate,
            Signo::SIGTERM => DefaultSignalAction::Terminate,
            Signo::SIGSTKFLT => DefaultSignalAction::Terminate,
            Signo::SIGCHLD => DefaultSignalAction::Ignore,
            Signo::SIGCONT => DefaultSignalAction::Continue,
            Signo::SIGSTOP => DefaultSignalAction::Stop,
            Signo::SIGTSTP => DefaultSignalAction::Stop,
            Signo::SIGTTIN => DefaultSignalAction::Stop,
            Signo::SIGTTOU => DefaultSignalAction::Stop,
            Signo::SIGURG => DefaultSignalAction::Ignore,
            Signo::SIGXCPU => DefaultSignalAction::CoreDump,
            Signo::SIGXFSZ => DefaultSignalAction::CoreDump,
            Signo::SIGVTALRM => DefaultSignalAction::Terminate,
            Signo::SIGPROF => DefaultSignalAction::Terminate,
            Signo::SIGWINCH => DefaultSignalAction::Ignore,
            Signo::SIGIO => DefaultSignalAction::Terminate,
            Signo::SIGPWR => DefaultSignalAction::Terminate,
            Signo::SIGSYS => DefaultSignalAction::CoreDump,
            // POSIX real-time signals default to process termination.
            _ => DefaultSignalAction::Terminate,
        }
    }
}

/// Signal set. Compatible with `struct sigset_t` in libc.
#[derive(Default, Clone, Copy, Not, BitOr, BitOrAssign, BitAnd, BitAndAssign)]
#[repr(transparent)]
pub struct SignalSet(u64);

impl SignalSet {
    /// Returns the native x86_64 bit representation of this signal set.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Builds a signal set from its native x86_64 bit representation.
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    fn signo_bit(signo: Signo) -> u64 {
        1 << (signo as u8 - 1)
    }

    /// Adds a signal to the set.
    pub fn add(&mut self, signal: Signo) -> bool {
        let bit = Self::signo_bit(signal);
        if self.0 & bit != 0 {
            return false;
        }
        self.0 |= bit;
        true
    }

    /// Removes a signal from the set.
    pub fn remove(&mut self, signal: Signo) -> bool {
        let bit = Self::signo_bit(signal);
        if self.0 & bit == 0 {
            return false;
        }
        self.0 &= !bit;
        true
    }

    /// Checks if the set contains a signal.
    pub fn has(&self, signal: Signo) -> bool {
        (self.0 & Self::signo_bit(signal)) != 0
    }

    /// Returns `true` if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// Dequeues the a signal in `mask` from this set, if any.
    pub fn dequeue(&mut self, mask: &SignalSet) -> Option<Signo> {
        let bits = self.0 & mask.0;
        if bits == 0 {
            None
        } else {
            let signal = bits.trailing_zeros();
            self.0 &= !(1 << signal);
            Signo::from_repr((signal + 1) as u8)
        }
    }
}

impl From<SignalSet> for kernel_sigset_t {
    fn from(value: SignalSet) -> Self {
        // SAFETY: `kernel_sigset_t` always has the same layout as `[c_ulong; 1]`.
        unsafe { mem::transmute::<u64, kernel_sigset_t>(value.0) }
    }
}

impl From<kernel_sigset_t> for SignalSet {
    fn from(value: kernel_sigset_t) -> Self {
        // SAFETY: `kernel_sigset_t` always has the same layout as `[c_ulong; 1]`.
        Self(unsafe { mem::transmute::<kernel_sigset_t, u64>(value) })
    }
}

impl fmt::Debug for SignalSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_set();
        for signo in Signo::iter() {
            if self.has(signo) {
                debug.entry(&signo);
            }
        }
        debug.finish()
    }
}

/// The payload carried by an `SI_TIMER` signal.
///
/// `value` is the bit-preserving representation of Linux's `sigval_t`.  It
/// can be interpreted as either the low 32-bit `sival_int` value or the full
/// x86_64 `sival_ptr` value by the eventual ABI consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalTimerPayload {
    pub tid: i32,
    pub overrun: i32,
    pub value: usize,
    pub sys_private: i32,
}

impl SignalTimerPayload {
    pub const fn new(tid: i32, overrun: i32, value: usize, sys_private: i32) -> Self {
        Self {
            tid,
            overrun,
            value,
            sys_private,
        }
    }
}

/// The payload carried by an `SI_QUEUE` or `SI_MESGQ` signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalRtPayload {
    pub pid: i32,
    pub uid: u32,
    pub value: usize,
}

impl SignalRtPayload {
    pub const fn new(pid: i32, uid: u32, value: usize) -> Self {
        Self { pid, uid, value }
    }
}

/// The payload carried by an `SI_SIGIO` signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalPollPayload {
    pub band: i64,
    pub fd: i32,
}

impl SignalPollPayload {
    pub const fn new(band: i64, fd: i32) -> Self {
        Self { band, fd }
    }
}

/// Signal information. Compatible with `struct siginfo` in libc.
#[derive(Clone)]
#[repr(C, align(8))]
pub struct SignalInfo([u8; 128]);

impl SignalInfo {
    /// Copies a fully initialized Linux `siginfo_t` record into the canonical
    /// signal-information storage without relying on nominal type layout
    /// compatibility at a consumer boundary.
    pub fn from_raw(raw: siginfo_t) -> Self {
        let mut bytes = [0u8; mem::size_of::<siginfo_t>()];
        // SAFETY: `raw` is an owned, fully initialized `siginfo_t`; copying its
        // exact object representation into the equally-sized byte storage does
        // not create a reference or interpret any union arm.
        unsafe {
            core::ptr::copy_nonoverlapping(
                (&raw as *const siginfo_t).cast::<u8>(),
                bytes.as_mut_ptr(),
                bytes.len(),
            );
        }
        Self(bytes)
    }

    pub fn new_kernel(signo: Signo) -> Self {
        let mut result = Self([0; 128]);
        result.set_signo(signo);
        result.set_code(SI_KERNEL as _);
        result
    }

    pub fn new_user(signo: Signo, code: i32, pid: u32, uid: u32) -> Self {
        let mut result = Self([0; 128]);
        result.set_signo(signo);
        result.set_code(code);
        result.set_pid(pid);
        result.set_uid(uid);
        result
    }

    /// Builds a synchronous hardware-fault record with its exact user
    /// address.
    pub fn new_fault(signo: Signo, code: i32, address: usize) -> Self {
        let mut result = Self([0; 128]);
        result.set_signo(signo);
        result.set_code(code);
        result
            .raw_mut()
            .__bindgen_anon_1
            .__bindgen_anon_1
            ._sifields
            ._sigfault
            ._addr = address as *mut c_void;
        result
    }

    /// Builds the `SIGSYS` record Linux exposes for a seccomp trap.
    pub fn new_sigsys(errno: i32, call_address: usize, syscall: i32, arch: u32) -> Self {
        let mut result = Self([0; 128]);
        result.set_signo(Signo::SIGSYS);
        result.raw_mut().__bindgen_anon_1.__bindgen_anon_1.si_errno = errno;
        result.set_code(SYS_SECCOMP as _);
        // SAFETY: `_sigsys` is the initialized payload selected by the
        // constructor's `SYS_SECCOMP` code. The writes stay behind the private
        // raw view and cannot expose mutable ABI storage to callers.
        unsafe {
            let sigsys = &mut result
                .raw_mut()
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._sigsys;
            sigsys._call_addr = call_address as *mut c_void;
            sigsys._syscall = syscall;
            sigsys._arch = arch;
        }
        result
    }

    /// Builds an `SI_TIMER` signal with its complete timer payload.
    pub fn new_timer(signo: Signo, payload: SignalTimerPayload) -> Self {
        let mut result = Self::new_kernel(signo);
        result.set_code(SI_TIMER);
        result.set_timer_payload(payload);
        result
    }

    /// Builds an `SI_QUEUE` or `SI_MESGQ` signal with its complete realtime
    /// payload. The caller selects the Linux code because both variants share
    /// this ABI payload.
    pub fn new_rt(signo: Signo, code: i32, payload: SignalRtPayload) -> Self {
        let mut result = Self::new_kernel(signo);
        result.set_code(code);
        result.set_rt_payload(payload);
        result
    }

    /// Builds an `SI_SIGIO` signal with its complete poll payload.
    pub fn new_poll(signo: Signo, payload: SignalPollPayload) -> Self {
        let mut result = Self::new_kernel(signo);
        result.set_code(SI_SIGIO);
        result.set_poll_payload(payload);
        result
    }

    pub fn signo(&self) -> Signo {
        self.try_signo()
            .expect("kernel SignalInfo has a valid signo")
    }

    /// Validates a raw ABI signal number without panicking.
    pub fn try_signo(&self) -> Option<Signo> {
        // SAFETY: si_signo is part of the common integer header shared by all
        // siginfo_t union variants, and every byte in SignalInfo is initialized.
        let raw = unsafe { self.as_raw().__bindgen_anon_1.__bindgen_anon_1.si_signo };
        u8::try_from(raw).ok().and_then(Signo::from_repr)
    }

    pub fn set_signo(&mut self, signo: Signo) {
        self.raw_mut().__bindgen_anon_1.__bindgen_anon_1.si_signo = signo as _;
    }

    pub fn code(&self) -> i32 {
        // SAFETY: si_code is part of the initialized common integer header.
        unsafe { self.as_raw().__bindgen_anon_1.__bindgen_anon_1.si_code }
    }

    pub fn set_code(&mut self, code: i32) {
        self.raw_mut().__bindgen_anon_1.__bindgen_anon_1.si_code = code;
    }

    pub fn pid(&self) -> u32 {
        // SAFETY: callers use this accessor for SI_USER/SI_QUEUE-style records
        // whose initialized _kill payload contains an integer PID.
        unsafe {
            self.as_raw()
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._kill
                ._pid as u32
        }
    }

    pub fn set_pid(&mut self, pid: u32) {
        self.raw_mut()
            .__bindgen_anon_1
            .__bindgen_anon_1
            ._sifields
            ._kill
            ._pid = pid as _;
    }

    pub fn uid(&self) -> u32 {
        // SAFETY: callers use this accessor for SI_USER/SI_TKILL-style records
        // whose initialized _kill payload contains the sender UID.
        unsafe {
            self.as_raw()
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._kill
                ._uid
        }
    }

    pub fn set_uid(&mut self, uid: u32) {
        self.raw_mut()
            .__bindgen_anon_1
            .__bindgen_anon_1
            ._sifields
            ._kill
            ._uid = uid as _;
    }

    pub fn errno(&self) -> i32 {
        // SAFETY: The union layout matches Linux's siginfo_t definition. bindgen keeps
        // this layout, so it is safe to read the errno field through the
        // anonymous union.
        unsafe { self.as_raw().__bindgen_anon_1.__bindgen_anon_1.si_errno }
    }

    /// Returns the user address carried by a synchronous fault record.
    pub fn fault_address(&self) -> usize {
        // SAFETY: the fixed 128-byte storage is fully initialized and the
        // payload is read as the Linux x86_64 `_sigfault` arm.
        unsafe {
            self.as_raw()
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._sigfault
                ._addr as usize
        }
    }

    /// Returns the userspace instruction address carried by a `SIGSYS`
    /// record.
    pub fn sigsys_call_address(&self) -> usize {
        // SAFETY: see [`Self::fault_address`].
        unsafe {
            self.as_raw()
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._sigsys
                ._call_addr as usize
        }
    }

    /// Returns the raw syscall number carried by a `SIGSYS` record.
    pub fn sigsys_syscall(&self) -> i32 {
        // SAFETY: see [`Self::sigsys_call_address`].
        unsafe {
            self.as_raw()
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._sigsys
                ._syscall
        }
    }

    /// Returns the Linux audit architecture carried by a `SIGSYS` record.
    pub fn sigsys_arch(&self) -> u32 {
        // SAFETY: see [`Self::sigsys_call_address`].
        unsafe {
            self.as_raw()
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._sigsys
                ._arch
        }
    }

    /// Reads the complete timer payload without exposing the raw union.
    pub fn timer_payload(&self) -> SignalTimerPayload {
        // SAFETY: all bytes in SignalInfo are initialized; reading the
        // sigval pointer arm preserves all 64 payload bits, including values
        // that a consumer later interprets as `sival_int`.
        unsafe {
            let timer = self
                .as_raw()
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._timer;
            SignalTimerPayload::new(
                timer._tid,
                timer._overrun,
                timer._sigval.sival_ptr as usize,
                timer._sys_private,
            )
        }
    }

    /// Replaces the complete timer payload while retaining the common header.
    pub fn set_timer_payload(&mut self, payload: SignalTimerPayload) {
        // SAFETY: writing a union arm is valid for the fully initialized
        // private ABI record and does not expose a mutable raw view.
        unsafe {
            let timer = &mut self
                .raw_mut()
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._timer;
            timer._tid = payload.tid;
            timer._overrun = payload.overrun;
            timer._sigval = sigval_from_bits(payload.value);
            timer._sys_private = payload.sys_private;
        }
    }

    /// Reads the complete `SI_QUEUE`/`SI_MESGQ` payload without exposing the
    /// raw union.
    pub fn rt_payload(&self) -> SignalRtPayload {
        // SAFETY: see [`Self::timer_payload`].
        unsafe {
            let rt = self
                .as_raw()
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._rt;
            SignalRtPayload::new(rt._pid, rt._uid, rt._sigval.sival_ptr as usize)
        }
    }

    /// Replaces the complete `SI_QUEUE`/`SI_MESGQ` payload while retaining
    /// the common header and selected code.
    pub fn set_rt_payload(&mut self, payload: SignalRtPayload) {
        // SAFETY: see [`Self::set_timer_payload`].
        unsafe {
            let rt = &mut self
                .raw_mut()
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._rt;
            rt._pid = payload.pid;
            rt._uid = payload.uid;
            rt._sigval = sigval_from_bits(payload.value);
        }
    }

    /// Reads the complete `SI_SIGIO` payload without exposing the raw union.
    pub fn poll_payload(&self) -> SignalPollPayload {
        // SAFETY: the fixed storage is initialized and `_sigpoll` consists
        // only of scalar fields on the x86_64 Linux ABI.
        unsafe {
            let poll = self
                .as_raw()
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._sigpoll;
            SignalPollPayload::new(poll._band, poll._fd)
        }
    }

    /// Replaces the complete `SI_SIGIO` payload while retaining the common
    /// header.
    pub fn set_poll_payload(&mut self, payload: SignalPollPayload) {
        // SAFETY: see [`Self::set_timer_payload`].
        unsafe {
            let poll = &mut self
                .raw_mut()
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._sigpoll;
            poll._band = payload.band;
            poll._fd = payload.fd;
        }
    }

    /// Returns the raw Linux ABI record.
    pub fn as_raw(&self) -> &siginfo_t {
        // SAFETY: the storage has exactly siginfo_t's asserted size and
        // alignment. linux_raw_sys models the record entirely with integer
        // scalars, arrays, and unions, so every fully initialized byte pattern
        // is a valid raw record.
        unsafe { &*self.0.as_ptr().cast::<siginfo_t>() }
    }

    fn raw_mut(&mut self) -> &mut siginfo_t {
        // SAFETY: as_raw's layout argument also applies to this exclusive view.
        unsafe { &mut *self.0.as_mut_ptr().cast::<siginfo_t>() }
    }
}

fn sigval_from_bits(value: usize) -> sigval_t {
    // Storing the bits through the pointer arm is valid for the C union and
    // preserves both the integer and pointer interpretations of sigval_t.
    sigval_t {
        sival_ptr: value as *mut c_void,
    }
}

const _: [(); mem::size_of::<siginfo_t>()] = [(); mem::size_of::<SignalInfo>()];
const _: [(); mem::align_of::<siginfo_t>()] = [(); mem::align_of::<SignalInfo>()];

impl fmt::Debug for SignalInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalInfo")
            .field("signo", &self.signo())
            .field("code", &self.code())
            .finish()
    }
}

/// Signal stack. Compatible with `struct sigaltstack` in libc.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalStack {
    pub sp: usize,
    pub flags: u32,
    // Linux's 64-bit stack_t has an explicit four-byte alignment hole before
    // ss_size. Keeping those bytes as a field prevents kernel stack contents
    // from leaking when a complete signal frame is copied to userspace.
    __padding: u32,
    pub size: usize,
}

impl Default for SignalStack {
    fn default() -> Self {
        Self {
            sp: 0,
            flags: SS_DISABLE,
            __padding: 0,
            size: 0,
        }
    }
}

impl SignalStack {
    /// Creates a fully initialized Linux alternate-stack record.
    pub const fn new(sp: usize, flags: u32, size: usize) -> Self {
        Self {
            sp,
            flags,
            __padding: 0,
            size,
        }
    }

    /// Normalizes the `uc_stack` record supplied by `rt_sigreturn`.
    pub(crate) fn prepare_restore(&self) -> Result<Self, SignalStackRestoreError> {
        const SS_AUTODISARM: u32 = 1 << 31;
        if self.flags != 0
            && self.flags != SS_DISABLE
            && self.flags != SS_ONSTACK
            && self.flags != SS_AUTODISARM
        {
            return Err(SignalStackRestoreError::InvalidFlags);
        }
        if self.flags == SS_DISABLE {
            return Ok(Self::default());
        }
        if self.sp.checked_add(self.size).is_none() {
            return Err(SignalStackRestoreError::AddressOverflow);
        }
        Ok(*self)
    }

    /// Checks if signal stack is disabled.
    pub fn disabled(&self) -> bool {
        self.flags & SS_DISABLE != 0
    }

    /// Returns the exclusive top of the configured alternate stack.
    ///
    /// A userspace-provided base and length are not trusted to fit in the
    /// address space. Callers must treat `None` as an unusable stack instead
    /// of allowing integer wraparound to select a kernel or low address.
    pub fn checked_top(&self) -> Option<usize> {
        (!self.disabled())
            .then(|| self.sp.checked_add(self.size))
            .flatten()
    }

    /// Implements Linux's overflow-safe `on_sig_stack()` range predicate.
    /// The exclusive base/inclusive-top shape matches the kernel rule for a
    /// downward-growing stack pointer.
    pub fn contains_sp(&self, sp: usize) -> bool {
        !self.disabled() && sp > self.sp && sp.wrapping_sub(self.sp) <= self.size
    }

    /// Returns the flags Linux exposes when querying `sigaltstack(2)` at the
    /// supplied userspace stack pointer.
    pub fn flags_at(&self, sp: usize) -> u32 {
        if self.disabled() {
            SS_DISABLE
        } else if self.contains_sp(sp) {
            SS_ONSTACK
        } else {
            0
        }
    }

    /// Checks that a complete object range remains within this alternate
    /// stack. This is used before publishing a nested signal frame.
    pub fn contains_range(&self, start: usize, len: usize) -> bool {
        if self.disabled() || start < self.sp {
            return false;
        }
        let Some(end) = start.checked_add(len) else {
            return false;
        };
        self.checked_top().is_some_and(|top| end <= top)
    }
}

/// Why a saved `uc_stack` record was not applied by `rt_sigreturn`.
///
/// Linux deliberately squashes non-copy `restore_altstack()` errors. The
/// reusable crate follows that observable rule: this value is retained in a
/// prepared restore for diagnostics while context and mask restoration can
/// still commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalStackRestoreError {
    /// The frame contains unsupported or contradictory `SS_*` flags.
    InvalidFlags,
    /// `ss_sp + ss_size` wraps the 64-bit userspace address space.
    AddressOverflow,
    /// The sigreturn syscall is executing on the currently configured stack.
    ActiveStack,
    /// The candidate is smaller than the consumer's architecture minimum.
    TooSmall,
    /// The candidate is outside the consumer's valid userspace range.
    InvalidAddress,
}

const _: [(); 24] = [(); mem::size_of::<SignalStack>()];
const _: [(); 8] = [(); mem::align_of::<SignalStack>()];
const _: [(); 0] = [(); mem::offset_of!(SignalStack, sp)];
const _: [(); 8] = [(); mem::offset_of!(SignalStack, flags)];
const _: [(); 16] = [(); mem::offset_of!(SignalStack, size)];
