use alloc::sync::{Arc, Weak};
use core::task::Context;

use axerrno::{AxError, AxResult, ax_bail};
use axpoll::{IoEvents, PollSet, Pollable};
use axtask::current;
use kspin::SpinNoIrq;
use starry_process::{ProcessGroup, Session};

use crate::task::AsThread;

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

    pub fn current_in_foreground(&self) -> bool {
        self.foreground
            .lock()
            .upgrade()
            .is_none_or(|pg| Arc::ptr_eq(&current().as_thread().proc_data.proc.group(), &pg))
    }

    pub fn foreground(&self) -> Option<Arc<ProcessGroup>> {
        self.foreground.lock().upgrade()
    }

    pub fn session(&self) -> Option<Arc<Session>> {
        self.session.lock().upgrade()
    }

    pub fn set_foreground(&self, pg: &Arc<ProcessGroup>) -> AxResult<()> {
        let mut guard = self.foreground.lock();
        let weak = Arc::downgrade(pg);
        if Weak::ptr_eq(&weak, &*guard) {
            return Ok(());
        }

        let Some(session) = self.session.lock().upgrade() else {
            ax_bail!(
                OperationNotPermitted,
                "No session associated with job control"
            );
        };
        if !Arc::ptr_eq(&pg.session(), &session) {
            ax_bail!(
                OperationNotPermitted,
                "Process group does not belong to the session"
            );
        }

        *guard = weak;
        drop(guard);
        self.poll_fg.wake();
        Ok(())
    }

    /// Associates this terminal with a session.
    ///
    /// Returns whether a new association was installed. Reclaiming a terminal
    /// owned by another live session is rejected instead of replacing it.
    pub fn claim_session(&self, session: &Arc<Session>) -> AxResult<bool> {
        let mut guard = self.session.lock();
        if let Some(current) = guard.upgrade() {
            if Arc::ptr_eq(&current, session) {
                return Ok(false);
            }
            return Err(AxError::OperationNotPermitted);
        }
        *guard = Arc::downgrade(session);
        Ok(true)
    }

    /// Removes this terminal's session and foreground process group.
    pub fn release_session(&self, session: &Arc<Session>) -> Option<Arc<ProcessGroup>> {
        // Keep the lock order consistent with `set_foreground`.
        let mut foreground = self.foreground.lock();
        let mut current_session = self.session.lock();
        let current = current_session.upgrade()?;
        if !Arc::ptr_eq(&current, session) {
            return None;
        }

        let old_foreground = foreground.upgrade();
        *foreground = Weak::new();
        *current_session = Weak::new();
        drop(current_session);
        drop(foreground);
        self.poll_fg.wake();
        old_foreground
    }
}

impl Pollable for JobControl {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.current_in_foreground());
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.poll_fg.register(context.waker());
        }
    }
}
