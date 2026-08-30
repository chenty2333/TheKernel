//! The deliberately small, hardware-only perf sampling ABI.
//!
//! Sampling is not a `PerfGroup`: it has one programmable counter and one
//! producer-owned mmap ring per task.  Keeping it separate prevents a PMU
//! overflow interrupt from acquiring the counting group's lifecycle lock.

use alloc::{borrow::Cow, sync::Arc};
use core::task::Context;

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use axsync::spin::SpinNoIrq;
use kernel_guard::NoPreemptIrqSave;
use memory_addr::PAGE_SIZE_4K;

use crate::{
    file::{
        FileLike, FileMmapProtection, FileMmapRequest, FileMmapSharing, FixedSharedMmapRegion,
        IoDst, IoSrc, IoctlContext, Kstat, PreparedFileMmap, anon_inode_stat,
    },
    mm::{SharedAtomicU64, SharedFixedView, SharedPages},
};

pub(crate) const PERF_SAMPLE_IP: u64 = 1;
pub(crate) const PERF_SAMPLE_TIME: u64 = 1 << 2;
pub(crate) const PERF_SAMPLE_CPU: u64 = 1 << 7;
pub(crate) const PERF_SAMPLE_PERIOD: u64 = 1 << 8;
pub(crate) const PERF_SAMPLE_SUPPORTED: u64 =
    PERF_SAMPLE_IP | PERF_SAMPLE_TIME | PERF_SAMPLE_CPU | PERF_SAMPLE_PERIOD;
const PERF_RECORD_LOST: u32 = 2;
const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_RECORD_MISC_USER: u16 = 2;
const PERF_EVENT_IOC_ENABLE: u32 = 0x2400;
const PERF_EVENT_IOC_DISABLE: u32 = 0x2401;
const PERF_EVENT_IOC_RESET: u32 = 0x2403;
const PERF_EVENT_IOC_ID: u32 = 0x8008_2407;
const PERF_FORMAT_TOTAL_TIME_ENABLED: u64 = 1;
const PERF_FORMAT_TOTAL_TIME_RUNNING: u64 = 2;
const PERF_FORMAT_ID: u64 = 4;
const PAGE: usize = PAGE_SIZE_4K;
const MIN_DATA: usize = PAGE;
const MAX_DATA: usize = 1024 * 1024;
const DATA_HEAD: usize = 1024;
const DATA_TAIL: usize = 1032;
const DATA_OFFSET: usize = 1040;
const DATA_SIZE: usize = 1048;

#[derive(Clone, Copy)]
pub(crate) enum SamplingEvent {
    Cycles,
    Instructions,
}

#[derive(Clone, Copy)]
pub(crate) struct SamplingConfig {
    pub id: u64,
    pub target_task_id: u64,
    pub event: SamplingEvent,
    pub period: u64,
    pub sample_type: u64,
    pub count_user: bool,
    pub count_kernel: bool,
    pub disabled: bool,
    pub read_format: u64,
}

struct Ring {
    region: FixedSharedMmapRegion,
    view: SharedFixedView,
    head: SharedAtomicU64,
    tail: SharedAtomicU64,
    data_size: usize,
    producer_head: u64,
    lost: u64,
}

struct SamplingState {
    enabled: bool,
    closed: bool,
    failed: bool,
    value: u64,
    enabled_total: u64,
    running_total: u64,
    enabled_since: u64,
    running_since: Option<u64>,
    ring: Option<Ring>,
}

/// An OFD-owned sampling event.  The producer state is IRQ-safe and has no
/// allocation path after mmap has installed its fixed backing.
pub(crate) struct PerfSamplingFile {
    config: SamplingConfig,
    state: SpinNoIrq<SamplingState>,
    waiters: PollSet<4>,
}

struct CpuCustody {
    event: Arc<PerfSamplingFile>,
    token: axhal::pmu::SamplingToken,
    cookie: u64,
}

static CUSTODY: [SpinNoIrq<Option<CpuCustody>>; axconfig::plat::MAX_CPU_NUM] =
    [const { SpinNoIrq::new(None) }; axconfig::plat::MAX_CPU_NUM];

