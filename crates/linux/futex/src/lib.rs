//! Pure futex ABI decoding. Queueing, user-memory faults, and restart state are external.
#![no_std]
#![forbid(unsafe_code)]

// ---------------------------------------------------------------------------
// Legacy `sys_futex` opcode space (include/uapi/linux/futex.h, Linux v7.2.3).
// ---------------------------------------------------------------------------

pub const FUTEX_WAIT: u32 = 0;
pub const FUTEX_WAKE: u32 = 1;
pub const FUTEX_FD: u32 = 2;
pub const FUTEX_REQUEUE: u32 = 3;
pub const FUTEX_CMP_REQUEUE: u32 = 4;
pub const FUTEX_WAKE_OP: u32 = 5;
pub const FUTEX_LOCK_PI: u32 = 6;
pub const FUTEX_UNLOCK_PI: u32 = 7;
pub const FUTEX_TRYLOCK_PI: u32 = 8;
pub const FUTEX_WAIT_BITSET: u32 = 9;
pub const FUTEX_WAKE_BITSET: u32 = 10;
pub const FUTEX_WAIT_REQUEUE_PI: u32 = 11;
pub const FUTEX_CMP_REQUEUE_PI: u32 = 12;
pub const FUTEX_LOCK_PI2: u32 = 13;

pub const FUTEX_PRIVATE_FLAG: u32 = 128;
pub const FUTEX_CLOCK_REALTIME: u32 = 256;
pub const FUTEX_ROBUST_UNLOCK: u32 = 512;
pub const FUTEX_ROBUST_LIST32: u32 = 1024;

/// Value of Linux's `FUTEX_CMD_MASK` (`~(FUTEX_PRIVATE_FLAG |
/// FUTEX_CLOCK_REALTIME | FUTEX_ROBUST_UNLOCK | FUTEX_ROBUST_LIST32)`) as the
/// low six bits it leaves in place.
///
/// It is *not* a field width: Linux masks with the complement of the modifier
/// bits, so a bit above bit 6 — including the `linux_raw_sys` legacy constant
/// `FUTEX_CMD_MASK == 0x7f` would mask away — survives into `cmd` and makes the
/// operation unknown (`-ENOSYS`). Use [`FUTEX_MODIFIER_MASK`] or
/// [`LegacyOp::decode`] rather than this constant.
pub const FUTEX_CMD_MASK: u32 = 0x7f;

/// The four modifier bits Linux removes from `op` before dispatching.
pub const FUTEX_MODIFIER_MASK: u32 =
    FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME | FUTEX_ROBUST_UNLOCK | FUTEX_ROBUST_LIST32;

/// `FUTEX_WAITERS` / `FUTEX_OWNER_DIED` / `FUTEX_TID_MASK` of a PI futex word.
pub const FUTEX_WAITERS: u32 = 0x8000_0000;
pub const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
pub const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

// ---------------------------------------------------------------------------
// futex2 flag space.
// ---------------------------------------------------------------------------

pub const FUTEX2_SIZE_MASK: u32 = 0x03;
pub const FUTEX2_SIZE_U8: u32 = 0x00;
pub const FUTEX2_SIZE_U16: u32 = 0x01;
pub const FUTEX2_SIZE_U32: u32 = 0x02;
pub const FUTEX2_SIZE_U64: u32 = 0x03;
pub const FUTEX2_NUMA: u32 = 0x04;
pub const FUTEX2_MPOL: u32 = 0x08;
pub const FUTEX2_PRIVATE: u32 = 128;
pub const FUTEX2_VALID_MASK: u32 = FUTEX2_SIZE_MASK | FUTEX2_NUMA | FUTEX2_MPOL | FUTEX2_PRIVATE;

/// The node-id word of a `FUTEX2_NUMA` futex holding `-1` means "no node".
pub const FUTEX_NO_NODE: u32 = u32::MAX;

pub const FUTEX_WAITV_MAX: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FutexError {
    InvalidCommand,
    InvalidFlags,
    InvalidAddress,
    InvalidCount,
    InvalidTimeout,
    InvalidValue,
    TooManyWaiters,
    DuplicateAddress,
}

// ---------------------------------------------------------------------------
// Legacy opcode decoding.
// ---------------------------------------------------------------------------

/// Every opcode `do_futex()` dispatches on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FutexCommand {
    Wait,
    Wake,
    Requeue,
    CmpRequeue,
    WakeOp,
    LockPi,
    UnlockPi,
    TrylockPi,
    WaitBitset,
    WakeBitset,
    WaitRequeuePi,
    CmpRequeuePi,
    LockPi2,
    /// `FUTEX_FD`, removed from Linux long ago; `do_futex()` has no case for it.
    Fd,
}

/// Decoded `(op, val, ...)` header of a `sys_futex` call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacyOp {
    /// `None` when `op & FUTEX_CMD_MASK` matches no opcode.
    pub command: Option<FutexCommand>,
    pub private: bool,
    pub realtime: bool,
    pub robust_unlock: bool,
    pub robust_list32: bool,
}

impl LegacyOp {
    /// `do_futex()`'s `cmd = op & FUTEX_CMD_MASK` plus `futex_to_flags()`.
    pub const fn decode(op: u32) -> Self {
        let cmd = op & !FUTEX_MODIFIER_MASK;
        Self {
            command: match cmd {
                FUTEX_WAIT => Some(FutexCommand::Wait),
                FUTEX_WAKE => Some(FutexCommand::Wake),
                FUTEX_FD => Some(FutexCommand::Fd),
                FUTEX_REQUEUE => Some(FutexCommand::Requeue),
                FUTEX_CMP_REQUEUE => Some(FutexCommand::CmpRequeue),
                FUTEX_WAKE_OP => Some(FutexCommand::WakeOp),
                FUTEX_LOCK_PI => Some(FutexCommand::LockPi),
                FUTEX_UNLOCK_PI => Some(FutexCommand::UnlockPi),
                FUTEX_TRYLOCK_PI => Some(FutexCommand::TrylockPi),
                FUTEX_WAIT_BITSET => Some(FutexCommand::WaitBitset),
                FUTEX_WAKE_BITSET => Some(FutexCommand::WakeBitset),
                FUTEX_WAIT_REQUEUE_PI => Some(FutexCommand::WaitRequeuePi),
                FUTEX_CMP_REQUEUE_PI => Some(FutexCommand::CmpRequeuePi),
                FUTEX_LOCK_PI2 => Some(FutexCommand::LockPi2),
                _ => None,
            },
            private: op & FUTEX_PRIVATE_FLAG != 0,
            realtime: op & FUTEX_CLOCK_REALTIME != 0,
            robust_unlock: op & FUTEX_ROBUST_UNLOCK != 0,
            robust_list32: op & FUTEX_ROBUST_LIST32 != 0,
        }
    }

