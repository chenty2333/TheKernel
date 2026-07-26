// SPDX-License-Identifier: Apache-2.0

use alloc::{
    borrow::Cow,
    sync::{Arc, Weak},
    task::Wake,
    vec::Vec,
};
use core::{
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    task::{Context, Waker},
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use bitflags::bitflags;
use hashbrown::HashMap;
use kspin::SpinNoIrq;
use linux_raw_sys::general::{EPOLLET, EPOLLONESHOT};
use ouroboros::self_referencing;
use spin::Once;
use thekernel_linux_fd::{
    DeliveryOutcome, EpollCore, EpollError, EpollGraph, EpollGraphId, EpollGraphLimits, EpollId,
    EpollInterest as LinuxEpollInterest, EpollKey, EpollToken, FdNumber, GraphEdgeToken,
    GraphError, GraphNodeToken, InterestMask, InterestMode, NotifyOutcome, ReadyEvent, ReadyMask,
};

use super::{
    FileDescription, FileLike, FileLikeKind, Kstat,
    desc::{DescriptorCloseRegistration, DescriptorCloseRegistrationError},
    get_file_description,
};
use crate::task::AX_FILE_LIMIT;

const EPOLL_MAX_NESTS: usize = 5;
const EPOLL_CORE_CAPACITY: usize = AX_FILE_LIMIT;
const EPOLL_GLOBAL_CORE_SLOTS: usize = 65_536;
const EPOLL_GRAPH_NODES: usize = 64;
const EPOLL_GRAPH_EDGES: usize = 16_384;
const EPOLL_GRAPH_WALK_LIMIT: usize = 65_536;
const EPOLL_WAITER_SLOTS: usize = 64;

const EPOLL_FAULT_NONE: u8 = 0;
const EPOLL_FAULT_PENDING_OVERFLOW: u8 = 1;
const EPOLL_FAULT_CORE_INVARIANT: u8 = 2;

static NEXT_EPOLL_ID: AtomicU64 = AtomicU64::new(1);
static EPOLL_CORE_SLOTS: AtomicUsize = AtomicUsize::new(0);
static EPOLL_GRAPH: Once<SpinNoIrq<EpollGraph>> = Once::new();

fn map_graph_error(error: GraphError) -> AxError {
    match error {
        GraphError::NoMemory => AxError::NoMemory,
        GraphError::Capacity | GraphError::ParentLimit | GraphError::GenerationExhausted => {
            LinuxError::ENOSPC.into()
        }
        GraphError::SelfCycle => AxError::InvalidInput,
        GraphError::Cycle | GraphError::Nesting | GraphError::WalkLimit => LinuxError::ELOOP.into(),
        GraphError::DuplicateNode | GraphError::Busy | GraphError::StaleToken => AxError::BadState,
        GraphError::Unbounded => AxError::InvalidInput,
        _ => AxError::BadState,
    }
}

fn map_epoll_error(error: EpollError) -> AxError {
    match error {
        EpollError::NoMemory => AxError::NoMemory,
        EpollError::Capacity | EpollError::GenerationExhausted | EpollError::ReadyQueueFull => {
            LinuxError::ENOSPC.into()
        }
        EpollError::Duplicate => AxError::AlreadyExists,
        EpollError::NotFound | EpollError::StaleToken => AxError::NotFound,
        EpollError::UnsupportedMode => AxError::OperationNotSupported,
        EpollError::RescanRequired => AxError::WouldBlock,
        _ => AxError::BadState,
    }
}

fn map_descriptor_close_error(error: DescriptorCloseRegistrationError) -> AxError {
    match error {
        DescriptorCloseRegistrationError::NoMemory => AxError::NoMemory,
        DescriptorCloseRegistrationError::Closed => AxError::BadFileDescriptor,
        DescriptorCloseRegistrationError::Full
        | DescriptorCloseRegistrationError::TokenSpaceExhausted => LinuxError::ENOSPC.into(),
    }
}

fn allocate_epoll_id() -> AxResult<EpollId> {
    let raw = NEXT_EPOLL_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            next.checked_add(1)
        })
        .map_err(|_| AxError::TooManyOpenFiles)?;
    EpollId::new(raw).ok_or(AxError::BadState)
}

fn graph_domain() -> AxResult<&'static SpinNoIrq<EpollGraph>> {
    EPOLL_GRAPH.try_call_once(|| {
        let id = EpollGraphId::new(1).ok_or(AxError::BadState)?;
        let limits = EpollGraphLimits::try_new(
            EPOLL_GRAPH_NODES,
            EPOLL_GRAPH_EDGES,
            EPOLL_GRAPH_NODES,
            EPOLL_MAX_NESTS,
            EPOLL_GRAPH_WALK_LIMIT,
        )
        .map_err(map_graph_error)?;
        EpollGraph::try_new(id, limits)
            .map(SpinNoIrq::new)
            .map_err(map_graph_error)
    })
}

struct EpollCoreCharge(usize);

impl EpollCoreCharge {
    fn try_new(slots: usize) -> AxResult<Self> {
        EPOLL_CORE_SLOTS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(slots)
                    .filter(|next| *next <= EPOLL_GLOBAL_CORE_SLOTS)
            })
            .map_err(|_| AxError::from(LinuxError::ENOSPC))?;
        Ok(Self(slots))
    }
}

impl Drop for EpollCoreCharge {
    fn drop(&mut self) {
        EPOLL_CORE_SLOTS.fetch_sub(self.0, Ordering::AcqRel);
    }
}

