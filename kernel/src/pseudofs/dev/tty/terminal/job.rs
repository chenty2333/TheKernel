use alloc::sync::{Arc, Weak};
use core::{mem, task::Context};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollSet, Pollable};
use axtask::current;
use kspin::SpinNoIrq;
use tk_linux_signal::{SignalDisposition, SignalInfo, Signo};

use crate::task::{AsThread, ProcessGroup, Session, process_domain, send_signal_to_process_group};

pub struct JobControl {
    foreground: SpinNoIrq<Weak<ProcessGroup>>,
    session: SpinNoIrq<Weak<Session>>,
    poll_fg: PollSet,
}

/// Result of attempting to retire one terminal/session association.
pub enum SessionRelease {
    /// Another hangup or detach had already retired the association.
    NotReleased,
    /// This caller performed the retirement and owns any foreground signals.
    Released(Option<Arc<ProcessGroup>>),
}

// Linux permits ignored/blocked SIGTTOU operations, but never background
// reads with ignored/blocked SIGTTIN. Orphaned groups cannot stop for either.
fn background_access_error(signal: Signo, ignored: bool, orphaned: bool) -> AxResult<()> {
    if ignored {
        if signal == Signo::SIGTTIN {
            Err(AxError::Io)
        } else {
            Ok(())
        }
    } else if orphaned {
        Err(AxError::Io)
    } else {
        Err(AxError::Interrupted)
    }
}

impl Default for JobControl {
    fn default() -> Self {
        Self::new()
    }
}

impl JobControl {
    pub fn new() -> Self {
        Self {
            foreground: SpinNoIrq::new(Weak::new()),
            session: SpinNoIrq::new(Weak::new()),
            poll_fg: PollSet::new(),
        }
    }

    pub(crate) fn poll_source(&self) -> &PollSet {
        &self.poll_fg
    }

    pub fn current_in_foreground(&self) -> bool {
        let foreground = {
            let guard = self.foreground.lock();
            guard.upgrade()
        };
        foreground.is_none_or(|pg| Arc::ptr_eq(&current().as_thread().proc_data.proc.group(), &pg))
    }

    /// Job-control checks apply only to the caller's controlling terminal,
    /// never to a different session's terminal or to poll readiness.
    pub fn check_access(&self, signal: Signo) -> AxResult<()> {
        let Some(session) = self.session() else {
            return Ok(());
        };
        let task = current();
        let thread = task.as_thread();
        let group = thread.proc_data.proc.group();
        if !Arc::ptr_eq(&session, &group.session()) || self.current_in_foreground() {
            return Ok(());
        }
        let ignored = thread.signal.signal_blocked(signal)
            || matches!(
                thread.proc_data.signal.action(signal).disposition,
                SignalDisposition::Ignore
            );
        if ignored {
            return background_access_error(signal, true, false);
        }
        let registry = process_domain()?.registry();
        let has_parent_in_session = group
            .any_process(registry, |process| {
                !process.is_zombie()
                    && process.parent().is_some_and(|parent| {
                        parent.pid() != 1
                            && !Arc::ptr_eq(&parent.group(), &group)
                            && Arc::ptr_eq(&parent.group().session(), &session)
                    })
            })
            .map_err(crate::task::process_error)?;
        if !has_parent_in_session {
            return background_access_error(signal, false, true);
        }
        send_signal_to_process_group(group.pgid(), Some(SignalInfo::new_kernel(signal)))?;
        Err(AxError::Interrupted)
    }

    pub fn foreground(&self) -> Option<Arc<ProcessGroup>> {
        self.foreground.lock().upgrade()
    }

    pub fn session(&self) -> Option<Arc<Session>> {
        self.session.lock().upgrade()
    }

    pub fn set_foreground(&self, pg: &Arc<ProcessGroup>) -> AxResult<()> {
        let pg_session = pg.session();
        let weak = Arc::downgrade(pg);
        let mut guard = self.foreground.lock();
        if Weak::ptr_eq(&weak, &*guard) {
            drop(guard);
            drop(weak);
            drop(pg_session);
            return Ok(());
        }

        let session = self.session.lock().upgrade();
        let Some(session) = session else {
            drop(guard);
            drop(weak);
            drop(pg_session);
            return Err(AxError::OperationNotPermitted);
        };
        if !Arc::ptr_eq(&pg_session, &session) {
            drop(guard);
            drop(weak);
            drop(session);
            drop(pg_session);
            return Err(AxError::OperationNotPermitted);
        }

        let old = mem::replace(&mut *guard, weak);
        drop(guard);
        drop(old);
        drop(session);
        drop(pg_session);
        self.poll_fg.wake();
        Ok(())
    }

    /// Associates this terminal with a session.
    ///
    /// Returns whether a new association was installed. Reclaiming a terminal
    /// owned by another live session is rejected instead of replacing it.
    pub fn claim_session(&self, session: &Arc<Session>) -> AxResult<bool> {
        let weak = Arc::downgrade(session);
        let mut guard = self.session.lock();
        let current = guard.upgrade();
        if let Some(current) = current {
            let same = Arc::ptr_eq(&current, session);
            drop(guard);
            drop(current);
            drop(weak);
            return if same {
                Ok(false)
            } else {
                Err(AxError::OperationNotPermitted)
            };
        }
        let old = mem::replace(&mut *guard, weak);
        drop(guard);
        drop(old);
        Ok(true)
    }

    /// Removes this terminal's session and foreground process group.
    pub fn release_session(&self, session: &Arc<Session>) -> SessionRelease {
        // Keep the lock order consistent with `set_foreground`.
        let mut foreground = self.foreground.lock();
        let mut current_session = self.session.lock();
        let current = current_session.upgrade();
        let Some(current) = current else {
            drop(current_session);
            drop(foreground);
            return SessionRelease::NotReleased;
        };
        if !Arc::ptr_eq(&current, session) {
            drop(current_session);
            drop(foreground);
            drop(current);
            return SessionRelease::NotReleased;
        }

        let old_foreground = foreground.upgrade();
        let retired_foreground = mem::replace(&mut *foreground, Weak::new());
        let retired_session = mem::replace(&mut *current_session, Weak::new());
        drop(current_session);
        drop(foreground);
        drop(retired_session);
        drop(retired_foreground);
        drop(current);
        self.poll_fg.wake();
        SessionRelease::Released(old_foreground)
    }
}

impl Pollable for JobControl {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::READABLE, self.current_in_foreground());
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        if events.contains(IoEvents::READABLE) {
            axpoll::PollRegistration::single(&self.poll_fg, context.waker())
        } else {
            axpoll::PollRegistration::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn background_signal_policy_matches_linux() {
        for signal in [Signo::SIGTTIN, Signo::SIGTTOU] {
            assert_eq!(
                background_access_error(signal, false, false),
                Err(AxError::Interrupted)
            );
            assert_eq!(
                background_access_error(signal, false, true),
                Err(AxError::Io)
            );
        }
        assert_eq!(
            background_access_error(Signo::SIGTTIN, true, false),
            Err(AxError::Io)
        );
        assert_eq!(background_access_error(Signo::SIGTTOU, true, true), Ok(()));
    }
}
