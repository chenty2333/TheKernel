//! The descriptor half of the initial perf-event implementation.
//!
//! It deliberately only represents software clocks.  Hardware PMU scheduling
//! is kept out of this object so an event FD remains an ordinary anonymous
//! descriptor while the per-CPU backend is introduced separately.

use alloc::{
    borrow::Cow,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{mem::size_of, task::Context};

use axerrno::{AxError, AxResult};
use axhal::time::monotonic_time_nanos;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use axsync::Mutex;

use crate::{
    file::{FileLike, IoDst, IoSrc, IoctlContext, Kstat, anon_inode_stat},
    mm::map_usercopy_error,
};

/// Linux's `_IO('$', n)` encodings for the operations supported by this stage.
const PERF_EVENT_IOC_ENABLE: u32 = 0x2400;
const PERF_EVENT_IOC_DISABLE: u32 = 0x2401;
const PERF_EVENT_IOC_REFRESH: u32 = 0x2402;
const PERF_EVENT_IOC_RESET: u32 = 0x2403;
const PERF_EVENT_IOC_ID: u32 = 0x8008_2407;
const PERF_IOC_FLAG_GROUP: usize = 1;

pub(crate) struct PerfGroup {
    /// A perf group is scoped to one exact task context. Task IDs are never
    /// reused by a live `AxTask`, so this remains stable even if the Linux TID
    /// namespace later recycles its visible number.
    target_task_id: u64,
    /// The only descriptor accepted as `group_fd`.
    leader_id: u64,
    members: Mutex<Vec<Weak<PerfEventFile>>>,
}

impl PerfGroup {
    pub(crate) fn new(target_task_id: u64, leader_id: u64) -> Arc<Self> {
        Arc::new(Self {
            target_task_id,
            leader_id,
            members: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn accepts_target(&self, target_task_id: u64) -> bool {
        self.target_task_id == target_task_id
    }

    fn is_leader(&self, id: u64) -> bool {
        self.leader_id == id
    }

    #[cfg(test)]
    pub(crate) fn is_group_leader_for_test(&self, id: u64) -> bool {
        self.is_leader(id)
    }

    fn add(&self, event: &Arc<PerfEventFile>) -> AxResult<()> {
        let mut members = self.members.lock();
        members.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        members.push(Arc::downgrade(event));
        Ok(())
    }

    fn with_live(&self, operation: impl Fn(&PerfEventFile)) {
        let mut members = self.members.lock();
        members.retain(|member| {
            let Some(member) = member.upgrade() else {
                return false;
            };
            operation(&member);
            true
        });
    }

    fn counts(&self, dst: &mut IoDst) -> AxResult<usize> {
        let members = self.members.lock();
        let mut live = Vec::new();
        live.try_reserve(members.len())
            .map_err(|_| AxError::NoMemory)?;
        live.extend(members.iter().filter_map(Weak::upgrade));
        let count = live.len();
        let bytes = (count + 1)
            .checked_mul(size_of::<u64>())
            .ok_or(AxError::InvalidInput)?;
        if dst.remaining_mut() < bytes {
            return Err(AxError::InvalidInput);
        }
        dst.write(&(count as u64).to_ne_bytes())?;
        for member in &live {
            dst.write(&member.count().to_ne_bytes())?;
        }
        Ok(bytes)
    }
}

/// An enabled software perf event. `accumulated` is always a complete stopped
/// interval; the running interval is sampled atomically from the monotonic
/// clock, avoiding a reader/ioctl lock in the common read path.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum SoftwareEvent {
    CpuClock,
    TaskClock,
    PageFaults,
    ContextSwitches,
}

pub struct PerfEventFile {
    id: u64,
    event: SoftwareEvent,
    group: Arc<PerfGroup>,
    read_group: bool,
    state: Mutex<PerfEventState>,
}

struct PerfEventState {
    enabled: bool,
    running: bool,
    accumulated: u64,
    started: u64,
}

impl PerfEventFile {
    pub fn new(
        id: u64,
        event: SoftwareEvent,
        disabled: bool,
        group: Arc<PerfGroup>,
        read_group: bool,
    ) -> AxResult<Arc<Self>> {
        let now = monotonic_time_nanos();
        let file = Arc::try_new(Self {
            id,
            event,
            group: group.clone(),
            read_group,
            state: Mutex::new(PerfEventState {
                enabled: !disabled,
                running: false,
                accumulated: 0,
                started: now,
            }),
        })
        .map_err(|_| AxError::NoMemory)?;
        group.add(&file)?;
        Ok(file)
    }

    pub(crate) fn group(&self) -> Arc<PerfGroup> {
        self.group.clone()
    }

    pub(crate) fn is_group_leader(&self) -> bool {
        self.group.is_leader(self.id)
    }

    fn count(&self) -> u64 {
        let state = self.state.lock();
        if state.running {
            state
                .accumulated
                .saturating_add(monotonic_time_nanos().saturating_sub(state.started))
        } else {
            state.accumulated
        }
    }

    fn disable(&self) {
        let mut state = self.state.lock();
        if state.running {
            state.accumulated = state
                .accumulated
                .saturating_add(monotonic_time_nanos().saturating_sub(state.started));
            state.running = false;
        }
        state.enabled = false;
    }

    fn enable(&self) {
        let mut state = self.state.lock();
        if !state.enabled {
            state.enabled = true;
        }
    }

    fn reset(&self) {
        let mut state = self.state.lock();
        state.accumulated = 0;
        state.started = monotonic_time_nanos();
    }

    pub(crate) fn on_enter(&self) {
        if !matches!(
            self.event,
            SoftwareEvent::CpuClock | SoftwareEvent::TaskClock
        ) {
            return;
        }
        let mut state = self.state.lock();
        if state.enabled && !state.running {
            state.started = monotonic_time_nanos();
            state.running = true;
        }
    }

    pub(crate) fn on_leave(&self) {
        let mut state = self.state.lock();
        if state.running {
            state.accumulated = state
                .accumulated
                .saturating_add(monotonic_time_nanos().saturating_sub(state.started));
            state.running = false;
        }
        if state.enabled && self.event == SoftwareEvent::ContextSwitches {
            state.accumulated = state.accumulated.saturating_add(1);
        }
    }

    pub(crate) fn on_fault(&self) {
        if self.event == SoftwareEvent::PageFaults {
            let mut state = self.state.lock();
            if state.enabled {
                state.accumulated = state.accumulated.saturating_add(1);
            }
        }
    }
}

impl FileLike for PerfEventFile {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }

    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        if self.read_group {
            return self.group.counts(dst);
        }
        if dst.remaining_mut() < size_of::<u64>() {
            return Err(AxError::InvalidInput);
        }
        dst.write(&self.count().to_ne_bytes())?;
        Ok(size_of::<u64>())
    }

    fn write(&self, _src: &mut IoSrc) -> AxResult<usize> {
        Err(AxError::BadFileDescriptor)
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        // PERF_IOC_FLAG_GROUP is intentionally not accepted before group
        // scheduling is installed; silently applying it to one member would
        // be observably wrong.
        if matches!(
            cmd,
            PERF_EVENT_IOC_ENABLE
                | PERF_EVENT_IOC_DISABLE
                | PERF_EVENT_IOC_REFRESH
                | PERF_EVENT_IOC_RESET
        ) && arg & !PERF_IOC_FLAG_GROUP != 0
        {
            return Err(AxError::InvalidInput);
        }
        match cmd {
            PERF_EVENT_IOC_ENABLE | PERF_EVENT_IOC_REFRESH => {
                if arg & PERF_IOC_FLAG_GROUP != 0 {
                    self.group.with_live(Self::enable);
                } else {
                    self.enable();
                }
                Ok(0)
            }
            PERF_EVENT_IOC_DISABLE => {
                if arg & PERF_IOC_FLAG_GROUP != 0 {
                    self.group.with_live(Self::disable);
                } else {
                    self.disable();
                }
                Ok(0)
            }
            PERF_EVENT_IOC_RESET => {
                if arg & PERF_IOC_FLAG_GROUP != 0 {
                    self.group.with_live(Self::reset);
                } else {
                    self.reset();
                }
                Ok(0)
            }
            PERF_EVENT_IOC_ID => {
                context
                    .user_memory()
                    .write_value(arg as *mut u64, self.id)
                    .map_err(map_usercopy_error)?;
                Ok(0)
            }
            _ => Err(AxError::InvalidInput),
        }
    }

    fn nonblocking(&self) -> bool {
        false
    }
    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        Ok(())
    }
    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[perf_event]".into())
    }
}

impl Pollable for PerfEventFile {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::empty()
    }
}
