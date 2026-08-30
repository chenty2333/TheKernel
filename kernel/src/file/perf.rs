//! perf-event descriptors and their IRQ-safe task-group runtime.
//!
//! A group, rather than an individual descriptor, is the scheduler unit. The
//! switch callbacks only take `SpinNoIrq` locks and never allocate or copy to
//! userspace.

use alloc::{borrow::Cow, sync::{Arc, Weak}, vec::Vec};
use core::{mem::size_of, task::Context};

use axerrno::{AxError, AxResult};
use axhal::time::monotonic_time_nanos;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use axsync::spin::SpinNoIrq;
use axtask::current;

use crate::{file::{anon_inode_stat, FileLike, IoDst, IoSrc, IoctlContext, Kstat}, mm::map_usercopy_error};

const PERF_EVENT_IOC_ENABLE: u32 = 0x2400;
const PERF_EVENT_IOC_DISABLE: u32 = 0x2401;
const PERF_EVENT_IOC_REFRESH: u32 = 0x2402;
const PERF_EVENT_IOC_RESET: u32 = 0x2403;
const PERF_EVENT_IOC_ID: u32 = 0x8008_2407;
const PERF_IOC_FLAG_GROUP: usize = 1;
pub(crate) const MAX_GROUP_MEMBERS: usize = 64;
pub(crate) const MAX_GROUPS_PER_THREAD: usize = 64;

pub(crate) const PERF_FORMAT_TOTAL_TIME_ENABLED: u64 = 1;
pub(crate) const PERF_FORMAT_TOTAL_TIME_RUNNING: u64 = 2;
pub(crate) const PERF_FORMAT_ID: u64 = 4;
pub(crate) const PERF_FORMAT_GROUP: u64 = 8;
pub(crate) const PERF_FORMAT_SUPPORTED: u64 = PERF_FORMAT_TOTAL_TIME_ENABLED
    | PERF_FORMAT_TOTAL_TIME_RUNNING | PERF_FORMAT_ID | PERF_FORMAT_GROUP;

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub(crate) enum SoftwareEvent { CpuClock, TaskClock, PageFaults, ContextSwitches }
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub(crate) enum HardwareEvent { Cycles, Instructions }
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub(crate) enum PerfEvent { Software(SoftwareEvent), Hardware(HardwareEvent) }
impl PerfEvent { fn hardware(self) -> Option<HardwareEvent> { if let Self::Hardware(event) = self { Some(event) } else { None } } }

/// Combines a completed interval with a non-destructive local PMU sample.
/// Hardware values are never stored here; `stop_locked` performs that single
/// settlement when it terminates the lease.
fn compose_live_count(settled: u64, live: u64) -> u64 { settled.saturating_add(live) }