fn interest_from_io(events: IoEvents) -> AxResult<InterestMask> {
    if events.contains(IoEvents::MESSAGE) {
        // Linux defines EPOLLMSG but does not implement a useful readiness
        // contract for it. The 0.1 core therefore rejects it explicitly.
        return Err(AxError::OperationNotSupported);
    }
    let mut bits = 0;
    for (event, interest) in [
        (IoEvents::READABLE, InterestMask::IN),
        (IoEvents::PRIORITY, InterestMask::PRI),
        (IoEvents::WRITABLE, InterestMask::OUT),
        (IoEvents::READ_NORMAL, InterestMask::READ_NORMAL),
        (IoEvents::READ_BAND, InterestMask::READ_BAND),
        (IoEvents::WRITE_NORMAL, InterestMask::WRITE_NORMAL),
        (IoEvents::WRITE_BAND, InterestMask::WRITE_BAND),
        (IoEvents::READ_HANGUP, InterestMask::READ_HANGUP),
    ] {
        if events.contains(event) {
            bits |= interest.bits();
        }
    }
    InterestMask::from_bits(bits).ok_or(AxError::InvalidInput)
}

fn ready_from_io(events: IoEvents) -> ReadyMask {
    let mut bits = 0;
    for (event, ready) in [
        (IoEvents::READABLE, ReadyMask::IN),
        (IoEvents::PRIORITY, ReadyMask::PRI),
        (IoEvents::WRITABLE, ReadyMask::OUT),
        (IoEvents::ERROR, ReadyMask::ERROR),
        (IoEvents::HANGUP, ReadyMask::HANGUP),
        (IoEvents::READ_NORMAL, ReadyMask::READ_NORMAL),
        (IoEvents::READ_BAND, ReadyMask::READ_BAND),
        (IoEvents::WRITE_NORMAL, ReadyMask::WRITE_NORMAL),
        (IoEvents::WRITE_BAND, ReadyMask::WRITE_BAND),
        (IoEvents::READ_HANGUP, ReadyMask::READ_HANGUP),
    ] {
        if events.contains(event) {
            bits |= ready.bits();
        }
    }
    ReadyMask::from_bits_retain(bits)
}

fn io_from_ready(events: ReadyMask) -> IoEvents {
    let mut io = IoEvents::empty();
    for (ready, event) in [
        (ReadyMask::IN, IoEvents::READABLE),
        (ReadyMask::PRI, IoEvents::PRIORITY),
        (ReadyMask::OUT, IoEvents::WRITABLE),
        (ReadyMask::ERROR, IoEvents::ERROR),
        (ReadyMask::HANGUP, IoEvents::HANGUP),
        (ReadyMask::READ_NORMAL, IoEvents::READ_NORMAL),
        (ReadyMask::READ_BAND, IoEvents::READ_BAND),
        (ReadyMask::WRITE_NORMAL, IoEvents::WRITE_NORMAL),
        (ReadyMask::WRITE_BAND, IoEvents::WRITE_BAND),
        (ReadyMask::READ_HANGUP, IoEvents::READ_HANGUP),
    ] {
        if !(events & ready).is_empty() {
            io |= event;
        }
    }
    io
}

#[derive(Clone, Copy, Default)]
pub struct EpollEvent {
    pub events: IoEvents,
    pub user_data: u64,
}

bitflags! {
    #[derive(Debug, Clone, Copy, Default)]
    pub struct EpollFlags: u32 {
        const EDGE_TRIGGER = EPOLLET;
        const ONESHOT = EPOLLONESHOT;
    }
}

#[self_referencing]
struct OwnedInterestRegistration {
    file: Arc<FileDescription>,
    #[borrows(file)]
    #[covariant]
    registration: PollRegistration<'this>,
}

#[derive(Default)]
struct RegistrationState {
    arming: bool,
    woke_during_arm: bool,
    registration: Option<OwnedInterestRegistration>,
}

#[derive(Default)]
struct TokenPublicationState {
    source_woke: bool,
    target_closed: bool,
}

struct EpollWakePort {
    poll_ready: PollSet<EPOLL_WAITER_SLOTS>,
    callback_pending: AtomicBool,
}

impl EpollWakePort {
    /// Publishes callback work without reaching the epoll graph or state lock.
    fn publish_callback(&self) {
        self.callback_pending.store(true, Ordering::Release);
        self.poll_ready.wake();
    }
}

struct InterestCallbackState {
    wake_port: Weak<EpollWakePort>,
    source_enabled: AtomicBool,
    source_woke: AtomicBool,
    target_closed: AtomicBool,
}

impl InterestCallbackState {
    fn publish_source_wake(&self) {
        self.source_woke.store(true, Ordering::Release);
        if let Some(port) = self.wake_port.upgrade() {
            port.publish_callback();
        }
    }

    fn publish_target_close(&self) {
        self.source_enabled.store(false, Ordering::Release);
        self.target_closed.store(true, Ordering::Release);
        if let Some(port) = self.wake_port.upgrade() {
            port.publish_callback();
        }
    }
}

struct InterestControl {
    file: Arc<FileDescription>,
    poll_events: IoEvents,
    one_shot: bool,
    active: AtomicBool,
    callback: Arc<InterestCallbackState>,
    pending: AtomicBool,
    waker: Once<Waker>,
    registration: SpinNoIrq<RegistrationState>,
    close_registration: SpinNoIrq<Option<DescriptorCloseRegistration>>,
}

struct InterestWake(Arc<InterestCallbackState>);
struct InterestCloseWake(Arc<InterestCallbackState>);

impl Wake for InterestWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.publish_source_wake();
    }
}

impl Wake for InterestCloseWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.publish_target_close();
    }
}

