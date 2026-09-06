use alloc::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::String,
    sync::{Arc, Weak},
    task::Wake,
    vec,
    vec::Vec,
};
use core::{
    any::Any,
    mem::size_of,
    sync::atomic::{AtomicU64, Ordering},
    task::Context,
    time::Duration,
};

#[allow(unused_imports)]
use axdriver::prelude::{
    AxInputDevice, BaseDriverOps, DevError, Event, EventType, InputDeviceId, InputDriverOps,
};
use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{
    DeviceId, FsName, FsNameBuf, Location, NodeFlags, NodePermission, NodeType, VfsResult,
};
use axio::prelude::*;
use axpoll::{IoEvents, PollRegistrationError, PollSet, Pollable, RegisterError};
use axsync::Mutex;
use axtask::future::{
    IrqWakerRegisterError, IrqWakerToken, cancel_irq_waker, register_irq_waker, update_irq_waker,
};
use bitmaps::Bitmap;
use lazy_static::lazy_static;
use linux_raw_sys::{
    general::{__kernel_old_time_t, __kernel_suseconds_t},
    ioctl::{
        EVIOCGID, EVIOCGMASK, EVIOCGRAB, EVIOCGREP, EVIOCGVERSION, EVIOCREVOKE, EVIOCSCLOCKID,
        EVIOCSMASK, EVIOCSREP,
    },
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::{
    file::{FileLike, IoDst, IoSrc, IoctlContext, Kstat, OfdIoStatus},
    mm::map_usercopy_error,
    pseudofs::{
        Device, DeviceOpen, DeviceOps, SimpleDirOps, SimpleFs,
        device_registry::{
            DeviceAttribute, DeviceHandle, DeviceIdentity, DeviceRegistration, DeviceReservation,
            MAX_DEVICES, global_device_registry,
        },
        try_boxed_names,
    },
    readiness::block_on_poll_io,
    time::wall_time,
};
const KEY_CNT: usize = EventType::Key.bits_count();
const EV_SYN: u16 = EventType::Synchronization as u16;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const EVDEV_CLIENT_QUEUE_EVENTS: usize = 256;
const EVDEV_DEVICE_FRAME_EVENTS: usize = 256;
const EVDEV_PUMP_EVENTS: usize = 256;
const EVENT_NODE_MODE: u16 = 0o660;

lazy_static! {
    static ref INPUT_MANAGER: Mutex<Option<Weak<InputManager>>> = Mutex::new(None);
}

/// Timestamp clock selected by an evdev open file description.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvdevClock {
    Realtime,
    Monotonic,
    Boottime,
}

/// A hardware event stamped once when it arrives at the evdev device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvdevRecord {
    event: Event,
    realtime_arrival: Duration,
    monotonic_arrival: Duration,
}

impl EvdevRecord {
    fn new(event: Event) -> Self {
        Self {
            event,
            realtime_arrival: wall_time(),
            monotonic_arrival: axhal::time::monotonic_time(),
        }
    }

    fn timestamp(self, clock: EvdevClock) -> Duration {
        match clock {
            EvdevClock::Realtime => self.realtime_arrival,
            // axhal's monotonic clock is boot-relative on the supported
            // x86_64 platform, so it is also the best available BOOTTIME
            // source until suspend accounting is introduced.
            EvdevClock::Monotonic | EvdevClock::Boottime => self.monotonic_arrival,
        }
    }
}

struct EvdevDeviceState {
    device: AxInputDevice,
    key_state: Bitmap<KEY_CNT>,
    led_state: Bitmap<16>,
    switch_state: Bitmap<18>,
    abs_values: BTreeMap<u8, i32>,
    msc_values: BTreeMap<u8, i32>,
    mt_slots: BTreeMap<u8, Vec<i32>>,
    mt_current_slot: usize,
    repeat: [u32; 2],
    frame: Vec<EvdevRecord>,
    frame_overflowed: bool,
    clients: BTreeMap<u64, Weak<EvdevClient>>,
    grab_owner: Option<u64>,
}

/// The one owner of an input controller.  Open file descriptions must be
/// created with [`EvdevDevice::open_client`]; they must never read the driver
/// directly, otherwise competing readers lose events.
pub struct EvdevDevice {
    state: Mutex<EvdevDeviceState>,
    next_client: AtomicU64,
    irq: Option<usize>,
    irq_pending: core::sync::atomic::AtomicBool,
    disconnected: core::sync::atomic::AtomicBool,
    paused: core::sync::atomic::AtomicBool,
    /// Incremented for every seat release.  A client captures this lease at
    /// open and is terminally revoked when its session loses the device;
    /// resume creates a new lease for future opens, never revives old FDs.
    session_lease: AtomicU64,
    waiters: Arc<PollSet>,
    irq_waker: core::task::Waker,
    irq_registration: spin::Mutex<Option<IrqWakerToken>>,
}

/// Per-open evdev state.  This is intentionally separate from the device so
/// masks, selected timestamp clock, poll readiness, overflow and grabs have
/// Linux open-file-description lifetime rather than device-node lifetime.
pub struct EvdevClient {
    id: u64,
    lease: u64,
    device: Weak<EvdevDevice>,
    revoked: core::sync::atomic::AtomicBool,
    state: Mutex<EvdevClientState>,
}

struct EvdevClientState {
    queue: VecDeque<EvdevRecord>,
    clock: EvdevClock,
    masks: BTreeMap<u16, Vec<u8>>,
    overflowed: bool,
}

impl EvdevDevice {
    pub fn new(device: AxInputDevice) -> Arc<Self> {
        let irq = device.irq_num();
        if let Some(irq) = irq {
            axhal::irq::set_enable(irq, true);
        }
        Arc::new_cyclic(|weak| Self {
            state: Mutex::new(EvdevDeviceState {
                device,
                key_state: Bitmap::new(),
                led_state: Bitmap::new(),
                switch_state: Bitmap::new(),
                abs_values: BTreeMap::new(),
                msc_values: BTreeMap::new(),
                mt_slots: BTreeMap::new(),
                mt_current_slot: 0,
                repeat: [250, 33],
                frame: Vec::new(),
                frame_overflowed: false,
                clients: BTreeMap::new(),
                grab_owner: None,
            }),
            next_client: AtomicU64::new(1),
            irq,
            irq_pending: core::sync::atomic::AtomicBool::new(false),
            disconnected: core::sync::atomic::AtomicBool::new(false),
            paused: core::sync::atomic::AtomicBool::new(false),
            session_lease: AtomicU64::new(1),
            waiters: Arc::new(PollSet::new()),
            irq_waker: core::task::Waker::from(Arc::new(InputIrqWake(weak.clone()))),
            irq_registration: spin::Mutex::new(None),
        })
    }

    /// Open-factory integration hook.  The VFS/OFD layer owns when this is
    /// called; this module deliberately has no global FD-table dependency.
    pub fn open_client(self: &Arc<Self>) -> Arc<EvdevClient> {
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        let lease = self.session_lease.load(Ordering::Acquire);
        let client = Arc::new(EvdevClient {
            id,
            lease,
            device: Arc::downgrade(self),
            // An open raced with PauseDevice.  It must not become a usable
            // descriptor after ResumeDevice.
            revoked: core::sync::atomic::AtomicBool::new(self.paused()),
            state: Mutex::new(EvdevClientState {
                queue: VecDeque::new(),
                clock: EvdevClock::Realtime,
                masks: BTreeMap::new(),
                overflowed: false,
            }),
        });
        self.state
            .lock()
            .clients
            .insert(id, Arc::downgrade(&client));
        client
    }

    fn live_client_count(&self) -> u64 {
        self.state
            .lock()
            .clients
            .values()
            .filter(|client| client.strong_count() != 0)
            .count() as u64
    }

    /// Physical removal is terminal for all existing file descriptions:
    /// subsequent reads report ENODEV and poll reports HUP|ERR.
    pub fn disconnect(&self) {
        if self.disconnected.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(token) = self.irq_registration.lock().take() {
            cancel_irq_waker(token);
        }
        if let Some(irq) = self.irq {
            axhal::irq::set_enable(irq, false);
        }
        let mut state = self.state.lock();
        state.grab_owner = None;
        state.clients.retain(|_, client| client.upgrade().is_some());
        drop(state);
        PollSet::wake(self.waiters.as_ref());
    }

