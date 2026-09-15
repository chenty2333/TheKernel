//! `pidfd_send_signal(2)` target/scope decoding, mirroring Linux
//! `kernel/signal.c:SYSCALL_DEFINE4(pidfd_send_signal, ...)`.
//!
//! ```text
//! #define PIDFD_SEND_SIGNAL_FLAGS                            \
//! 	(PIDFD_SIGNAL_THREAD | PIDFD_SIGNAL_THREAD_GROUP | \
//! 	 PIDFD_SIGNAL_PROCESS_GROUP)
//!
//! 	/* Enforce flags be set to 0 until we add an extension. */
//! 	if (flags & ~PIDFD_SEND_SIGNAL_FLAGS)
//! 		return -EINVAL;
//!
//! 	/* Ensure that only a single signal scope determining flag is set. */
//! 	if (hweight32(flags & PIDFD_SEND_SIGNAL_FLAGS) > 1)
//! 		return -EINVAL;
//!
//! 	switch (pidfd) {
//! 	case PIDFD_SELF_THREAD:
//! 		pid = get_task_pid(current, PIDTYPE_PID);
//! 		type = PIDTYPE_PID;
//! 		break;
//! 	case PIDFD_SELF_THREAD_GROUP:
//! 		pid = get_task_pid(current, PIDTYPE_TGID);
//! 		type = PIDTYPE_TGID;
//! 		break;
//! 	default: {
//! 		...
//! 		if (fd_file(f)->f_flags & PIDFD_THREAD)
//! 			type = PIDTYPE_PID;
//! 		else
//! 			type = PIDTYPE_TGID;
//! 		return do_pidfd_send_signal(pid, sig, type, info, flags);
//! 	}
//! 	}
//! ```
//!
//! `PIDFD_SELF_THREAD` / `PIDFD_SELF_THREAD_GROUP` are *negative magic
//! descriptors* defined in `include/uapi/linux/fcntl.h`; they never reach the
//! descriptor table. The `flags` word is validated before any descriptor
//! lookup happens, so an unknown or ambiguous flag word is `-EINVAL` even for
//! a bad `pidfd`.

/// `PIDFD_SIGNAL_THREAD` — deliver to the addressed thread only.
pub const PIDFD_SIGNAL_THREAD: u32 = 1 << 0;
/// `PIDFD_SIGNAL_THREAD_GROUP` — deliver to the addressed thread group.
pub const PIDFD_SIGNAL_THREAD_GROUP: u32 = 1 << 1;
/// `PIDFD_SIGNAL_PROCESS_GROUP` — deliver to the addressed process group.
pub const PIDFD_SIGNAL_PROCESS_GROUP: u32 = 1 << 2;

/// Linux's `PIDFD_SEND_SIGNAL_FLAGS`.
pub const PIDFD_SEND_SIGNAL_FLAGS: u32 =
    PIDFD_SIGNAL_THREAD | PIDFD_SIGNAL_THREAD_GROUP | PIDFD_SIGNAL_PROCESS_GROUP;

/// `PIDFD_SELF_THREAD` from `include/uapi/linux/fcntl.h` (aliased as
/// `PIDFD_SELF` in `include/uapi/linux/pidfd.h`).
pub const PIDFD_SELF_THREAD: i32 = -10000;
/// `PIDFD_SELF_THREAD_GROUP` from `include/uapi/linux/fcntl.h` (aliased as
/// `PIDFD_SELF_PROCESS`).
pub const PIDFD_SELF_THREAD_GROUP: i32 = -10001;

/// Linux's `enum pid_type` ordering, which the siginfo `-EPERM` rule compares
/// against (`type > PIDTYPE_TGID`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SignalScope {
    /// `PIDTYPE_PID`.
    Thread,
    /// `PIDTYPE_TGID`.
    ThreadGroup,
    /// `PIDTYPE_PGID`.
    ProcessGroup,
}

impl SignalScope {
    /// Whether `type > PIDTYPE_TGID` holds for this scope, i.e. the delivery
    /// is a process-group delivery.
    pub const fn is_process_group(self) -> bool {
        matches!(self, Self::ProcessGroup)
    }
}

/// Which object a `pidfd_send_signal` call addresses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalTarget {
    /// `pidfd == PIDFD_SELF_THREAD`: the calling thread, resolved without a
    /// descriptor lookup.
    SelfThread,
    /// `pidfd == PIDFD_SELF_THREAD_GROUP`: the calling thread group leader.
    SelfThreadGroup,
    /// Any other value: a real descriptor, resolved through the fd table.
    Descriptor,
}

/// Fully resolved `pidfd_send_signal` request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PidfdSignalPlan {
    /// The addressed object.
    pub target: SignalTarget,
    /// The final `enum pid_type` after the flag override.
    pub scope: SignalScope,
}

/// The only pre-delivery rejection this decoder produces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PidfdSignalError {
    /// `-EINVAL` for an unknown flag bit or for two scope flags at once.
    InvalidInput,
}