impl PerfSamplingFile {
    pub(crate) fn try_new(config: SamplingConfig) -> AxResult<Arc<Self>> {
        let now = axhal::time::monotonic_time_nanos();
        Arc::try_new(Self {
            state: SpinNoIrq::new(SamplingState {
                enabled: !config.disabled,
                closed: false,
                failed: false,
                value: 0,
                enabled_total: 0,
                running_total: 0,
                enabled_since: now,
                running_since: None,
                ring: None,
            }),
            config,
            waiters: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn enabled(&self) -> bool {
        let state = self.state.lock();
        state.enabled && !state.closed && !state.failed
    }

    fn target_current(&self) -> bool {
        axtask::current().id().as_u64() == self.config.target_task_id
    }

    pub(crate) fn enter_current(self: &Arc<Self>) {
        if !self.target_current() {
            return;
        }
        if !self.enabled() {
            return;
        }
        if self.state.lock().ring.is_none() {
            return;
        }
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        if cpu >= CUSTODY.len() || CUSTODY[cpu].lock().is_some() {
            return;
        }
        let event = match self.config.event {
            SamplingEvent::Cycles => axhal::pmu::Event::Cycles,
            SamplingEvent::Instructions => axhal::pmu::Event::Instructions,
        };
        let program = axhal::pmu::SamplingProgram {
            event,
            period: self.config.period,
            count_user: self.config.count_user,
            count_kernel: self.config.count_kernel,
            cookie: self.config.id,
        };
        let Ok(token) = axhal::pmu::sampling_arm_local(program) else {
            return;
        };
        *CUSTODY[cpu].lock() = Some(CpuCustody {
            event: self.clone(),
            token,
            cookie: self.config.id,
        });
        self.state.lock().running_since = Some(axhal::time::monotonic_time_nanos());
    }

    pub(crate) fn leave_current() {
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        let Some(custody) = CUSTODY.get(cpu).and_then(|slot| slot.lock().take()) else {
            return;
        };
        if let Ok(sample) = axhal::pmu::sampling_stop_local(custody.token) {
            if let Ok(caps) = axhal::pmu::capabilities() {
                let preload = caps
                    .programmable_mask()
                    .saturating_add(1)
                    .saturating_sub(custody.event.config.period);
                let partial = sample.residual.wrapping_sub(preload) & caps.programmable_mask();
                let mut state = custody.event.state.lock();
                state.value = state.value.saturating_add(partial);
                if let Some(since) = state.running_since.take() {
                    state.running_total = state
                        .running_total
                        .saturating_add(axhal::time::monotonic_time_nanos().saturating_sub(since));
                }
                if sample.overflowed || sample.lost {
                    state.failed = true;
                }
            }
        } else {
            custody.event.state.lock().failed = true;
        }
    }

    /// Terminal kexec cleanup.  The caller will never resume ordinary task
    /// execution, so retain the Arc rather than allowing the final fixed-view
    /// drop to run in the terminal IPI context.
    pub(crate) fn quiesce_current_cpu() {
        let _irq = NoPreemptIrqSave::new();
        let cpu = axhal::percpu::this_cpu_id();
        if let Some(custody) = CUSTODY.get(cpu).and_then(|slot| slot.lock().take()) {
            let _ = axhal::pmu::sampling_stop_local(custody.token);
            core::mem::forget(custody.event);
        }
        let _ = axhal::pmu::sampling_quiesce_local();
    }

    pub(crate) fn handle_pmi(frame: &axcpu::TrapFrame) {
        let Ok(Some((sample, generation))) = axhal::pmu::sampling_take_pmi() else {
            return;
        };
        let cpu = axhal::percpu::this_cpu_id();
        let Some(event) = CUSTODY.get(cpu).and_then(|slot| {
            let custody = slot.lock();
            (custody.as_ref()?.cookie == sample.cookie)
                .then(|| custody.as_ref().unwrap().event.clone())
        }) else {
            return;
        };
        event.publish_sample(
            frame.rip,
            frame.cs,
            axhal::time::monotonic_time_nanos(),
            cpu as u32,
        );
        if event.enabled() {
            let _ = axhal::pmu::sampling_rearm_local(sample.cookie, generation);
        }
    }

    pub(crate) fn init_irq() -> bool {
        axhal::irq::register_context(axhal::pmu::SAMPLING_IRQ_VECTOR, perf_sampling_pmi)
    }

    /// The PMI path owns this lock while copying into the fixed view.  A tail
    /// outside the producer window is treated as full, never as an address.
    pub(crate) fn publish_sample(&self, ip: u64, cs: u64, time: u64, cpu: u32) {
        let mut state = self.state.lock();
        if !state.enabled || state.closed || state.failed {
            return;
        }
        if state.ring.is_none() {
            return;
        }
        let mut sample = [0_u8; 40];
        let size = encode_sample(
            &mut sample,
            self.config.sample_type,
            ip,
            cs,
            time,
            cpu,
            self.config.period,
        );
        state.value = state.value.saturating_add(self.config.period);
        let ring = state.ring.as_mut().expect("ring checked above");
        if publish_record(ring, &sample[..size], self.config.id) {
            self.waiters.wake();
        }
    }

    fn install_ring(&self, request: FileMmapRequest) -> AxResult {
        if !self.target_current() {
            return Err(AxError::OperationNotSupported);
        }
        if request.offset() != 0
            || request.sharing() != FileMmapSharing::Shared
            || request.protection().contains(FileMmapProtection::EXECUTE)
            || !request
                .protection()
                .contains(FileMmapProtection::READ | FileMmapProtection::WRITE)
        {
            return Err(AxError::InvalidInput);
        }
        let total = request.length();
        let Some(data_size) = total.checked_sub(PAGE) else {
            return Err(AxError::InvalidInput);
        };
        if request.page_size() != PAGE
            || data_size < MIN_DATA
            || data_size > MAX_DATA
            || !data_size.is_power_of_two()
            || !data_size.is_multiple_of(PAGE)
        {
            return Err(AxError::InvalidInput);
        }
        if let Some(ring) = self.state.lock().ring.as_ref() {
            return if ring.data_size == data_size {
                Ok(())
            } else {
                Err(AxError::ResourceBusy)
            };
        }
        let pages = Arc::try_new(SharedPages::new_fixed(
            total,
            axhal::paging::PageSize::Size4K,
        )?)
        .map_err(|_| AxError::NoMemory)?;
        let view = pages.fixed_view()?;
        let head = view.atomic_u64(DATA_HEAD)?;
        let tail = view.atomic_u64(DATA_TAIL)?;
        // These words are immutable geometry, but atomics avoid introducing a
        // second ordinary-byte writer into a mapping shared with userspace.
        view.atomic_u64(DATA_OFFSET)?.store_release(PAGE as u64);
        view.atomic_u64(DATA_SIZE)?.store_release(data_size as u64);
        let region = FixedSharedMmapRegion::try_new(
            0,
            pages,
            FileMmapProtection::READ | FileMmapProtection::WRITE,
        )?;
        let mut state = self.state.lock();
        if let Some(ring) = state.ring.as_ref() {
            return if ring.data_size == data_size {
                Ok(())
            } else {
                Err(AxError::ResourceBusy)
            };
        }
        state.ring = Some(Ring {
            region,
            view,
            head,
            tail,
            data_size,
            producer_head: 0,
            lost: 0,
        });
        Ok(())
    }

    fn read_count(&self, dst: &mut IoDst) -> AxResult<usize> {
        if !self.target_current() {
            return Err(AxError::OperationNotSupported);
        }
        let state = self.state.lock();
        if state.failed {
            return Err(AxError::Io);
        }
        let now = axhal::time::monotonic_time_nanos();
        let value = state.value;
        let enabled = state.enabled_total.saturating_add(if state.enabled {
            now.saturating_sub(state.enabled_since)
        } else {
            0
        });
        let running = state.running_total.saturating_add(
            state
                .running_since
                .map_or(0, |since| now.saturating_sub(since)),
        );
        drop(state);
        let words = 1
            + usize::from(self.config.read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0)
            + usize::from(self.config.read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0)
            + usize::from(self.config.read_format & PERF_FORMAT_ID != 0);
        if dst.remaining_mut() < words * 8 {
            return Err(AxError::InvalidInput);
        }
        dst.write(&value.to_ne_bytes())?;
        if self.config.read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0 {
            dst.write(&enabled.to_ne_bytes())?;
        }
        if self.config.read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0 {
            dst.write(&running.to_ne_bytes())?;
        }
        if self.config.read_format & PERF_FORMAT_ID != 0 {
            dst.write(&self.config.id.to_ne_bytes())?;
        }
        Ok(words * 8)
    }
}

fn perf_sampling_pmi(_: usize, frame: &axcpu::TrapFrame) {
    PerfSamplingFile::handle_pmi(frame);
}

impl FileLike for PerfSamplingFile {
    fn final_close(&self) {
        self.state.lock().closed = true;
        self.waiters.wake();
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.read_count(dst)
    }
    fn write(&self, _: &mut IoSrc) -> AxResult<usize> {
        Err(AxError::BadFileDescriptor)
    }
    fn prepare_mmap(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        self.install_ring(request)?;
        Ok(self
            .state
            .lock()
            .ring
            .as_ref()
            .map(|ring| ring.region.prepare(request))
            .transpose()?
            .flatten())
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        if !self.target_current() {
            return Err(AxError::OperationNotSupported);
        }
        if cmd == PERF_EVENT_IOC_ID {
            context
                .user_memory()
                .write_value(arg as *mut u64, self.config.id)
                .map_err(crate::mm::map_usercopy_error)?;
            return Ok(0);
        }
        if matches!(cmd, PERF_EVENT_IOC_DISABLE | PERF_EVENT_IOC_RESET) {
            Self::leave_current();
        }
        let now = axhal::time::monotonic_time_nanos();
        let mut state = self.state.lock();
        match cmd {
            PERF_EVENT_IOC_ENABLE if arg == 0 => {
                if !state.enabled {
                    state.enabled = true;
                    state.enabled_since = now;
                }
            }
            PERF_EVENT_IOC_DISABLE if arg == 0 => {
                if state.enabled {
                    state.enabled_total = state
                        .enabled_total
                        .saturating_add(now.saturating_sub(state.enabled_since));
                    state.enabled = false;
                }
            }
            PERF_EVENT_IOC_RESET if arg == 0 => {
                state.value = 0;
                state.failed = false;
                if let Some(ring) = state.ring.as_mut() {
                    ring.lost = 0;
                }
            }
            _ => return Err(AxError::InvalidInput),
        }
        Ok(0)
    }
    fn nonblocking(&self) -> bool {
        false
    }
    fn set_nonblocking(&self, _: bool) -> AxResult {
        Ok(())
    }
    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[perf_event]".into())
    }
}

impl Pollable for PerfSamplingFile {
    fn poll(&self) -> IoEvents {
        let state = self.state.lock();
        let mut events = IoEvents::empty();
        if state.closed {
            events |= IoEvents::HANGUP;
        }
        if state.failed {
            events |= IoEvents::ERROR;
        }
        if state
            .ring
            .as_ref()
            .is_some_and(|ring| ring.producer_head != ring.tail.load_acquire())
        {
            events |= IoEvents::READABLE;
        }
        events
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if events.intersects(IoEvents::READABLE | IoEvents::HANGUP | IoEvents::ERROR) {
            PollRegistration::single(&self.waiters, context.waker())
        } else {
            PollRegistration::empty()
        }
    }
}

fn header(out: &mut [u8], kind: u32, misc: u16, size: usize) {
    out[..4].copy_from_slice(&kind.to_ne_bytes());
    out[4..6].copy_from_slice(&misc.to_ne_bytes());
    out[6..8].copy_from_slice(&(size as u16).to_ne_bytes());
}
fn push_u64(out: &mut [u8], cursor: &mut usize, value: u64) {
    out[*cursor..*cursor + 8].copy_from_slice(&value.to_ne_bytes());
    *cursor += 8;
}
fn encode_sample(
    out: &mut [u8; 40],
    types: u64,
    ip: u64,
    cs: u64,
    time: u64,
    cpu: u32,
    period: u64,
) -> usize {
    let mut cursor = 8;
    if types & PERF_SAMPLE_IP != 0 {
        push_u64(out, &mut cursor, ip);
    }
    if types & PERF_SAMPLE_TIME != 0 {
        push_u64(out, &mut cursor, time);
    }
    if types & PERF_SAMPLE_CPU != 0 {
        out[cursor..cursor + 4].copy_from_slice(&cpu.to_ne_bytes());
        cursor += 8;
    }
    if types & PERF_SAMPLE_PERIOD != 0 {
        push_u64(out, &mut cursor, period);
    }
    header(
        out,
        PERF_RECORD_SAMPLE,
        if cs & 3 == 3 {
            PERF_RECORD_MISC_USER
        } else {
            0
        },
        cursor,
    );
    cursor
}

fn publish_record(ring: &mut Ring, sample: &[u8], id: u64) -> bool {
    let tail = ring.tail.load_acquire();
    let Some(used) = ring.producer_head.checked_sub(tail) else {
        ring.lost = ring.lost.saturating_add(1);
        return false;
    };
    if used > ring.data_size as u64 {
        ring.lost = ring.lost.saturating_add(1);
        return false;
    }
    let mut lost = [0_u8; 24];
    header(&mut lost, PERF_RECORD_LOST, 0, 24);
    lost[8..16].copy_from_slice(&id.to_ne_bytes());
    let pending = ring.lost;
    lost[16..24].copy_from_slice(&pending.to_ne_bytes());
    let required = sample.len() + if pending != 0 { lost.len() } else { 0 };
    if (ring.data_size as u64).saturating_sub(used) < required as u64 {
        ring.lost = ring.lost.saturating_add(1);
        return false;
    }
    // Prove the final publication point before copying either LOST or SAMPLE:
    // a wrapped head is never allowed to expose a partial old epoch.
    if ring.producer_head.checked_add(required as u64).is_none() {
        return false;
    }
    let mut head = ring.producer_head;
    if pending != 0 {
        if write_record(ring, head, &lost).is_err() {
            return false;
        }
        head += lost.len() as u64;
        ring.lost = 0;
    }
    if write_record(ring, head, sample).is_err() {
        return false;
    }
    let Some(next) = head.checked_add(sample.len() as u64) else {
        return false;
    };
    ring.producer_head = next;
    ring.head.store_release(next);
    true
}
fn write_record(ring: &Ring, head: u64, bytes: &[u8]) -> AxResult {
    let offset = (head as usize) & (ring.data_size - 1);
    // SAFETY: `PerfSamplingFile::state` serializes this sole producer; the
    // acquire tail / release head protocol above proves these bytes are not
    // consumer-owned, and `bytes.len() <= data_size` is fixed by record ABI.
    unsafe { ring.view.write_wrapped(PAGE, ring.data_size, offset, bytes) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sample_payload_order_and_misc() {
        let mut out = [0; 40];
        let n = encode_sample(
            &mut out,
            PERF_SAMPLE_IP | PERF_SAMPLE_TIME | PERF_SAMPLE_CPU | PERF_SAMPLE_PERIOD,
            1,
            3,
            2,
            4,
            5,
        );
        assert_eq!(n, 40);
        assert_eq!(
            u32::from_ne_bytes(out[..4].try_into().unwrap()),
            PERF_RECORD_SAMPLE
        );
        assert_eq!(
            u16::from_ne_bytes(out[4..6].try_into().unwrap()),
            PERF_RECORD_MISC_USER
        );
        assert_eq!(u64::from_ne_bytes(out[8..16].try_into().unwrap()), 1);
        assert_eq!(u64::from_ne_bytes(out[16..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_ne_bytes(out[24..28].try_into().unwrap()), 4);
        assert_eq!(u64::from_ne_bytes(out[32..40].try_into().unwrap()), 5);
    }
    #[test]
    fn sample_type_sizes_cover_each_field() {
        for bit in [
            PERF_SAMPLE_IP,
            PERF_SAMPLE_TIME,
            PERF_SAMPLE_CPU,
            PERF_SAMPLE_PERIOD,
        ] {
            let mut out = [0; 40];
            assert_eq!(encode_sample(&mut out, bit, 0, 0, 0, 0, 0), 16);
        }
    }
}
