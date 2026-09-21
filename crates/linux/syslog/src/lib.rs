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
/// The two statics `do_syslog()`'s console actions read and write.
///
/// `loglevel` is `console_loglevel`: one number every console compares against,
/// which is why the actions below return a whole state rather than a per-console
/// answer. `saved` is `saved_console_loglevel`, whose "no `CONSOLE_OFF` is in
/// effect" sentinel is `LOGLEVEL_DEFAULT`, which Linux spells -1
/// (`include/linux/kern_levels.h:29`) because its loglevels are `int`s. Here the
/// sentinel is [`Console::NONE`], a value past every level a console can be set
/// to -- it cannot be 0, because 0 is `CONSOLE_LOGLEVEL_SILENT` and
/// `/proc/sys/kernel/printk` is a plain `proc_dointvec` with no clamp
/// (`kernel/printk/sysctl.c:24`), so a human can legitimately ask for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Console {
    pub loglevel: u8,
    pub saved: u8,
}
impl Console {
    /// `LOGLEVEL_DEFAULT`: no `CONSOLE_OFF` is in effect. `u8::MAX`, above
    /// `CONSOLE_LOGLEVEL_DEBUG` (10) and above any level `CONSOLE_OFF` could
    /// save; only a raw `/proc/sys/kernel/printk` write of 255 collides with it.
    pub const NONE: u8 = u8::MAX;
    /// `CONSOLE_LOGLEVEL_DEFAULT`, the level a fresh boot prints at.
    pub const DEFAULT: Self = Self {
        loglevel: 7,
        saved: Self::NONE,
    };
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Plan {
    Noop,
    Copy {
        cursor: u64,
        newest: bool,
        commit: Commit,
    },
    /// Write the console state. The actions that do not change it return the
    /// state unchanged, so a caller commits the whole struct without having to
    /// ask which action fired.
    Console(Console),
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
    console: Console,
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
        // `SYSLOG_ACTION_CONSOLE_OFF` is idempotent the way Linux's guard makes
        // it: only the first one saves the level, so a second cannot overwrite
        // `saved` with the quiet level the first installed -- which would turn
        // the matching ON into a request to keep the console shut.
        //
        // The level it installs is `minimum_console_loglevel` (printk.c:1787),
        // 1, not 0: the floor still lets `KERN_EMERG` reach a screen, which is
        // the whole reason a death notice is worth publishing at priority 0.
        Action::ConsoleOff if console.saved == Console::NONE => Plan::Console(Console {
            loglevel: 1,
            saved: console.loglevel,
        }),
        Action::ConsoleOff => Plan::Console(console),
        Action::ConsoleOn if console.saved != Console::NONE => Plan::Console(Console {
            loglevel: console.saved,
            saved: Console::NONE,
        }),
        Action::ConsoleOn => Plan::Console(console),
        Action::ConsoleLevel => {
            // `do_syslog_console_level()`: 1 is as quiet as a console gets and 8
            // is as loud, and anything else is `EINVAL` -- it does not clamp.
            if !(1..=8).contains(&len) {
                return Err(PlanError::InvalidArgument);
            }
            // Setting a level clears the saved one: Linux's comment calls it
            // "implicitly re-enable logging to console" (printk.c:1803-1804), so
            // a `CONSOLE_ON` after an explicit level is a no-op rather than a
            // resurrection of a quietness the operator has already replaced.
            Plan::Console(Console {
                loglevel: len as u8,
                saved: Console::NONE,
            })
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
            plan(
                Action::Read,
                true,
                0,
                false,
                Cursors { read: 0, clear: 0 },
                Console::DEFAULT
            ),
            Err(PlanError::PermissionDenied)
        );
        assert_eq!(
            plan(
                Action::ReadAll,
                true,
                -1,
                false,
                Cursors { read: 0, clear: 0 },
                Console::DEFAULT
            ),
            Err(PlanError::InvalidArgument)
        );
        assert_eq!(
            plan(
                Action::ConsoleLevel,
                true,
                9,
                true,
                Cursors { read: 0, clear: 0 },
                Console::DEFAULT
            ),
            Err(PlanError::InvalidArgument)
        );
        assert_eq!(
            plan(
                Action::Close,
                false,
                -1,
                true,
                Cursors { read: 0, clear: 0 },
                Console::DEFAULT
            ),
            Ok(Plan::Noop)
        );
        let mut c = Cursors { read: 2, clear: 4 };
        assert_eq!(
            plan(Action::ReadClear, true, 8, true, c, Console::DEFAULT),
            Ok(Plan::Copy {
                cursor: 4,
                newest: true,
                commit: Commit::Clear,
            })
        );
        commit(&mut c, Commit::Clear, 8);
        assert_eq!(c, Cursors { read: 2, clear: 8 });
        assert_eq!(
            plan(Action::Clear, false, -1, true, c, Console::DEFAULT),
            Ok(Plan::Clear)
        );
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
                plan(Action::Read, false, len, true, cursors, Console::DEFAULT),
                Err(PlanError::InvalidArgument)
            );
            assert_eq!(
                plan(
                    Action::ReadAll,
                    false,
                    len,
                    false,
                    cursors,
                    Console::DEFAULT
                ),
                Err(PlanError::InvalidArgument)
            );
            assert_eq!(
                plan(
                    Action::ReadClear,
                    false,
                    len,
                    true,
                    cursors,
                    Console::DEFAULT
                ),
                Err(PlanError::InvalidArgument)
            );
        }
        // A present buffer with a zero length is an explicit no-op: it must
        // not consume the cursor, which Linux reaches through `if (!len)`.
        assert_eq!(
            plan(Action::Read, true, 0, true, cursors, Console::DEFAULT),
            Ok(Plan::Noop)
        );
        assert_eq!(
            plan(Action::ReadAll, true, 0, false, cursors, Console::DEFAULT),
            Ok(Plan::Noop)
        );
        assert_eq!(
            plan(Action::ReadClear, true, 0, true, cursors, Console::DEFAULT),
            Ok(Plan::Noop)
        );
    }

    #[test]
    fn permission_precedes_action_length_validation() {
        let cursors = Cursors { read: 0, clear: 0 };
        // check_syslog_permissions() is called before the action switch.
        assert_eq!(
            plan(Action::Read, true, -1, false, cursors, Console::DEFAULT),
            Err(PlanError::PermissionDenied)
        );
        assert_eq!(
            plan(
                Action::ConsoleLevel,
                true,
                9,
                false,
                cursors,
                Console::DEFAULT
            ),
            Err(PlanError::PermissionDenied)
        );
        // READ_ALL and SIZE_BUFFER are unrestricted when dmesg_restrict is 0.
        assert_eq!(
            plan(Action::ReadAll, true, 4, false, cursors, Console::DEFAULT),
            Ok(Plan::Copy {
                cursor: 0,
                newest: true,
                commit: Commit::None,
            })
        );
        assert_eq!(
            plan(
                Action::SizeBuffer,
                false,
                -1,
                false,
                cursors,
                Console::DEFAULT
            ),
            Ok(Plan::Capacity)
        );
    }
    /// `SYSLOG_ACTION_CONSOLE_OFF`/`ON` are a save-and-restore pair, not a
    /// boolean. The guard that makes them one is small enough to read as an
    /// optimization and is not: without it, the second OFF overwrites the saved
    /// level with the silence the first OFF installed, and the ON that follows
    /// asks for a console muted forever.
    #[test]
    fn console_off_saves_once_and_console_on_restores() {
        let cursors = Cursors { read: 0, clear: 0 };
        let privileged = true;
        let loud = Console {
            loglevel: 8,
            saved: Console::NONE,
        };
        assert_eq!(
            plan(Action::ConsoleOff, false, -1, privileged, cursors, loud),
            Ok(Plan::Console(Console {
                loglevel: 1,
                saved: 8,
            })),
            "CONSOLE_OFF installs minimum_console_loglevel, so a KERN_EMERG line \
             still reaches a console"
        );
        let muted = Console {
            loglevel: 1,
            saved: 8,
        };
        assert_eq!(
            plan(Action::ConsoleOff, false, -1, privileged, cursors, muted),
            Ok(Plan::Console(muted)),
            "a second CONSOLE_OFF must not save the silence it just installed"
        );
        assert_eq!(
            plan(Action::ConsoleOn, false, -1, privileged, cursors, muted),
            Ok(Plan::Console(loud))
        );
        assert_eq!(
            plan(
                Action::ConsoleOn,
                false,
                -1,
                privileged,
                cursors,
                Console::DEFAULT
            ),
            Ok(Plan::Console(Console::DEFAULT)),
            "CONSOLE_ON with nothing saved is not a request for a default level"
        );
        // An explicit level clears the saved one -- Linux's "implicitly re-enable
        // logging to console" -- so the OFF's matching ON becomes a no-op rather
        // than undoing the level just asked for.
        assert_eq!(
            plan(Action::ConsoleLevel, false, 4, privileged, cursors, muted),
            Ok(Plan::Console(Console {
                loglevel: 4,
                saved: Console::NONE,
            }))
        );
        assert_eq!(
            plan(Action::ConsoleOn, false, -1, privileged, cursors, Console {
                loglevel: 4,
                saved: Console::NONE,
            }),
            Ok(Plan::Console(Console {
                loglevel: 4,
                saved: Console::NONE,
            })),
            "after an explicit level there is nothing left to restore"
        );
        // Neither action is a copy, so unlike the three read actions neither
        // looks at the buffer or the length; both are restricted actions.
        assert!(plan(Action::ConsoleOn, false, 1024, privileged, cursors, loud).is_ok());
        assert_eq!(
            plan(Action::ConsoleOff, false, -1, false, cursors, loud),
            Err(PlanError::PermissionDenied)
        );
        assert_eq!(
            plan(Action::ConsoleOn, false, -1, false, cursors, loud),
            Err(PlanError::PermissionDenied)
        );
    }
}
