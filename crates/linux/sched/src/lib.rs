#![no_std]
#![forbid(unsafe_code)]
//! Pure, side-effect-free Linux `sched_setattr` policy.

pub const SCHED_ATTR_SIZE_VER0: u32 = 48;
pub const SCHED_ATTR_SIZE: u32 = 56;
pub const SCHED_ATTR_MAX_SIZE: u32 = 4096;
pub const SCHED_NORMAL: u32 = 0;
pub const SCHED_FIFO: u32 = 1;
pub const SCHED_RR: u32 = 2;
pub const SCHED_BATCH: u32 = 3;
pub const SCHED_IDLE: u32 = 5;
pub const SCHED_DEADLINE: u32 = 6;
pub const SCHED_RESET_ON_FORK: u32 = 0x4000_0000;
pub const SCHED_FLAG_RESET_ON_FORK: u64 = 0x01;
pub const SCHED_FLAG_RECLAIM: u64 = 0x02;
pub const SCHED_FLAG_DL_OVERRUN: u64 = 0x04;
pub const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
pub const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
pub const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;
pub const SCHED_FLAG_UTIL_CLAMP_MAX: u64 = 0x40;
pub const SCHED_FLAG_ALL: u64 = SCHED_FLAG_RESET_ON_FORK
    | SCHED_FLAG_RECLAIM
    | SCHED_FLAG_DL_OVERRUN
    | SCHED_FLAG_KEEP_POLICY
    | SCHED_FLAG_KEEP_PARAMS
    | SCHED_FLAG_UTIL_CLAMP_MIN
    | SCHED_FLAG_UTIL_CLAMP_MAX;