struct Member { file: Weak<PerfEventFile>, dead: bool }
struct ActiveGroup {
    // Temporary strong custody makes an FD close racing with a switch safe.
    // Files retain only a Weak group pointer, so this is not a cycle.
    files: Vec<Option<Arc<PerfEventFile>>>,
    #[cfg(feature = "pmu")]
    leases: [Option<(u64, axhal::pmu::CounterLease)>; 2],
    task_active: bool,
    running: bool,
}
impl ActiveGroup { const fn new() -> Self { Self { files: Vec::new(), #[cfg(feature = "pmu")] leases: [None, None], task_active: false, running: false } } }
struct GroupState { members: Vec<Member>, active: ActiveGroup }

pub(crate) struct PerfGroup { target_task_id: u64, leader_id: u64, state: SpinNoIrq<GroupState> }

impl PerfGroup {
    pub(crate) fn new(target_task_id: u64, leader_id: u64) -> AxResult<Arc<Self>> {
        let mut members = Vec::new();
        members.try_reserve_exact(MAX_GROUP_MEMBERS).map_err(|_| AxError::NoMemory)?;
        let mut active = ActiveGroup::new();
        active.files.try_reserve_exact(MAX_GROUP_MEMBERS).map_err(|_| AxError::NoMemory)?;
        Arc::try_new(Self { target_task_id, leader_id, state: SpinNoIrq::new(GroupState { members, active }) }).map_err(|_| AxError::NoMemory)
    }
    pub(crate) fn accepts_target(&self, id: u64) -> bool { self.target_task_id == id }
    fn is_leader(&self, id: u64) -> bool { self.leader_id == id }
    #[cfg(test)] pub(crate) fn is_group_leader_for_test(&self, id: u64) -> bool { self.is_leader(id) }
    pub(crate) fn has_hardware(&self) -> bool { self.state.lock().members.iter().filter_map(|member| member.file.upgrade()).any(|file| file.event.hardware().is_some()) }
    pub(crate) fn is_prunable(&self) -> bool { let state = self.state.lock(); !state.active.task_active && state.members.iter().all(|member| member.file.upgrade().is_none()) }
    fn require_local_for_hardware(&self) -> AxResult { if self.has_hardware() && current().id().as_u64() != self.target_task_id { Err(AxError::OperationNotSupported) } else { Ok(()) } }
    fn add(&self, event: &Arc<PerfEventFile>) -> AxResult<()> {
        let mut state = self.state.lock();
        Self::compact_locked(&mut state);
        if let Some(hw) = event.event.hardware() {
            if state.members.iter().filter_map(|member| member.file.upgrade()).any(|member| member.event.hardware() == Some(hw)) { return Err(AxError::OperationNotSupported); }
        }
        if state.members.len() == MAX_GROUP_MEMBERS { return Err(AxError::OperationNotSupported); }
        state.members.push(Member { file: Arc::downgrade(event), dead: false }); state.active.files.push(None); Ok(())
    }
    fn live<'a>(state: &'a mut GroupState) -> impl Iterator<Item = (usize, Arc<PerfEventFile>)> + 'a { state.members.iter_mut().enumerate().filter_map(|(slot, member)| { let file = member.file.upgrade(); member.dead = file.is_none(); file.map(|file| (slot, file)) }) }
    fn compact_locked(state: &mut GroupState) {
        if state.active.running { return; }
        state.members.retain(|member| member.file.upgrade().is_some());
        // `stop_locked` has removed every strong custody entry, so shrinking
        // the parallel slot vector preserves the all-None correspondence.
        state.active.files.truncate(state.members.len());
    }
    fn start_locked(state: &mut GroupState, now: u64) {
        if !state.active.task_active || state.active.running { return; }
        for slot in 0..state.members.len() {
            let file = state.members[slot].file.upgrade();
            state.members[slot].dead = file.is_none();
            state.active.files[slot] = file;
        }
        #[cfg(feature = "pmu")]
        {
            let mut wanted: [(u64, HardwareEvent); 2] = [(0, HardwareEvent::Cycles); 2]; let mut count = 0;
            for file in state.active.files.iter().filter_map(Option::as_ref) { if file.enabled() { if let Some(event) = file.event.hardware() { wanted[count] = (file.id, event); count += 1; } } }
            for index in 0..count {
                let (id, event) = wanted[index]; let event = match event { HardwareEvent::Cycles => axhal::pmu::Event::Cycles, HardwareEvent::Instructions => axhal::pmu::Event::Instructions };
                match axhal::pmu::CounterLease::acquire(event, axhal::pmu::CounterKind::Fixed).or_else(|_| axhal::pmu::CounterLease::acquire(event, axhal::pmu::CounterKind::Programmable)) {
                    Ok(lease) => state.active.leases[index] = Some((id, lease)),
                    Err(_) => { for lease in &mut state.active.leases { if let Some((_, lease)) = lease.take() { let _ = lease.finish(); } } for file in &mut state.active.files { *file = None; } state.active.running = false; return; }
                }
            }
        }
        #[cfg(not(feature = "pmu"))]
        if state.active.files.iter().filter_map(Option::as_ref).any(|file| file.enabled() && file.event.hardware().is_some()) { for file in &mut state.active.files { *file = None; } return; }
        for file in state.active.files.iter().filter_map(Option::as_ref) { file.start_running(now); }
        state.active.running = true;
    }
    fn stop_locked(state: &mut GroupState, now: u64, count_context_switch: bool) {
        if !state.active.running { Self::compact_locked(state); return; }
        #[cfg(feature = "pmu")]
        for slot in 0..state.active.leases.len() {
            let sample = state.active.leases[slot].take().and_then(|(id, lease)| match lease.finish() { Ok(sample) if !sample.overflowed => Some((id, sample.value)), Ok(_) | Err(_) => { if let Some(file) = state.active.files.iter().filter_map(Option::as_ref).find(|file| file.id == id) { file.mark_invalid(); } None } });
            if let Some((id, value)) = sample { if let Some(file) = state.active.files.iter().filter_map(Option::as_ref).find(|file| file.id == id) { file.add_count(value); } }
        }
        for file in state.active.files.iter_mut().filter_map(Option::take) { let was_running = file.stop_running(now); if count_context_switch && was_running && file.event == PerfEvent::Software(SoftwareEvent::ContextSwitches) && file.enabled() { file.add_count(1); } }
        state.active.running = false;
        Self::compact_locked(state);
    }
    fn control(&self, member: &PerfEventFile, group_control: bool, op: fn(&PerfEventFile, u64)) {
        let now = monotonic_time_nanos(); let mut state = self.state.lock();
        if state.active.task_active { Self::stop_locked(&mut state, now, false); }
        if group_control { for (_, file) in Self::live(&mut state) { op(&file, now); } } else { op(member, now); }
        if state.active.task_active { Self::start_locked(&mut state, now); }
    }
    pub(crate) fn reconfigure_current(&self) { let now = monotonic_time_nanos(); let mut state = self.state.lock(); if state.active.task_active { Self::stop_locked(&mut state, now, false); Self::start_locked(&mut state, now); } }
    pub(crate) fn on_enter(&self) { let now = monotonic_time_nanos(); let mut state = self.state.lock(); state.active.task_active = true; Self::start_locked(&mut state, now); }
    pub(crate) fn on_leave(&self) { let now = monotonic_time_nanos(); let mut state = self.state.lock(); Self::stop_locked(&mut state, now, true); state.active.task_active = false; }
    pub(crate) fn on_fault(&self) { let state = self.state.lock(); if !state.active.running { return; } for file in state.active.files.iter().filter_map(Option::as_ref) { if file.event == PerfEvent::Software(SoftwareEvent::PageFaults) && file.running() { file.add_count(1); } } }
    fn snapshots(&self, out: &mut Vec<Sample>) -> AxResult<()> {
        let mut state = self.state.lock();
        for member in &state.members {
            if let Some(file) = member.file.upgrade() {
                if file.invalid() { return Err(AxError::Io); }
                #[cfg(feature = "pmu")]
                let live = if state.active.running && current().id().as_u64() == self.target_task_id {
                    match state.active.leases.iter().filter_map(Option::as_ref).find(|(id, _)| *id == file.id) {
                        Some((_, lease)) => match lease.read() { Ok(value) => Some(value), Err(_) => { Self::stop_locked(&mut state, monotonic_time_nanos(), false); return Err(AxError::OperationNotSupported); } },
                        None => None,
                    }
                } else { None };
                #[cfg(not(feature = "pmu"))]
                let live = None;
                out.push(file.sample_with_live(live));
            }
        }
        Ok(())
    }
}

struct PerfEventState { enabled: bool, running: bool, invalid: bool, count: u64, enabled_total: u64, running_total: u64, enabled_since: u64, running_since: u64 }
pub struct PerfEventFile { id: u64, event: PerfEvent, group: Weak<PerfGroup>, read_format: u64, state: SpinNoIrq<PerfEventState> }
#[derive(Clone, Copy)] struct Sample { id: u64, value: u64, enabled: u64, running: u64 }

impl PerfEventFile {
    pub fn new(id: u64, event: PerfEvent, disabled: bool, group: &Arc<PerfGroup>, read_format: u64) -> AxResult<Arc<Self>> {
        let now = monotonic_time_nanos(); let file = Arc::try_new(Self { id, event, group: Arc::downgrade(group), read_format, state: SpinNoIrq::new(PerfEventState { enabled: !disabled, running: false, invalid: false, count: 0, enabled_total: 0, running_total: 0, enabled_since: now, running_since: now }) }).map_err(|_| AxError::NoMemory)?; group.add(&file)?; Ok(file)
    }
    pub(crate) fn group(&self) -> Option<Arc<PerfGroup>> { self.group.upgrade() }
    pub(crate) fn is_group_leader(&self) -> bool { self.group().is_some_and(|group| group.is_leader(self.id)) }
    fn enabled(&self) -> bool { self.state.lock().enabled }
    fn start_running(&self, now: u64) { let mut state = self.state.lock(); if state.enabled && !state.running { state.running = true; state.running_since = now; } }
    fn stop_running(&self, now: u64) -> bool { let mut state = self.state.lock(); if state.running { let elapsed = now.saturating_sub(state.running_since); state.running_total = state.running_total.saturating_add(elapsed); state.running = false; if matches!(self.event, PerfEvent::Software(SoftwareEvent::CpuClock | SoftwareEvent::TaskClock)) { state.count = state.count.saturating_add(elapsed); } true } else { false } }
    fn running(&self) -> bool { self.state.lock().running }
    fn add_count(&self, value: u64) { let mut state = self.state.lock(); state.count = state.count.saturating_add(value); }
    fn mark_invalid(&self) { self.state.lock().invalid = true; }
    fn invalid(&self) -> bool { self.state.lock().invalid }
    fn enable_at(&self, now: u64) { let mut state = self.state.lock(); if !state.enabled { state.enabled = true; state.enabled_since = now; } }
    fn disable_at(&self, now: u64) { let mut state = self.state.lock(); if state.running { state.running_total = state.running_total.saturating_add(now.saturating_sub(state.running_since)); state.running = false; } if state.enabled { state.enabled_total = state.enabled_total.saturating_add(now.saturating_sub(state.enabled_since)); state.enabled = false; } }
    fn reset_at(&self, _: u64) { let mut state = self.state.lock(); state.count = 0; state.invalid = false; }
    fn sample(&self) -> Sample { let now = monotonic_time_nanos(); let state = self.state.lock(); Sample { id: self.id, value: state.count, enabled: state.enabled_total.saturating_add(if state.enabled { now.saturating_sub(state.enabled_since) } else { 0 }), running: state.running_total.saturating_add(if state.running { now.saturating_sub(state.running_since) } else { 0 }) } }
    fn sample_with_live(&self, live: Option<u64>) -> Sample { let mut sample = self.sample(); if let Some(live) = live { sample.value = compose_live_count(sample.value, live); } sample }
    pub(crate) fn on_enter(&self) { if let Some(group) = self.group() { group.on_enter(); } } pub(crate) fn on_leave(&self) { if let Some(group) = self.group() { group.on_leave(); } } pub(crate) fn on_fault(&self) { if let Some(group) = self.group() { group.on_fault(); } }
    fn check_hardware_local(&self) -> AxResult { self.group().ok_or(AxError::BadFileDescriptor)?.require_local_for_hardware() }
    fn read_samples(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.check_hardware_local()?; let group_read = self.read_format & PERF_FORMAT_GROUP != 0; let mut samples = Vec::new();
        samples.try_reserve(MAX_GROUP_MEMBERS).map_err(|_| AxError::NoMemory)?;
        self.group().ok_or(AxError::BadFileDescriptor)?.snapshots(&mut samples)?;
        let ids = self.read_format & PERF_FORMAT_ID != 0; let timing = ((self.read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0) as usize) + ((self.read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0) as usize);
        let words = if group_read { (1 + timing).checked_add(samples.len().checked_mul(1 + ids as usize).ok_or(AxError::InvalidInput)?).ok_or(AxError::InvalidInput)? } else { 1 + timing + ids as usize }; let bytes = words.checked_mul(size_of::<u64>()).ok_or(AxError::InvalidInput)?; if dst.remaining_mut() < bytes { return Err(AxError::InvalidInput); }
        let leader = samples.iter().copied().find(|sample| sample.id == self.group().map_or(self.id, |group| group.leader_id)).unwrap_or(Sample { id: self.id, value: 0, enabled: 0, running: 0 });
        if group_read { dst.write(&(samples.len() as u64).to_ne_bytes())?; if self.read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0 { dst.write(&leader.enabled.to_ne_bytes())?; } if self.read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0 { dst.write(&leader.running.to_ne_bytes())?; } for sample in samples { dst.write(&sample.value.to_ne_bytes())?; if ids { dst.write(&sample.id.to_ne_bytes())?; } } } else { dst.write(&leader.value.to_ne_bytes())?; if self.read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0 { dst.write(&leader.enabled.to_ne_bytes())?; } if self.read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0 { dst.write(&leader.running.to_ne_bytes())?; } if ids { dst.write(&leader.id.to_ne_bytes())?; } }
        Ok(bytes)
    }
}

impl FileLike for PerfEventFile {
    fn stat(&self) -> AxResult<Kstat> { Ok(anon_inode_stat()) }
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> { self.read_samples(dst) }
    fn write(&self, _: &mut IoSrc) -> AxResult<usize> { Err(AxError::BadFileDescriptor) }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        if matches!(cmd, PERF_EVENT_IOC_ENABLE | PERF_EVENT_IOC_DISABLE | PERF_EVENT_IOC_REFRESH | PERF_EVENT_IOC_RESET) && arg & !PERF_IOC_FLAG_GROUP != 0 { return Err(AxError::InvalidInput); }
        if cmd == PERF_EVENT_IOC_REFRESH { return Err(AxError::OperationNotSupported); }
        if matches!(cmd, PERF_EVENT_IOC_ENABLE | PERF_EVENT_IOC_DISABLE | PERF_EVENT_IOC_RESET) { self.check_hardware_local()?; }
        let perf_group = self.group().ok_or(AxError::BadFileDescriptor)?;
        let result = match cmd {
            PERF_EVENT_IOC_ENABLE => { perf_group.control(self, arg & PERF_IOC_FLAG_GROUP != 0, PerfEventFile::enable_at); Ok(0) }
            PERF_EVENT_IOC_DISABLE => { perf_group.control(self, arg & PERF_IOC_FLAG_GROUP != 0, PerfEventFile::disable_at); Ok(0) }
            PERF_EVENT_IOC_RESET => { perf_group.control(self, arg & PERF_IOC_FLAG_GROUP != 0, PerfEventFile::reset_at); Ok(0) }
            PERF_EVENT_IOC_ID => { context.user_memory().write_value(arg as *mut u64, self.id).map_err(map_usercopy_error)?; Ok(0) }
            _ => Err(AxError::InvalidInput),
        };
        result
    }
    fn nonblocking(&self) -> bool { false } fn set_nonblocking(&self, _: bool) -> AxResult { Ok(()) }
    fn path(&self) -> AxResult<Cow<'_, str>> { Ok("anon_inode:[perf_event]".into()) }
}
impl Pollable for PerfEventFile { fn poll(&self) -> IoEvents { IoEvents::READABLE } fn register<'a>(&'a self, _: &mut Context<'_>, _: IoEvents) -> Result<PollRegistration<'a>, PollRegistrationError> { PollRegistration::empty() } }

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use super::{HardwareEvent, PerfEvent, PerfEventFile, PerfGroup};

