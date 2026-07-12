mod epoll;
mod poll;
mod select;

use alloc::vec::Vec;
use core::{future::pending, task::Context, time::Duration};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::uspace::UserContext;
use axpoll::{IoEvents, Pollable};
use axtask::{current, future};
use linux_raw_sys::general::{
    EPOLLERR, EPOLLHUP, EPOLLIN, EPOLLMSG, EPOLLOUT, EPOLLPRI, EPOLLRDBAND, EPOLLRDHUP,
    EPOLLRDNORM, EPOLLWRBAND, EPOLLWRNORM, POLLERR, POLLHUP, POLLIN, POLLMSG, POLLNVAL, POLLOUT,
    POLLPRI, POLLRDBAND, POLLRDHUP, POLLRDNORM, POLLREMOVE, POLLWRBAND, POLLWRNORM,
};
use starry_signal::SignalSet;

pub use self::{epoll::*, poll::*, select::*};
use crate::{
    file::{FileHandle, FileLike},
    task::{
        AsThread, ProcStateHint, check_signals, has_pending_syscall_signal, with_proc_state_hint,
    },
};

trait RegistrationPlanEntry {
    fn source_key(&self) -> u64;
    fn poll_events(&self) -> IoEvents;
    fn registration_events(&self) -> Option<IoEvents>;
    fn set_registration_events(&mut self, events: Option<IoEvents>);
}

fn prepare_registration_sources<T: RegistrationPlanEntry>(entries: &mut [T]) -> usize {
    entries.sort_unstable_by_key(RegistrationPlanEntry::source_key);
    let mut unique = 0;
    let mut start = 0;
    while start < entries.len() {
        let source_key = entries[start].source_key();
        let mut merged = IoEvents::empty();
        let mut end = start;
        while end < entries.len() && entries[end].source_key() == source_key {
            merged |= entries[end].poll_events();
            entries[end].set_registration_events(None);
            end += 1;
        }
        entries[start].set_registration_events(Some(merged));
        unique += 1;
        start = end;
    }
    entries.sort_unstable_by_key(|entry| entry.registration_events().is_none());
    unique
}

struct FdPollEntry {
    source_key: u64,
    file: FileHandle<dyn FileLike>,
    events: IoEvents,
    registration_events: Option<IoEvents>,
    output_index: usize,
}

impl RegistrationPlanEntry for FdPollEntry {
    fn source_key(&self) -> u64 {
        self.source_key
    }

    fn poll_events(&self) -> IoEvents {
        self.events
    }

    fn registration_events(&self) -> Option<IoEvents> {
        self.registration_events
    }

    fn set_registration_events(&mut self, events: Option<IoEvents>) {
        self.registration_events = events;
    }
}

struct FdPollSet {
    entries: Vec<FdPollEntry>,
    registration_sources: usize,
}

impl FdPollSet {
    fn try_with_capacity(capacity: usize) -> AxResult<Self> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            entries,
            registration_sources: 0,
        })
    }

    fn push(&mut self, file: FileHandle<dyn FileLike>, events: IoEvents) {
        let source_key = file.open_file_description_key();
        let output_index = self.entries.len();
        self.entries.push(FdPollEntry {
            source_key,
            file,
            events,
            registration_events: None,
            output_index,
        });
    }

    fn finish(mut self) -> Self {
        self.registration_sources = prepare_registration_sources(&mut self.entries);
        self
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn entries(&self) -> &[FdPollEntry] {
        &self.entries
    }
}

impl Pollable for FdPollSet {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        let mut prepared = axpoll::PreparedPollRegistration::try_new(self.registration_sources)?;
        for entry in &self.entries[..self.registration_sources] {
            let events = entry
                .registration_events
                .ok_or(axpoll::PollRegistrationError::InvalidState)?;
            prepared.arm_nested(|| entry.file.register(context, events))?;
        }
        prepared.commit()
    }
}

fn linux_poll_events(events: u32) -> IoEvents {
    let mut generic = IoEvents::empty();
    for (linux, event) in [
        (POLLIN, IoEvents::READABLE),
        (POLLPRI, IoEvents::PRIORITY),
        (POLLOUT, IoEvents::WRITABLE),
        (POLLERR, IoEvents::ERROR),
        (POLLHUP, IoEvents::HANGUP),
        (POLLNVAL, IoEvents::INVALID),
        (POLLRDNORM, IoEvents::READ_NORMAL),
        (POLLRDBAND, IoEvents::READ_BAND),
        (POLLWRNORM, IoEvents::WRITE_NORMAL),
        (POLLWRBAND, IoEvents::WRITE_BAND),
        (POLLMSG, IoEvents::MESSAGE),
        (POLLREMOVE, IoEvents::REMOVED),
        (POLLRDHUP, IoEvents::READ_HANGUP),
    ] {
        if events & linux != 0 {
            generic |= event;
        }
    }
    generic
}

