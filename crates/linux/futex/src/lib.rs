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
