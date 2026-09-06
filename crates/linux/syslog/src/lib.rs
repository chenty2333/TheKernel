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
pub fn plan(
    action: Action,
    len: isize,
    privileged: bool,
    cursors: Cursors,
) -> Result<Plan, PlanError> {
    if matches!(
        action,
        Action::Read | Action::ReadAll | Action::ReadClear | Action::ConsoleLevel
    ) && len < 0
    {
        return Err(PlanError::InvalidArgument);
    }
    if !matches!(action, Action::ReadAll | Action::SizeBuffer) && !privileged {
        return Err(PlanError::PermissionDenied);
    }
    Ok(match action {
        Action::Close | Action::Open => Plan::Noop,
        Action::Read => Plan::Copy {
            cursor: cursors.read,
            newest: false,
            commit: Commit::Read,
        },
        Action::ReadAll => Plan::Copy {
            cursor: cursors.clear,
            newest: true,
            commit: Commit::None,
        },
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
            plan(Action::Read, 0, false, Cursors { read: 0, clear: 0 }),
            Err(PlanError::PermissionDenied)
        );
        assert_eq!(
            plan(Action::ReadAll, -1, false, Cursors { read: 0, clear: 0 }),
            Err(PlanError::InvalidArgument)
        );
        assert_eq!(
            plan(Action::ConsoleLevel, 9, true, Cursors { read: 0, clear: 0 }),
            Err(PlanError::InvalidArgument)
        );
        assert_eq!(
            plan(Action::Close, -1, true, Cursors { read: 0, clear: 0 }),
            Ok(Plan::Noop)
        );
        let mut c = Cursors { read: 2, clear: 4 };
        assert_eq!(
            plan(Action::ReadClear, 8, true, c),
            Ok(Plan::Copy {
                cursor: 4,
                newest: true,
                commit: Commit::Clear,
            })
        );
        commit(&mut c, Commit::Clear, 8);
        assert_eq!(c, Cursors { read: 2, clear: 8 });
        assert_eq!(plan(Action::Clear, -1, true, c), Ok(Plan::Clear));
        commit(&mut c, Commit::Clear, 10);
        assert_eq!(c, Cursors { read: 2, clear: 10 });
    }
}
