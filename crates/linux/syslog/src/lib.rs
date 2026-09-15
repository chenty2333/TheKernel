#![no_std]

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Close,
    Open,
    Read,
    ReadAll,
    ReadClear,
    Clear,
    ConsoleOff,
    ConsoleOn,
    ConsoleLevel,
    SizeUnread,
    SizeBuffer,
}
impl TryFrom<i32> for Action {
    type Error = ();
    fn try_from(value: i32) -> Result<Self, ()> {
        Ok(match value {
            0 => Self::Close,
            1 => Self::Open,
            2 => Self::Read,
            3 => Self::ReadAll,
            4 => Self::ReadClear,
            5 => Self::Clear,
            6 => Self::ConsoleOff,
            7 => Self::ConsoleOn,
            8 => Self::ConsoleLevel,
            9 => Self::SizeUnread,
            10 => Self::SizeBuffer,
            _ => return Err(()),
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursors {
    pub read: u64,
    pub clear: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Plan {
    Noop,
    Copy {
        cursor: u64,
        newest: bool,
        commit: Commit,
    },
    Console {
        enabled: bool,
    },
    ConsoleLevel(u8),
    Clear,
    Unread {
        cursor: u64,
    },
    Capacity,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Commit {
    None,
    Read,
    Clear,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanError {
    InvalidArgument,
    PermissionDenied,
}
/// Plans one `syslog(2)` action from the decoded request.
///
/// `buf_present` is `buf != NULL`, which Linux tests before the length. The
/// order below is `do_syslog()`'s: `check_syslog_permissions()` runs before
/// the action switch, so an unprivileged `READ` with a negative length is
/// `EPERM` and not `EINVAL`.
pub fn plan(
    action: Action,
    buf_present: bool,
    len: isize,
    privileged: bool,
    cursors: Cursors,
) -> Result<Plan, PlanError> {
    if !matches!(action, Action::ReadAll | Action::SizeBuffer) && !privileged {
        return Err(PlanError::PermissionDenied);
    }
    // do_syslog(): `if (!buf || len < 0) return -EINVAL; if (!len) return 0;`
    if matches!(action, Action::Read | Action::ReadAll | Action::ReadClear)
        && (!buf_present || len < 0)
    {
        return Err(PlanError::InvalidArgument);
    }
    Ok(match action {
        Action::Close | Action::Open => Plan::Noop,
        Action::Read if len == 0 => Plan::Noop,
        Action::Read => Plan::Copy {
            cursor: cursors.read,
            newest: false,
            commit: Commit::Read,
        },
        Action::ReadAll if len == 0 => Plan::Noop,
        Action::ReadAll => Plan::Copy {
            cursor: cursors.clear,
            newest: true,
            commit: Commit::None,
        },
        Action::ReadClear if len == 0 => Plan::Noop,
        Action::ReadClear => Plan::Copy {
            cursor: cursors.clear,
            newest: true,
            commit: Commit::Clear,
        },
        Action::Clear => Plan::Clear,
        Action::ConsoleOff => Plan::Console { enabled: false },
        Action::ConsoleOn => Plan::Console { enabled: true },
        Action::ConsoleLevel => {
            if !(1..=8).contains(&len) {
                return Err(PlanError::InvalidArgument);
            }
            Plan::ConsoleLevel(len as u8)
        }
        Action::SizeUnread => Plan::Unread {
            cursor: cursors.read,
        },
        Action::SizeBuffer => Plan::Capacity,
    })
}
pub fn commit(cursors: &mut Cursors, commit: Commit, end: u64) {
    match commit {
        Commit::None => {}
        Commit::Read => cursors.read = cursors.read.max(end),
        Commit::Clear => cursors.clear = cursors.clear.max(end),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validation_and_transitions() {
        assert_eq!(
            plan(Action::Read, true, 0, false, Cursors { read: 0, clear: 0 }),
            Err(PlanError::PermissionDenied)
        );
        assert_eq!(
            plan(Action::ReadAll, true, -1, false, Cursors { read: 0, clear: 0 }),
            Err(PlanError::InvalidArgument)
        );
        assert_eq!(
            plan(Action::ConsoleLevel, true, 9, true, Cursors { read: 0, clear: 0 }),
            Err(PlanError::InvalidArgument)
        );
        assert_eq!(
            plan(Action::Close, false, -1, true, Cursors { read: 0, clear: 0 }),
            Ok(Plan::Noop)
        );
        let mut c = Cursors { read: 2, clear: 4 };
        assert_eq!(
            plan(Action::ReadClear, true, 8, true, c),
            Ok(Plan::Copy {
                cursor: 4,
                newest: true,
                commit: Commit::Clear,
            })
        );
        commit(&mut c, Commit::Clear, 8);
        assert_eq!(c, Cursors { read: 2, clear: 8 });
        assert_eq!(plan(Action::Clear, false, -1, true, c), Ok(Plan::Clear));
        commit(&mut c, Commit::Clear, 10);
        assert_eq!(c, Cursors { read: 2, clear: 10 });
    }

    #[test]
    fn null_buffer_is_einval_and_zero_length_copy_is_success() {
        let cursors = Cursors { read: 0, clear: 0 };
        // do_syslog() rejects `!buf` before it consults `len`, so a NULL
        // buffer is EINVAL even when the length is zero.
        for len in [0, 1, 16] {
            assert_eq!(
                plan(Action::Read, false, len, true, cursors),
                Err(PlanError::InvalidArgument)
            );
            assert_eq!(
                plan(Action::ReadAll, false, len, false, cursors),
                Err(PlanError::InvalidArgument)
            );
            assert_eq!(
                plan(Action::ReadClear, false, len, true, cursors),
                Err(PlanError::InvalidArgument)
            );
        }
        // A present buffer with a zero length is an explicit no-op: it must
        // not consume the cursor, which Linux reaches through `if (!len)`.
        assert_eq!(plan(Action::Read, true, 0, true, cursors), Ok(Plan::Noop));
        assert_eq!(
            plan(Action::ReadAll, true, 0, false, cursors),
            Ok(Plan::Noop)
        );
        assert_eq!(
            plan(Action::ReadClear, true, 0, true, cursors),
            Ok(Plan::Noop)
        );
    }

    #[test]
    fn permission_precedes_action_length_validation() {
        let cursors = Cursors { read: 0, clear: 0 };
        // check_syslog_permissions() is called before the action switch.
        assert_eq!(
            plan(Action::Read, true, -1, false, cursors),
            Err(PlanError::PermissionDenied)
        );
        assert_eq!(
            plan(Action::ConsoleLevel, true, 9, false, cursors),
            Err(PlanError::PermissionDenied)
        );
        // READ_ALL and SIZE_BUFFER are unrestricted when dmesg_restrict is 0.
        assert_eq!(
            plan(Action::ReadAll, true, 4, false, cursors),
            Ok(Plan::Copy {
                cursor: 0,
                newest: true,
                commit: Commit::None,
            })
        );
        assert_eq!(
            plan(Action::SizeBuffer, false, -1, false, cursors),
            Ok(Plan::Capacity)
        );
    }
}