    /// The two `-ENOSYS` gates at the top of `do_futex()`, in Linux order.
    ///
    /// `FUTEX_LOCK_PI` is deliberately absent from the realtime allow-list:
    /// `do_futex()` forces `FLAGS_CLOCKRT` for it, so userspace passing
    /// `FUTEX_CLOCK_REALTIME` explicitly is rejected; `FUTEX_LOCK_PI2` exists
    /// precisely to let userspace choose the clock.
    pub const fn rejected_with_enosys(self) -> bool {
        if self.realtime
            && !matches!(
                self.command,
                Some(
                    FutexCommand::WaitBitset | FutexCommand::WaitRequeuePi | FutexCommand::LockPi2
                )
            )
        {
            return true;
        }
        if self.robust_unlock
            && !matches!(
                self.command,
                Some(FutexCommand::Wake | FutexCommand::WakeBitset | FutexCommand::UnlockPi)
            )
        {
            return true;
        }
        false
    }

    /// `futex_cmd_has_timeout()`: only these opcodes make `sys_futex` read the
    /// user `timespec` argument at all.
    pub const fn has_timeout(self) -> bool {
        matches!(
            self.command,
            Some(
                FutexCommand::Wait
                    | FutexCommand::LockPi
                    | FutexCommand::LockPi2
                    | FutexCommand::WaitBitset
                    | FutexCommand::WaitRequeuePi
            )
        )
    }

    /// `FUTEX_WAIT` is the only relative timeout; every other timed opcode
    /// receives an absolute `timespec`.
    pub const fn timeout_is_relative(self) -> bool {
        matches!(self.command, Some(FutexCommand::Wait))
    }

    /// Clock a timed operation uses. `FUTEX_LOCK_PI` is unconditionally
    /// `CLOCK_REALTIME` because `do_futex()` ORs in `FLAGS_CLOCKRT`.
    pub const fn timeout_clock(self) -> Clock {
        if matches!(self.command, Some(FutexCommand::LockPi)) || self.realtime {
            Clock::Realtime
        } else {
            Clock::Monotonic
        }
    }

    /// `FUTEX_WAKE`/`FUTEX_WAKE_BITSET` wake at least one waiter even when
    /// userspace passes 0 or a negative count: `futex_wake()` tests
    /// `if (++ret >= nr_wake) break;` *after* waking, so a non-positive limit
    /// still wakes the first match.
    pub const fn legacy_wake_count(value: u32) -> usize {
        if (value as i32) <= 0 {
            1
        } else {
            value as usize
        }
    }
}

// ---------------------------------------------------------------------------
// PI futex user word.
// ---------------------------------------------------------------------------

/// Decoded PI futex word: `FUTEX_WAITERS | FUTEX_OWNER_DIED | TID`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PiWord {
    pub tid: u32,
    pub waiters: bool,
    pub owner_died: bool,
}

impl PiWord {
    pub const fn decode(value: u32) -> Self {
        Self {
            tid: value & FUTEX_TID_MASK,
            waiters: value & FUTEX_WAITERS != 0,
            owner_died: value & FUTEX_OWNER_DIED != 0,
        }
    }

    pub const fn encode(self) -> u32 {
        let mut value = self.tid & FUTEX_TID_MASK;
        if self.waiters {
            value |= FUTEX_WAITERS;
        }
        if self.owner_died {
            value |= FUTEX_OWNER_DIED;
        }
        value
    }

    pub const fn is_owned(self) -> bool {
        self.tid != 0
    }
}

/// Value transition required by `futex_lock_pi_atomic()` for the user word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PiAcquire {
    /// `-EDEADLK`: the word already names the acquiring task.
    Deadlock,
    /// The word is unowned: the caller may take it over with this value.
    /// `FUTEX_OWNER_DIED` is preserved, `FUTEX_WAITERS` is forced only when
    /// the requeue path asked for it.
    TakeOver { new_value: u32 },
    /// The word names a live owner: publish `FUTEX_WAITERS` so the owner is
    /// forced through the kernel on unlock, then attach to that owner.
    SetWaiters { new_value: u32 },
}

/// `futex_lock_pi_atomic()`'s value inspection, minus the kernel state lookup.
///
/// Order is the ABI: the deadlock check happens before the word is classified,
/// so a task that already owns the futex sees `-EDEADLK` even when stale
/// `FUTEX_WAITERS`/`FUTEX_OWNER_DIED` bits are set.
pub const fn plan_pi_acquire(word: PiWord, vpid: u32, set_waiters: bool) -> PiAcquire {
    if word.tid == vpid {
        return PiAcquire::Deadlock;
    }
    if !word.is_owned() {
        let mut new_value = word.tid;
        if word.owner_died {
            new_value |= FUTEX_OWNER_DIED;
        }
        new_value |= vpid;
        if set_waiters {
            new_value |= FUTEX_WAITERS;
        }
        return PiAcquire::TakeOver { new_value };
    }
    PiAcquire::SetWaiters {
        new_value: word.encode() | FUTEX_WAITERS,
    }
}

/// Value transition required by `__futex_unlock_pi()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PiUnlock {
    /// `-EPERM`: the caller is not the task named in the user word.
    NotOwner,
    /// No kernel waiters: `cmpxchg(uaddr, uval, 0)` clears both the TID and
    /// the stale `FUTEX_WAITERS`/`FUTEX_OWNER_DIED` bits, because the caller
    /// demonstrably owns the futex.
    Clear { expected: u32 },
    /// Hand the futex to @tid. `FUTEX_WAITERS` stays set because waiters
    /// remain, and `FUTEX_OWNER_DIED` is dropped because the new owner is
    /// alive (`wake_futex_pi()`).
    Handoff { new_value: u32 },
}

