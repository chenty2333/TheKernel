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
use axtask::{AxTaskRef, WeakAxTaskRef};
use spin::Once;
use thekernel_linux_process_adapter::Pid;

use crate::{
    file::{FileLike, Kstat, PseudoInode},
    task::{
        AsThread, Cred, CredentialSlot, Process, ProcessData, ProcessImageAccessSnapshot, Thread,
    },
};

pub struct PidFd {
    inode: PseudoInode,
    proc_data: Weak<ProcessData>,
    process: Weak<Process>,
    exit_event: Arc<PollSet>,
    thread_exit: Option<Arc<AtomicBool>>,
    thread_credential: Option<Weak<CredentialSlot>>,
    thread_task: Option<Once<WeakAxTaskRef>>,
    thread_tid: Option<Pid>,

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
            thread_task: None,
            thread_tid: None,

            non_blocking: AtomicBool::new(false),
        }
    }

    /// Builds a thread pidfd before its scheduler task is published. The clone
    /// path binds the exact task with [`Self::bind_thread_task`] before the file
    /// descriptor itself becomes visible.
    pub(crate) fn new_thread_unbound(thread: &Thread) -> Self {
        Self {
            inode: PseudoInode::pidfd(),
            proc_data: Arc::downgrade(&thread.proc_data),
            process: Arc::downgrade(&thread.proc_data.proc),
            exit_event: thread.exit_event.clone(),
            thread_exit: Some(thread.exit.clone()),
            thread_credential: Some(thread.credential_slot_weak()),
            thread_task: Some(Once::new()),
            thread_tid: Some(thread.tid()),

            non_blocking: AtomicBool::new(false),
        }
    }

    /// Builds a thread pidfd for an already-existing exact task.
    pub fn new_thread(task: &AxTaskRef) -> AxResult<Self> {
        let pidfd = Self::new_thread_unbound(task.as_thread());
        pidfd.bind_thread_task(task)?;
        Ok(pidfd)
    }

    /// Binds the exact scheduler task named by a not-yet-published thread
    /// pidfd. This is a one-shot construction operation, never a numeric-TID
    /// lookup or a rebinding point.
    pub(crate) fn bind_thread_task(&self, task: &AxTaskRef) -> AxResult<()> {
        let binding = self
            .thread_task
            .as_ref()
            .ok_or(AxError::OperationNotPermitted)?;
        let thread = task.try_as_thread().ok_or(AxError::OperationNotPermitted)?;
        let expected_exit = self
            .thread_exit
            .as_ref()
            .ok_or(AxError::OperationNotPermitted)?;
        let expected_tid = self.thread_tid.ok_or(AxError::OperationNotPermitted)?;
        if !Arc::ptr_eq(&thread.exit, expected_exit)
            || thread.tid() != expected_tid
            || !self
                .proc_data
                .upgrade()
                .is_some_and(|process| Arc::ptr_eq(&process, &thread.proc_data))
        {
            return Err(AxError::OperationNotPermitted);
        }
        binding.call_once(|| Arc::downgrade(task));
        let bound = binding
            .get()
            .and_then(Weak::upgrade)
            .ok_or(AxError::NoSuchProcess)?;
        if Arc::ptr_eq(&bound, task) {
            Ok(())
        } else {
            Err(AxError::OperationNotPermitted)
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
        let process = self.proc_data.upgrade().ok_or(AxError::NoSuchProcess)?;
        if self
            .thread_exit
            .as_ref()
            .is_some_and(|exit| exit.load(Ordering::Acquire))
        {
            return Err(AxError::NoSuchProcess);
        }
        Ok(process)
    }

    /// Pins the exact task named by a thread pidfd across authorization and
    /// publication. Process pidfds return `None`. Exit is sampled both before
    /// and after the weak-task upgrade so an exit racing resolution fails
    /// closed instead of falling back to a reused numeric TID.
    pub(crate) fn signal_thread_task(&self) -> AxResult<Option<AxTaskRef>> {
        let Some(binding) = &self.thread_task else {
            return Ok(None);
        };
        let expected_tid = self.thread_tid.ok_or(AxError::OperationNotPermitted)?;
        let exit = self
            .thread_exit
            .as_ref()
            .ok_or(AxError::OperationNotPermitted)?;
        if exit.load(Ordering::Acquire) {
            return Err(AxError::NoSuchProcess);
        }
        let task = binding
            .get()
            .and_then(Weak::upgrade)
            .ok_or(AxError::NoSuchProcess)?;
        let thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
        if !Arc::ptr_eq(&thread.exit, exit)
            || thread.tid() != expected_tid
            || exit.load(Ordering::Acquire)
        {
            return Err(AxError::NoSuchProcess);
        }
        Ok(Some(task))
    }

    /// Returns the stable PID identity captured by a thread pidfd.
    pub(crate) const fn signal_thread_tid(&self) -> Option<Pid> {
        self.thread_tid
    }

    /// Resolves the one exited thread identity Linux keeps signalable through
    /// a thread pidfd: the thread-group leader. A nonleader disappears when
    /// its exact task exits, while the leader's pid identity remains published
    /// until the whole process is reaped.
    pub(crate) fn signal_exited_leader_process(&self) -> AxResult<Option<Arc<Process>>> {
        let Some(tid) = self.thread_tid else {
            return Ok(None);
        };
        let exit = self
            .thread_exit
            .as_ref()
            .ok_or(AxError::OperationNotPermitted)?;
        if !exit.load(Ordering::Acquire) {
            return Err(AxError::NoSuchProcess);
        }
        let process = self.process()?;
        if tid != process.pid() {
            return Err(AxError::NoSuchProcess);
        }
        Ok(Some(process))
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
            let credential = slot.upgrade().ok_or(AxError::NoSuchProcess)?.current();
            if self
                .thread_exit
                .as_ref()
                .is_some_and(|exit| exit.load(Ordering::Acquire))
            {
                return Err(AxError::NoSuchProcess);
            }
            Ok(credential)
        } else {
            Ok(proc_data.group_leader_cred())
        }
    }

    /// Pins the exact process image and access identity named by this pidfd.
    /// Thread pidfds preserve their task-local credential slot and reject an
    /// exit racing either side of the snapshot; process pidfds use the
    /// persistent Linux group-leader binding.
    pub(crate) fn image_access_snapshot(&self) -> AxResult<ProcessImageAccessSnapshot> {
        let proc_data = self.process_data()?;
        if let Some(slot) = &self.thread_credential {
            let slot = slot.upgrade().ok_or(AxError::NoSuchProcess)?;
            let snapshot = proc_data.credential_image_access_snapshot(&slot);
            if self
                .thread_exit
                .as_ref()
                .is_some_and(|exit| exit.load(Ordering::Acquire))
            {
                return Err(AxError::NoSuchProcess);
            }
            Ok(snapshot)
        } else {
            Ok(proc_data.group_leader_image_access_snapshot())
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