impl InterestControl {
    fn try_new(
        wake_port: &Arc<EpollWakePort>,
        file: Arc<FileDescription>,
        poll_events: IoEvents,
        one_shot: bool,
    ) -> AxResult<Arc<Self>> {
        let callback = Arc::try_new(InterestCallbackState {
            wake_port: Arc::downgrade(wake_port),
            source_enabled: AtomicBool::new(true),
            source_woke: AtomicBool::new(false),
            target_closed: AtomicBool::new(false),
        })
        .map_err(|_| AxError::NoMemory)?;
        let control = Arc::try_new(Self {
            file,
            poll_events,
            one_shot,
            active: AtomicBool::new(true),
            callback: Arc::clone(&callback),
            pending: AtomicBool::new(false),
            waker: Once::new(),
            registration: SpinNoIrq::new(RegistrationState::default()),
            close_registration: SpinNoIrq::new(None),
        })
        .map_err(|_| AxError::NoMemory)?;
        let wake =
            Arc::try_new(InterestWake(Arc::clone(&callback))).map_err(|_| AxError::NoMemory)?;
        control.waker.call_once(|| Waker::from(wake));
        let close_wake =
            Arc::try_new(InterestCloseWake(callback)).map_err(|_| AxError::NoMemory)?;
        let close_waker = Waker::from(close_wake);
        let close_registration = control
            .file
            .register_descriptor_close(&close_waker)
            .map_err(map_descriptor_close_error)?;
        *control.close_registration.lock() = Some(close_registration);
        Ok(control)
    }

    fn is_source_enabled(&self) -> bool {
        self.active.load(Ordering::Acquire) && self.callback.source_enabled.load(Ordering::Acquire)
    }

    fn begin_registration(&self) -> bool {
        let mut state = self.registration.lock();
        if !self.is_source_enabled() || state.arming || state.registration.is_some() {
            return false;
        }
        state.arming = true;
        state.woke_during_arm = false;
        true
    }

    fn finish_registration(&self, registration: OwnedInterestRegistration) {
        let retired = {
            let mut state = self.registration.lock();
            state.arming = false;
            if self.is_source_enabled() && !state.woke_during_arm {
                state.registration = Some(registration);
                None
            } else {
                Some(registration)
            }
        };
        drop(retired);
    }

    fn abort_registration(&self) {
        let mut state = self.registration.lock();
        state.arming = false;
        state.woke_during_arm = false;
    }

    fn registration_fired(&self) {
        let retired = {
            let mut state = self.registration.lock();
            if state.arming {
                state.woke_during_arm = true;
                None
            } else {
                state.registration.take()
            }
        };
        drop(retired);
    }

    fn cancel_registration(&self) {
        let retired = self.registration.lock().registration.take();
        drop(retired);
    }

    fn ensure_armed(&self) -> AxResult<()> {
        if !self.is_source_enabled() || !self.begin_registration() {
            return Ok(());
        }
        let Some(waker) = self.waker.get() else {
            self.abort_registration();
            return Err(AxError::BadState);
        };
        let file = Arc::clone(&self.file);
        let events = self.poll_events;
        match OwnedInterestRegistration::try_new(file, |file| {
            let mut context = Context::from_waker(waker);
            file.register(&mut context, events)
        }) {
            Ok(registration) => {
                self.finish_registration(registration);
                Ok(())
            }
            Err(error) => {
                self.abort_registration();
                Err(crate::readiness::registration_error(error))
            }
        }
    }

    fn check_arm_check(&self) -> AxResult<ReadyMask> {
        if !self.is_source_enabled() {
            return Ok(ReadyMask::EMPTY);
        }
        let before = self.file.poll();
        self.ensure_armed()?;
        let after = self.file.poll();
        Ok(ready_from_io(before | after))
    }

    fn publish_token(&self) -> TokenPublicationState {
        TokenPublicationState {
            source_woke: self.take_source_wake_hint(),
            target_closed: self.take_target_close_hint(),
        }
    }

    fn take_source_wake_hint(&self) -> bool {
        self.callback.source_woke.swap(false, Ordering::AcqRel)
    }

    fn take_target_close_hint(&self) -> bool {
        self.callback.target_closed.swap(false, Ordering::AcqRel)
    }

    fn disable_after_one_shot(&self) {
        if self.one_shot {
            self.callback.source_enabled.store(false, Ordering::Release);
            self.cancel_registration();
            self.pending.store(false, Ordering::Release);
        }
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
        self.callback.source_enabled.store(false, Ordering::Release);
        self.callback.source_woke.store(false, Ordering::Release);
        self.callback.target_closed.store(false, Ordering::Release);
        self.pending.store(false, Ordering::Release);
        self.cancel_registration();
        let close_registration = self.close_registration.lock().take();
        drop(close_registration);
    }
}

struct PendingQueue {
    items: Vec<Option<EpollToken>>,
    head: usize,
    len: usize,
}

impl PendingQueue {
    fn try_new(capacity: usize) -> AxResult<Self> {
        let mut items = Vec::new();
        items
            .try_reserve_exact(capacity)
            .map_err(|_| AxError::NoMemory)?;
        items.resize(capacity, None);
        Ok(Self {
            items,
            head: 0,
            len: 0,
        })
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, token: EpollToken) -> Result<(), ()> {
        if self.items.is_empty() || self.len == self.items.len() {
            return Err(());
        }
        let index = (self.head + self.len) % self.items.len();
        self.items[index] = Some(token);
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<EpollToken> {
        if self.len == 0 {
            return None;
        }
        let token = self.items[self.head].take();
        self.head = (self.head + 1) % self.items.len();
        self.len -= 1;
        token
    }

    fn remove(&mut self, token: EpollToken) {
        if self.len == 0 {
            return;
        }
        let capacity = self.items.len();
        let mut retained = 0;
        for read in 0..self.len {
            let index = (self.head + read) % capacity;
            let item = self.items[index].take();
            if item != Some(token) {
                let write = (self.head + retained) % capacity;
                self.items[write] = item;
                retained += 1;
            }
        }
        self.len = retained;
    }
}

struct InterestRecord {
    key: EpollKey,
    token: EpollToken,
    edge: GraphEdgeToken,
    control: Arc<InterestControl>,
}

struct EpollState {
    core: EpollCore<u64, Arc<InterestControl>>,
    by_key: HashMap<EpollKey, EpollToken>,
    by_slot: Vec<Option<InterestRecord>>,
    pending: PendingQueue,
}

impl EpollState {
    fn try_new(id: EpollId) -> AxResult<Self> {
        let core = EpollCore::try_new(id, EPOLL_CORE_CAPACITY).map_err(map_epoll_error)?;
        let mut by_key = HashMap::new();
        by_key
            .try_reserve(EPOLL_CORE_CAPACITY)
            .map_err(|_| AxError::NoMemory)?;
        let mut by_slot = Vec::new();
        by_slot
            .try_reserve_exact(EPOLL_CORE_CAPACITY)
            .map_err(|_| AxError::NoMemory)?;
        by_slot.resize_with(EPOLL_CORE_CAPACITY, || None);
        Ok(Self {
            core,
            by_key,
            by_slot,
            pending: PendingQueue::try_new(EPOLL_CORE_CAPACITY)?,
        })
    }