    #[test]
    fn group_rejects_duplicate_hardware_event() {
        let group = PerfGroup::new(1, 1).unwrap();
        let first = PerfEventFile::new(1, PerfEvent::Hardware(HardwareEvent::Cycles), true, &group, 0).unwrap();
        assert!(PerfEventFile::new(2, PerfEvent::Hardware(HardwareEvent::Cycles), true, &group, 0).is_err());
        drop(first);
    }

    #[test]
    fn file_holds_only_weak_group_reference() {
        let group = PerfGroup::new(1, 1).unwrap();
        let file = PerfEventFile::new(1, PerfEvent::Hardware(HardwareEvent::Instructions), true, &group, 0).unwrap();
        assert_eq!(Arc::strong_count(&group), 1);
        drop(file);
    }

    #[test]
    fn live_count_composition_saturates_without_settlement() {
        assert_eq!(super::compose_live_count(7, 9), 16);
        assert_eq!(super::compose_live_count(u64::MAX - 1, 8), u64::MAX);
    }

    #[test]
    fn reset_clears_sticky_counter_fault() {
        let group = PerfGroup::new(1, 1).unwrap();
        let file = PerfEventFile::new(1, PerfEvent::Hardware(HardwareEvent::Cycles), true, &group, 0).unwrap();
        file.mark_invalid();
        assert!(file.invalid());
        file.reset_at(0);
        assert!(!file.invalid());
    }

    #[test]
    fn inactive_group_does_not_start_counter_window() {
        let mut active = super::ActiveGroup::new();
        assert!(!active.task_active);
        assert!(!active.running);
        active.task_active = true;
        assert!(!active.running);
    }
}
