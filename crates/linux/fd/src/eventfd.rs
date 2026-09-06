//! Pure eventfd counter and file-lease transition policy.
#![allow(missing_docs)]

use crate::ReadyMask;

/// Linux's largest representable eventfd counter value.
pub const EVENTFD_COUNTER_MAX: u64 = u64::MAX - 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventFdError {
    Overflow,
    WouldBlock,
    InvalidValue,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventFdSnapshot {
    counter: u64,
    semaphore: bool,
}
impl EventFdSnapshot {
    pub const fn new(counter: u64, semaphore: bool) -> Result<Self, EventFdError> {
        if counter > EVENTFD_COUNTER_MAX {
            Err(EventFdError::InvalidValue)
        } else {
            Ok(Self { counter, semaphore })
        }
    }
    pub const fn counter(self) -> u64 {
        self.counter
    }
    pub const fn semaphore(self) -> bool {
        self.semaphore
    }
    pub const fn readiness(self) -> ReadyMask {
        let mut bits = 0;
        if self.counter != 0 {
            bits |= ReadyMask::IN.bits();
        }
        if self.counter < EVENTFD_COUNTER_MAX {
            bits |= ReadyMask::OUT.bits();
        }
        ReadyMask::from_bits_retain(bits)
    }
    pub fn plan_write(self, value: u64) -> Result<EventFdPlan, EventFdError> {
        if value == u64::MAX {
            return Err(EventFdError::InvalidValue);
        }
        let counter = self
            .counter
            .checked_add(value)
            .filter(|v| *v <= EVENTFD_COUNTER_MAX)
            .ok_or(EventFdError::Overflow)?;
        Ok(EventFdPlan::Write {
            before: self,
            after: Self { counter, ..self },
        })
    }
    pub fn plan_read(self) -> Result<EventFdPlan, EventFdError> {
        if self.counter == 0 {
            return Err(EventFdError::WouldBlock);
        }
        let value = if self.semaphore { 1 } else { self.counter };
        Ok(EventFdPlan::Read {
            before: self,
            value,
            after: Self {
                counter: self.counter - value,
                ..self
            },
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventFdPlan {
    Write {
        before: EventFdSnapshot,
        after: EventFdSnapshot,
    },
    Read {
        before: EventFdSnapshot,
        value: u64,
        after: EventFdSnapshot,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct LeaseId(u64);
impl LeaseId {
    pub const fn new(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseType {
    Read,
    Write,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeaseSnapshot {
    pub lease: Option<(LeaseId, LeaseType)>,
    pub breaking: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseError {
    Busy,
    AlreadyBreaking,
    NotOwner,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeasePlan {
    Admit {
        lease: LeaseId,
        kind: LeaseType,
        after: LeaseSnapshot,
    },
    Break {
        lease: LeaseId,
        after: LeaseSnapshot,
    },
    Release {
        lease: LeaseId,
        after: LeaseSnapshot,
    },
}
impl LeaseSnapshot {
    pub const fn empty() -> Self {
        Self {
            lease: None,
            breaking: false,
        }
    }
    pub fn plan_admit(self, lease: LeaseId, kind: LeaseType) -> Result<LeasePlan, LeaseError> {
        if self.lease.is_some() || self.breaking {
            Err(LeaseError::Busy)
        } else {
            Ok(LeasePlan::Admit {
                lease,
                kind,
                after: Self {
                    lease: Some((lease, kind)),
                    breaking: false,
                },
            })
        }
    }
    pub fn plan_break(self, lease: LeaseId) -> Result<LeasePlan, LeaseError> {
        if self.breaking {
            return Err(LeaseError::AlreadyBreaking);
        }
        if self.lease.map(|v| v.0) != Some(lease) {
            return Err(LeaseError::NotOwner);
        }
        Ok(LeasePlan::Break {
            lease,
            after: Self {
                breaking: true,
                ..self
            },
        })
    }
    pub fn plan_release(self, lease: LeaseId) -> Result<LeasePlan, LeaseError> {
        if self.lease.map(|v| v.0) != Some(lease) {
            Err(LeaseError::NotOwner)
        } else {
            Ok(LeasePlan::Release {
                lease,
                after: Self::empty(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn eventfd_readiness_and_semaphore_transition() {
        let state = EventFdSnapshot::new(2, true).unwrap();
        assert_eq!(
            state.readiness().bits(),
            ReadyMask::IN.bits() | ReadyMask::OUT.bits()
        );
        assert!(
            matches!(state.plan_read().unwrap(), EventFdPlan::Read { value: 1, after, .. } if after.counter() == 1)
        );
        assert!(
            EventFdSnapshot::new(EVENTFD_COUNTER_MAX, false)
                .unwrap()
                .plan_write(1)
                .is_err()
        );
    }
    #[test]
    fn lease_break_is_explicit_and_owner_checked() {
        let id = LeaseId::new(1).unwrap();
        let state = match LeaseSnapshot::empty()
            .plan_admit(id, LeaseType::Write)
            .unwrap()
        {
            LeasePlan::Admit { after, .. } => after,
            _ => panic!(),
        };
        let breaking = match state.plan_break(id).unwrap() {
            LeasePlan::Break { after, .. } => after,
            _ => panic!(),
        };
        assert!(breaking.breaking);
        assert!(
            breaking
                .plan_admit(LeaseId::new(2).unwrap(), LeaseType::Read)
                .is_err()
        );
    }
}