fn io_to_linux_poll(events: IoEvents) -> u32 {
    let mut linux = 0;
    for (event, bit) in [
        (IoEvents::READABLE, POLLIN),
        (IoEvents::PRIORITY, POLLPRI),
        (IoEvents::WRITABLE, POLLOUT),
        (IoEvents::ERROR, POLLERR),
        (IoEvents::HANGUP, POLLHUP),
        (IoEvents::INVALID, POLLNVAL),
        (IoEvents::READ_NORMAL, POLLRDNORM),
        (IoEvents::READ_BAND, POLLRDBAND),
        (IoEvents::WRITE_NORMAL, POLLWRNORM),
        (IoEvents::WRITE_BAND, POLLWRBAND),
        (IoEvents::MESSAGE, POLLMSG),
        (IoEvents::REMOVED, POLLREMOVE),
        (IoEvents::READ_HANGUP, POLLRDHUP),
    ] {
        if events.contains(event) {
            linux |= bit;
        }
    }
    linux
}

fn linux_epoll_events(events: u32) -> AxResult<IoEvents> {
    let mut remaining = events;
    let mut generic = IoEvents::empty();
    for (linux, event) in [
        (EPOLLIN, IoEvents::READABLE),
        (EPOLLPRI, IoEvents::PRIORITY),
        (EPOLLOUT, IoEvents::WRITABLE),
        (EPOLLERR, IoEvents::ERROR),
        (EPOLLHUP, IoEvents::HANGUP),
        (EPOLLRDNORM, IoEvents::READ_NORMAL),
        (EPOLLRDBAND, IoEvents::READ_BAND),
        (EPOLLWRNORM, IoEvents::WRITE_NORMAL),
        (EPOLLWRBAND, IoEvents::WRITE_BAND),
        (EPOLLMSG, IoEvents::MESSAGE),
        (EPOLLRDHUP, IoEvents::READ_HANGUP),
    ] {
        if remaining & linux != 0 {
            generic |= event;
            remaining &= !linux;
        }
    }
    if remaining == 0 {
        Ok(generic)
    } else {
        Err(AxError::InvalidInput)
    }
}

fn io_to_linux_epoll(events: IoEvents) -> u32 {
    let mut linux = 0;
    for (event, bit) in [
        (IoEvents::READABLE, EPOLLIN),
        (IoEvents::PRIORITY, EPOLLPRI),
        (IoEvents::WRITABLE, EPOLLOUT),
        (IoEvents::ERROR, EPOLLERR),
        (IoEvents::HANGUP, EPOLLHUP),
        (IoEvents::READ_NORMAL, EPOLLRDNORM),
        (IoEvents::READ_BAND, EPOLLRDBAND),
        (IoEvents::WRITE_NORMAL, EPOLLWRNORM),
        (IoEvents::WRITE_BAND, EPOLLWRBAND),
        (IoEvents::MESSAGE, EPOLLMSG),
        (IoEvents::READ_HANGUP, EPOLLRDHUP),
    ] {
        if events.contains(event) {
            linux |= bit;
        }
    }
    linux
}

fn flatten_blocked_timeout<T>(
    result: Result<Result<AxResult<T>, future::TimeoutError>, axtask::future::BlockOnError>,
) -> Result<AxResult<T>, future::Elapsed> {
    match result {
        Err(error) => Ok(Err(error.into())),
        Ok(Err(future::TimeoutError::Elapsed(elapsed))) => Err(elapsed),
        Ok(Err(future::TimeoutError::Timer(error))) => Ok(Err(error.into())),
        Ok(Ok(result)) => Ok(result),
    }
}

