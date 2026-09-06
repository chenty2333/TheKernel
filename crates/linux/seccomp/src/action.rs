use crate::{
    SECCOMP_RET_ACTION_FULL, SECCOMP_RET_ALLOW, SECCOMP_RET_DATA, SECCOMP_RET_ERRNO,
    SECCOMP_RET_KILL_PROCESS, SECCOMP_RET_KILL_THREAD, SECCOMP_RET_LOG, SECCOMP_RET_TRACE,
    SECCOMP_RET_TRAP, SECCOMP_RET_USER_NOTIF,
};

/// Maximum errno value Linux accepts from `SECCOMP_RET_ERRNO`.
pub const MAX_ERRNO: u16 = 4095;

/// A raw result returned by a seccomp classic-BPF program.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Action(u32);

impl Action {
    /// Creates an action from an untrusted filter return value.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the exact filter return value, including its data field.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Returns the complete 16-bit Linux action field.
    pub const fn action_bits(self) -> u32 {
        self.0 & SECCOMP_RET_ACTION_FULL
    }

    /// Returns the low-order action data.
    pub const fn data(self) -> u16 {
        (self.0 & SECCOMP_RET_DATA) as u16
    }

    /// Returns Linux's signed action-precedence key.
    ///
    /// Smaller values are more restrictive. In particular,
    /// `KILL_PROCESS` is negative after the signed conversion and therefore
    /// outranks `KILL_THREAD` and every positive action.
    pub const fn precedence(self) -> i32 {
        self.action_bits() as i32
    }

    /// Classifies the result without silently treating unknown actions as
    /// allow. Consumers must terminate for [`ActionClass::Unknown`].
    pub const fn classify(self) -> ActionClass {
        match self.action_bits() {
            SECCOMP_RET_KILL_PROCESS => ActionClass::KillProcess,
            SECCOMP_RET_KILL_THREAD => ActionClass::KillThread,
            SECCOMP_RET_TRAP => ActionClass::Trap { data: self.data() },
            SECCOMP_RET_ERRNO => ActionClass::Errno {
                errno: if self.data() > MAX_ERRNO {
                    MAX_ERRNO
                } else {
                    self.data()
                },
            },
            SECCOMP_RET_USER_NOTIF => ActionClass::UserNotification { data: self.data() },
            SECCOMP_RET_TRACE => ActionClass::Trace { data: self.data() },
            SECCOMP_RET_LOG => ActionClass::Log,
            SECCOMP_RET_ALLOW => ActionClass::Allow,
            _ => ActionClass::Unknown { raw: self.0 },
        }
    }
}

/// Kernel work selected by a seccomp filter result.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ActionClass {
    /// Terminate the complete thread group.
    KillProcess,
    /// Terminate only the calling task.
    KillThread,
    /// Queue a synchronous `SIGSYS` with the filter data.
    Trap {
        /// Low-order data copied to `siginfo.si_errno`.
        data: u16,
    },
    /// Skip the syscall and return a bounded errno.
    Errno {
        /// Linux errno capped at 4095.
        errno: u16,
    },
    /// Enter the user-notification lifecycle.
    UserNotification {
        /// Low-order data retained for the matching filter.
        data: u16,
    },
    /// Enter a ptrace seccomp event.
    Trace {
        /// Event message supplied by the filter.
        data: u16,
    },
    /// Audit-log and execute the syscall.
    Log,
    /// Execute the syscall.
    Allow,
    /// An undefined action. Linux treats this as a kill action.
    Unknown {
        /// Exact unrecognized return value.
        raw: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_process_has_strictest_signed_precedence() {
        let process = Action::from_raw(SECCOMP_RET_KILL_PROCESS);
        let thread = Action::from_raw(SECCOMP_RET_KILL_THREAD);
        let trap = Action::from_raw(SECCOMP_RET_TRAP);
        assert!(process.precedence() < thread.precedence());
        assert!(thread.precedence() < trap.precedence());
    }

    #[test]
    fn errno_is_capped_but_raw_data_is_preserved() {
        let action = Action::from_raw(SECCOMP_RET_ERRNO | 0xffff);
        assert_eq!(action.data(), 0xffff);
        assert_eq!(action.classify(), ActionClass::Errno { errno: 4095 });
    }

    #[test]
    fn unknown_action_never_classifies_as_allow() {
        assert_eq!(
            Action::from_raw(0x1234_5678).classify(),
            ActionClass::Unknown { raw: 0x1234_5678 }
        );
    }
}