    fn record(&self, token: EpollToken) -> Option<&InterestRecord> {
        self.by_slot
            .get(token.slot())?
            .as_ref()
            .filter(|record| record.token == token)
    }
}

struct EpollInner {
    node: GraphNodeToken,
    state: SpinNoIrq<EpollState>,
    wake_port: Arc<EpollWakePort>,
    fault: AtomicU8,
    _charge: EpollCoreCharge,
}

impl EpollInner {
    fn try_new() -> AxResult<Arc<Self>> {
        let id = allocate_epoll_id()?;
        let charge = EpollCoreCharge::try_new(EPOLL_CORE_CAPACITY)?;
        let state = EpollState::try_new(id)?;
        let wake_port = Arc::try_new(EpollWakePort {
            poll_ready: PollSet::new(),
            callback_pending: AtomicBool::new(false),
        })
        .map_err(|_| AxError::NoMemory)?;
        let node = graph_domain()?
            .lock()
            .register(id)
            .map_err(map_graph_error)?;
        Arc::try_new(Self {
            node,
            state: SpinNoIrq::new(state),
            wake_port,
            fault: AtomicU8::new(EPOLL_FAULT_NONE),
            _charge: charge,
        })
        .map_err(|_| AxError::NoMemory)
    }

    fn set_fault(&self, fault: u8) {
        let _ = self.fault.compare_exchange(
            EPOLL_FAULT_NONE,
            fault,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.wake_port.poll_ready.wake();
    }

    fn check_fault(&self) -> AxResult<()> {
        match self.fault.load(Ordering::Acquire) {
            EPOLL_FAULT_NONE => Ok(()),
            // `EPOLL_FAULT_PENDING_OVERFLOW`, `EPOLL_FAULT_CORE_INVARIANT`,
            // and any fault code added later all mean the same thing to a
            // caller: this epoll instance can no longer be trusted.
            _ => Err(AxError::BadState),
        }
    }

    fn enqueue_pending(&self, token: EpollToken, control: &InterestControl) {
        if !control.is_source_enabled() {
            return;
        }
        let mut state = self.state.lock();
        let valid = state
            .record(token)
            .is_some_and(|record| core::ptr::eq(record.control.as_ref(), control));
        if !valid
            || control
                .pending
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        if state.pending.push(token).is_err() {
            control.pending.store(false, Ordering::Release);
            drop(state);
            error!("epoll pending source queue violated admitted capacity");
            self.set_fault(EPOLL_FAULT_PENDING_OVERFLOW);
            return;
        }
        drop(state);
        self.wake_port.poll_ready.wake();
    }

    fn requeue_pending(&self, token: EpollToken, control: &Arc<InterestControl>) {
        let mut state = self.state.lock();
        let valid = state
            .record(token)
            .is_some_and(|record| Arc::ptr_eq(&record.control, control));
        if !valid
            || control
                .pending
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        if state.pending.push(token).is_err() {
            control.pending.store(false, Ordering::Release);
            drop(state);
            self.set_fault(EPOLL_FAULT_PENDING_OVERFLOW);
        } else {
            drop(state);
            self.wake_port.poll_ready.wake();
        }
    }

    fn notify_ready(&self, token: EpollToken, ready: ReadyMask) -> AxResult<()> {
        if ready.is_empty() {
            return Ok(());
        }
        let outcome = {
            let mut state = self.state.lock();
            if state.record(token).is_none() {
                return Ok(());
            }
            state.core.notify(token, ready)
        };
        match outcome {
            Ok(NotifyOutcome::Enqueued) => {
                self.wake_port.poll_ready.wake();
                Ok(())
            }
            Ok(NotifyOutcome::Coalesced | NotifyOutcome::Ignored) => Ok(()),
            Err(EpollError::ReadyQueueFull) => {
                self.wake_port.poll_ready.wake();
                Ok(())
            }
            Err(EpollError::StaleToken) => Ok(()),
            Err(error) => Err(map_epoll_error(error)),
        }
    }

    fn remove_closed_target(&self, token: EpollToken, control: &InterestControl) {
        let domain = match graph_domain() {
            Ok(domain) => domain,
            Err(error) => {
                error!("epoll close teardown could not access graph domain: {error:?}");
                self.set_fault(EPOLL_FAULT_CORE_INVARIANT);
                return;
            }
        };
        let mut graph = domain.lock();
        let mut state = self.state.lock();
        let valid = state
            .record(token)
            .is_some_and(|record| core::ptr::eq(record.control.as_ref(), control));
        if !valid {
            return;
        }
        let retired = match state.core.remove(token) {
            Ok(retired) => retired,
            Err(error) => {
                drop(state);
                drop(graph);
                error!("epoll close teardown lost core token: {error:?}");
                self.set_fault(EPOLL_FAULT_CORE_INVARIANT);
                return;
            }
        };
        let Some(record) = state.by_slot.get_mut(token.slot()).and_then(Option::take) else {
            drop(state);
            drop(graph);
            drop(retired);
            self.set_fault(EPOLL_FAULT_CORE_INVARIANT);
            return;
        };
        state.by_key.remove(&record.key);
        state.pending.remove(token);
        let graph_result = graph.remove_interest(record.edge);
        drop(state);
        drop(graph);

        record.control.deactivate();
        let (_, _, _, _, retired_control) = retired.into_parts();
        drop(retired_control);
        drop(record);
        if let Err(error) = graph_result {
            error!("epoll close graph teardown failed: {error:?}");
            self.set_fault(EPOLL_FAULT_CORE_INVARIANT);
        }
    }

    fn drain_callback_hints(&self) {
        // Callback publication is lock-free. Task context scans the finite
        // per-epoll interest table and owns every registration/core/graph
        // mutation and destructor which follows from those hints.
        if !self
            .wake_port
            .callback_pending
            .swap(false, Ordering::AcqRel)
        {
            return;
        }
        for slot in 0..EPOLL_CORE_CAPACITY {
            let Some((token, control)) = ({
                let state = self.state.lock();
                state.by_slot.get(slot).and_then(|record| {
                    record
                        .as_ref()
                        .map(|record| (record.token, Arc::clone(&record.control)))
                })
            }) else {
                continue;
            };

            if control.take_target_close_hint() {
                control.take_source_wake_hint();
                self.remove_closed_target(token, &control);
            } else if control.take_source_wake_hint() {
                control.registration_fired();
                self.enqueue_pending(token, &control);
            }
        }
    }

    fn drain_pending(&self) -> AxResult<()> {
        self.drain_callback_hints();
        self.check_fault()?;
        loop {
            let next = {
                let mut state = self.state.lock();
                let Some(token) = state.pending.pop() else {
                    return Ok(());
                };
                let Some(control) = state
                    .record(token)
                    .map(|record| Arc::clone(&record.control))
                else {
                    continue;
                };
                control.pending.store(false, Ordering::Release);
                (token, control)
            };
            let (token, control) = next;
            match control.check_arm_check() {
                Ok(ready) => self.notify_ready(token, ready)?,
                Err(error) => {
                    self.requeue_pending(token, &control);
                    return Err(error);
                }
            }
        }
    }

    fn recover_core(&self) -> AxResult<()> {
        loop {
            let progress = {
                let mut state = self.state.lock();
                let Some(token) = state.core.rescan_token() else {
                    return Ok(());
                };
                state
                    .core
                    .rescan_ready(token, EPOLL_CORE_CAPACITY)
                    .map_err(map_epoll_error)?
            };
            if progress.complete {
                self.wake_port.poll_ready.wake();
                return Ok(());
            }
        }
    }

    fn has_ready(&self) -> bool {
        if self.fault.load(Ordering::Acquire) != EPOLL_FAULT_NONE
            || self.wake_port.callback_pending.load(Ordering::Acquire)
        {
            return true;
        }
        let mut state = self.state.lock();
        if !state.pending.is_empty() || state.core.needs_rescan() {
            return true;
        }
        !matches!(state.core.prepare_delivery(), Ok(None))
    }

    fn prepare_deliveries(self: &Arc<Self>, maximum: usize) -> AxResult<EpollBatch> {
        self.drain_pending()?;
        self.recover_core()?;
        self.check_fault()?;

        let limit = maximum.min(EPOLL_CORE_CAPACITY);
        let mut deliveries = Vec::new();
        deliveries
            .try_reserve_exact(limit)
            .map_err(|_| AxError::NoMemory)?;
        while deliveries.len() < limit {
            let next = {
                let mut state = self.state.lock();
                let Some(ready) = state.core.begin_delivery().map_err(map_epoll_error)? else {
                    break;
                };
                let token = ready.delivery.interest();
                let Some(control) = state
                    .record(token)
                    .map(|record| Arc::clone(&record.control))
                else {
                    let _ = state
                        .core
                        .finish_delivery(ready.delivery, DeliveryOutcome::Fault);
                    drop(state);
                    self.set_fault(EPOLL_FAULT_CORE_INVARIANT);
                    return Err(AxError::BadState);
                };
                PreparedDelivery { ready, control }
            };
            if let Err(error) = next.control.ensure_armed() {
                let mut state = self.state.lock();
                let _ = state
                    .core
                    .finish_delivery(next.ready.delivery, DeliveryOutcome::Fault);
                drop(state);
                self.wake_port.poll_ready.wake();
                return Err(error);
            }
            deliveries.push(next);
        }
        if deliveries.is_empty() {
            Err(AxError::WouldBlock)
        } else {
            Ok(EpollBatch {
                inner: Arc::clone(self),
                deliveries: Some(deliveries),
            })
        }
    }

    fn finish_deliveries(&self, deliveries: Vec<PreparedDelivery>, copied_prefix: usize) {
        for (index, delivery) in deliveries.into_iter().enumerate() {
            let copied = index < copied_prefix;
            let outcome = delivery_outcome(index, copied_prefix, || {
                ready_from_io(delivery.control.file.poll())
            });
            let result = self
                .state
                .lock()
                .core
                .finish_delivery(delivery.ready.delivery, outcome);
            match result {
                Ok(()) | Err(EpollError::StaleToken) => {}
                Err(EpollError::ReadyQueueFull) => {
                    self.wake_port.poll_ready.wake();
                }
                Err(error) => {
                    error!("epoll delivery completion invariant failed: {error:?}");
                    self.set_fault(EPOLL_FAULT_CORE_INVARIANT);
                }
            }
            if copied {
                delivery.control.disable_after_one_shot();
            }
        }
        if self.has_ready() {
            self.wake_port.poll_ready.wake();
        }
    }
}

impl Drop for EpollInner {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        while let Some(slot) = state.by_slot.iter().position(Option::is_some) {
            let Some(record) = state.by_slot[slot].take() else {
                error!("epoll adapter slot disappeared during exclusive teardown");
                break;
            };
            state.by_key.remove(&record.key);
            state.pending.remove(record.token);
            let retired = state.core.remove(record.token).ok();
            let graph_result = graph_domain().and_then(|domain| {
                domain
                    .lock()
                    .remove_interest(record.edge)
                    .map_err(map_graph_error)
            });
            if let Err(error) = graph_result {
                error!("epoll graph edge cleanup failed: {error:?}");
            }
            record.control.deactivate();
            drop(record);
            drop(retired);
        }
        if let Err(error) = graph_domain()
            .and_then(|domain| domain.lock().unregister(self.node).map_err(map_graph_error))
        {
            error!("epoll graph node cleanup failed: {error:?}");
        }
    }
}

struct PreparedDelivery {
    ready: ReadyEvent<u64>,
    control: Arc<InterestControl>,
}

fn delivery_outcome(
    index: usize,
    copied_prefix: usize,
    still_ready: impl FnOnce() -> ReadyMask,
) -> DeliveryOutcome {
    if index < copied_prefix {
        DeliveryOutcome::Copied {
            still_ready: still_ready(),
        }
    } else {
        DeliveryOutcome::Fault
    }
}

pub struct EpollBatch {
    inner: Arc<EpollInner>,
    deliveries: Option<Vec<PreparedDelivery>>,
}

impl EpollBatch {
    pub fn len(&self) -> usize {
        self.deliveries.as_ref().map_or(0, Vec::len)
    }