/// Decodes `pidfd` and `flags` into a delivery plan.
///
/// `descriptor_default_scope` is the scope a plain pidfd implies: Linux uses
/// `PIDTYPE_PID` when the descriptor carries `PIDFD_THREAD` and `PIDTYPE_TGID`
/// otherwise. It is ignored for the two self identifiers, which have their own
/// fixed defaults.
pub const fn pidfd_signal_plan(
    pidfd: i32,
    flags: u32,
    descriptor_default_scope: SignalScope,
) -> Result<PidfdSignalPlan, PidfdSignalError> {
    if flags & !PIDFD_SEND_SIGNAL_FLAGS != 0 {
        return Err(PidfdSignalError::InvalidInput);
    }
    if (flags & PIDFD_SEND_SIGNAL_FLAGS).count_ones() > 1 {
        return Err(PidfdSignalError::InvalidInput);
    }

    let (target, default_scope) = match pidfd {
        PIDFD_SELF_THREAD => (SignalTarget::SelfThread, SignalScope::Thread),
        PIDFD_SELF_THREAD_GROUP => (SignalTarget::SelfThreadGroup, SignalScope::ThreadGroup),
        _ => (SignalTarget::Descriptor, descriptor_default_scope),
    };
    // `do_pidfd_send_signal()`'s `switch (flags)` overrides the inferred type.
    let scope = match flags {
        PIDFD_SIGNAL_THREAD => SignalScope::Thread,
        PIDFD_SIGNAL_THREAD_GROUP => SignalScope::ThreadGroup,
        PIDFD_SIGNAL_PROCESS_GROUP => SignalScope::ProcessGroup,
        _ => default_scope,
    };
    Ok(PidfdSignalPlan { target, scope })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESCRIPTOR_TGID: SignalScope = SignalScope::ThreadGroup;

    fn plan(pidfd: i32, flags: u32) -> Result<PidfdSignalPlan, PidfdSignalError> {
        pidfd_signal_plan(pidfd, flags, DESCRIPTOR_TGID)
    }

    #[test]
    fn scope_flag_values_match_linux() {
        assert_eq!(PIDFD_SIGNAL_THREAD, 1);
        assert_eq!(PIDFD_SIGNAL_THREAD_GROUP, 2);
        assert_eq!(PIDFD_SIGNAL_PROCESS_GROUP, 4);
        assert_eq!(PIDFD_SEND_SIGNAL_FLAGS, 7);
        assert_eq!(PIDFD_SELF_THREAD, -10000);
        assert_eq!(PIDFD_SELF_THREAD_GROUP, -10001);
    }

    #[test]
    fn unknown_bits_and_multiple_scope_flags_are_einval_before_any_lookup() {
        // An unknown bit is rejected even with a nonsense descriptor.
        assert_eq!(plan(-1, 8), Err(PidfdSignalError::InvalidInput));
        assert_eq!(plan(i32::MAX, 1 << 31), Err(PidfdSignalError::InvalidInput));
        // Two scope flags at once.
        for flags in [3u32, 5, 6, 7] {
            assert_eq!(plan(3, flags), Err(PidfdSignalError::InvalidInput));
        }
        // Zero and each single scope flag are accepted.
        for flags in [0u32, 1, 2, 4] {
            assert!(plan(3, flags).is_ok());
        }
    }

    #[test]
    fn self_identifiers_bypass_the_descriptor_and_keep_their_default_scope() {
        // PIDFD_SELF_THREAD defaults to the calling thread even though a plain
        // descriptor would default to the thread group.
        assert_eq!(
            plan(PIDFD_SELF_THREAD, 0),
            Ok(PidfdSignalPlan {
                target: SignalTarget::SelfThread,
                scope: SignalScope::Thread,
            })
        );
        assert_eq!(
            plan(PIDFD_SELF_THREAD_GROUP, 0),
            Ok(PidfdSignalPlan {
                target: SignalTarget::SelfThreadGroup,
                scope: SignalScope::ThreadGroup,
            })
        );
        assert_eq!(
            plan(0, 0),
            Ok(PidfdSignalPlan {
                target: SignalTarget::Descriptor,
                scope: SignalScope::ThreadGroup,
            })
        );
        // A PIDFD_THREAD descriptor defaults to thread scope.
        assert_eq!(
            pidfd_signal_plan(3, 0, SignalScope::Thread)
                .expect("admitted")
                .scope,
            SignalScope::Thread
        );
    }

    #[test]
    fn scope_flags_override_every_default() {
        for pidfd in [PIDFD_SELF_THREAD, PIDFD_SELF_THREAD_GROUP, 7] {
            assert_eq!(plan(pidfd, PIDFD_SIGNAL_THREAD).expect("ok").scope, SignalScope::Thread);
            assert_eq!(
                plan(pidfd, PIDFD_SIGNAL_THREAD_GROUP).expect("ok").scope,
                SignalScope::ThreadGroup
            );
            assert_eq!(
                plan(pidfd, PIDFD_SIGNAL_PROCESS_GROUP).expect("ok").scope,
                SignalScope::ProcessGroup
            );
        }
    }

    #[test]
    fn only_process_group_scope_is_above_pidtype_tgid() {
        assert!(!SignalScope::Thread.is_process_group());
        assert!(!SignalScope::ThreadGroup.is_process_group());
        assert!(SignalScope::ProcessGroup.is_process_group());
        assert!(SignalScope::Thread < SignalScope::ThreadGroup);
        assert!(SignalScope::ThreadGroup < SignalScope::ProcessGroup);
    }
}