fn wait_io_result(
    mut uctx: Option<&mut UserContext>,
    sigmask: Option<SignalSet>,
    mut wait_once: impl FnMut() -> Result<AxResult<isize>, future::Elapsed>,
) -> AxResult<isize> {
    let curr = current();
    let thr = curr.as_thread();
    let old_blocked = sigmask.map(|set| thr.signal.set_blocked(set));

    if let Some(uctx) = uctx.as_deref_mut() {
        // If a handler runs while the syscall is blocked, sigreturn must
        // observe -EINTR as the interrupted syscall result.
        uctx.set_retval(-LinuxError::EINTR.code() as usize);
    }

    // `wait_once` owns the condition-first boundary: readiness is checked
    // before a task interrupt is consumed. Dispatching signals here would let
    // a pending signal hide an already-ready descriptor. Every exit below
    // restores the temporary mask unless a newly-entered handler must leave
    // that restoration to sigreturn.
    with_proc_state_hint(ProcStateHint::Interruptible, || {
        loop {
            match wait_once() {
                Ok(Ok(res)) => {
                    if let Some(old_blocked) = old_blocked {
                        thr.signal.set_blocked(old_blocked);
                    }
                    return Ok(res);
                }
                Ok(Err(AxError::Interrupted)) => {
                    if let Some(uctx) = uctx.as_deref_mut() {
                        let handler_depth = thr.signal_handler_depth();
                        let handled = check_signals(thr, uctx, old_blocked);
                        if handled {
                            if let Some(old_blocked) = old_blocked
                                && thr.signal_handler_depth() == handler_depth
                            {
                                thr.signal.set_blocked(old_blocked);
                            }
                            return Err(AxError::Interrupted);
                        }
                    } else if has_pending_syscall_signal(thr) {
                        if let Some(old_blocked) = old_blocked {
                            thr.signal.set_blocked(old_blocked);
                        }
                        return Err(AxError::Interrupted);
                    }
                }
                Ok(Err(err)) => {
                    if let Some(old_blocked) = old_blocked {
                        thr.signal.set_blocked(old_blocked);
                    }
                    return Err(err);
                }
                Err(_) => {
                    if let Some(old_blocked) = old_blocked {
                        thr.signal.set_blocked(old_blocked);
                    }
                    return Ok(0);
                }
            }
        }
    })
}

fn wait_signal_only(
    uctx: Option<&mut UserContext>,
    timeout: Option<Duration>,
    sigmask: Option<SignalSet>,
) -> AxResult<isize> {
    let deadline = timeout.map(|dur| axhal::time::wall_time().saturating_add(dur));
    let mut wait_once = || {
        flatten_blocked_timeout(future::block_on(future::timeout(
            deadline.map(|end| end.saturating_sub(axhal::time::wall_time())),
            async {
                future::interruptible(pending::<()>())
                    .await
                    .map_err(AxError::from)?;
                Ok(0)
            },
        )))
    };

    wait_io_result(uctx, sigmask, &mut wait_once)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct TestPlanEntry {
        source_key: u64,
        events: IoEvents,
        registration_events: Option<IoEvents>,
        output_index: usize,
    }

    impl RegistrationPlanEntry for TestPlanEntry {
        fn source_key(&self) -> u64 {
            self.source_key
        }

        fn poll_events(&self) -> IoEvents {
            self.events
        }

        fn registration_events(&self) -> Option<IoEvents> {
            self.registration_events
        }

        fn set_registration_events(&mut self, events: Option<IoEvents>) {
            self.registration_events = events;
        }
    }

    #[test]
    fn duplicate_poll_sources_merge_one_registration_interest() {
        let mut duplicate = [TestPlanEntry {
            source_key: 7,
            events: IoEvents::READABLE,
            registration_events: None,
            output_index: 0,
        }; 65];
        for (index, entry) in duplicate.iter_mut().enumerate() {
            entry.output_index = index;
        }
        let unique = prepare_registration_sources(&mut duplicate);
        assert_eq!(unique, 1);
        assert_eq!(duplicate[0].registration_events, Some(IoEvents::READABLE));

        let mut mixed = [
            TestPlanEntry {
                source_key: 3,
                events: IoEvents::READ_HANGUP,
                registration_events: None,
                output_index: 0,
            },
            TestPlanEntry {
                source_key: 1,
                events: IoEvents::READABLE,
                registration_events: None,
                output_index: 1,
            },
            TestPlanEntry {
                source_key: 2,
                events: IoEvents::PRIORITY,
                registration_events: None,
                output_index: 2,
            },
            TestPlanEntry {
                source_key: 1,
                events: IoEvents::WRITABLE,
                registration_events: None,
                output_index: 3,
            },
        ];
        let unique = prepare_registration_sources(&mut mixed);
        assert_eq!(unique, 3);
        let mut registrations = mixed[..unique]
            .iter()
            .map(|entry| (entry.source_key, entry.registration_events.unwrap()))
            .collect::<Vec<_>>();
        registrations.sort_unstable_by_key(|(key, _)| *key);
        assert_eq!(
            registrations,
            [
                (1, IoEvents::READABLE | IoEvents::WRITABLE),
                (2, IoEvents::PRIORITY),
                (3, IoEvents::READ_HANGUP),
            ]
        );

        let mut restored_order = mixed
            .iter()
            .map(|entry| (entry.output_index, entry.source_key))
            .collect::<Vec<_>>();
        restored_order.sort_unstable_by_key(|(output_index, _)| *output_index);
        assert_eq!(restored_order, [(0, 3), (1, 1), (2, 2), (3, 1)]);
    }
}