pub const UCLAMP_SCALE: u32 = 1024;
/// Linux's per-side `sched_util_{min,max}` reset marker.
pub const UCLAMP_RESET: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SchedAttr {
    pub size: u32,
    pub policy: u32,
    pub flags: u64,
    pub nice: i32,
    pub priority: u32,
    pub runtime: u64,
    pub deadline: u64,
    pub period: u64,
    pub util_min: u32,
    pub util_max: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedInput {
    pub attr: SchedAttr,
    pub supplied_size: u32,
    pub tail_nonzero: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedSnapshot {
    pub policy: u32,
    pub nice: i32,
    pub priority: u32,
    pub reset_on_fork: bool,
    pub util_min: u32,
    pub util_max: u32,
    /// Whether each stored request is explicit rather than inherited from the
    /// scheduling class default.  The values remain meaningful even while the
    /// corresponding bit is clear; this makes a class transition lossless.
    pub util_min_user_defined: bool,
    pub util_max_user_defined: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FeatureSet {
    pub deadline: bool,
    pub util_clamp: bool,
    pub reclaim: bool,
    pub dl_overrun: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeadlineParams {
    pub runtime: u64,
    pub deadline: u64,
    pub period: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedUpdatePlan {
    pub policy: u32,
    pub nice: i32,
    pub priority: u32,
    pub deadline: Option<DeadlineParams>,
    pub util_min: u32,
    pub util_max: u32,
    /// Requested values and ownership bits, before class/cgroup/runqueue
    /// policy turns them into effective clamps.
    pub uclamp: UclampRequest,
    pub reset_on_fork: bool,
    pub reclaim: bool,
    pub dl_overrun: bool,
}

/// Per-task uclamp request as represented by Linux's sched attribute ABI.
///
/// `min` and `max` are independently owned.  In particular, a request is
/// allowed to be temporarily inverted while changing class; consumers must
/// use [`UclampRequest::effective`] before programming a scheduler or HWP.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UclampRequest {
    pub min: u32,
    pub max: u32,
    pub min_user_defined: bool,
    pub max_user_defined: bool,
}

impl UclampRequest {
    pub const fn class_default(policy: u32) -> Self {
        if matches!(policy, SCHED_FIFO | SCHED_RR) {
            Self {
                min: UCLAMP_SCALE,
                max: UCLAMP_SCALE,
                min_user_defined: false,
                max_user_defined: false,
            }
        } else {
            Self {
                min: 0,
                max: UCLAMP_SCALE,
                min_user_defined: false,
                max_user_defined: false,
            }
        }
    }

    pub const fn effective(self, policy: u32) -> EffectiveUclamp {
        let default = Self::class_default(policy);
        let min = if self.min_user_defined {
            self.min
        } else {
            default.min
        };
        let max = if self.max_user_defined {
            self.max
        } else {
            default.max
        };
        // A class transition may briefly produce a requested min above max.
        // Keep the restriction (the lower maximum) rather than exposing an
        // invalid pair to an effective consumer.
        EffectiveUclamp {
            min: if min > max { max } else { min },
            max,
        }
    }

    /// The child-side request after RESET_ON_FORK returns the child to its
    /// post-reset normal scheduling class.
    pub const fn reset_on_fork(self, reset: bool) -> Self {
        if reset {
            Self::class_default(SCHED_NORMAL)
        } else {
            self
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectiveUclamp {
    pub min: u32,
    pub max: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reject {
    SizeTooSmall,
    SizeTooLarge,
    NonZeroTail,
    UnknownFlags,
    InvalidPolicy,
    InvalidPriority,
    InvalidNice,
    InvalidDeadline,
    InvalidUclamp,
    MissingUtilClampFields,
    UnsupportedDeadline,
    UnsupportedUclamp,
    UnsupportedFlag,
}

pub fn plan(
    input: SchedInput,
    before: SchedSnapshot,
    features: FeatureSet,
) -> Result<SchedUpdatePlan, Reject> {
    let size = if input.supplied_size == 0 {
        SCHED_ATTR_SIZE_VER0
    } else {
        input.supplied_size
    };
    if size < SCHED_ATTR_SIZE_VER0 {
        return Err(Reject::SizeTooSmall);
    }
    if size > SCHED_ATTR_MAX_SIZE {
        return Err(Reject::SizeTooLarge);
    }
    if size > SCHED_ATTR_SIZE && input.tail_nonzero {
        return Err(Reject::NonZeroTail);
    }
    if input.attr.flags & !SCHED_FLAG_ALL != 0 {
        return Err(Reject::UnknownFlags);
    }
    let mut policy = input.attr.policy;
    let reset_in_policy = policy & SCHED_RESET_ON_FORK != 0;
    policy &= !SCHED_RESET_ON_FORK;
    if !matches!(
        policy,
        SCHED_NORMAL | SCHED_FIFO | SCHED_RR | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE
    ) {
        return Err(Reject::InvalidPolicy);
    }
    if input.attr.flags & SCHED_FLAG_KEEP_POLICY != 0 {
        policy = before.policy;
    }
    let keep_params = input.attr.flags & SCHED_FLAG_KEEP_PARAMS != 0;
    let nice = if keep_params {
        before.nice
    } else {
        input.attr.nice
    };
    let priority = if keep_params {
        before.priority
    } else {
        input.attr.priority
    };
    if !(matches!(policy, SCHED_FIFO | SCHED_RR) && (1..=99).contains(&priority))
        && !(!matches!(policy, SCHED_FIFO | SCHED_RR) && priority == 0)
    {
        return Err(Reject::InvalidPriority);
    }
    if !(-20..=19).contains(&nice) {
        return Err(Reject::InvalidNice);
    }
    let deadline = if policy == SCHED_DEADLINE {
        if !features.deadline {
            return Err(Reject::UnsupportedDeadline);
        }
        if keep_params {
            None
        } else {
            if priority != 0
                || input.attr.runtime == 0
                || input.attr.deadline == 0
                || input.attr.runtime > input.attr.deadline
                || (input.attr.period != 0 && input.attr.deadline > input.attr.period)
            {
                return Err(Reject::InvalidDeadline);
            }
            Some(DeadlineParams {
                runtime: input.attr.runtime,
                deadline: input.attr.deadline,
                period: input.attr.period,
            })
        }
    } else {
        None
    };
    let uclamp_requested =
        input.attr.flags & (SCHED_FLAG_UTIL_CLAMP_MIN | SCHED_FLAG_UTIL_CLAMP_MAX) != 0;
    if uclamp_requested && size < SCHED_ATTR_SIZE {
        return Err(Reject::MissingUtilClampFields);
    }
    // sched_{set,get}attr keeps the uclamp ABI available even when a runtime
    // scheduler/HWP consumer is unavailable.  `features.util_clamp` is kept
    // in FeatureSet for source compatibility, but is deliberately not an ABI
    // gate here.
    let _ = features.util_clamp;
    let old_request = UclampRequest {
        min: before.util_min,
        max: before.util_max,
        min_user_defined: before.util_min_user_defined,
        max_user_defined: before.util_max_user_defined,
    };
    let class_default = UclampRequest::class_default(policy);
    let min_requested = input.attr.flags & SCHED_FLAG_UTIL_CLAMP_MIN != 0;
    let max_requested = input.attr.flags & SCHED_FLAG_UTIL_CLAMP_MAX != 0;
    let uclamp = UclampRequest {
        min: if min_requested && input.attr.util_min == UCLAMP_RESET {
            class_default.min
        } else if min_requested {
            input.attr.util_min
        } else {
            old_request.min
        },
        max: if max_requested && input.attr.util_max == UCLAMP_RESET {
            class_default.max
        } else if max_requested {
            input.attr.util_max
        } else {
            old_request.max
        },
        min_user_defined: if min_requested {
            input.attr.util_min != UCLAMP_RESET
        } else {
            old_request.min_user_defined
        },
        max_user_defined: if max_requested {
            input.attr.util_max != UCLAMP_RESET
        } else {
            old_request.max_user_defined
        },
    };
    if (uclamp.min_user_defined && uclamp.min > UCLAMP_SCALE)
        || (uclamp.max_user_defined && uclamp.max > UCLAMP_SCALE)
    {
        return Err(Reject::InvalidUclamp);
    }
    let effective_uclamp = uclamp.effective(policy);
    if input.attr.flags & SCHED_FLAG_RECLAIM != 0 && !features.reclaim {
        return Err(Reject::UnsupportedFlag);
    }
    if input.attr.flags & SCHED_FLAG_DL_OVERRUN != 0 && !features.dl_overrun {
        return Err(Reject::UnsupportedFlag);
    }
    Ok(SchedUpdatePlan {
        policy,
        nice,
        priority,
        deadline,
        util_min: effective_uclamp.min,
        util_max: effective_uclamp.max,
        uclamp,
        reset_on_fork: reset_in_policy || input.attr.flags & SCHED_FLAG_RESET_ON_FORK != 0,
        reclaim: input.attr.flags & SCHED_FLAG_RECLAIM != 0,
        dl_overrun: input.attr.flags & SCHED_FLAG_DL_OVERRUN != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snap() -> SchedSnapshot {
        SchedSnapshot {
            policy: SCHED_NORMAL,
            nice: 0,
            priority: 0,
            reset_on_fork: false,
            util_min: 0,
            util_max: UCLAMP_SCALE,
            util_min_user_defined: false,
            util_max_user_defined: false,
        }
    }
    fn input() -> SchedInput {
        SchedInput {
            attr: SchedAttr {
                size: SCHED_ATTR_SIZE,
                ..SchedAttr::default()
            },
            supplied_size: SCHED_ATTR_SIZE,
            tail_nonzero: false,
        }
    }
    #[test]
    fn validation_order_and_tail() {
        let mut x = input();
        x.supplied_size = 47;
        x.attr.flags = !0;
        assert_eq!(
            plan(x, snap(), FeatureSet::default()),
            Err(Reject::SizeTooSmall)
        );
        x.supplied_size = 57;
        x.tail_nonzero = true;
        assert_eq!(
            plan(x, snap(), FeatureSet::default()),
            Err(Reject::NonZeroTail)
        );
    }
    #[test]
    fn deadline_and_uclamp_boundaries() {
        let mut x = input();
        x.attr.policy = SCHED_DEADLINE;
        x.attr.runtime = 1;
        x.attr.deadline = 1;
        assert_eq!(
            plan(x, snap(), FeatureSet::default()),
            Err(Reject::UnsupportedDeadline)
        );
        let mut x = input();
        x.attr.flags = SCHED_FLAG_UTIL_CLAMP_MIN | SCHED_FLAG_UTIL_CLAMP_MAX;
        x.attr.util_min = 1024;
        x.attr.util_max = 1024;
        assert!(
            plan(
                x,
                snap(),
                FeatureSet {
                    util_clamp: true,
                    ..FeatureSet::default()
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn deadline_keep_params_ignores_supplied_deadline_tuple() {
        let mut x = input();
        x.attr.flags = SCHED_FLAG_KEEP_PARAMS;
        x.attr.policy = SCHED_DEADLINE;
        let mut before = snap();
        before.policy = SCHED_DEADLINE;
        before.priority = 0;
        assert_eq!(
            plan(
                x,
                before,
                FeatureSet {
                    deadline: true,
                    ..FeatureSet::default()
                },
            )
            .unwrap()
            .deadline,
            None
        );
    }

    #[test]
    fn uclamp_requires_extended_attribute_prefix() {
        let mut x = input();
        x.supplied_size = SCHED_ATTR_SIZE_VER0;
        x.attr.flags = SCHED_FLAG_UTIL_CLAMP_MIN;
        assert_eq!(
            plan(
                x,
                snap(),
                FeatureSet {
                    util_clamp: true,
                    ..FeatureSet::default()
                },
            ),
            Err(Reject::MissingUtilClampFields)
        );
    }

    #[test]
    fn uclamp_clear_is_independent_per_side() {
        let mut before = snap();
        before.util_min = 300;
        before.util_max = 700;
        before.util_min_user_defined = true;
        before.util_max_user_defined = true;
        let mut x = input();
        x.attr.flags = SCHED_FLAG_UTIL_CLAMP_MIN;
        x.attr.util_min = UCLAMP_RESET;
        let plan = plan(x, before, FeatureSet::default()).unwrap();
        assert_eq!(plan.uclamp.min, 0);
        assert!(!plan.uclamp.min_user_defined);
        assert_eq!(plan.uclamp.max, 700);
        assert!(plan.uclamp.max_user_defined);
        assert_eq!((plan.util_min, plan.util_max), (0, 700));
    }

    #[test]
    fn rt_defaults_and_class_transition_keep_requested_state() {
        let mut before = snap();
        before.util_max = 512;
        before.util_max_user_defined = true;
        let mut x = input();
        x.attr.policy = SCHED_FIFO;
        x.attr.priority = 1;
        let fifo_plan = plan(x, before, FeatureSet::default()).unwrap();
        // The requested RT default minimum and the old explicit maximum are
        // intentionally inverted; the effective pair stays scheduler-safe.
        assert_eq!((fifo_plan.uclamp.min, fifo_plan.uclamp.max), (0, 512));
        assert_eq!((fifo_plan.util_min, fifo_plan.util_max), (512, 512));

        let mut rt = snap();
        rt.policy = SCHED_FIFO;
        rt.priority = 1;
        rt.util_min = UCLAMP_SCALE;
        rt.util_max = UCLAMP_SCALE;
        let mut x = input();
        x.attr.flags = SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS;
        let plan = plan(x, rt, FeatureSet::default()).unwrap();
        assert_eq!((plan.util_min, plan.util_max), (UCLAMP_SCALE, UCLAMP_SCALE));
    }

    #[test]
    fn keep_policy_and_reset_on_fork_preserve_abi_state() {
        let mut before = snap();
        before.policy = SCHED_FIFO;
        before.priority = 1;
        before.util_min = UCLAMP_SCALE;
        let mut x = input();
        x.attr.flags = SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS | SCHED_FLAG_RESET_ON_FORK;
        let plan = plan(x, before, FeatureSet::default()).unwrap();
        assert_eq!(plan.policy, SCHED_FIFO);
        assert!(plan.reset_on_fork);
        assert_eq!(
            plan.uclamp.reset_on_fork(plan.reset_on_fork),
            UclampRequest::class_default(SCHED_NORMAL)
        );
    }

    #[test]
    fn uclamp_setter_is_feature_independent() {
        let mut x = input();
        x.attr.flags = SCHED_FLAG_UTIL_CLAMP_MAX;
        x.attr.util_max = 600;
        assert_eq!(
            plan(x, snap(), FeatureSet::default()).unwrap().util_max,
            600
        );
    }
}