    fn disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire)
    }

    fn paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Session pause is terminal for the outgoing session's descriptions.
    /// This differs from physical removal only in that the node can later
    /// grant a new lease to the newly active session.
    pub fn pause(&self) {
        if self.disconnected() {
            return;
        }
        if !self.paused.swap(true, Ordering::AcqRel) {
            self.session_lease.fetch_add(1, Ordering::AcqRel);
            if let Some(irq) = self.irq {
                axhal::irq::set_enable(irq, false);
            }
            let clients = {
                let mut state = self.state.lock();
                state.frame.clear();
                state.frame_overflowed = false;
                state
                    .clients
                    .values()
                    .filter_map(Weak::upgrade)
                    .collect::<Vec<_>>()
            };
            for client in clients {
                client.revoke();
            }
            PollSet::wake(self.waiters.as_ref());
        }
    }

    pub fn resume(&self) {
        if self.disconnected() || !self.paused.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some(irq) = self.irq {
            axhal::irq::set_enable(irq, true);
        }
        PollSet::wake(self.waiters.as_ref());
    }

    /// Drains pending hardware input and fans out only complete SYN_REPORT
    /// frames.  It is safe to call from read and poll paths.
    pub fn pump(&self) -> AxResult<()> {
        if self.disconnected() {
            return Err(LinuxError::ENODEV.into());
        }
        if self.paused() {
            return Ok(());
        }
        self.irq_pending.store(false, Ordering::Release);
        let mut state = self.state.lock();
        let mut drained = 0;
        for _ in 0..EVDEV_PUMP_EVENTS {
            let event = match state.device.read_event() {
                Ok(event) => event,
                Err(DevError::Again) => break,
                Err(error) => return Err(map_dev_error(error)),
            };
            drained += 1;
            if event.event_type == EventType::Key as u16 {
                match event.value {
                    0 => {
                        if (event.code as usize) < KEY_CNT {
                            state.key_state.set(event.code as usize, false);
                        }
                    }
                    1 => {
                        if (event.code as usize) < KEY_CNT {
                            state.key_state.set(event.code as usize, true);
                        }
                    }
                    _ => {}
                }
            }
            if event.event_type == EventType::Led as u16 && (event.code as usize) < 16 {
                state.led_state.set(event.code as usize, event.value != 0);
            }
            if event.event_type == EventType::Switch as u16 && (event.code as usize) < 18 {
                state
                    .switch_state
                    .set(event.code as usize, event.value != 0);
            }
            if event.event_type == EventType::Misc as u16 {
                state
                    .msc_values
                    .insert(event.code as u8, event.value as i32);
            }
            if event.event_type == EventType::Absolute as u16 {
                const ABS_MT_SLOT: u16 = 0x2f;
                if event.code == ABS_MT_SLOT {
                    state.mt_current_slot = event.value as usize;
                } else if event.code >= 0x2f {
                    let slot = state.mt_current_slot;
                    let slots = state.mt_slots.entry(event.code as u8).or_default();
                    if slot >= slots.len() {
                        slots.resize(slot + 1, 0);
                    }
                    slots[slot] = event.value as i32;
                }
            }
            if event.event_type == EventType::Absolute as u16 {
                state
                    .abs_values
                    .insert(event.code as u8, event.value as i32);
            }
            if !state.frame_overflowed {
                if state.frame.len() == EVDEV_DEVICE_FRAME_EVENTS {
                    state.frame.clear();
                    state.frame_overflowed = true;
                } else {
                    state.frame.push(EvdevRecord::new(event));
                }
            }
            if event.event_type == EV_SYN && event.code == SYN_REPORT {
                let owner = state.grab_owner;
                let frame_overflowed = core::mem::replace(&mut state.frame_overflowed, false);
                let frame = core::mem::replace(&mut state.frame, Vec::new());
                state.clients.retain(|id, client| {
                    let Some(client) = client.upgrade() else {
                        return false;
                    };
                    if owner.is_none_or(|owner| owner == *id) {
                        if frame_overflowed {
                            client.mark_overflow();
                        } else {
                            client.enqueue_frame(&frame);
                        }
                    }
                    true
                });
            }
        }
        // A bounded drain leaves any excess work for the next task-context
        // poll/read pass rather than monopolising an IRQ wakeup.
        if drained == EVDEV_PUMP_EVENTS {
            self.irq_pending.store(true, Ordering::Release);
            PollSet::wake(self.waiters.as_ref());
        }
        Ok(())
    }

    pub fn event_bits(&self, ty: EventType, out: &mut [u8]) -> AxResult<bool> {
        self.state
            .lock()
            .device
            .get_event_bits(ty, out)
            .map_err(|_| AxError::InvalidInput)
    }

    pub fn property_bits(&self, out: &mut [u8]) -> AxResult<bool> {
        self.state
            .lock()
            .device
            .get_property_bits(out)
            .map_err(|_| AxError::InvalidInput)
    }

    /// Returns Linux's six `input_absinfo` i32 fields, including current value.
    pub fn abs_info(&self, axis: u8) -> AxResult<[i32; 6]> {
        self.pump()?;
        let mut state = self.state.lock();
        let info = state
            .device
            .get_abs_info(axis)
            .map_err(|_| AxError::InvalidInput)?
            .ok_or(AxError::InvalidInput)?;
        Ok([
            state.abs_values.get(&axis).copied().unwrap_or(0),
            info.min as i32,
            info.max as i32,
            info.fuzz as i32,
            info.flat as i32,
            info.res as i32,
        ])
    }

    /// Snapshots global key state after ingesting driver-pending events.
    /// This deliberately leaves every evdev client's OFD queue intact.
    pub fn key_state(&self, out: &mut [u8]) -> AxResult<usize> {
        self.pump()?;
        Ok(copy_bytes(self.state.lock().key_state.as_bytes(), out))
    }

    fn state_bitmap(&self, event_type: EventType, out: &mut [u8]) -> AxResult<usize> {
        self.pump()?;
        // Linux exposes the zero-initialized state bitmap even when this
        // device does not advertise the event class (e.g. LEDs on a mouse).
        let state = self.state.lock();
        let bytes = match event_type {
            EventType::Led => state.led_state.as_bytes(),
            EventType::Switch => state.switch_state.as_bytes(),
            _ => return Err(AxError::InvalidInput),
        };
        Ok(copy_bytes(bytes, out))
    }

    fn repeat(&self) -> [u32; 2] {
        self.state.lock().repeat
    }

    fn set_repeat(&self, repeat: [u32; 2]) -> AxResult<()> {
        if !self.event_bits(EventType::Repeat, &mut []).unwrap_or(false) {
            return Err(LinuxError::EINVAL.into());
        }
        self.state.lock().repeat = repeat;
        Ok(())
    }

    fn mt_slots(&self, axis: u8, out: &mut [i32]) -> AxResult<()> {
        self.pump()?;
        let state = self.state.lock();
        let values = state.mt_slots.get(&axis).ok_or(AxError::InvalidInput)?;
        for (out, value) in out.iter_mut().zip(values) {
            *out = *value;
        }
        Ok(())
    }

    fn set_grab(&self, client: u64, grab: bool) -> AxResult<()> {
        let mut state = self.state.lock();
        if grab {
            match state.grab_owner {
                Some(owner) if owner != client => Err(LinuxError::EBUSY.into()),
                _ => {
                    state.grab_owner = Some(client);
                    Ok(())
                }
            }
        } else {
            if state.grab_owner == Some(client) {
                state.grab_owner = None;
            }
            Ok(())
        }
    }

    fn close_client(&self, client: u64) {
        let mut state = self.state.lock();
        state.clients.remove(&client);
        if state.grab_owner == Some(client) {
            state.grab_owner = None;
        }
    }

    fn ensure_irq_bridge(&self) -> Result<(), PollRegistrationError> {
        let Some(irq) = self.irq else {
            return Ok(());
        };
        let mut registration = self.irq_registration.lock();
        if let Some(token) = *registration {
            if update_irq_waker(token, &self.irq_waker).is_ok() {
                return Ok(());
            }
            *registration = None;
        }
        let token = register_irq_waker(irq, &self.irq_waker).map_err(|error| match error {
            IrqWakerRegisterError::Waiter(error) => {
                PollRegistrationError::Source { index: 0, error }
            }
            IrqWakerRegisterError::SourceCapacityExhausted
            | IrqWakerRegisterError::HookInstallationInProgress => PollRegistrationError::Source {
                index: 0,
                error: RegisterError::Full,
            },
            IrqWakerRegisterError::HookUnavailable => PollRegistrationError::Source {
                index: 0,
                error: RegisterError::Closed,
            },
        })?;
        *registration = Some(token);
        Ok(())
    }
}

struct InputIrqWake(Weak<EvdevDevice>);

impl Wake for InputIrqWake {
    fn wake(self: Arc<Self>) {
        if let Some(device) = self.0.upgrade() {
            device.irq_pending.store(true, Ordering::Release);
            PollSet::wake(device.waiters.as_ref());
        }
    }
    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(device) = self.0.upgrade() {
            device.irq_pending.store(true, Ordering::Release);
            PollSet::wake(device.waiters.as_ref());
        }
    }
}

impl Drop for EvdevDevice {
    fn drop(&mut self) {
        if let Some(token) = self.irq_registration.get_mut().take() {
            cancel_irq_waker(token);
        }
        if let Some(irq) = self.irq {
            axhal::irq::set_enable(irq, false);
        }
    }
}

impl EvdevClient {
    fn valid_lease(&self) -> bool {
        self.device.upgrade().is_some_and(|device| {
            !device.paused()
                && self.lease == device.session_lease.load(Ordering::Acquire)
                && !device.disconnected()
        })
    }

    pub fn set_clock(&self, clock: EvdevClock) {
        self.state.lock().clock = clock;
    }

    fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        self.state.lock().queue.clear();
        if let Some(device) = self.device.upgrade() {
            PollSet::wake(device.waiters.as_ref());
        }
    }

    /// Applies Linux clock IDs accepted by EVIOCSCLOCKID.
    pub fn set_clock_id(&self, clock_id: i32) -> AxResult<()> {
        let clock = match clock_id {
            0 => EvdevClock::Realtime,
            1 => EvdevClock::Monotonic,
            7 => EvdevClock::Boottime,
            _ => return Err(LinuxError::EINVAL.into()),
        };
        self.set_clock(clock);
        Ok(())
    }

    pub fn clock(&self) -> EvdevClock {
        self.state.lock().clock
    }

    /// Replaces one EVIOCSMASK event-type bitmap.  An absent bitmap means all
    /// codes for that type are enabled, matching Linux's default.
    pub fn set_mask(&self, event_type: u16, bitmap: Vec<u8>) {
        self.state.lock().masks.insert(event_type, bitmap);
    }

    pub fn clear_mask(&self, event_type: u16) {
        self.state.lock().masks.remove(&event_type);
    }

    /// Implements the default all-enabled evdev client mask without claiming
    /// that unsupported event types have meaningful codes. Unknown types are
    /// consequently returned as all zeroes, as Linux documents for
    /// EVIOCGMASK.
    fn mask_bits(&self, event_type: u16, out: &mut [u8]) {
        out.fill(0);
        let state = self.state.lock();
        if let Some(mask) = state.masks.get(&event_type) {
            copy_bytes(mask, out);
            return;
        }
        let Some(event_type) = EventType::from_repr(event_type as u8) else {
            return;
        };
        let bytes = event_type.bits_count().div_ceil(8);
        for byte in out.iter_mut().take(bytes) {
            *byte = u8::MAX;
        }
        if let Some(last) = out.get_mut(bytes.saturating_sub(1)) {
            let remainder = event_type.bits_count() % 8;
            if remainder != 0 {
                *last = (1 << remainder) - 1;
            }
        }
    }

    pub fn grab(&self, grab: bool) -> AxResult<()> {
        if self.revoked.load(Ordering::Acquire) || !self.valid_lease() {
            return Err(LinuxError::ENODEV.into());
        }
        self.device
            .upgrade()
            .ok_or(AxError::InvalidInput)?
            .set_grab(self.id, grab)
    }

    pub fn poll_ready(&self) -> bool {
        if self.revoked.load(Ordering::Acquire) || !self.valid_lease() {
            return false;
        }
        if self
            .device
            .upgrade()
            .is_some_and(|device| device.disconnected())
        {
            return false;
        }
        if self.device.upgrade().is_some_and(|device| device.paused()) {
            return false;
        }
        if let Some(device) = self.device.upgrade() {
            let _ = device.pump();
        }
        !self.state.lock().queue.is_empty()
    }

    pub fn pop(&self) -> Option<EvdevRecord> {
        if let Some(device) = self.device.upgrade() {
            let _ = device.pump();
        }
        self.state.lock().queue.pop_front()
    }

    fn accepts(masks: &BTreeMap<u16, Vec<u8>>, event: &Event) -> bool {
        let Some(bitmap) = masks.get(&event.event_type) else {
            return true;
        };
        let code = event.code as usize;
        bitmap
            .get(code / 8)
            .is_some_and(|byte| byte & (1 << (code % 8)) != 0)
    }

    fn enqueue_frame(&self, frame: &[EvdevRecord]) {
        let mut state = self.state.lock();
        let accepted = frame
            .iter()
            .filter(|record| {
                record.event.event_type == EV_SYN || Self::accepts(&state.masks, &record.event)
            })
            .count();
        if accepted == 0 {
            return;
        }
        let needed = accepted + usize::from(state.overflowed);
        if needed > EVDEV_CLIENT_QUEUE_EVENTS
            || state.queue.len() + needed > EVDEV_CLIENT_QUEUE_EVENTS
        {
            state.queue.clear();
            state.overflowed = true;
            return;
        }
        if state.overflowed {
            let dropped = EvdevRecord {
                event: Event {
                    event_type: EV_SYN,
                    code: SYN_DROPPED,
                    value: 0,
                },
                ..frame[0]
            };
            state.queue.push_back(dropped);
            state.overflowed = false;
        }
        for record in frame.iter().copied() {
            let accepted =
                record.event.event_type == EV_SYN || Self::accepts(&state.masks, &record.event);
            if accepted {
                state.queue.push_back(record);
            }
        }
        if let Some(device) = self.device.upgrade() {
            PollSet::wake(device.waiters.as_ref());
        }
    }

    fn mark_overflow(&self) {
        let mut state = self.state.lock();
        state.queue.clear();
        state.overflowed = true;
    }
}

impl Pollable for EvdevClient {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::READABLE, self.poll_ready());
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, PollRegistrationError> {
        if !events.contains(IoEvents::READABLE) {
            return axpoll::PollRegistration::empty();
        }
        self.poll_ready();
        let device = self
            .device
            .upgrade()
            .ok_or(PollRegistrationError::InvalidState)?;
        let registration =
            axpoll::PollRegistration::single_owned(device.waiters.clone(), context.waker())?;
        if !device.paused() {
            device.ensure_irq_bridge()?;
        }
        Ok(registration)
    }
}

impl Drop for EvdevClient {
    fn drop(&mut self) {
        if let Some(device) = self.device.upgrade() {
            device.close_client(self.id);
        }
    }
}

fn copy_bytes(src: &[u8], dst: &mut [u8]) -> usize {
    let len = src.len().min(dst.len());
    dst[..len].copy_from_slice(&src[..len]);
    len
}

fn write_zeroes(context: &IoctlContext, address: usize, len: usize) -> AxResult<()> {
    let zeroes = [0u8; 256];
    let mut written = 0;
    while written < len {
        let chunk = (len - written).min(zeroes.len());
        let target = address.checked_add(written).ok_or(AxError::BadAddress)?;
        context
            .user_memory()
            .write_bytes(target, &zeroes[..chunk])
            .map_err(map_usercopy_error)?;
        written += chunk;
    }
    Ok(())
}

fn map_dev_error(error: DevError) -> AxError {
    match error {
        DevError::AlreadyExists => AxError::AlreadyExists,
        DevError::Again => AxError::WouldBlock,
        DevError::BadState => AxError::BadState,
        DevError::InvalidParam => AxError::InvalidInput,
        DevError::Io => AxError::Io,
        DevError::NoMemory => AxError::NoMemory,
        DevError::ResourceBusy => AxError::ResourceBusy,
        DevError::Unsupported => AxError::OperationNotSupported,
    }
}

fn zero_bits_len(size: usize, bits: usize) -> usize {
    bits.div_ceil(8).min(size)
}

fn linux_bitmap_len(size: usize, max_bit: usize) -> usize {
    max_bit
        .div_ceil(usize::BITS as usize)
        .saturating_mul(size_of::<usize>())
        .min(size)
}

fn set_bit_if_fits(bits: &mut [u8], bit: usize) {
    if let Some(byte) = bits.get_mut(bit / 8) {
        *byte |= 1 << (bit % 8);
    }
}

fn event_mask_len(event_type: u16) -> usize {
    EventType::from_repr(event_type as u8)
        .map(|event_type| event_type.bits_count().div_ceil(8))
        .unwrap_or(0)
}

fn input_mask_event_type(event_type: u32) -> Option<u16> {
    (event_type <= EventType::MAX as u32).then_some(event_type as u16)
}

fn return_str(context: &IoctlContext, arg: usize, size: usize, s: &str) -> AxResult<usize> {
    let mut bytes = vec![0; size];
    let copied = copy_bytes(s.as_bytes(), &mut bytes);
    context
        .user_memory()
        .write_bytes(arg, &bytes)
        .map_err(map_usercopy_error)?;
    Ok(copied)
}
fn return_zero_bits(
    context: &IoctlContext,
    arg: usize,
    size: usize,
    bits: usize,
) -> AxResult<usize> {
    let len = zero_bits_len(size, bits);
    let bytes = vec![0; len];
    context
        .user_memory()
        .write_bytes(arg, &bytes)
        .map_err(map_usercopy_error)?;
    Ok(len)
}

#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable)]
pub struct KernelTimeval {
    pub tv_sec: __kernel_old_time_t,
    pub tv_usec: __kernel_suseconds_t,
}

#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable)]
struct InputEvent {
    time: KernelTimeval,
    event_type: u16,
    code: u16,
    value: i32,
}

/// Linux's fixed-size `struct input_mask`, used by EVIOCGMASK and
/// EVIOCSMASK on the supported x86_64 ABI.
#[repr(C)]
#[derive(FromBytes, Immutable)]
struct InputMask {
    event_type: u32,
    codes_size: u32,
    codes_ptr: u64,
}

