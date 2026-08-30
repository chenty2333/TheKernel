//! The descriptor half of the initial perf-event implementation.
//!
//! It deliberately only represents software clocks.  Hardware PMU scheduling
//! is kept out of this object so an event FD remains an ordinary anonymous
//! descriptor while the per-CPU backend is introduced separately.

use alloc::{borrow::Cow, sync::Arc};
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

/// An enabled software perf event. `accumulated` is always a complete stopped
/// interval; the running interval is sampled atomically from the monotonic
/// clock, avoiding a reader/ioctl lock in the common read path.
pub struct PerfEventFile {
    id: u64,
    state: Mutex<PerfEventState>,
}

struct PerfEventState {
    enabled: bool,
    accumulated: u64,
    started: u64,
}

impl PerfEventFile {
    pub fn new(id: u64, disabled: bool) -> Arc<Self> {
        let now = monotonic_time_nanos();
        Arc::new(Self {
            id,
            state: Mutex::new(PerfEventState {
                enabled: !disabled,
                accumulated: 0,
                started: now,
            }),
        })
    }

    fn count(&self) -> u64 {
        let state = self.state.lock();
        if state.enabled {
            state
                .accumulated
                .saturating_add(monotonic_time_nanos().saturating_sub(state.started))
        } else {
            state.accumulated
        }
    }

    fn disable(&self) {
        let mut state = self.state.lock();
        if state.enabled {
            state.accumulated = state
                .accumulated
                .saturating_add(monotonic_time_nanos().saturating_sub(state.started));
            state.enabled = false;
        }
    }

    fn enable(&self) {
        let mut state = self.state.lock();
        if !state.enabled {
            state.started = monotonic_time_nanos();
            state.enabled = true;
        }
    }

    fn reset(&self) {
        let mut state = self.state.lock();
        state.accumulated = 0;
        state.started = monotonic_time_nanos();
    }
}

impl FileLike for PerfEventFile {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }

    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
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
        ) && arg != 0
        {
            return Err(AxError::InvalidInput);
        }
        match cmd {
            PERF_EVENT_IOC_ENABLE | PERF_EVENT_IOC_REFRESH => {
                self.enable();
                Ok(0)
            }
            PERF_EVENT_IOC_DISABLE => {
                self.disable();
                Ok(0)
            }
            PERF_EVENT_IOC_RESET => {
                self.reset();
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
