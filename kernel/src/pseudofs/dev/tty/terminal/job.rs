use alloc::sync::{Arc, Weak};
use core::{mem, task::Context};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollSet, Pollable};
use axtask::current;
use kspin::SpinNoIrq;

use crate::task::{AsThread, ProcessGroup, Session};

pub struct JobControl {
    foreground: SpinNoIrq<Weak<ProcessGroup>>,
    session: SpinNoIrq<Weak<Session>>,
    poll_fg: PollSet,
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
    pub fn release_session(&self, session: &Arc<Session>) -> Option<Arc<ProcessGroup>> {
        // Keep the lock order consistent with `set_foreground`.
        let mut foreground = self.foreground.lock();
        let mut current_session = self.session.lock();
        let current = current_session.upgrade();
        let Some(current) = current else {
            drop(current_session);
            drop(foreground);
            return None;
        };
        if !Arc::ptr_eq(&current, session) {
            drop(current_session);
            drop(foreground);
            drop(current);
            return None;
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
        old_foreground
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