impl EvdevClient {
    /// Copies integral `input_event` records only; a short trailing buffer is
    /// deliberately untouched so every successful read preserves record and
    /// frame boundaries.
    pub fn read_at(&self, buf: &mut [u8]) -> VfsResult<usize> {
        if self.revoked.load(Ordering::Acquire) || !self.valid_lease() {
            return Err(LinuxError::ENODEV.into());
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if buf.len() < size_of::<InputEvent>() {
            return Err(AxError::InvalidInput);
        }
        if let Some(device) = self.device.upgrade() {
            if device.disconnected() {
                return Err(LinuxError::ENODEV.into());
            }
            if device.paused() {
                return Err(AxError::WouldBlock);
            }
            device.pump()?;
        }
        let (chunks, _) = buf.as_chunks_mut::<{ size_of::<InputEvent>() }>();
        let mut state = self.state.lock();
        let mut read = 0;
        for out in chunks {
            let Some(record) = state.queue.pop_front() else {
                break;
            };
            let time = record.timestamp(state.clock);
            let event = InputEvent {
                time: KernelTimeval {
                    tv_sec: time.as_secs() as _,
                    tv_usec: time.subsec_micros() as _,
                },
                event_type: record.event.event_type,
                code: record.event.code,
                value: record.event.value as i32,
            };
            out.copy_from_slice(event.as_bytes());
            read += out.len();
        }
        if read == 0 {
            Err(AxError::WouldBlock)
        } else {
            Ok(read)
        }
    }
}

// The node object is immutable and shared; every ordinary open is replaced
// with an EvdevFile that owns exactly one EvdevClient (one Linux OFD).
pub struct EvdevNode {
    device: Arc<EvdevDevice>,
}

impl EvdevNode {
    pub fn new(device: AxInputDevice) -> Self {
        Self {
            device: EvdevDevice::new(device),
        }
    }

    fn from_evdev(device: Arc<EvdevDevice>) -> Self {
        Self { device }
    }
}

struct EvdevFile {
    client: Arc<EvdevClient>,
    device: Arc<EvdevDevice>,
    location: Location,
    nonblocking: core::sync::atomic::AtomicBool,
}

impl EvdevFile {
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        let nr = (cmd & 0xff) as u8;
        let ty = ((cmd >> 8) & 0xff) as u8;
        let size = ((cmd >> 16) & 0x3fff) as usize;
        let dir = (cmd >> 30) & 3;
        match cmd {
            EVIOCGVERSION => {
                context
                    .user_memory()
                    .write_bytes(arg, &0x10001u32.to_ne_bytes())
                    .map_err(map_usercopy_error)?;
                Ok(0)
            }
            EVIOCGID => {
                let id = self.device.state.lock().device.device_id();
                let mut bytes = [0u8; size_of::<InputDeviceId>()];
                bytes[0..2].copy_from_slice(&id.bus_type.to_ne_bytes());
                bytes[2..4].copy_from_slice(&id.vendor.to_ne_bytes());
                bytes[4..6].copy_from_slice(&id.product.to_ne_bytes());
                bytes[6..8].copy_from_slice(&id.version.to_ne_bytes());
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(map_usercopy_error)?;
                Ok(0)
            }
            EVIOCGRAB => {
                let mut bytes = [core::mem::MaybeUninit::uninit(); size_of::<i32>()];
                context
                    .user_memory()
                    .read_bytes(arg, &mut bytes)
                    .map_err(map_usercopy_error)?;
                let bytes = bytes.map(|byte| unsafe { byte.assume_init() });
                self.client.grab(i32::from_ne_bytes(bytes) != 0)?;
                Ok(0)
            }
            EVIOCREVOKE => {
                if arg != 0 {
                    return Err(LinuxError::EINVAL.into());
                }
                self.client.revoke();
                Ok(0)
            }
            EVIOCSCLOCKID => {
                let mut bytes = [core::mem::MaybeUninit::uninit(); size_of::<i32>()];
                context
                    .user_memory()
                    .read_bytes(arg, &mut bytes)
                    .map_err(map_usercopy_error)?;
                let bytes = bytes.map(|byte| unsafe { byte.assume_init() });
                self.client.set_clock_id(i32::from_ne_bytes(bytes))?;
                Ok(0)
            }
            EVIOCGREP => self.repeat(context, arg),
            EVIOCSREP => self.set_repeat(context, arg),
            EVIOCSMASK => self.set_event_mask(context, arg),
            EVIOCGMASK => self.get_event_mask(context, arg),
            _ if ty != b'E' => Err(AxError::NotATty),
            _ if dir != 2 => Err(AxError::NotATty),
            _ => match nr {
                0x06 => self.device_string(context, arg, size, |device| {
                    String::from(device.device_name())
                }),
                0x07 => self.device_string(context, arg, size, |device| {
                    String::from(device.physical_location())
                }),
                0x08 => self.device_string(context, arg, size, |device| {
                    String::from(device.unique_id())
                }),
                0x09 => self.device_bits(
                    context,
                    arg,
                    size,
                    EventType::MAX as usize,
                    |device, out| device.property_bits(out),
                ),
                0x18 => {
                    let mut bytes = vec![0; size];
                    self.device.key_state(&mut bytes)?;
                    context
                        .user_memory()
                        .write_bytes(arg, &bytes)
                        .map_err(map_usercopy_error)?;
                    Ok(0)
                }
                0x19 => self.state_bitmap(context, arg, size, EventType::Led),
                0x1b => self.state_bitmap(context, arg, size, EventType::Switch),
                0x0a => self.mt_slots(context, arg, size),
                _ if nr & !EventType::MAX == EventType::COUNT => {
                    self.event_bits(context, arg, size, nr & EventType::MAX)
                }
                _ if nr & !0x3f == 0x40 => {
                    let fields = self.device.abs_info(nr & 0x3f)?;
                    let mut bytes = [0u8; 24];
                    for (index, value) in fields.into_iter().enumerate() {
                        bytes[index * 4..][..4].copy_from_slice(&value.to_ne_bytes());
                    }
                    context
                        .user_memory()
                        .write_bytes(arg, &bytes[..bytes.len().min(size)])
                        .map_err(map_usercopy_error)?;
                    Ok(0)
                }
                _ => Err(AxError::NotATty),
            },
        }
    }

    fn device_string(
        &self,
        context: &IoctlContext,
        arg: usize,
        size: usize,
        get: impl FnOnce(&mut AxInputDevice) -> String,
    ) -> AxResult<usize> {
        let string = get(&mut self.device.state.lock().device);
        return_str(context, arg, size, &string)?;
        Ok(0)
    }

    fn repeat(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        let repeat = self.device.repeat();
        let bytes = [repeat[0].to_ne_bytes(), repeat[1].to_ne_bytes()].concat();
        context
            .user_memory()
            .write_bytes(arg, &bytes)
            .map_err(map_usercopy_error)?;
        Ok(0)
    }

    fn set_repeat(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        let mut bytes = [core::mem::MaybeUninit::uninit(); size_of::<[u32; 2]>()];
        context
            .user_memory()
            .read_bytes(arg, &mut bytes)
            .map_err(map_usercopy_error)?;
        let bytes = bytes.map(|byte| unsafe { byte.assume_init() });
        self.device.set_repeat([
            u32::from_ne_bytes(bytes[..4].try_into().expect("repeat delay")),
            u32::from_ne_bytes(bytes[4..].try_into().expect("repeat period")),
        ])?;
        Ok(0)
    }

    fn state_bitmap(
        &self,
        context: &IoctlContext,
        arg: usize,
        size: usize,
        event_type: EventType,
    ) -> AxResult<usize> {
        let mut bytes = vec![0; size];
        self.device.state_bitmap(event_type, &mut bytes)?;
        context
            .user_memory()
            .write_bytes(arg, &bytes)
            .map_err(map_usercopy_error)?;
        Ok(0)
    }

    fn mt_slots(&self, context: &IoctlContext, arg: usize, size: usize) -> AxResult<usize> {
        if size < size_of::<i32>() || (size - size_of::<i32>()) % size_of::<i32>() != 0 {
            return Err(LinuxError::EINVAL.into());
        }
        let mut axis = [core::mem::MaybeUninit::uninit(); size_of::<i32>()];
        context
            .user_memory()
            .read_bytes(arg, &mut axis)
            .map_err(map_usercopy_error)?;
        let axis = i32::from_ne_bytes(axis.map(|byte| unsafe { byte.assume_init() }));
        if !(0..=u8::MAX as i32).contains(&axis) {
            return Err(LinuxError::EINVAL.into());
        }
        let slot_count = (size - size_of::<i32>()) / size_of::<i32>();
        let mut values = vec![0i32; slot_count];
        self.device.mt_slots(axis as u8, &mut values)?;
        let mut bytes = vec![0; size];
        bytes[..4].copy_from_slice(&axis.to_ne_bytes());
        for (index, value) in values.into_iter().enumerate() {
            bytes[4 + index * 4..][..4].copy_from_slice(&value.to_ne_bytes());
        }
        context
            .user_memory()
            .write_bytes(arg, &bytes)
            .map_err(map_usercopy_error)?;
        Ok(0)
    }

    fn input_mask(&self, context: &IoctlContext, arg: usize) -> AxResult<InputMask> {
        let mut bytes = [core::mem::MaybeUninit::uninit(); size_of::<InputMask>()];
        context
            .user_memory()
            .read_bytes(arg, &mut bytes)
            .map_err(map_usercopy_error)?;
        let bytes = bytes.map(|byte| unsafe { byte.assume_init() });
        InputMask::read_from_bytes(&bytes).map_err(|_| AxError::InvalidInput)
    }

    fn set_event_mask(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        let mask = self.input_mask(context, arg)?;
        // Linux ignores mask bits beyond the known code range. More
        // importantly, never make an untrusted ioctl length turn into an
        // unbounded allocation in the kernel.
        let Some(event_type) = input_mask_event_type(mask.event_type) else {
            return Ok(0);
        };
        let codes_size = (mask.codes_size as usize).min(event_mask_len(event_type));
        let mut uninit = vec![core::mem::MaybeUninit::uninit(); codes_size];
        context
            .user_memory()
            .read_bytes(mask.codes_ptr as usize, &mut uninit)
            .map_err(map_usercopy_error)?;
        // `read_bytes` succeeded for the complete slice, so every element is
        // initialized before it crosses into the per-OFD queue state.
        let bitmap = uninit
            .into_iter()
            .map(|byte| unsafe { byte.assume_init() })
            .collect();
        self.client.set_mask(event_type, bitmap);
        Ok(0)
    }

