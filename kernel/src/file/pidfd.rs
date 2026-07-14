use alloc::{
    borrow::Cow,
    sync::{Arc, Weak},
};
use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollSet, Pollable};

use crate::{
    file::{FileLike, Kstat, PseudoInode},
    task::{Cred, CredentialSlot, Process, ProcessData, Thread},
};

pub struct PidFd {
    inode: PseudoInode,
    proc_data: Weak<ProcessData>,
    process: Weak<Process>,
    exit_event: Arc<PollSet>,
    thread_exit: Option<Arc<AtomicBool>>,
    thread_credential: Option<Weak<CredentialSlot>>,

    non_blocking: AtomicBool,
}
impl PidFd {
    pub fn new_process(proc_data: &Arc<ProcessData>) -> Self {
        Self {
            inode: PseudoInode::pidfd(),
            proc_data: Arc::downgrade(proc_data),
            process: Arc::downgrade(&proc_data.proc),
            exit_event: proc_data.exit_event.clone(),
            thread_exit: None,
            thread_credential: None,

            non_blocking: AtomicBool::new(false),
        }
    }

    pub fn new_thread(thread: &Thread) -> Self {
        Self {
            inode: PseudoInode::pidfd(),
            proc_data: Arc::downgrade(&thread.proc_data),
            process: Arc::downgrade(&thread.proc_data.proc),
            exit_event: thread.exit_event.clone(),
            thread_exit: Some(thread.exit.clone()),
            thread_credential: Some(thread.credential_slot_weak()),

            non_blocking: AtomicBool::new(false),
        }
    }

    pub fn process_data(&self) -> AxResult<Arc<ProcessData>> {
        // For threads, the pidfd is invalid once the thread exits, even if its
        // process is still alive.
        if let Some(thread_exit) = &self.thread_exit
            && thread_exit.load(Ordering::Acquire)
        {
            return Err(AxError::NoSuchProcess);
        }
        self.proc_data.upgrade().ok_or(AxError::NoSuchProcess)
    }

    pub fn process(&self) -> AxResult<Arc<Process>> {
        self.process.upgrade().ok_or(AxError::NoSuchProcess)
    }

    /// Takes the credential snapshot governing access through this pidfd.
    /// Thread pidfds weakly retain the exact task publication slot across
    /// de-threading; process pidfds explicitly resolve the Linux group leader.
    pub fn credential_snapshot(&self) -> AxResult<Arc<Cred>> {
        let proc_data = self.process_data()?;
        if let Some(slot) = &self.thread_credential {
            Ok(slot.upgrade().ok_or(AxError::NoSuchProcess)?.current())
        } else {
            Ok(proc_data.group_leader_cred())
        }
    }
}
impl FileLike for PidFd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.inode.stat())
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[pidfd]".into())
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
        self.non_blocking.store(nonblocking, Ordering::Release);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }
}

impl Pollable for PidFd {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let exited = if let Some(thread_exit) = &self.thread_exit {
            thread_exit.load(Ordering::Acquire)
        } else {
            self.process
                .upgrade()
                .is_none_or(|process| process.is_zombie())
        };
        events.set(IoEvents::READABLE, exited);
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        if events.contains(IoEvents::READABLE) {
            axpoll::PollRegistration::single(&self.exit_event, context.waker())
        } else {
            axpoll::PollRegistration::empty()
        }
    }
}