pub const fn plan_pi_unlock(word: PiWord, vpid: u32, next_owner: Option<u32>) -> PiUnlock {
    if word.tid != vpid {
        return PiUnlock::NotOwner;
    }
    match next_owner {
        Some(tid) => PiUnlock::Handoff {
            new_value: FUTEX_WAITERS | (tid & FUTEX_TID_MASK),
        },
        None => PiUnlock::Clear {
            expected: word.encode(),
        },
    }
}

/// The word the *woken* PI waiter must observe to conclude that the unlock
/// handed it the futex.
pub const fn pi_handoff_value(tid: u32) -> u32 {
    FUTEX_WAITERS | (tid & FUTEX_TID_MASK)
}

/// `fixup_pi_state_owner()`'s new user value: the dying-owner bit survives a
/// concurrent `FUTEX_OWNER_DIED` set by robust-list cleanup.
pub const fn pi_fixup_value(observed: u32, new_owner: u32, owner_died: bool) -> u32 {
    let mut value = observed & FUTEX_OWNER_DIED;
    value |= FUTEX_WAITERS | (new_owner & FUTEX_TID_MASK);
    if owner_died {
        value |= FUTEX_OWNER_DIED;
    }
    value
}

// ---------------------------------------------------------------------------
// WAKE_OP.
// ---------------------------------------------------------------------------

/// Decoded legacy FUTEX_WAKE_OP. The comparison is deliberately validated
/// after the RMW, as Linux still updates the word for an unknown comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WakeOp {
    operation: u8,
    comparison: u8,
    argument: u32,
    compare_argument: i32,
}

impl WakeOp {
    pub fn decode(encoded: u32) -> Result<Self, FutexError> {
        let operation = ((encoded >> 28) & 7) as u8;
        if operation > 4 {
            return Err(FutexError::InvalidCommand);
        }
        let argument = ((encoded << 8) as i32) >> 20;
        Ok(Self {
            operation,
            comparison: ((encoded >> 24) & 15) as u8,
            // Linux masks even invalid signed shift arguments to five bits.
            argument: if encoded & (1 << 31) != 0 {
                1u32 << (argument as u32 & 31)
            } else {
                argument as u32
            },
            compare_argument: ((encoded << 20) as i32) >> 20,
        })
    }

    pub fn updated(self, old: u32) -> u32 {
        match self.operation {
            0 => self.argument,
            1 => old.wrapping_add(self.argument),
            2 => old | self.argument,
            3 => old & !self.argument,
            4 => old ^ self.argument,
            _ => unreachable!("validated wake operation"),
        }
    }

    pub fn compare(self, old: u32) -> Result<bool, FutexError> {
        let old = old as i32;
        Ok(match self.comparison {
            0 => old == self.compare_argument,
            1 => old != self.compare_argument,
            2 => old < self.compare_argument,
            3 => old <= self.compare_argument,
            4 => old > self.compare_argument,
            5 => old >= self.compare_argument,
            _ => return Err(FutexError::InvalidCommand),
        })
    }
}

// ---------------------------------------------------------------------------
// Timeouts.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Clock {
    Monotonic,
    Realtime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timeout {
    pub seconds: i64,
    pub nanos: i32,
    pub absolute: bool,
    pub clock: Clock,
}

impl Timeout {
    pub const fn validate(self) -> Result<Self, FutexError> {
        if self.seconds < 0 || self.nanos < 0 || self.nanos >= 1_000_000_000 {
            Err(FutexError::InvalidTimeout)
        } else {
            Ok(self)
        }
    }
}

// ---------------------------------------------------------------------------
// Address/key shape.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FutexWord {
    pub address: usize,
    pub expected: u32,
    pub private: bool,
}

impl FutexWord {
    pub const fn new(address: usize, expected: u32, private: bool) -> Result<Self, FutexError> {
        Self::with_bytes(address, expected, private, 4)
    }