    fn get_event_mask(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        let mask = self.input_mask(context, arg)?;
        let Some(event_type) = input_mask_event_type(mask.event_type) else {
            write_zeroes(context, mask.codes_ptr as usize, mask.codes_size as usize)?;
            return Ok(0);
        };
        let mut bitmap = vec![0; (mask.codes_size as usize).min(event_mask_len(event_type))];
        self.client.mask_bits(event_type, &mut bitmap);
        context
            .user_memory()
            .write_bytes(mask.codes_ptr as usize, &bitmap)
            .map_err(map_usercopy_error)?;
        Ok(0)
    }

    fn device_bits(
        &self,
        context: &IoctlContext,
        arg: usize,
        size: usize,
        max_bit: usize,
        get: impl FnOnce(&EvdevDevice, &mut [u8]) -> AxResult<bool>,
    ) -> AxResult<usize> {
        let len = linux_bitmap_len(size, max_bit);
        let mut bits = vec![0; len];
        get(&self.device, &mut bits)?;
        context
            .user_memory()
            .write_bytes(arg, &bits)
            .map_err(map_usercopy_error)?;
        Ok(len)
    }

    fn event_bits(
        &self,
        context: &IoctlContext,
        arg: usize,
        size: usize,
        ty: u8,
    ) -> AxResult<usize> {
        if ty == 0 {
            let len = linux_bitmap_len(size, EventType::MAX as usize);
            let mut bits = vec![0; len];
            for value in 0..EventType::COUNT {
                let Some(event_type) = EventType::from_repr(value) else {
                    continue;
                };
                if self.device.event_bits(event_type, &mut []).unwrap_or(false) {
                    set_bit_if_fits(&mut bits, value as usize);
                }
            }
            context
                .user_memory()
                .write_bytes(arg, &bits)
                .map_err(map_usercopy_error)?;
            return Ok(len);
        }
        // These are the EVIOCGBIT classes accepted by Linux's
        // handle_eviocgbit(), independently of the device's evbit support.
        let event_type = match EventType::from_repr(ty) {
            Some(event_type @ (EventType::Key
                | EventType::Relative
                | EventType::Absolute
                | EventType::Misc
                | EventType::Switch
                | EventType::Led
                | EventType::Sound
                | EventType::ForceFeedback)) => event_type,
            _ => return Err(LinuxError::EINVAL.into()),
        };
        let max_bit = event_type.bits_count().saturating_sub(1);
        let len = linux_bitmap_len(size, max_bit);
        let mut bits = vec![0; len];
        // Unsupported classes are valid empty capability bitmaps, not an
        // invalid ioctl. libevdev queries every class during initialization.
        self.device.event_bits(event_type, &mut bits)?;
        context
            .user_memory()
            .write_bytes(arg, &bits)
            .map_err(map_usercopy_error)?;
        Ok(len)
    }
}

impl DeviceOps for EvdevNode {
    fn open_description(&self, location: &Location, _flags: u32) -> VfsResult<Option<DeviceOpen>> {
        crate::pseudofs::dev::tty::remember_input_node(location)?;
        let client = self.device.open_client();
        let file: Arc<dyn FileLike> = Arc::try_new(EvdevFile {
            client,
            device: self.device.clone(),
            location: location.clone(),
            nonblocking: core::sync::atomic::AtomicBool::new(false),
        })
        .map_err(|_| AxError::NoMemory)?;
        Ok(Some(DeviceOpen::new(file, None)))
    }

    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput)
    }
    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
            | NodeFlags::STREAM
            | NodeFlags::NO_POSITIONED_READ
            | NodeFlags::NO_POSITIONED_WRITE
            | NodeFlags::NO_SEEK
    }
}

impl FileLike for EvdevFile {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        if self.device.disconnected() {
            return Err(LinuxError::ENODEV.into());
        }
        let mut bytes = [0u8; EVDEV_CLIENT_QUEUE_EVENTS * size_of::<InputEvent>()];
        let capacity = dst.remaining_mut().min(bytes.len());
        let bytes = &mut bytes[..capacity];
        let read = block_on_poll_io(self, IoEvents::READABLE, self.nonblocking(), || match self
            .client
            .read_at(bytes)
        {
            Err(AxError::WouldBlock) if self.device.disconnected() => {
                Err(LinuxError::ENODEV.into())
            }
            result => result,
        })?;
        dst.write_all(&bytes[..read])?;
        Ok(read)
    }
    fn read_with_operation_status(&self, status: OfdIoStatus, dst: &mut IoDst) -> AxResult<usize> {
        if self.device.disconnected() {
            return Err(LinuxError::ENODEV.into());
        }
        let mut bytes = [0u8; EVDEV_CLIENT_QUEUE_EVENTS * size_of::<InputEvent>()];
        let capacity = dst.remaining_mut().min(bytes.len());
        let read = block_on_poll_io(
            self,
            IoEvents::READABLE,
            self.nonblocking() || status.rwf_nowait(),
            || match self.client.read_at(&mut bytes[..capacity]) {
                Err(AxError::WouldBlock) if self.device.disconnected() => {
                    Err(LinuxError::ENODEV.into())
                }
                result => result,
            },
        )?;
        dst.write_all(&bytes[..read])?;
        Ok(read)
    }
    fn write(&self, _src: &mut IoSrc) -> AxResult<usize> {
        Err(AxError::InvalidInput)
    }
    fn stat(&self) -> AxResult<Kstat> {
        let metadata = self.location.metadata()?;
        Ok(Kstat {
            dev: crate::mounts::linux_device_id(metadata.device).0,
            mnt_id: self.location.mountpoint().mount_id(),
            ino: metadata.inode,
            nlink: metadata.nlink as _,
            mode: ((metadata.node_type as u8 as u32) << 12) | metadata.mode.bits() as u32,
            uid: metadata.uid,
            gid: metadata.gid,
            size: metadata.size,
            blksize: metadata.block_size as _,
            blocks: metadata.blocks,
            rdev: metadata.rdev,
            atime: metadata.atime,
            btime: metadata.btime,
            mtime: metadata.mtime,
            ctime: metadata.ctime,
            ..Kstat::default()
        })
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Owned(self.location.absolute_path()?))
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        self.ioctl(context, cmd, arg)
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
    fn set_nonblocking(&self, value: bool) -> AxResult {
        self.nonblocking.store(value, Ordering::Release);
        Ok(())
    }
}