    /// Returns one already-prepared event without changing delivery ownership.
    pub fn event(&self, index: usize) -> Option<EpollEvent> {
        self.deliveries
            .as_ref()?
            .get(index)
            .map(|delivery| EpollEvent {
                events: io_from_ready(delivery.ready.events),
                user_data: delivery.ready.user_data,
            })
    }

    /// Commits the copied prefix and faults/requeues every remaining delivery.
    pub fn complete_prefix(mut self, copied: usize) -> usize {
        let deliveries = self.deliveries.take().unwrap_or_default();
        let copied = copied.min(deliveries.len());
        self.inner.finish_deliveries(deliveries, copied);
        copied
    }
}

impl Drop for EpollBatch {
    fn drop(&mut self) {
        if let Some(deliveries) = self.deliveries.take() {
            self.inner.finish_deliveries(deliveries, 0);
        }
    }
}

pub struct Epoll {
    inner: Arc<EpollInner>,
}

impl Epoll {
    pub fn new() -> AxResult<Self> {
        Ok(Self {
            inner: EpollInner::try_new()?,
        })
    }

    fn target(fd: i32) -> AxResult<(EpollKey, Arc<FileDescription>)> {
        let number = FdNumber::from_i32(fd).ok_or(AxError::BadFileDescriptor)?;
        let file = get_file_description(fd)?;
        let key = EpollKey {
            ofd: file.id().linux_id(),
            fd: number,
        };
        Ok((key, file))
    }

