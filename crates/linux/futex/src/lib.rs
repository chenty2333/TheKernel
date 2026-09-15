//! Pure futex ABI decoding. Queueing, user-memory faults, and restart state are external.
#![no_std]
#![forbid(unsafe_code)]

pub const FUTEX_PRIVATE_FLAG: u32 = 128;
pub const FUTEX_CLOCK_REALTIME: u32 = 256;
pub const FUTEX_CMD_MASK: u32 = 0x7f;
pub const FUTEX_WAIT: u32 = 0;
pub const FUTEX_WAKE: u32 = 1;
pub const FUTEX_REQUEUE: u32 = 3;
pub const FUTEX_CMP_REQUEUE: u32 = 4;
pub const FUTEX_WAKE_OP: u32 = 5;
pub const FUTEX_WAIT_BITSET: u32 = 9;
pub const FUTEX_WAKE_BITSET: u32 = 10;
pub const FUTEX2_SIZE_U32: u32 = 2;
pub const FUTEX2_PRIVATE: u32 = 128;
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FutexWord {
    pub address: usize,
    pub expected: u32,
    pub private: bool,
}
impl FutexWord {
    pub const fn new(address: usize, expected: u32, private: bool) -> Result<Self, FutexError> {
        if address & 3 != 0 {
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
    let command = op & FUTEX_CMD_MASK;
    let private = op & FUTEX_PRIVATE_FLAG != 0;
    let realtime = op & FUTEX_CLOCK_REALTIME != 0;
    if op & !(FUTEX_CMD_MASK | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME) != 0 {
        return Err(FutexError::InvalidFlags);
    }
    if address & 3 != 0 {
        return Err(FutexError::InvalidAddress);
    }
    let clock = if realtime {
        Clock::Realtime
    } else {
        Clock::Monotonic
    };
    match command {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let bitset = if command == FUTEX_WAIT {
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
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let bitset = if command == FUTEX_WAKE {
                u32::MAX
            } else {
                value3
            };
            if bitset == 0 {
                return Err(FutexError::InvalidValue);
            }
            Ok(LegacyPlan::Wake {
                address,
                count: legacy_count(value),
                bitset,
                private,
            })
        }
        FUTEX_WAKE_OP => {
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
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            if address2 & 3 != 0 {
                return Err(FutexError::InvalidAddress);
            }
            Ok(LegacyPlan::Requeue {
                source: address,
                target: address2,
                wake: legacy_count(value),
                requeue: legacy_count(timeout.map_or(0, |v| v.seconds as u32)),
                compare: if command == FUTEX_CMP_REQUEUE {
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

const fn legacy_count(v: u32) -> usize {
    if (v as i32) < 0 { 1 } else { v as usize }
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Futex2Flags {
    pub private: bool,
}
pub const fn parse_futex2_flags(flags: u32) -> Result<Futex2Flags, FutexError> {
    if flags & !(FUTEX2_SIZE_U32 | FUTEX2_PRIVATE) != 0
        || flags & FUTEX2_SIZE_U32 != FUTEX2_SIZE_U32
    {
        Err(FutexError::InvalidFlags)
    } else {
        Ok(Futex2Flags {
            private: flags & FUTEX2_PRIVATE != 0,
        })
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
}

fn requeue_word(waiter: FutexWaitV) -> Result<FutexWord, FutexError> {
    if waiter.reserved != 0 || waiter.val > u32::MAX as u64 {
        return Err(FutexError::InvalidValue);
    }
    let flags = parse_futex2_flags(waiter.flags)?;
    FutexWord::new(waiter.uaddr as usize, waiter.val as u32, flags.private)
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
    let source = requeue_word(source)?;
    let target = requeue_word(target)?;
    if source_flags != target_flags {
        return Err(FutexError::InvalidFlags);
    }
    if wake < 0 || requeue < 0 {
        return Err(FutexError::InvalidCount);
    }
    Ok(Futex2RequeuePlan {
        source,
        target,
        wake: wake as usize,
        requeue: requeue as usize,
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
    fn legacy_flags_first() {
        assert_eq!(
            plan_legacy(3, 0x8000, 0, None, 0, 0),
            Err(FutexError::InvalidFlags)
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