impl Pollable for EvdevFile {
    fn poll(&self) -> IoEvents {
        let mut events = self.client.poll();
        if self.device.disconnected()
            || self.client.revoked.load(Ordering::Acquire)
            || !self.client.valid_lease()
        {
            events |= IoEvents::HANGUP | IoEvents::ERROR;
        }
        events
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, PollRegistrationError> {
        self.client.register(context, events)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InputDeviceKey {
    token: axinput::InputDeviceToken,
    device_id: DeviceId,
    generation: u64,
}

struct InputSlot {
    key: InputDeviceKey,
    minor: u32,
    event: Arc<EvdevDevice>,
    /// One devfs inode per published device generation. Reusing this object
    /// preserves node mode, ACLs and xattrs across independent path walks.
    node: Arc<Device>,
    /// Physical PCI and VirtIO parent kobjects, present for PCI transport
    /// devices and absent only for the bootstrap virtual fallback.
    transport_handles: Option<[DeviceHandle<'static, MAX_DEVICES>; 2]>,
    parent_handle: DeviceHandle<'static, MAX_DEVICES>,
    event_handle: DeviceHandle<'static, MAX_DEVICES>,
}

struct InputManagerState {
    devices: BTreeMap<axinput::InputDeviceToken, InputSlot>,
    free_minors: BTreeSet<u32>,
    next_minor: u32,
}

/// Dynamic `/dev/input` directory. Stable driver tokens plus a non-wrapping
/// generation identify physical devices; recyclable `eventN` minors do not.
pub struct InputManager {
    fs: Arc<SimpleFs>,
    /// Serializes complete add/remove transactions across devfs and sysfs.
    lifecycle: Mutex<()>,
    state: Mutex<InputManagerState>,
}

impl InputManager {
    fn new(fs: Arc<SimpleFs>) -> Arc<Self> {
        Arc::new(Self {
            fs,
            lifecycle: Mutex::new(()),
            state: Mutex::new(InputManagerState {
                devices: BTreeMap::new(),
                free_minors: BTreeSet::new(),
                next_minor: 0,
            }),
        })
    }

    fn identity(
        &self,
        token: axinput::InputDeviceToken,
        epoch: u64,
    ) -> VfsResult<(u32, InputDeviceKey)> {
        let mut state = self.state.lock();
        if state.devices.contains_key(&token) {
            return Err(AxError::AlreadyExists);
        }
        let minor = if let Some(minor) = state.free_minors.pop_first() {
            minor
        } else {
            let minor = state.next_minor;
            if minor > u32::MAX - 64 {
                return Err(AxError::NoMemory);
            }
            state.next_minor = state.next_minor.checked_add(1).ok_or(AxError::NoMemory)?;
            minor
        };
        let generation = epoch;
        Ok((
            minor,
            InputDeviceKey {
                token,
                device_id: DeviceId::new(13, 64 + minor),
                generation,
            },
        ))
    }

    fn release_minor(&self, minor: u32) {
        self.state.lock().free_minors.insert(minor);
    }

    fn add_device(&self, mut registered: axinput::RegisteredInputDevice) {
        let _lifecycle = self.lifecycle.lock();
        let (minor, key) = match self.identity(registered.token, registered.epoch) {
            Ok(value) => value,
            Err(error) => {
                error!("input device registration rejected: {error}");
                return;
            }
        };
        let input_name = format!("input{minor}");
        let event_name = format!("event{minor}");
        let dev_id = key.device_id;
        let bus_identity = registered.identity;
        let transport_path = input_transport_path(bus_identity);
        let sysfs = input_sysfs_description(&mut registered.device, bus_identity);
        let parent_identity =
            DeviceIdentity::without_dev("pci".into(), "input".into(), input_name.clone()).and_then(
                |identity| identity.child_of_path(transport_path.clone(), "input".into()),
            );
        let event_identity =
            DeviceIdentity::new("pci".into(), "input".into(), event_name.clone(), dev_id)
                .and_then(|identity| identity.with_devname(format!("input/{event_name}")))
                .and_then(|identity| {
                    identity
                        .child_of_path(format!("{transport_path}/input"), input_name.clone())
                });
        let published = (|| -> VfsResult<_> {
            let parent = DeviceRegistration::try_new(
                parent_identity?,
                "input".into(),
                sysfs.attributes(),
                None,
            )?;
            let event =
                DeviceRegistration::try_new(event_identity?, "input".into(), Vec::new(), None)?;
            if !has_pci_transport(bus_identity) {
                let parent_reservation = global_device_registry().reserve(parent.identity().clone())?;
                let event_reservation = global_device_registry().reserve(event.identity().clone())?;
                let (parent_handle, event_handle) = DeviceReservation::publish_pair_quiet(
                    parent_reservation,
                    parent,
                    event_reservation,
                    event,
                )?;
                return Ok((None, parent_handle, event_handle));
            }
            let pci = pci_sysfs_registration(bus_identity)?;
            let virtio = virtio_sysfs_registration(bus_identity)?;
            let pci_reservation = global_device_registry().reserve(pci.identity().clone())?;
            let virtio_reservation = global_device_registry().reserve(virtio.identity().clone())?;
            let parent_reservation = global_device_registry().reserve(parent.identity().clone())?;
            let event_reservation = global_device_registry().reserve(event.identity().clone())?;
            let [pci_handle, virtio_handle, parent_handle, event_handle] =
                DeviceReservation::publish_many_quiet([
                    (pci_reservation, pci),
                    (virtio_reservation, virtio),
                    (parent_reservation, parent),
                    (event_reservation, event),
                ])?;
            Ok((Some([pci_handle, virtio_handle]), parent_handle, event_handle))
        })();
        let (transport_handles, parent_handle, event_handle) = match published {
            Ok(handles) => handles,
            Err(error) => {
                error!("input sysfs publication failed: {error}");
                self.release_minor(minor);
                return;
            }
        };
        let event = EvdevDevice::new(registered.device);
        let node = Device::new_with_permissions(
            self.fs.clone(),
            NodeType::CharacterDevice,
            dev_id,
            NodePermission::from_bits_truncate(EVENT_NODE_MODE),
            Arc::new(EvdevNode::from_evdev(event.clone())),
        );
        let mut state = self.state.lock();
        if state
            .devices
            .insert(
                key.token,
                InputSlot {
                    key,
                    minor,
                    event,
                    node,
                    transport_handles,
                    parent_handle,
                    event_handle,
                },
            )
            .is_some()
        {
            unreachable!("duplicate token admitted after identity allocation");
        }
        drop(state);
        // `/dev/input/eventN` now resolves through the manager before either
        // uevent becomes observable to udev/libinput.
        if let Some([pci_handle, virtio_handle]) = transport_handles {
            let _ = pci_handle.add();
            let _ = virtio_handle.add();
        }
        let _ = parent_handle.add();
        let _ = event_handle.add();
    }

    fn remove_device(&self, token: axinput::InputDeviceToken, epoch: u64) {
        let _lifecycle = self.lifecycle.lock();
        let slot = {
            let mut state = self.state.lock();
            match state.devices.get(&token) {
                Some(slot) if slot.key.generation == epoch => state.devices.remove(&token),
                _ => None,
            }
        };
        let Some(slot) = slot else {
            return;
        };
        slot.event.disconnect();
        let _ = slot.event_handle.remove();
        let _ = slot.parent_handle.remove();
        if let Some([pci_handle, virtio_handle]) = slot.transport_handles {
            let _ = virtio_handle.remove();
            let _ = pci_handle.remove();
        }
        self.release_minor(slot.minor);
    }

    pub fn pause_all(&self) {
        let events = self
            .state
            .lock()
            .devices
            .values()
            .map(|slot| slot.event.clone())
            .collect::<Vec<_>>();
        for event in events {
            event.pause();
        }
    }

    pub fn resume_all(&self) {
        let events = self
            .state
            .lock()
            .devices
            .values()
            .map(|slot| slot.event.clone())
            .collect::<Vec<_>>();
        for event in events {
            event.resume();
        }
    }

    /// Replays a bounded `change` notification for each currently published
    /// input object after a uevent/netlink receiver reports packet loss.
    pub fn rescan(&self) {
        let handles = self
            .state
            .lock()
            .devices
            .values()
            .flat_map(|slot| {
                slot.transport_handles
                    .into_iter()
                    .flatten()
                    .chain([slot.parent_handle, slot.event_handle])
            })
            .collect::<Vec<_>>();
        for handle in handles {
            let _ = handle.change();
        }
    }

    fn metrics(&self) -> (u64, u64) {
        let state = self.state.lock();
        let devices = state.devices.len() as u64;
        let clients = state
            .devices
            .values()
            .map(|slot| slot.event.live_client_count())
            .sum();
        (devices, clients)
    }
}

impl axinput::InputDeviceListener for InputManager {
    fn device_added(&self, device: axinput::RegisteredInputDevice) {
        self.add_device(device);
    }

    fn device_removed(&self, token: axinput::InputDeviceToken, epoch: u64) {
        self.remove_device(token, epoch);
    }
}

impl SimpleDirOps for InputManager {
    fn child_names<'a>(&'a self) -> VfsResult<crate::pseudofs::ChildNames<'a>> {
        let names = self
            .state
            .lock()
            .devices
            .values()
            .map(|slot| format!("event{}", slot.minor))
            .collect::<Vec<_>>();
        try_boxed_names(
            names
                .into_iter()
                .map(|name| FsNameBuf::from_vec(name.into_bytes()).map(Cow::Owned))
                .collect::<VfsResult<Vec<_>>>()?
                .into_iter(),
        )
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<crate::pseudofs::NodeOpsMux> {
        let minor = parse_decimal_name(
            name.as_bytes()
                .strip_prefix(b"event")
                .ok_or(AxError::NotFound)?,
        )
        .ok_or(AxError::NotFound)?;
        let node = self
            .state
            .lock()
            .devices
            .values()
            .find(|slot| slot.minor == minor)
            .map(|slot| slot.node.clone())
            .ok_or(AxError::NotFound)?;
        Ok(node.into())
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

fn parse_decimal_name(bytes: &[u8]) -> Option<u32> {
    (!bytes.is_empty()).then_some(())?;
    bytes.iter().try_fold(0u32, |value, byte| {
        byte.checked_sub(b'0')
            .filter(|digit| *digit < 10)
            .and_then(|digit| value.checked_mul(10)?.checked_add(u32::from(digit)))
    })
}

pub fn input_devices(fs: Arc<SimpleFs>) -> Arc<InputManager> {
    let manager = InputManager::new(fs);
    *INPUT_MANAGER.lock() = Some(Arc::downgrade(&manager));
    axinput::install_listener(manager.clone());
    manager
}

/// Session ownership hooks.  Pause is reversible and never invalidates an
/// event FD; physical removal and EVIOCREVOKE retain their terminal semantics.
pub fn pause_input_devices() {
    if let Some(manager) = INPUT_MANAGER.lock().as_ref().and_then(Weak::upgrade) {
        manager.pause_all();
    }
}

pub fn resume_input_devices() {
    if let Some(manager) = INPUT_MANAGER.lock().as_ref().and_then(Weak::upgrade) {
        manager.resume_all();
    }
}

pub fn rescan_input_devices() {
    if let Some(manager) = INPUT_MANAGER.lock().as_ref().and_then(Weak::upgrade) {
        manager.rescan();
    }
}

/// Read-only aggregate for the graphics debug endpoint.  It does not prune
/// stale weak client references, because observing metrics must not change
/// evdev state or device lifetime.
pub(crate) fn input_metrics() -> (u64, u64) {
    INPUT_MANAGER
        .lock()
        .as_ref()
        .and_then(Weak::upgrade)
        .map_or((0, 0), |manager| manager.metrics())
}

fn attribute(name: &str, value: String) -> DeviceAttribute {
    DeviceAttribute::try_new(name.into(), move || Ok(value.clone()))
        .expect("static input sysfs attribute")
}

fn attribute_dir(name: &str, children: Vec<DeviceAttribute>) -> DeviceAttribute {
    DeviceAttribute::try_directory(name.into(), children).expect("static input sysfs directory")
}

struct InputSysfsDescription {
    name: String,
    phys: String,
    uniq: String,
    id: InputDeviceId,
    modalias: String,
    capabilities: Vec<(&'static str, String)>,
    properties: String,
    pci_vendor: u16,
    pci_device: u16,
    pci_modalias: String,
    virtio_index: u32,
}

impl InputSysfsDescription {
    fn attributes(&self) -> Vec<DeviceAttribute> {
        vec![
            attribute("name", self.name.clone()),
            attribute("phys", self.phys.clone()),
            attribute("uniq", self.uniq.clone()),
            attribute("modalias", self.modalias.clone()),
            attribute("pci_vendor", format!("{:04x}\n", self.pci_vendor)),
            attribute("pci_device", format!("{:04x}\n", self.pci_device)),
            attribute("pci_modalias", self.pci_modalias.clone()),
            attribute("virtio_index", format!("{}\n", self.virtio_index)),
            attribute("properties", self.properties.clone()),
            attribute_dir(
                "id",
                vec![
                    attribute("bustype", format!("{:04x}\n", self.id.bus_type)),
                    attribute("vendor", format!("{:04x}\n", self.id.vendor)),
                    attribute("product", format!("{:04x}\n", self.id.product)),
                    attribute("version", format!("{:04x}\n", self.id.version)),
                ],
            ),
            attribute_dir(
                "capabilities",
                self.capabilities
                    .iter()
                    .map(|(name, value)| attribute(name, value.clone()))
                    .collect(),
            ),
        ]
    }
}

/// Transport kobject path immediately above the input subsystem directory.
/// The input kobject is represented separately by `child_of_path` so the
/// event child can name that same parent without duplicating either name.
fn input_transport_path(identity: axdriver::InputBusIdentity) -> String {
    if identity.vendor_id == 0 && identity.device_id == 0 {
        return format!("virtual/virtio{}", identity.virtio_index);
    }
    format!(
        "{}/{}/{}",
        pci_root_name(identity),
        pci_bdf_name(identity),
        virtio_name(identity),
    )
}

fn has_pci_transport(identity: axdriver::InputBusIdentity) -> bool {
    identity.vendor_id != 0 || identity.device_id != 0
}

fn pci_root_name(identity: axdriver::InputBusIdentity) -> String {
    format!("pci{:04x}:{:02x}", identity.domain, identity.bus)
}

fn pci_bdf_name(identity: axdriver::InputBusIdentity) -> String {
    format!(
        "{:04x}:{:02x}:{:02x}.{:x}",
        identity.domain, identity.bus, identity.device, identity.function
    )
}

fn virtio_name(identity: axdriver::InputBusIdentity) -> String {
    format!("virtio{}", identity.virtio_index)
}

fn pci_sysfs_registration(
    identity: axdriver::InputBusIdentity,
) -> VfsResult<alloc::sync::Arc<DeviceRegistration>> {
    let root = pci_root_name(identity);
    let bdf = pci_bdf_name(identity);
    DeviceRegistration::try_bus_device(
        DeviceIdentity::without_dev(root, "pci".into(), bdf)?,
        "pci_device".into(),
        vec![
            attribute("vendor", format!("0x{:04x}\n", identity.vendor_id)),
            attribute("device", format!("0x{:04x}\n", identity.device_id)),
            attribute(
                "modalias",
                format!(
                    "pci:v{:08X}d{:08X}sv*sd*bc*sc*i*\n",
                    identity.vendor_id, identity.device_id
                ),
            ),
        ],
        "pci".into(),
        false,
    )
}

fn virtio_sysfs_registration(
    identity: axdriver::InputBusIdentity,
) -> VfsResult<alloc::sync::Arc<DeviceRegistration>> {
    let root = pci_root_name(identity);
    let bdf = pci_bdf_name(identity);
    DeviceRegistration::try_bus_device(
        DeviceIdentity::without_dev(root.clone(), "virtio".into(), virtio_name(identity))?
            .child_of_path(root, bdf)?,
        "virtio_device".into(),
        vec![
            attribute("modalias", "virtio:d00000012v00001AF4\n".into()),
            attribute("virtio_index", format!("{}\n", identity.virtio_index)),
        ],
        "virtio".into(),
        true,
    )
}

fn input_sysfs_description(
    device: &mut AxInputDevice,
    identity: axdriver::InputBusIdentity,
) -> InputSysfsDescription {
    // Discovery owns this object exclusively until it is installed in EvdevNode.
    let id = device.device_id();
    let mut event_bits = vec![0; (EventType::COUNT as usize).div_ceil(8)];
    for value in 0..EventType::COUNT {
        let Some(kind) = EventType::from_repr(value) else {
            continue;
        };
        if device.get_event_bits(kind, &mut []).unwrap_or(false) {
            set_bit_if_fits(&mut event_bits, value as usize);
        }
    }
    let mut capabilities = Vec::new();
    for (name, kind) in [
        ("key", EventType::Key),
        ("rel", EventType::Relative),
        ("abs", EventType::Absolute),
        ("msc", EventType::Misc),
        ("led", EventType::Led),
        ("snd", EventType::Sound),
        ("ff", EventType::ForceFeedback),
        ("sw", EventType::Switch),
    ] {
        let mut bits = vec![0; kind.bits_count().div_ceil(8)];
        let _ = device.get_event_bits(kind, &mut bits);
        capabilities.push((name, bitmap_hex(&bits)));
    }
    capabilities.insert(0, ("ev", bitmap_hex(&event_bits)));
    let mut properties = vec![0; 0x20usize.div_ceil(8)];
    if !device.get_property_bits(&mut properties).unwrap_or(false) {
        properties.fill(0);
    }
    InputSysfsDescription {
        name: device.device_name().into(),
        phys: {
            let physical = device.physical_location();
            if physical.is_empty() {
                format!("{}/input", input_transport_path(identity))
            } else {
                physical.into()
            }
        },
        uniq: device.unique_id().into(),
        id,
        modalias: format!(
            "input:b{:04x}v{:04x}p{:04x}e{:04x}\n",
            id.bus_type, id.vendor, id.product, id.version
        ),
        capabilities,
        properties: bitmap_hex(&properties),
        pci_vendor: identity.vendor_id,
        pci_device: identity.device_id,
        pci_modalias: format!(
            "pci:v{:08X}d{:08X}sv*sd*bc*sc*i*\n",
            identity.vendor_id, identity.device_id
        ),
        virtio_index: identity.virtio_index,
    }
}

fn bitmap_hex(bytes: &[u8]) -> String {
    // input_print_bitmap() renders native unsigned-long words, high word
    // first, so libinput/udev can parse word boundaries.  On the supported
    // x86_64 ABI an unsigned long is 64 bits.
    const WORD_BYTES: usize = size_of::<usize>();
    let word_count = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map(|last| last / WORD_BYTES + 1)
        .unwrap_or(1);
    let mut out = String::new();
    for word in (0..word_count).rev() {
        if word != word_count - 1 {
            out.push(' ');
        }
        let mut value = 0usize;
        for (index, byte) in bytes
            .get(word * WORD_BYTES..bytes.len().min((word + 1) * WORD_BYTES))
            .unwrap_or(&[])
            .iter()
            .enumerate()
        {
            value |= (*byte as usize) << (index * u8::BITS as usize);
        }
        use core::fmt::Write;
        let _ = write!(out, "{value:x}");
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> EvdevClient {
        EvdevClient {
            id: 1,
            lease: 1,
            device: Weak::new(),
            revoked: core::sync::atomic::AtomicBool::new(false),
            state: Mutex::new(EvdevClientState {
                queue: VecDeque::new(),
                clock: EvdevClock::Realtime,
                masks: BTreeMap::new(),
                overflowed: false,
            }),
        }
    }

    fn record(event_type: u16, code: u16) -> EvdevRecord {
        EvdevRecord {
            event: Event {
                event_type,
                code,
                value: 1,
            },
            realtime_arrival: Duration::from_secs(1),
            monotonic_arrival: Duration::from_secs(2),
        }
    }

    #[test]
    fn property_bitmap_uses_the_encoded_byte_length() {
        for size in [0, 1, 2, 7, 8, 9, 0x3fff] {
            assert_eq!(zero_bits_len(size, size.saturating_mul(8)), size);
        }
    }

    #[test]
    fn sysfs_bitmaps_use_space_separated_native_words_high_first() {
        let mut keyboard = [0u8; 16];
        set_bit_if_fits(&mut keyboard, 1);
        set_bit_if_fits(&mut keyboard, 65);
        assert_eq!(bitmap_hex(&keyboard), "2 2\n");

        let mut low_only = [0u8; 16];
        set_bit_if_fits(&mut low_only, 1);
        assert_eq!(bitmap_hex(&low_only), "2\n");
        assert_eq!(bitmap_hex(&[]), "0\n");
    }

    #[test]
    fn sysfs_bitmaps_preserve_partial_native_words() {
        // Event types and input properties occupy four bytes, while relative
        // axes need only two. These still contain a nonempty native word.
        assert_eq!(bitmap_hex(&[3, 0, 0, 0]), "3\n");
        assert_eq!(bitmap_hex(&[3, 0]), "3\n");
        assert_eq!(bitmap_hex(&[1]), "1\n");
        assert_eq!(bitmap_hex(&[0, 0, 0, 0, 0, 0, 0, 0, 2]), "2 0\n");
    }

    #[test]
    fn event_nodes_default_to_udev_managed_group_access() {
        assert_eq!(
            NodePermission::from_bits_truncate(EVENT_NODE_MODE).bits(),
            0o660
        );
    }

    #[test]
    fn input_kobjects_have_linux_canonical_parent_and_event_paths() {
        let transport = "pci0000:00/0000:00:03.0/virtio0";
        let parent = DeviceIdentity::without_dev("pci".into(), "input".into(), "input0".into())
            .unwrap()
            .child_of_path(transport.into(), "input".into())
            .unwrap();
        let parent = DeviceRegistration::try_new(parent, "input".into(), Vec::new(), None).unwrap();
        assert!(parent.uevent_payload().contains(
            "DEVPATH=/devices/pci0000:00/0000:00:03.0/virtio0/input/input0\n"
        ));

        let identity = DeviceIdentity::new(
            "pci".into(),
            "input".into(),
            "event0".into(),
            DeviceId::new(13, 64),
        )
        .unwrap()
        .with_devname("input/event0".into())
        .unwrap()
        .child_of_path(
            format!("{transport}/input"),
            "input0".into(),
        )
        .unwrap();
        let registration =
            DeviceRegistration::try_new(identity, "input".into(), Vec::new(), None).unwrap();
        assert!(registration.uevent_payload().contains(
            "DEVPATH=/devices/pci0000:00/0000:00:03.0/virtio0/input/input0/event0\n"
        ));
    }

    #[test]
    fn input_parent_sysfs_has_linux_id_and_capability_paths() {
        let description = InputSysfsDescription {
            name: "virtio keyboard".into(),
            phys: String::new(),
            uniq: String::new(),
            id: InputDeviceId {
                bus_type: 0x06,
                vendor: 0x1234,
                product: 0x5678,
                version: 1,
            },
            modalias: "input:b0006v1234p5678e0001\n".into(),
            capabilities: vec![
                ("ev", "03\n".into()),
                ("key", "1\n".into()),
                ("rel", "0\n".into()),
                ("abs", "0\n".into()),
                ("msc", "0\n".into()),
                ("led", "0\n".into()),
                ("snd", "0\n".into()),
                ("ff", "0\n".into()),
                ("sw", "0\n".into()),
            ],
            properties: "0\n".into(),
            pci_vendor: 0x1af4,
            pci_device: 0x1052,
            pci_modalias: "pci:v00001AF4d00001052sv*sd*bc*sc*i*\n".into(),
            virtio_index: 16,
        };
        let attributes = description.attributes();
        let names = attributes
            .iter()
            .map(DeviceAttribute::name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "name",
                "phys",
                "uniq",
                "modalias",
                "pci_vendor",
                "pci_device",
                "pci_modalias",
                "virtio_index",
                "properties",
                "id",
                "capabilities"
            ]
        );
        assert_eq!(
            attributes.iter().find(|attribute| attribute.name() == "id").unwrap().directory_child_names().unwrap(),
            vec!["bustype", "vendor", "product", "version"]
        );
        assert_eq!(
            attributes.iter().find(|attribute| attribute.name() == "capabilities").unwrap().directory_child_names().unwrap(),
            vec!["ev", "key", "rel", "abs", "msc", "led", "snd", "ff", "sw"]
        );
    }

    #[test]
    fn variable_bitmaps_are_clamped_without_padding() {
        assert_eq!(zero_bits_len(1, 1), 1);
        assert_eq!(zero_bits_len(2, 9), 2);
        assert_eq!(zero_bits_len(8, 9), 2);
        assert_eq!(linux_bitmap_len(64, EventType::MAX as usize), 8);
        assert_eq!(linux_bitmap_len(4, EventType::MAX as usize), 4);
        assert_eq!(linux_bitmap_len(64, 0), 0);
    }

    #[test]
    fn event_type_bitmap_respects_zero_and_short_user_buffers() {
        let mut empty = [];
        set_bit_if_fits(&mut empty, EventType::ForceFeedback as usize);
        let mut one = [0u8; 1];
        set_bit_if_fits(&mut one, EventType::ForceFeedback as usize);
        assert_eq!(one, [0]);
        set_bit_if_fits(&mut one, EventType::Key as usize);
        assert_eq!(one, [1 << EventType::Key as u8]);
    }

    #[test]
    fn linux_repeat_power_and_ff_status_event_types_are_queryable() {
        assert_eq!(EventType::from_repr(0x14), Some(EventType::Repeat));
        assert_eq!(EventType::from_repr(0x16), Some(EventType::Power));
        assert_eq!(
            EventType::from_repr(0x17),
            Some(EventType::ForceFeedbackStatus)
        );
        assert_eq!(EventType::Repeat.bits_count(), 2);
        assert_eq!(EventType::Power.bits_count(), 1);
        assert_eq!(EventType::ForceFeedbackStatus.bits_count(), 2);
    }

    #[test]
    fn client_mask_keeps_frame_boundaries_and_syn() {
        let _context = crate::test_support::scheduler_test_context();
        let client = client();
        client.set_mask(EventType::Key as u16, vec![0]);
        client.enqueue_frame(&[
            record(EventType::Key as u16, 30),
            record(EV_SYN, SYN_REPORT),
        ]);
        assert_eq!(client.pop().unwrap().event.code, SYN_REPORT);
        assert!(client.pop().is_none());
    }

    #[test]
    fn clients_keep_independent_ofd_masks_and_queues() {
        let _context = crate::test_support::scheduler_test_context();
        let masked = client();
        let unmasked = client();
        masked.set_mask(EventType::Key as u16, vec![0]);
        let frame = [
            record(EventType::Key as u16, 30),
            record(EV_SYN, SYN_REPORT),
        ];
        masked.enqueue_frame(&frame);
        unmasked.enqueue_frame(&frame);
        assert_eq!(masked.pop().unwrap().event.code, SYN_REPORT);
        assert_eq!(unmasked.pop().unwrap().event.code, 30);
        assert_eq!(unmasked.pop().unwrap().event.code, SYN_REPORT);
    }

    #[test]
    fn overflow_reports_syn_dropped_before_next_complete_frame() {
        let _context = crate::test_support::scheduler_test_context();
        let client = client();
        let too_large = vec![record(EventType::Key as u16, 1); EVDEV_CLIENT_QUEUE_EVENTS + 1];
        client.enqueue_frame(&too_large);
        client.enqueue_frame(&[record(EventType::Key as u16, 1), record(EV_SYN, SYN_REPORT)]);
        assert_eq!(client.pop().unwrap().event.code, SYN_DROPPED);
        assert_eq!(client.pop().unwrap().event.code, 1);
        assert_eq!(client.pop().unwrap().event.code, SYN_REPORT);
    }

    #[test]
    fn discarded_device_frame_marks_the_next_client_frame_dropped() {
        let _context = crate::test_support::scheduler_test_context();
        let client = client();
        client.mark_overflow();
        client.enqueue_frame(&[record(EventType::Key as u16, 1), record(EV_SYN, SYN_REPORT)]);
        assert_eq!(client.pop().unwrap().event.code, SYN_DROPPED);
        assert_eq!(client.pop().unwrap().event.code, 1);
    }

    #[test]
    fn clock_ids_follow_linux_evdev_values() {
        let _context = crate::test_support::scheduler_test_context();
        let client = client();
        client.set_clock_id(1).unwrap();
        assert_eq!(client.clock(), EvdevClock::Monotonic);
        client.set_clock_id(7).unwrap();
        assert_eq!(client.clock(), EvdevClock::Boottime);
        assert_eq!(client.set_clock_id(2), Err(LinuxError::EINVAL.into()));
    }

    #[test]
    fn event_masks_are_per_ofd_and_default_to_known_codes() {
        let _context = crate::test_support::scheduler_test_context();
        let first = client();
        let second = client();
        let mut bits = [0; 2];
        first.mask_bits(EventType::Relative as u16, &mut bits);
        assert_eq!(bits, [0xff, 0xff]);

        first.set_mask(EventType::Relative as u16, vec![0b0000_0100]);
        first.mask_bits(EventType::Relative as u16, &mut bits);
        assert_eq!(bits, [0b0000_0100, 0]);
        second.mask_bits(EventType::Relative as u16, &mut bits);
        assert_eq!(bits, [0xff, 0xff]);
    }

    #[test]
    fn event_mask_lengths_are_bounded_to_known_codes() {
        assert_eq!(event_mask_len(EventType::Key as u16), 0x300 / 8);
        assert_eq!(event_mask_len(EventType::Absolute as u16), 0x40 / 8);
        assert_eq!(
            input_mask_event_type(EventType::MAX as u32),
            Some(EventType::MAX as u16)
        );
        assert_eq!(input_mask_event_type(EventType::Key as u32), Some(1));
        assert_eq!(input_mask_event_type(u32::MAX), None);
    }
}