    fn validate_target(file: &FileDescription) -> AxResult<()> {
        match FileLikeKind::from_file_like(file) {
            FileLikeKind::Regular | FileLikeKind::Directory => Err(LinuxError::EPERM.into()),
            FileLikeKind::Fifo | FileLikeKind::Socket | FileLikeKind::Other => Ok(()),
        }
    }

    fn child_node(file: &FileDescription) -> Option<GraphNodeToken> {
        file.inner
            .downcast_ref::<Epoll>()
            .map(|child| child.inner.node)
    }

    pub fn add(&self, fd: i32, event: EpollEvent, flags: EpollFlags) -> AxResult<()> {
        let (key, file) = Self::target(fd)?;
        Self::validate_target(&file)?;
        let interest = interest_from_io(event.events)?;
        let mode = InterestMode {
            edge: flags.contains(EpollFlags::EDGE_TRIGGER),
            one_shot: flags.contains(EpollFlags::ONESHOT),
            exclusive: false,
        };
        let control = InterestControl::try_new(
            &self.inner.wake_port,
            Arc::clone(&file),
            event.events,
            mode.one_shot,
        )?;
        let initial_ready = match control.check_arm_check() {
            Ok(ready) => ready,
            Err(error) => {
                control.deactivate();
                return Err(error);
            }
        };

        let child = Self::child_node(&file);
        let domain = graph_domain()?;
        let mut graph = domain.lock();
        let mut state = self.inner.state.lock();
        if state.by_key.contains_key(&key) {
            drop(state);
            drop(graph);
            control.deactivate();
            return Err(AxError::AlreadyExists);
        }
        let edge = match graph.add_interest(self.inner.node, child) {
            Ok(edge) => edge,
            Err(error) => {
                drop(state);
                drop(graph);
                control.deactivate();
                return Err(map_graph_error(error));
            }
        };
        let published =
            LinuxEpollInterest::new(key, interest, mode, event.user_data, control.clone());
        let token = match state.core.add(published) {
            Ok(token) => token,
            Err(error) => {
                let core_error = error.error;
                let (_, _, _, _, returned) = error.interest.into_parts();
                let rollback = graph.remove_interest(edge);
                drop(state);
                drop(graph);
                control.deactivate();
                drop(returned);
                if let Err(error) = rollback {
                    error!("epoll ADD graph rollback failed: {error:?}");
                    self.inner.set_fault(EPOLL_FAULT_CORE_INVARIANT);
                }
                return Err(map_epoll_error(core_error));
            }
        };
        if state.by_slot.get(token.slot()).is_none_or(Option::is_some) {
            let retired = state.core.remove(token).ok();
            let rollback = graph.remove_interest(edge);
            drop(state);
            drop(graph);
            control.deactivate();
            drop(retired);
            if rollback.is_err() {
                self.inner.set_fault(EPOLL_FAULT_CORE_INVARIANT);
            }
            return Err(AxError::BadState);
        }
        state.by_key.insert(key, token);
        state.by_slot[token.slot()] = Some(InterestRecord {
            key,
            token,
            edge,
            control: control.clone(),
        });
        let publication = control.publish_token();
        drop(state);
        drop(graph);

        if publication.target_closed {
            self.inner.remove_closed_target(token, &control);
            return Ok(());
        }
        if publication.source_woke {
            control.registration_fired();
            self.inner.enqueue_pending(token, &control);
        }
        self.inner.notify_ready(token, initial_ready)?;
        Ok(())
    }