    /// `get_futex_key()` aligns on the *whole* futex, which `FUTEX2_NUMA`
    /// doubles to eight bytes because the second word carries the node id.
    pub const fn with_bytes(
        address: usize,
        expected: u32,
        private: bool,
        bytes: usize,
    ) -> Result<Self, FutexError> {
        if address % bytes != 0 {
            Err(FutexError::InvalidAddress)
        } else {
            Ok(Self {
                address,
                expected,
                private,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyPlan {
    Wait {
        word: FutexWord,
        bitset: u32,
        timeout: Option<Timeout>,
    },
    Wake {
        address: usize,
        count: usize,
        bitset: u32,
        private: bool,
    },
    WakeOp {
        source: usize,
        target: usize,
        wake: usize,
        wake2: usize,
        operation: WakeOp,
        private: bool,
    },
    Requeue {
        source: usize,
        target: usize,
        wake: usize,
        requeue: usize,
        compare: Option<u32>,
        private: bool,
    },
}

pub fn plan_legacy(
    address: usize,
    op: u32,
    value: u32,
    timeout: Option<Timeout>,
    address2: usize,
    value3: u32,
) -> Result<LegacyPlan, FutexError> {
    let decoded = LegacyOp::decode(op);
    let private = decoded.private;
    let realtime = decoded.realtime;
    if decoded.command.is_none() {
        return Err(FutexError::InvalidCommand);
    }
    if address & 3 != 0 {
        return Err(FutexError::InvalidAddress);
    }
    let clock = decoded.timeout_clock();
    match decoded.command {
        Some(FutexCommand::Wait | FutexCommand::WaitBitset) => {
            let bitset = if decoded.command == Some(FutexCommand::Wait) {
                u32::MAX
            } else {
                value3
            };
            if bitset == 0 {
                return Err(FutexError::InvalidValue);
            }
            let t = match timeout {
                Some(v) => Some(Timeout { clock, ..v }.validate()?),
                None => None,
            };
            Ok(LegacyPlan::Wait {
                word: FutexWord::new(address, value, private)?,
                bitset,
                timeout: t,
            })
        }
        Some(FutexCommand::Wake | FutexCommand::WakeBitset) => {
            let bitset = if decoded.command == Some(FutexCommand::Wake) {
                u32::MAX
            } else {
                value3
            };
            if bitset == 0 {
                return Err(FutexError::InvalidValue);
            }
            Ok(LegacyPlan::Wake {
                address,
                count: LegacyOp::legacy_wake_count(value),
                bitset,
                private,
            })
        }
        Some(FutexCommand::WakeOp) => {
            if realtime {
                return Err(FutexError::InvalidFlags);
            }
            if address2 & 3 != 0 {
                return Err(FutexError::InvalidAddress);
            }
            Ok(LegacyPlan::WakeOp {
                source: address,
                target: address2,
                wake: wake_op_count(value),
                wake2: wake_op_count(timeout.map_or(0, |v| v.seconds as u32)),
                operation: WakeOp::decode(value3)?,
                private,
            })
        }
        Some(FutexCommand::Requeue | FutexCommand::CmpRequeue) => {
            if address2 & 3 != 0 {
                return Err(FutexError::InvalidAddress);
            }
            if (value as i32) < 0 || (timeout.map_or(0, |v| v.seconds as u32) as i32) < 0 {
                return Err(FutexError::InvalidCount);
            }
            Ok(LegacyPlan::Requeue {
                source: address,
                target: address2,
                wake: value as usize,
                requeue: timeout.map_or(0, |v| v.seconds as u32) as usize,
                compare: if decoded.command == Some(FutexCommand::CmpRequeue) {
                    Some(value3)
                } else {
                    None
                },
                private,
            })
        }
        _ => Err(FutexError::InvalidCommand),
    }
}

/// WAKE_OP tests the limit after each matching wake, unlike strict futex2.
pub const fn wake_op_count(value: u32) -> usize {
    if (value as i32) <= 0 {
        1
    } else {
        value as usize
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FutexWaitV {
    pub val: u64,
    pub uaddr: u64,
    pub flags: u32,
    pub reserved: u32,
}
const _: () = {
    assert!(core::mem::size_of::<FutexWaitV>() == 24);
    assert!(core::mem::align_of::<FutexWaitV>() == 8);
};

// ---------------------------------------------------------------------------
// futex2.
// ---------------------------------------------------------------------------

/// Decoded futex2 flag word. `FUTEX2_NUMA` doubles the futex so a second
/// 32-bit word can carry the node id; `FUTEX2_MPOL` lets `get_futex_key()`
/// derive the node from the VMA memory policy instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Futex2Flags {
    pub private: bool,
    pub numa: bool,
    pub mpol: bool,
}

impl Futex2Flags {
    /// `get_futex_key()`: `size = futex_size(flags); if (flags & FLAGS_NUMA)
    /// size *= 2;` — the futex is naturally aligned to and `access_ok()`'d for
    /// this many bytes.
    pub const fn word_bytes(self) -> usize {
        if self.numa { 8 } else { 4 }
    }
}

/// `futex2_to_flags()` + `futex_flags_valid()` for a 64-bit kernel.
///
/// Linux accepts `FUTEX2_NUMA` and `FUTEX2_MPOL`; only the size field, the
/// three reserved bits and (when `FUTEX2_NUMA` is set) the node-id width
/// check can reject a flag word. Only 32-bit futexes are implemented, and
/// `nr_node_ids` (1 on this machine) is far below the 8-bit `FUTEX_NO_NODE`
/// limit, so the node-width check never fires here.
pub const fn parse_futex2_flags(flags: u32) -> Result<Futex2Flags, FutexError> {
    if flags & !FUTEX2_VALID_MASK != 0 || flags & FUTEX2_SIZE_MASK != FUTEX2_SIZE_U32 {
        Err(FutexError::InvalidFlags)
    } else {
        Ok(Futex2Flags {
            private: flags & FUTEX2_PRIVATE != 0,
            numa: flags & FUTEX2_NUMA != 0,
            mpol: flags & FUTEX2_MPOL != 0,
        })
    }
}

/// `get_futex_key()`'s `FUTEX2_NUMA` node-id validation.
///
/// Linux rejects a node that is neither `FUTEX_NO_NODE` nor a possible node:
/// `(node != FUTEX_NO_NODE) && ((unsigned int)node >= MAX_NUMNODES ||
/// !node_possible(node))`. This machine has a single possible node, so only
/// node 0 is representable; `FUTEX_NO_NODE` is accepted and rewritten to
/// `numa_node_id()` == 0. `FUTEX2_MPOL` can only ever resolve to node 0 here
/// too (`futex_mpol()` maps a VMA policy to `first_node()`/`home_node`, both
/// of which are 0 or `NUMA_NO_NODE` on a single-node machine), which is why
/// the key placement is unaffected by accepting the flag.
pub const fn validate_numa_node(node: u32) -> Result<u32, FutexError> {
    if node == FUTEX_NO_NODE || node == 0 {
        Ok(0)
    } else {
        Err(FutexError::InvalidValue)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Futex2Plan<'a> {
    Wait {
        word: FutexWord,
        mask: u32,
        timeout: Option<Timeout>,
    },
    Wake {
        address: usize,
        mask: u32,
        count: usize,
        private: bool,
    },
    WaitV {
        words: &'a [FutexWaitV],
        timeout: Option<Timeout>,
    },
    Requeue {
        source: FutexWord,
        target: FutexWord,
        wake: usize,
        requeue: usize,
    },
}

/// Fully decoded futex2 requeue request.  Both endpoints use the same futex2
/// flags, including the private/shared keying mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Futex2RequeuePlan {
    pub source: FutexWord,
    pub target: FutexWord,
    pub wake: usize,
    pub requeue: usize,
    pub flags: Futex2Flags,
}

fn requeue_word(waiter: FutexWaitV) -> Result<(FutexWord, Futex2Flags), FutexError> {
    if waiter.reserved != 0 || waiter.val > u32::MAX as u64 {
        return Err(FutexError::InvalidValue);
    }
    let flags = parse_futex2_flags(waiter.flags)?;
    Ok((
        FutexWord::with_bytes(
            waiter.uaddr as usize,
            waiter.val as u32,
            flags.private,
            flags.word_bytes(),
        )?,
        flags,
    ))
}

/// Decodes the two futex2 requeue endpoints before any queue or user-memory
/// work. The syscall-wide flags must be zero, and Linux currently requires
/// both endpoint descriptors to carry identical flags.
pub const fn validate_requeue_flags(flags: u32) -> Result<(), FutexError> {
    if flags == 0 {
        Ok(())
    } else {
        Err(FutexError::InvalidFlags)
    }
}

pub fn plan_requeue(
    source: FutexWaitV,
    target: FutexWaitV,
    flags: u32,
    wake: i32,
    requeue: i32,
) -> Result<Futex2RequeuePlan, FutexError> {
    validate_requeue_flags(flags)?;
    let source_flags = source.flags;
    let target_flags = target.flags;
    let (source, source_decoded) = requeue_word(source)?;
    let (target, _) = requeue_word(target)?;
    if source_flags != target_flags {
        return Err(FutexError::InvalidFlags);
    }
    // `futex_requeue()` rejects a negative count only after `futex_parse_waitv()`
    // has validated both descriptors.
    if wake < 0 || requeue < 0 {
        return Err(FutexError::InvalidCount);
    }
    Ok(Futex2RequeuePlan {
        source,
        target,
        wake: wake as usize,
        requeue: requeue as usize,
        flags: source_decoded,
    })
}

pub fn plan_waitv(
    words: &[FutexWaitV],
    flags: u32,
    timeout: Option<Timeout>,
) -> Result<Futex2Plan<'_>, FutexError> {
    if flags != 0 {
        return Err(FutexError::InvalidFlags);
    }
    if words.is_empty() || words.len() > FUTEX_WAITV_MAX {
        return Err(FutexError::TooManyWaiters);
    }
    for (i, w) in words.iter().enumerate() {
        if w.reserved != 0 || w.val > u32::MAX as u64 {
            return Err(FutexError::InvalidValue);
        }
        let _ = parse_futex2_flags(w.flags)?;
        if w.uaddr as usize & 3 != 0 {
            return Err(FutexError::InvalidAddress);
        }
        for earlier in &words[..i] {
            if earlier.uaddr == w.uaddr {
                return Err(FutexError::DuplicateAddress);
            }
        }
    }
    Ok(Futex2Plan::WaitV {
        words,
        timeout: match timeout {
            Some(v) => Some(v.validate()?),
            None => None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waitv_rejects_duplicate_after_shape() {
        let w = FutexWaitV {
            val: 1,
            uaddr: 4,
            flags: 2,
            reserved: 0,
        };
        assert_eq!(
            plan_waitv(&[w, w], 0, None),
            Err(FutexError::DuplicateAddress)
        );
    }

    #[test]
    fn legacy_plan_rejects_an_unknown_opcode_before_the_address() {
        assert_eq!(
            plan_legacy(0, 0x8000, 0, None, 0, 0),
            Err(FutexError::InvalidCommand)
        );
        assert_eq!(
            plan_legacy(3, FUTEX_WAKE, 1, None, 0, 0),
            Err(FutexError::InvalidAddress)
        );
    }

    #[test]
    fn requeue_requires_matching_endpoint_keying() {
        let source = FutexWaitV {
            val: 7,
            uaddr: 4,
            flags: FUTEX2_SIZE_U32 | FUTEX2_PRIVATE,
            reserved: 0,
        };
        let target = FutexWaitV {
            val: 0,
            uaddr: 8,
            flags: FUTEX2_SIZE_U32,
            reserved: 0,
        };
        assert_eq!(
            plan_requeue(source, target, 0, 1, 2),
            Err(FutexError::InvalidFlags)
        );
        assert_eq!(
            plan_requeue(target, source, 0, 1, 2),
            Err(FutexError::InvalidFlags)
        );

        let matching = FutexWaitV {
            flags: FUTEX2_SIZE_U32 | FUTEX2_PRIVATE,
            ..target
        };
        let plan = plan_requeue(source, matching, 0, 1, 2).unwrap();
        assert!(plan.source.private);
        assert!(plan.target.private);
    }

    #[test]
    fn requeue_rejects_unknown_endpoint_flags() {
        let source = FutexWaitV {
            val: 0,
            uaddr: 4,
            flags: FUTEX2_SIZE_U32,
            reserved: 0,
        };
        let invalid_target = FutexWaitV {
            flags: FUTEX2_SIZE_U32 | 0x10,
            ..source
        };
        assert_eq!(
            plan_requeue(source, invalid_target, 0, 0, 0),
            Err(FutexError::InvalidFlags)
        );
    }

    #[test]
    fn requeue_validates_counts_after_both_descriptors() {
        let bad_target = FutexWaitV {
            val: 0,
            uaddr: 8,
            flags: FUTEX2_SIZE_U32 | 0x10,
            reserved: 0,
        };
        let good = FutexWaitV {
            flags: FUTEX2_SIZE_U32,
            ..bad_target
        };
        // The negative count is rejected only once both endpoints parsed.
        assert_eq!(
            plan_requeue(good, bad_target, 0, -1, 0),
            Err(FutexError::InvalidFlags)
        );
        assert_eq!(
            plan_requeue(good, good, 0, -1, 0),
            Err(FutexError::InvalidCount)
        );
        // An unaligned endpoint is an address error, not a value error.
        assert_eq!(
            plan_requeue(good, FutexWaitV { uaddr: 5, ..good }, 0, 0, 0),
            Err(FutexError::InvalidAddress)
        );
    }

    #[test]
    fn futex2_numa_and_mpol_are_valid_flags() {
        let shared = parse_futex2_flags(FUTEX2_SIZE_U32).unwrap();
        assert_eq!(
            shared,
            Futex2Flags {
                private: false,
                numa: false,
                mpol: false
            }
        );
        assert_eq!(shared.word_bytes(), 4);

        let numa = parse_futex2_flags(FUTEX2_SIZE_U32 | FUTEX2_NUMA).unwrap();
        assert!(numa.numa);
        assert_eq!(numa.word_bytes(), 8);

        let mpol = parse_futex2_flags(FUTEX2_SIZE_U32 | FUTEX2_MPOL | FUTEX2_PRIVATE).unwrap();
        assert!(mpol.mpol && mpol.private && !mpol.numa);
        assert_eq!(mpol.word_bytes(), 4);

        let both = parse_futex2_flags(FUTEX2_SIZE_U32 | FUTEX2_NUMA | FUTEX2_MPOL).unwrap();
        assert!(both.numa && both.mpol);
    }

    #[test]
    fn futex2_rejects_reserved_bits_and_non_u32_sizes() {
        for flags in [
            FUTEX2_SIZE_U8,
            FUTEX2_SIZE_U16,
            FUTEX2_SIZE_U64,
            FUTEX2_SIZE_U32 | 0x10,
            FUTEX2_SIZE_U32 | 0x20,
            FUTEX2_SIZE_U32 | 0x40,
            FUTEX2_NUMA | FUTEX2_MPOL | FUTEX2_PRIVATE,
        ] {
            assert_eq!(
                parse_futex2_flags(flags),
                Err(FutexError::InvalidFlags),
                "flags {flags:#x}"
            );
        }
    }

    #[test]
    fn numa_futex_is_eight_byte_aligned_and_only_accepts_node_zero() {
        assert_eq!(
            FutexWord::with_bytes(0x1000, 0, true, 8).unwrap().address,
            0x1000
        );
        assert_eq!(
            FutexWord::with_bytes(0x1004, 0, true, 8),
            Err(FutexError::InvalidAddress)
        );
        // A plain 32-bit futex still only needs four-byte alignment.
        assert!(FutexWord::with_bytes(0x1004, 0, true, 4).is_ok());

        assert_eq!(validate_numa_node(FUTEX_NO_NODE), Ok(0));
        assert_eq!(validate_numa_node(0), Ok(0));
        for node in [1u32, 2, 63, 64, 0x7fff_ffff] {
            assert_eq!(
                validate_numa_node(node),
                Err(FutexError::InvalidValue),
                "node {node}"
            );
        }
    }

    #[test]
    fn numa_requeue_plan_reports_the_doubled_word() {
        let endpoint = FutexWaitV {
            val: 1,
            uaddr: 0x1000,
            flags: FUTEX2_SIZE_U32 | FUTEX2_NUMA,
            reserved: 0,
        };
        let plan = plan_requeue(endpoint, endpoint, 0, 1, 1).unwrap();
        assert!(plan.flags.numa);
        assert_eq!(plan.flags.word_bytes(), 8);
        assert_eq!(
            plan_requeue(
                endpoint,
                FutexWaitV {
                    uaddr: 0x1004,
                    ..endpoint
                },
                0,
                1,
                1
            ),
            Err(FutexError::InvalidAddress)
        );
    }
}

#[cfg(test)]
mod wake_op_tests {
    use super::*;

    fn encoded(op: u32, cmp: u32, arg: i32, compare: i32) -> u32 {
        (op << 28) | (cmp << 24) | ((arg as u32 & 0xfff) << 12) | (compare as u32 & 0xfff)
    }

    #[test]
    fn wake_op_legacy_plan_preserves_zero_and_signed_wake_limits() {
        assert_eq!(wake_op_count(0), 1);
        assert_eq!(wake_op_count(u32::MAX), 1);
        assert_eq!(wake_op_count(7), 7);
        assert!(matches!(
            plan_legacy(
                0x1000,
                FUTEX_WAKE_OP | FUTEX_PRIVATE_FLAG,
                0,
                None,
                0x2000,
                encoded(1, 0, 1, 0)
            ),
            Ok(LegacyPlan::WakeOp {
                wake: 1,
                wake2: 1,
                private: true,
                ..
            })
        ));
    }

    #[test]
    fn wake_op_decodes_signed_arguments_and_all_operations() {
        for (op, expected) in [(0, u32::MAX), (1, 4), (2, u32::MAX), (3, 0), (4, !5)] {
            let operation = WakeOp::decode(encoded(op, 0, -1, -1)).unwrap();
            assert_eq!(operation.updated(5), expected);
            assert_eq!(operation.compare(u32::MAX), Ok(true));
        }
        assert_eq!(
            WakeOp::decode(encoded(1, 0, 1, 0))
                .unwrap()
                .updated(u32::MAX),
            0
        );
    }

    #[test]
    fn wake_op_comparisons_use_signed_pre_update_value() {
        for (cmp, expected) in [
            (0, false),
            (1, true),
            (2, true),
            (3, true),
            (4, false),
            (5, false),
        ] {
            assert_eq!(
                WakeOp::decode(encoded(0, cmp, 0, 0))
                    .unwrap()
                    .compare(u32::MAX),
                Ok(expected)
            );
        }
        for shift in [-1, 31, 63] {
            assert_eq!(
                WakeOp::decode(encoded(8, 0, shift, 0)).unwrap().updated(0),
                1 << 31
            );
        }
        assert_eq!(WakeOp::decode(encoded(8, 0, 32, 0)).unwrap().updated(0), 1);
        assert_eq!(
            WakeOp::decode(encoded(7, 0, 0, 0)),
            Err(FutexError::InvalidCommand)
        );
        let invalid_compare = WakeOp::decode(encoded(0, 15, 7, 0)).unwrap();
        assert_eq!(invalid_compare.updated(1), 7);
        assert_eq!(invalid_compare.compare(1), Err(FutexError::InvalidCommand));
    }
}

#[cfg(test)]
mod legacy_op_tests {
    use super::*;

    fn command(op: u32) -> Option<FutexCommand> {
        LegacyOp::decode(op).command
    }

    #[test]
    fn opcode_decode_covers_every_linux_op() {
        for (op, expected) in [
            (FUTEX_WAIT, FutexCommand::Wait),
            (FUTEX_WAKE, FutexCommand::Wake),
            (FUTEX_FD, FutexCommand::Fd),
            (FUTEX_REQUEUE, FutexCommand::Requeue),
            (FUTEX_CMP_REQUEUE, FutexCommand::CmpRequeue),
            (FUTEX_WAKE_OP, FutexCommand::WakeOp),
            (FUTEX_LOCK_PI, FutexCommand::LockPi),
            (FUTEX_UNLOCK_PI, FutexCommand::UnlockPi),
            (FUTEX_TRYLOCK_PI, FutexCommand::TrylockPi),
            (FUTEX_WAIT_BITSET, FutexCommand::WaitBitset),
            (FUTEX_WAKE_BITSET, FutexCommand::WakeBitset),
            (FUTEX_WAIT_REQUEUE_PI, FutexCommand::WaitRequeuePi),
            (FUTEX_CMP_REQUEUE_PI, FutexCommand::CmpRequeuePi),
            (FUTEX_LOCK_PI2, FutexCommand::LockPi2),
        ] {
            assert_eq!(command(op), Some(expected), "op {op}");
            assert_eq!(command(op | FUTEX_PRIVATE_FLAG), Some(expected));
            assert!(LegacyOp::decode(op | FUTEX_PRIVATE_FLAG).private);
        }
    }

    #[test]
    fn unknown_high_bits_are_not_masked_away() {
        // Linux's FUTEX_CMD_MASK is the complement of the modifier bits, so
        // bit 11 and above reach the dispatch switch and yield -ENOSYS.
        for op in [0x8000_0001u32, 0x0000_0800, 1 << 11, 0xff00_0000, 14] {
            assert_eq!(command(op), None, "op {op:#x}");
            assert_eq!(LegacyOp::decode(op).command, None);
        }
        // Bits 0..=6 stay in the command field.
        assert_eq!(command(0x0000_0001), Some(FutexCommand::Wake));
    }

    #[test]
    fn realtime_gate_matches_do_futex() {
        for (cmd, allowed) in [
            (FUTEX_WAIT, false),
            (FUTEX_WAKE, false),
            (FUTEX_LOCK_PI, false),
            (FUTEX_TRYLOCK_PI, false),
            (FUTEX_UNLOCK_PI, false),
            (FUTEX_WAIT_BITSET, true),
            (FUTEX_WAIT_REQUEUE_PI, true),
            (FUTEX_LOCK_PI2, true),
        ] {
            let op = LegacyOp::decode(cmd | FUTEX_CLOCK_REALTIME);
            assert_eq!(op.rejected_with_enosys(), !allowed, "cmd {cmd}");
        }
        // Without the flag nothing is rejected by the realtime gate.
        for cmd in [FUTEX_WAIT, FUTEX_LOCK_PI, FUTEX_WAIT_REQUEUE_PI] {
            assert!(!LegacyOp::decode(cmd).rejected_with_enosys());
        }
    }

    #[test]
    fn robust_unlock_gate_matches_do_futex() {
        for (cmd, allowed) in [
            (FUTEX_WAKE, true),
            (FUTEX_WAKE_BITSET, true),
            (FUTEX_UNLOCK_PI, true),
            (FUTEX_WAIT, false),
            (FUTEX_REQUEUE, false),
            (FUTEX_LOCK_PI, false),
            (FUTEX_CMP_REQUEUE_PI, false),
        ] {
            let op = LegacyOp::decode(cmd | FUTEX_ROBUST_UNLOCK);
            assert_eq!(op.rejected_with_enosys(), !allowed, "cmd {cmd}");
        }
        assert!(
            !LegacyOp::decode(FUTEX_UNLOCK_PI | FUTEX_ROBUST_UNLOCK | FUTEX_ROBUST_LIST32)
                .rejected_with_enosys()
        );
    }

    #[test]
    fn timeout_shape_matches_futex_cmd_has_timeout() {
        assert!(LegacyOp::decode(FUTEX_WAIT).has_timeout());
        assert!(LegacyOp::decode(FUTEX_LOCK_PI).has_timeout());
        assert!(LegacyOp::decode(FUTEX_LOCK_PI2).has_timeout());
        assert!(LegacyOp::decode(FUTEX_WAIT_BITSET).has_timeout());
        assert!(LegacyOp::decode(FUTEX_WAIT_REQUEUE_PI).has_timeout());
        for cmd in [
            FUTEX_WAKE,
            FUTEX_WAKE_BITSET,
            FUTEX_REQUEUE,
            FUTEX_CMP_REQUEUE,
            FUTEX_WAKE_OP,
            FUTEX_UNLOCK_PI,
            FUTEX_TRYLOCK_PI,
            FUTEX_CMP_REQUEUE_PI,
        ] {
            assert!(!LegacyOp::decode(cmd).has_timeout(), "cmd {cmd}");
        }
        assert!(LegacyOp::decode(FUTEX_WAIT).timeout_is_relative());
        assert!(!LegacyOp::decode(FUTEX_WAIT_BITSET).timeout_is_relative());
        assert!(!LegacyOp::decode(FUTEX_LOCK_PI).timeout_is_relative());
    }

    #[test]
    fn timeout_clock_follows_do_futex() {
        // do_futex() ORs FLAGS_CLOCKRT into FUTEX_LOCK_PI, so it is always
        // CLOCK_REALTIME and never accepts the userspace flag.
        assert_eq!(
            LegacyOp::decode(FUTEX_LOCK_PI).timeout_clock(),
            Clock::Realtime
        );
        assert_eq!(
            LegacyOp::decode(FUTEX_LOCK_PI2).timeout_clock(),
            Clock::Monotonic
        );
        assert_eq!(
            LegacyOp::decode(FUTEX_LOCK_PI2 | FUTEX_CLOCK_REALTIME).timeout_clock(),
            Clock::Realtime
        );
        assert_eq!(
            LegacyOp::decode(FUTEX_WAIT_REQUEUE_PI).timeout_clock(),
            Clock::Monotonic
        );
        assert_eq!(
            LegacyOp::decode(FUTEX_WAIT).timeout_clock(),
            Clock::Monotonic
        );
    }

    #[test]
    fn legacy_wake_wakes_one_waiter_for_a_non_positive_count() {
        // futex_wake() increments its wake counter before comparing it with
        // nr_wake, so FUTEX_WAKE with 0 or a negative value wakes one waiter.
        assert_eq!(LegacyOp::legacy_wake_count(0), 1);
        assert_eq!(LegacyOp::legacy_wake_count((-1i32) as u32), 1);
        assert_eq!(LegacyOp::legacy_wake_count(i32::MIN as u32), 1);
        assert_eq!(LegacyOp::legacy_wake_count(1), 1);
        assert_eq!(LegacyOp::legacy_wake_count(7), 7);
        assert_eq!(
            LegacyOp::legacy_wake_count(i32::MAX as u32),
            i32::MAX as usize
        );
    }

    #[test]
    fn legacy_plan_uses_the_wake_floor_but_not_for_requeue() {
        assert!(matches!(
            plan_legacy(0x1000, FUTEX_WAKE, 0, None, 0, 0),
            Ok(LegacyPlan::Wake { count: 1, .. })
        ));
        // futex_requeue() compares `++task_count <= nr_wake`, so a zero wake
        // limit genuinely wakes nobody.
        assert!(matches!(
            plan_legacy(
                0x1000,
                FUTEX_REQUEUE,
                0,
                Some(Timeout {
                    seconds: 3,
                    nanos: 0,
                    absolute: true,
                    clock: Clock::Monotonic,
                }),
                0x2000,
                0
            ),
            Ok(LegacyPlan::Requeue {
                wake: 0,
                requeue: 3,
                ..
            })
        ));
    }

    #[test]
    fn negative_requeue_counts_are_invalid_input() {
        let timeout = Some(Timeout {
            seconds: -1,
            nanos: 0,
            absolute: true,
            clock: Clock::Monotonic,
        });
        assert_eq!(
            plan_legacy(0x1000, FUTEX_REQUEUE, 1, timeout, 0x2000, 0),
            Err(FutexError::InvalidCount)
        );
        let timeout = Some(Timeout {
            seconds: 1,
            nanos: 0,
            absolute: true,
            clock: Clock::Monotonic,
        });
        assert_eq!(
            plan_legacy(
                0x1000,
                FUTEX_CMP_REQUEUE,
                (-1i32) as u32,
                timeout,
                0x2000,
                0
            ),
            Err(FutexError::InvalidCount)
        );
    }
}

#[cfg(test)]
mod pi_word_tests {
    use super::*;

    #[test]
    fn pi_word_round_trips_all_bits() {
        for value in [
            0u32,
            1,
            FUTEX_TID_MASK,
            FUTEX_WAITERS,
            FUTEX_OWNER_DIED,
            FUTEX_WAITERS | FUTEX_OWNER_DIED,
            FUTEX_WAITERS | FUTEX_OWNER_DIED | 42,
        ] {
            assert_eq!(PiWord::decode(value).encode(), value, "value {value:#x}");
        }
        let word = PiWord::decode(FUTEX_WAITERS | 7);
        assert!(word.is_owned() && word.waiters && !word.owner_died);
        assert_eq!(word.tid, 7);
    }

    #[test]
    fn lock_pi_deadlock_is_checked_before_the_word_shape() {
        let vpid = 77;
        // A stale FUTEX_WAITERS bit does not hide the self-deadlock.
        for value in [vpid, vpid | FUTEX_WAITERS, vpid | FUTEX_OWNER_DIED] {
            assert_eq!(
                plan_pi_acquire(PiWord::decode(value), vpid, false),
                PiAcquire::Deadlock
            );
        }
    }

    #[test]
    fn lock_pi_takeover_preserves_owner_died_and_honours_set_waiters() {
        let vpid = 77;
        assert_eq!(
            plan_pi_acquire(PiWord::decode(0), vpid, false),
            PiAcquire::TakeOver { new_value: 77 }
        );
        // A stale WAITERS bit alone is dropped on a clean takeover.
        assert_eq!(
            plan_pi_acquire(PiWord::decode(FUTEX_WAITERS), vpid, false),
            PiAcquire::TakeOver { new_value: 77 }
        );
        // OWNER_DIED survives: userspace must still observe the dead owner.
        assert_eq!(
            plan_pi_acquire(
                PiWord::decode(FUTEX_OWNER_DIED | FUTEX_WAITERS),
                vpid,
                false
            ),
            PiAcquire::TakeOver {
                new_value: FUTEX_OWNER_DIED | 77
            }
        );
        // FUTEX_CMP_REQUEUE_PI forces the WAITERS bit for the new owner.
        assert_eq!(
            plan_pi_acquire(PiWord::decode(FUTEX_OWNER_DIED), vpid, true),
            PiAcquire::TakeOver {
                new_value: FUTEX_OWNER_DIED | FUTEX_WAITERS | 77
            }
        );
    }

    #[test]
    fn lock_pi_publishes_waiters_before_attaching_to_a_live_owner() {
        assert_eq!(
            plan_pi_acquire(PiWord::decode(9), 77, false),
            PiAcquire::SetWaiters {
                new_value: 9 | FUTEX_WAITERS
            }
        );
        assert_eq!(
            plan_pi_acquire(PiWord::decode(FUTEX_OWNER_DIED | 9), 77, false),
            PiAcquire::SetWaiters {
                new_value: FUTEX_OWNER_DIED | FUTEX_WAITERS | 9
            }
        );
    }

    #[test]
    fn unlock_pi_only_releases_a_futex_we_own() {
        let vpid = 12;
        assert_eq!(
            plan_pi_unlock(PiWord::decode(13), vpid, None),
            PiUnlock::NotOwner
        );
        // Even with no waiters, a stale WAITERS bit must survive the compare
        // when the word names somebody else.
        assert_eq!(
            plan_pi_unlock(PiWord::decode(FUTEX_WAITERS | 13), vpid, None),
            PiUnlock::NotOwner
        );
    }

    #[test]
    fn unlock_pi_clears_owned_word_and_hands_off_to_the_top_waiter() {
        let vpid = 12;
        assert_eq!(
            plan_pi_unlock(PiWord::decode(vpid), vpid, None),
            PiUnlock::Clear { expected: vpid }
        );
        // The compare value is the exact word we read, including stale bits.
        assert_eq!(
            plan_pi_unlock(PiWord::decode(vpid | FUTEX_WAITERS), vpid, None),
            PiUnlock::Clear {
                expected: vpid | FUTEX_WAITERS
            }
        );
        // Handoff always keeps WAITERS and drops a stale OWNER_DIED.
        assert_eq!(
            plan_pi_unlock(PiWord::decode(vpid | FUTEX_OWNER_DIED), vpid, Some(31)),
            PiUnlock::Handoff {
                new_value: FUTEX_WAITERS | 31
            }
        );
        assert_eq!(pi_handoff_value(31), FUTEX_WAITERS | 31);
    }

    #[test]
    fn fixup_preserves_a_concurrent_owner_died() {
        assert_eq!(pi_fixup_value(0, 5, false), FUTEX_WAITERS | 5);
        assert_eq!(
            pi_fixup_value(FUTEX_OWNER_DIED, 5, false),
            FUTEX_OWNER_DIED | FUTEX_WAITERS | 5
        );
        assert_eq!(
            pi_fixup_value(0, 5, true),
            FUTEX_OWNER_DIED | FUTEX_WAITERS | 5
        );
    }
}