    pub fn modify(&self, fd: i32, event: EpollEvent, flags: EpollFlags) -> AxResult<()> {
        let (key, file) = Self::target(fd)?;
        let interest = interest_from_io(event.events)?;
        let mode = InterestMode {
            edge: flags.contains(EpollFlags::EDGE_TRIGGER),
            one_shot: flags.contains(EpollFlags::ONESHOT),
            exclusive: false,
        };
        let control =
            InterestControl::try_new(&self.inner.wake_port, file, event.events, mode.one_shot)?;
        let initial_ready = match control.check_arm_check() {
            Ok(ready) => ready,
            Err(error) => {
                control.deactivate();
                return Err(error);
            }
        };

        let domain = graph_domain()?;
        let graph = domain.lock();
        let mut state = self.inner.state.lock();
        let Some(old_token) = state.by_key.get(&key).copied() else {
            drop(state);
            drop(graph);
            control.deactivate();
            return Err(AxError::NotFound);
        };
        let replacement =
            LinuxEpollInterest::new(key, interest, mode, event.user_data, control.clone());
        let (token, retired) = match state.core.modify(old_token, replacement) {
            Ok(result) => result,
            Err(error) => {
                let core_error = error.error;
                let (_, _, _, _, returned) = error.interest.into_parts();
                drop(state);
                drop(graph);
                control.deactivate();
                drop(returned);
                return Err(map_epoll_error(core_error));
            }
        };
        state.pending.remove(old_token);
        let Some(old_record) = state.by_slot[old_token.slot()].take() else {
            drop(state);
            drop(graph);
            control.deactivate();
            drop(retired);
            self.inner.set_fault(EPOLL_FAULT_CORE_INVARIANT);
            return Err(AxError::BadState);
        };
        state.by_key.insert(key, token);
        state.by_slot[token.slot()] = Some(InterestRecord {
            key,
            token,
            edge: old_record.edge,
            control: control.clone(),
        });
        let publication = control.publish_token();
        drop(state);
        drop(graph);

        old_record.control.deactivate();
        let (_, _, _, _, retired_control) = retired.into_parts();
        drop(retired_control);
        drop(old_record);
        if publication.target_closed {
            self.inner.remove_closed_target(token, &control);
            return Ok(());
        }
        if publication.source_woke {
            control.registration_fired();
            self.inner.enqueue_pending(token, &control);
        }
        self.inner.notify_ready(token, initial_ready)?;
        Ok(())
    }

    pub fn delete(&self, fd: i32) -> AxResult<()> {
        let (key, _) = Self::target(fd)?;
        let domain = graph_domain()?;
        let mut graph = domain.lock();
        let mut state = self.inner.state.lock();
        let token = state.by_key.get(&key).copied().ok_or(AxError::NotFound)?;
        let retired = state.core.remove(token).map_err(map_epoll_error)?;
        let Some(record) = state.by_slot[token.slot()].take() else {
            drop(state);
            drop(graph);
            self.inner.set_fault(EPOLL_FAULT_CORE_INVARIANT);
            drop(retired);
            return Err(AxError::BadState);
        };
        state.by_key.remove(&key);
        state.pending.remove(token);
        let graph_result = graph.remove_interest(record.edge);
        drop(state);
        drop(graph);

        record.control.deactivate();
        let (_, _, _, _, retired_control) = retired.into_parts();
        drop(retired_control);
        drop(record);
        graph_result.map_err(map_graph_error)
    }

    pub fn prepare_events(&self, maximum: usize) -> AxResult<EpollBatch> {
        self.inner.prepare_deliveries(maximum)
    }
}

impl FileLike for Epoll {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(super::anon_inode_stat())
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[eventpoll]".into())
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        Ok(())
    }
}

impl Pollable for Epoll {
    fn poll(&self) -> IoEvents {
        if self.inner.has_ready() {
            IoEvents::READABLE
        } else {
            IoEvents::empty()
        }
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if events.contains(IoEvents::READABLE) {
            PollRegistration::single(&self.inner.wake_port.poll_ready, context.waker())
        } else {
            PollRegistration::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{borrow::Cow, sync::Arc, task::Wake};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Waker},
    };

    use super::*;

    struct CountingWake(AtomicUsize);

    struct CloseTestFile {
        source: PollSet<1>,
    }

    impl CloseTestFile {
        fn new() -> Self {
            Self {
                source: PollSet::new(),
            }
        }
    }

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Pollable for CloseTestFile {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<PollRegistration<'a>, PollRegistrationError> {
            PollRegistration::single(&self.source, context.waker())
        }
    }

    impl FileLike for CloseTestFile {
        fn stat(&self) -> AxResult<Kstat> {
            Err(AxError::InvalidInput)
        }

        fn path(&self) -> AxResult<Cow<'_, str>> {
            Ok(Cow::Borrowed("epoll-close-test"))
        }

        fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
            Ok(())
        }
    }

    fn token(core: &mut EpollCore<u64, ()>, fd: u32) -> EpollToken {
        core.add(LinuxEpollInterest::new(
            EpollKey {
                ofd: thekernel_linux_fd::OfdId::new(fd as u64 + 1).unwrap(),
                fd: FdNumber::new(fd),
            },
            InterestMask::IN,
            InterestMode::default(),
            0,
            (),
        ))
        .unwrap()
    }

    #[test]
    fn pending_queue_is_exactly_bounded_and_removable() {
        let id = EpollId::new(1).unwrap();
        let mut core = EpollCore::<u64, ()>::try_new(id, 3).unwrap();
        let first = token(&mut core, 0);
        let second = token(&mut core, 1);
        let third = token(&mut core, 2);
        let mut queue = PendingQueue::try_new(2).unwrap();
        assert!(queue.push(first).is_ok());
        assert!(queue.push(second).is_ok());
        assert!(queue.push(third).is_err());
        queue.remove(first);
        assert_eq!(queue.pop(), Some(second));
        assert!(queue.is_empty());
    }

    #[test]
    fn copied_prefix_faults_and_requeues_only_suffix() {
        let id = EpollId::new(2).unwrap();
        let mut core = EpollCore::<u64, ()>::try_new(id, 2).unwrap();
        let first = token(&mut core, 0);
        let second = token(&mut core, 1);
        core.notify(first, ReadyMask::IN).unwrap();
        core.notify(second, ReadyMask::IN).unwrap();

        let first_delivery = core.begin_delivery().unwrap().unwrap();
        let second_delivery = core.begin_delivery().unwrap().unwrap();
        assert_eq!(first_delivery.delivery.interest(), first);
        assert_eq!(second_delivery.delivery.interest(), second);

        for (index, delivery) in [first_delivery, second_delivery].into_iter().enumerate() {
            core.finish_delivery(
                delivery.delivery,
                delivery_outcome(index, 1, || ReadyMask::EMPTY),
            )
            .unwrap();
        }

        let retried = core.begin_delivery().unwrap().unwrap();
        assert_eq!(retried.delivery.interest(), second);
        core.finish_delivery(
            retried.delivery,
            DeliveryOutcome::Copied {
                still_ready: ReadyMask::EMPTY,
            },
        )
        .unwrap();
        assert!(core.begin_delivery().unwrap().is_none());
    }

    #[test]
    fn io_masks_do_not_depend_on_generic_bit_layout() {
        let io = IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::HANGUP;
        let interest = interest_from_io(io).unwrap();
        assert_eq!(
            interest.bits(),
            InterestMask::IN.bits() | InterestMask::OUT.bits()
        );
        let ready = ready_from_io(io);
        assert_eq!(io_from_ready(ready), io);
    }

    #[test]
    fn ready_waiters_retain_each_owned_registration() {
        let waiters = PollSet::<EPOLL_WAITER_SLOTS>::new();
        let counter = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let _first = waiters.register(&waker).unwrap();
        let _second = waiters.register(&waker).unwrap();
        assert_eq!(waiters.wake(), 2);
        assert_eq!(counter.0.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn per_instance_core_storage_has_a_global_charge() {
        let before = EPOLL_CORE_SLOTS.load(Ordering::Acquire);
        let charge = EpollCoreCharge::try_new(7).unwrap();
        assert_eq!(EPOLL_CORE_SLOTS.load(Ordering::Acquire), before + 7);
        drop(charge);
        assert_eq!(EPOLL_CORE_SLOTS.load(Ordering::Acquire), before);
    }

    #[test]
    fn callbacks_publish_hints_for_task_context_source_and_close_work() {
        let epoll = Epoll::new().unwrap();
        let test_file = Arc::new(CloseTestFile::new());
        let file = FileDescription::new(test_file.clone()).unwrap();
        file.begin_descriptor_publication().unwrap().commit();
        let key = EpollKey {
            ofd: file.id().linux_id(),
            fd: FdNumber::new(9),
        };
        let mode = InterestMode::default();
        let control = InterestControl::try_new(
            &epoll.inner.wake_port,
            file.clone(),
            IoEvents::READABLE,
            false,
        )
        .unwrap();
        assert!(control.check_arm_check().unwrap().is_empty());

        let domain = graph_domain().unwrap();
        let mut graph = domain.lock();
        let mut state = epoll.inner.state.lock();
        let edge = graph.add_interest(epoll.inner.node, None).unwrap();
        let token = state
            .core
            .add(LinuxEpollInterest::new(
                key,
                InterestMask::IN,
                mode,
                0,
                control.clone(),
            ))
            .unwrap();
        state.by_key.insert(key, token);
        state.by_slot[token.slot()] = Some(InterestRecord {
            key,
            token,
            edge,
            control: control.clone(),
        });
        let publication = control.publish_token();
        assert!(!publication.source_woke);
        assert!(!publication.target_closed);
        drop(state);
        drop(graph);

        let state = epoll.inner.state.lock();
        assert_eq!(test_file.source.wake(), 1);
        assert!(state.record(token).is_some());
        assert!(
            epoll
                .inner
                .wake_port
                .callback_pending
                .load(Ordering::Acquire)
        );
        drop(state);

        epoll.inner.drain_callback_hints();
        let state = epoll.inner.state.lock();
        assert!(state.record(token).is_some());
        assert!(!state.pending.is_empty());
        assert!(control.pending.load(Ordering::Acquire));
        drop(state);

        let state = epoll.inner.state.lock();
        file.descriptor_closed();
        assert!(state.record(token).is_some());
        assert!(
            epoll
                .inner
                .wake_port
                .callback_pending
                .load(Ordering::Acquire)
        );
        drop(state);

        epoll.inner.drain_callback_hints();

        let state = epoll.inner.state.lock();
        assert!(state.record(token).is_none());
        assert!(!state.by_key.contains_key(&key));
        drop(state);
        assert!(!control.is_source_enabled());
    }
}
