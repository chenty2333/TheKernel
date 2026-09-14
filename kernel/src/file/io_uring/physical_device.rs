//! Device registry, generation fences, and DMA admission guards.

use super::*;

/// The lower handle/cookie namespace is device-local. Keep the exact shared
/// queue identity alongside every upper route instead of trying to infer it
/// from a raw completion handle. This fixed table is deliberately small: the
/// admission path may scan it, while completion routing remains a direct
/// identity carried by the route group.
pub(super) const PHYSICAL_COMPLETION_MAX_DEVICES: usize = 8;
/// Completion routing is shared only by rings targeting the *same* lower
/// device. A failed device may retain every one of its QD owners until a
/// typed reset proof arrives, so one QD-sized slab for all devices would let
/// that failure deny direct-DMA admission to healthy siblings. Production
/// therefore has one bounded QD namespace per installed-device slot. Tests
/// exercise the same capacity contract: a test-only smaller table would hide
/// the exact sibling-isolation property this router exists to preserve.
pub(super) const PHYSICAL_COMPLETION_ROUTER_CAPACITY: usize =
    PHYSICAL_COMPLETION_MAX_DEVICES * IO_URING_PHYSICAL_MAX_QD;

pub(super) type PhysicalCompletionDeviceIdentities = [usize; PHYSICAL_COMPLETION_MAX_DEVICES];

pub(super) fn physical_completion_device_identities() -> (PhysicalCompletionDeviceIdentities, usize)
{
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    let mut identities = [0; PHYSICAL_COMPLETION_MAX_DEVICES];
    let mut len = 0;
    for slot in registry.slots.iter().flatten() {
        if len == identities.len() {
            break;
        }
        identities[len] = slot.identity;
        len += 1;
    }
    (identities, len)
}

pub(super) struct PhysicalCompletionDeviceSlot {
    pub(super) identity: usize,
    /// Monotonic process-lifetime callback context for this slot.  Lower
    /// callbacks may already have loaded an old context when unregister
    /// starts; a replacement slot therefore must never reuse the old token,
    /// even if allocator address reuse makes `identity` equal.
    pub(super) callback_context: usize,
    // Production slots always carry `Some`.  A test-only registered-slot
    // fixture may omit the lower handle after the callback has already
    // supplied its terminal event; it can exercise upper acknowledgement
    // without fabricating a concrete AxBlockDevice transport.
    pub(super) device: Option<SharedBlockDevice>,
    pub(super) generation: u64,
    pub(super) configured: bool,
    pub(super) active: bool,
    /// Admission is fenced independently from lower transport activity.  A
    /// removal/reset may close this bit while an already published owner is
    /// still drained by the completion worker.
    pub(super) admission_open: bool,
    /// Removal is a per-device intent.  Reset retirement must not reopen a
    /// slot that a concurrent unregister has already fenced.
    pub(super) removal_pending: bool,
    pub(super) reset_pending: bool,
    pub(super) in_flight: usize,
    /// Progress notifications are accounted per exact lower device.  The
    /// generation is the lower progress generation (not the transport
    /// generation stored above); a worker may clear this marker only after it
    /// has observed the same generation again.
    pub(super) progress_pending: bool,
    pub(super) progress_generation: u64,
    /// A progress-sequence overflow is terminal for this slot.  Never wrap
    /// the sequence: once the bounded marker can no longer be advanced, keep
    /// the device fenced in reset custody until the slot is removed.
    pub(super) progress_overflowed: bool,
    /// Terminal sequence exhaustion is a separate stable fence.  It cannot
    /// be represented by the progress sequence because a terminal proof must
    /// retain its own compare/consume identity.
    pub(super) terminal_sequence_overflowed: bool,
    /// Terminal notifications belong to this device's transport generation.
    /// Keeping the mailbox in the registry prevents a vda reset from
    /// consuming (or reopening) vdb's generation.
    pub(super) terminal_state: u8,
    pub(super) terminal_generation: u64,
    pub(super) terminal_sequence: u64,
    pub(super) terminal_consumed_sequence: u64,
}

pub(super) struct PhysicalCompletionDeviceRegistry {
    pub(super) slots: [Option<PhysicalCompletionDeviceSlot>; PHYSICAL_COMPLETION_MAX_DEVICES],
}

impl PhysicalCompletionDeviceRegistry {
    pub(super) const fn new() -> Self {
        Self {
            slots: [const { None }; PHYSICAL_COMPLETION_MAX_DEVICES],
        }
    }
}

pub(super) static PHYSICAL_COMPLETION_DEVICE_REGISTRY: SpinNoIrq<PhysicalCompletionDeviceRegistry> =
    SpinNoIrq::new(PhysicalCompletionDeviceRegistry::new());
/// Callback contexts are process-lifetime incarnation tokens rather than raw
/// device identities.  Never reuse one: an IRQ that loaded an old context
/// after unregister must be rejected by a newly installed slot.
pub(super) static PHYSICAL_COMPLETION_CALLBACK_CONTEXT_NEXT: AtomicUsize = AtomicUsize::new(1);
pub(super) static PHYSICAL_COMPLETION_DEFAULT_IDENTITY: AtomicUsize = AtomicUsize::new(0);
pub(super) static PHYSICAL_COMPLETION_DEVICE_ACTIVE: AtomicBool = AtomicBool::new(false);
pub(super) static PHYSICAL_COMPLETION_DEVICE_GENERATION: AtomicU64 = AtomicU64::new(0);
pub(super) static PHYSICAL_COMPLETION_WORKER_STOPPED: AtomicBool = AtomicBool::new(false);
pub(super) static PHYSICAL_COMPLETION_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
pub(super) static PHYSICAL_COMPLETION_WORKER_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Fixed-slot round-robin cursor for one bounded completion pass. Each
/// activation still visits every registered device at most once, but a busy
/// first slot cannot permanently receive the first lower-drain opportunity.
pub(super) static PHYSICAL_COMPLETION_DEVICE_CURSOR: AtomicUsize = AtomicUsize::new(0);
/// A wake is a bounded generation hand-off to the dedicated completion task.
/// It is intentionally separate from the generic deferred-work list: the
/// completion task may block in the lower wait, while policy/fanotify work
/// must continue on its own worker.
pub(super) static PHYSICAL_COMPLETION_WORK_PENDING: AtomicBool = AtomicBool::new(false);
/// A reset request is kept live until the lower reset proves quiescence.  In
/// particular, a `ResourceBusy` admission result must not look like a stopped
/// worker: the submitter that owns the admission guard may still commit its
/// route after the first reset attempt returns.
pub(super) static PHYSICAL_COMPLETION_RESET_PENDING: AtomicBool = AtomicBool::new(false);
/// Lower reset/retirement notifications are delivered by the shared-device
/// terminal callback.  The callback only publishes this bounded marker and
/// wakes the upper worker; it never drains or retires an upper owner itself.
pub(super) const PHYSICAL_COMPLETION_TERMINAL_NONE: u8 = 0;
pub(super) const PHYSICAL_COMPLETION_TERMINAL_QUIESCED: u8 = 1;
pub(super) const PHYSICAL_COMPLETION_TERMINAL_RETIRED: u8 = 2;
pub(super) const PHYSICAL_COMPLETION_TERMINAL_QUARANTINED: u8 = 3;
pub(super) static PHYSICAL_COMPLETION_TERMINAL_STATE: AtomicU8 =
    AtomicU8::new(PHYSICAL_COMPLETION_TERMINAL_NONE);
pub(super) static PHYSICAL_COMPLETION_TERMINAL_GENERATION: AtomicU64 = AtomicU64::new(0);
/// Monotonic mailbox sequence for lower terminal notifications.  The state
/// and generation fields are read/written under the short pair lock below;
/// the sequence lets the upper worker prove that the event it retired is
/// still the current event before clearing custody or reopening admission.
pub(super) static PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub(super) static PHYSICAL_COMPLETION_TERMINAL_CONSUMED_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub(super) static PHYSICAL_COMPLETION_TERMINAL_SEQUENCE_OVERFLOWED: AtomicBool =
    AtomicBool::new(false);
pub(super) static PHYSICAL_COMPLETION_TERMINAL_EVENT_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());
/// The notifier context is process-lifetime storage, so uninstalling the
/// callback never depends on a ring or a mount remaining alive.
pub(super) static PHYSICAL_COMPLETION_TERMINAL_CONTEXT: u8 = 0;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PhysicalCompletionProgressState {
    pub(super) pending: bool,
    pub(super) generation: u64,
    pub(super) overflowed: bool,
}

/// Advances one exact device's durable progress marker. Keeping this small
/// state transition separate makes the checked (non-wrapping) arithmetic and
/// snapshot/clear protocol directly testable without constructing a hardware
/// block device in host unit tests.
pub(super) fn advance_physical_completion_progress(
    progress: &mut PhysicalCompletionProgressState,
) -> AxResult<()> {
    progress.pending = true;
    if progress.overflowed {
        // Overflow is a stable fence, not a transient arithmetic error.  A
        // later callback must not keep publishing wakes or retrying a reset
        // merely because the marker can no longer advance.
        return Err(AxError::BadState);
    }
    match progress.generation.checked_add(1) {
        Some(next) => {
            progress.generation = next;
            Ok(())
        }
        None => {
            progress.overflowed = true;
            Err(AxError::BadState)
        }
    }
}

pub(super) fn allocate_physical_completion_callback_context() -> AxResult<usize> {
    let mut current = PHYSICAL_COMPLETION_CALLBACK_CONTEXT_NEXT.load(Ordering::Acquire);
    loop {
        let next = current.checked_add(1).ok_or(AxError::BadState)?;
        match PHYSICAL_COMPLETION_CALLBACK_CONTEXT_NEXT.compare_exchange(
            current,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
}

pub(super) fn advance_physical_completion_terminal_sequence(sequence: u64) -> AxResult<u64> {
    sequence.checked_add(1).ok_or(AxError::BadState)
}

pub(super) fn clear_physical_completion_progress_if_unchanged(
    progress: &mut PhysicalCompletionProgressState,
    observed_generation: Option<u64>,
) {
    if observed_generation == Some(progress.generation) && !progress.overflowed {
        progress.pending = false;
    }
}

/// Serializes the short lifetime hand-off between submitter admission and
/// device teardown.  A route reservation is not enough by itself: it is
/// possible to reserve a ring slot, then lose the worker/device before the
/// route reservation is installed.  Production admission therefore holds a
/// counted guard from the ring-slot reservation through route commit; stop
/// must reject while that guard is live.
pub(super) struct PhysicalCompletionAdmissionState {
    /// `configured` distinguishes the production device owner from unit-test
    /// ring reservations made before any device is installed.  The latter
    /// retain their local capacity semantics without opening production I/O.
    pub(super) configured: bool,
    pub(super) open: bool,
    pub(super) in_flight: usize,
    /// A worker-stop notification observed while a submitter owns the
    /// publication/commit fence.  The generation change is deferred until
    /// the last guard drops, so a published effect cannot be stamped stale
    /// between lower publication and route custody.
    pub(super) generation_bump_pending: bool,
    /// Serializes the fallible lower broker installation without holding this
    /// IRQ-disabling lock.  Teardown and a second installer wait for the
    /// final slot publication/rollback instead of observing a half-installed
    /// owner.
    pub(super) install_in_progress: bool,
}

pub(super) static PHYSICAL_COMPLETION_ADMISSION_STATE: SpinNoIrq<PhysicalCompletionAdmissionState> =
    SpinNoIrq::new(PhysicalCompletionAdmissionState {
        configured: false,
        open: false,
        in_flight: 0,
        generation_bump_pending: false,
        install_in_progress: false,
    });

#[inline]
pub(super) fn physical_completion_default_identity() -> usize {
    PHYSICAL_COMPLETION_DEFAULT_IDENTITY.load(Ordering::Acquire)
}

pub(super) fn physical_completion_device_slot(identity: usize) -> Option<usize> {
    PHYSICAL_COMPLETION_DEVICE_REGISTRY
        .lock()
        .slots
        .iter()
        .position(|slot| slot.as_ref().is_some_and(|slot| slot.identity == identity))
}

pub(super) fn physical_completion_device_for(identity: usize) -> Option<SharedBlockDevice> {
    PHYSICAL_COMPLETION_DEVICE_REGISTRY
        .lock()
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == identity)
        .and_then(|slot| slot.device.clone())
}

pub(super) fn physical_completion_generation_for(identity: usize) -> Option<u64> {
    PHYSICAL_COMPLETION_DEVICE_REGISTRY
        .lock()
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == identity)
        .map(|slot| slot.generation)
        .or_else(|| {
            // Only zero is the synthetic identity used by allocation-free
            // unit tests. A non-zero production identity remains authorized
            // exclusively by its live registry slot; falling back to the
            // root generation would alias a torn-down vda with stale routes.
            (identity == 0).then(|| PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire))
        })
}

pub(super) fn physical_completion_progress_notifier(context: usize) {
    // The context is a process-lifetime slot incarnation token, not a raw
    // device address.  Account the progress edge before waking the owner;
    // the registry lock makes worker clear/recheck and this callback one
    // protocol, so a lower IRQ cannot disappear in the PollSet arm window.
    let _ = mark_physical_completion_device_progress_from_callback(context);
}

/// Publishes one durable progress edge for an exact lower device.
///
/// The transport generation identifies reset/reinitialization and normally
/// remains zero for the lifetime of a live queue.  It is therefore not a
/// usable wake sequence.  This slot-local sequence advances for every lower
/// callback and every upper publication edge, and is compared under the same
/// registry lock by the worker before it clears `progress_pending`.
///
/// A sequence overflow is impossible in normal operation but must not wrap:
/// doing so could make an old worker snapshot equal a new edge.  Instead the
/// exact device is fenced into reset custody and the marker remains pending.
pub(super) fn mark_physical_completion_device_progress(device_identity: usize) -> AxResult<()> {
    mark_physical_completion_device_progress_matching(|slot| slot.identity == device_identity)
}

pub(super) fn mark_physical_completion_device_progress_from_callback(
    context: usize,
) -> AxResult<()> {
    mark_physical_completion_device_progress_matching(|slot| slot.callback_context == context)
}

pub(super) fn mark_physical_completion_device_progress_matching(
    matches: impl Fn(&PhysicalCompletionDeviceSlot) -> bool,
) -> AxResult<()> {
    let (identity, result, first_overflow) = {
        let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        let Some(slot) = registry
            .slots
            .iter_mut()
            .flatten()
            .find(|slot| matches(slot))
        else {
            // A late callback for an unregistered/incarnation-mismatched
            // device must not wake or mutate a sibling slot that happens to
            // reuse the same raw identity.
            return Err(AxError::BadState);
        };

        let was_overflowed = slot.progress_overflowed;
        let mut progress = PhysicalCompletionProgressState {
            pending: slot.progress_pending,
            generation: slot.progress_generation,
            overflowed: slot.progress_overflowed,
        };
        let result = advance_physical_completion_progress(&mut progress);
        slot.progress_pending = progress.pending;
        slot.progress_generation = progress.generation;
        slot.progress_overflowed = progress.overflowed;
        if result.is_err() && !was_overflowed {
            slot.active = false;
            slot.admission_open = false;
            slot.reset_pending = true;
        }
        (
            slot.identity,
            result,
            !was_overflowed && progress.overflowed,
        )
    };

    if first_overflow {
        // Only the first overflow is a liveness edge. Once fenced, repeated
        // callbacks are rejected without rearming reset/wake state.
        if identity == physical_completion_default_identity() {
            PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
            PHYSICAL_COMPLETION_RESET_PENDING.store(true, Ordering::Release);
        }
        // The registry lock is deliberately released before waking.  IRQ
        // callers never retain it across task notification.
        wake_physical_completion_worker();
        return Err(AxError::BadState);
    }
    if result.is_err() {
        return Err(AxError::BadState);
    }
    // The registry lock is deliberately released before waking.  IRQ callers
    // never retain it across task notification, and the release/acquire pair
    // makes the marker visible before the worker runs.
    wake_physical_completion_worker();
    Ok(())
}

pub(super) fn register_physical_completion_device_slot(
    device: SharedBlockDevice,
    generation: u64,
    _active: bool,
) -> AxResult<usize> {
    let identity = device.identity_token();
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    if registry
        .slots
        .iter()
        .flatten()
        .any(|slot| slot.identity == identity)
    {
        // A live (or removal-fenced) slot owns this exact lower identity.
        // Never reset its progress/terminal sequence underneath published
        // routes; the caller must wait for exact removal before reinstalling.
        return Err(AxError::AlreadyExists);
    }
    let callback_context = allocate_physical_completion_callback_context()?;
    let Some(slot) = registry.slots.iter_mut().find(|slot| slot.is_none()) else {
        return Err(AxError::ResourceBusy);
    };
    *slot = Some(PhysicalCompletionDeviceSlot {
        identity,
        callback_context,
        device: Some(device),
        generation,
        configured: false,
        active: false,
        admission_open: false,
        removal_pending: false,
        reset_pending: false,
        in_flight: 0,
        progress_pending: false,
        progress_generation: 0,
        progress_overflowed: false,
        terminal_sequence_overflowed: false,
        terminal_state: PHYSICAL_COMPLETION_TERMINAL_NONE,
        terminal_generation: generation,
        terminal_sequence: 0,
        terminal_consumed_sequence: 0,
    });
    Ok(callback_context)
}

pub(super) fn remove_physical_completion_device_slot(
    identity: usize,
    callback_context: usize,
) -> Option<SharedBlockDevice> {
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    let index = registry.slots.iter().position(|slot| {
        slot.as_ref().is_some_and(|slot| {
            slot.identity == identity && slot.callback_context == callback_context
        })
    })?;
    registry.slots[index].take().and_then(|slot| slot.device)
}

pub(super) fn publish_physical_completion_device_slot(
    identity: usize,
    active: bool,
    admission_open: bool,
) -> AxResult<bool> {
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    let slot = registry
        .slots
        .iter_mut()
        .flatten()
        .find(|slot| slot.identity == identity)
        .ok_or(AxError::BadState)?;
    let publishable = slot.terminal_state == PHYSICAL_COMPLETION_TERMINAL_NONE
        && slot.terminal_sequence == slot.terminal_consumed_sequence
        && !slot.reset_pending
        && !slot.progress_overflowed
        && !slot.terminal_sequence_overflowed
        && !slot.removal_pending;
    slot.configured = true;
    slot.active = active && publishable;
    slot.admission_open = admission_open && publishable;
    slot.removal_pending = false;
    Ok(slot.active)
}

pub(super) fn physical_completion_terminal_context() -> usize {
    (&PHYSICAL_COMPLETION_TERMINAL_CONTEXT as *const u8) as usize
}

pub(super) fn physical_completion_terminal_code(
    availability: BlockCompletionAvailability,
) -> (u8, u64) {
    match availability {
        BlockCompletionAvailability::Live { generation } => {
            (PHYSICAL_COMPLETION_TERMINAL_QUIESCED, generation)
        }
        BlockCompletionAvailability::Retired { generation } => {
            (PHYSICAL_COMPLETION_TERMINAL_RETIRED, generation)
        }
        BlockCompletionAvailability::Quarantined { generation } => {
            (PHYSICAL_COMPLETION_TERMINAL_QUARANTINED, generation)
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct PhysicalCompletionTerminalEvent {
    pub(super) sequence: u64,
    pub(super) consumed_sequence: u64,
    pub(super) state: u8,
    pub(super) generation: u64,
}

pub(super) fn physical_completion_terminal_event() -> Option<PhysicalCompletionTerminalEvent> {
    let _event = PHYSICAL_COMPLETION_TERMINAL_EVENT_LOCK.lock();
    let sequence = PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE.load(Ordering::Acquire);
    let consumed_sequence = PHYSICAL_COMPLETION_TERMINAL_CONSUMED_SEQUENCE.load(Ordering::Acquire);
    let state = PHYSICAL_COMPLETION_TERMINAL_STATE.load(Ordering::Acquire);
    if state == PHYSICAL_COMPLETION_TERMINAL_NONE || sequence == consumed_sequence {
        return None;
    }
    Some(PhysicalCompletionTerminalEvent {
        sequence,
        consumed_sequence,
        state,
        generation: PHYSICAL_COMPLETION_TERMINAL_GENERATION.load(Ordering::Acquire),
    })
}

pub(super) fn clear_physical_completion_terminal_event() {
    let _event = PHYSICAL_COMPLETION_TERMINAL_EVENT_LOCK.lock();
    let sequence = PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE.load(Ordering::Acquire);
    PHYSICAL_COMPLETION_TERMINAL_CONSUMED_SEQUENCE.store(sequence, Ordering::Release);
    PHYSICAL_COMPLETION_TERMINAL_STATE.store(PHYSICAL_COMPLETION_TERMINAL_NONE, Ordering::Release);
}

/// Reads the terminal mailbox for one exact device. The registry lock is also
/// the publication fence for slot teardown, so a late lower callback cannot
/// be mistaken for a replacement device that happens to reuse its raw
/// handles.
pub(super) fn physical_completion_terminal_event_for_device(
    device_identity: usize,
) -> Option<PhysicalCompletionTerminalEvent> {
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    let slot = registry
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == device_identity)?;
    if slot.terminal_state == PHYSICAL_COMPLETION_TERMINAL_NONE
        || slot.terminal_sequence == slot.terminal_consumed_sequence
    {
        return None;
    }
    Some(PhysicalCompletionTerminalEvent {
        sequence: slot.terminal_sequence,
        consumed_sequence: slot.terminal_consumed_sequence,
        state: slot.terminal_state,
        generation: slot.terminal_generation,
    })
}

pub(super) fn clear_physical_completion_terminal_event_for_device(device_identity: usize) {
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    if let Some(slot) = registry
        .slots
        .iter_mut()
        .flatten()
        .find(|slot| slot.identity == device_identity)
    {
        slot.terminal_consumed_sequence = slot.terminal_sequence;
        slot.terminal_state = PHYSICAL_COMPLETION_TERMINAL_NONE;
    }
}

pub(super) fn physical_completion_terminal_event_for_device_reset(
    device_identity: usize,
    outcome: BlockResetOutcome,
    generation: u64,
) -> Option<PhysicalCompletionTerminalEvent> {
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    let slot = registry
        .slots
        .iter_mut()
        .flatten()
        .find(|slot| slot.identity == device_identity)?;
    if slot.terminal_state != PHYSICAL_COMPLETION_TERMINAL_NONE
        && slot.terminal_sequence != slot.terminal_consumed_sequence
    {
        return Some(PhysicalCompletionTerminalEvent {
            sequence: slot.terminal_sequence,
            consumed_sequence: slot.terminal_consumed_sequence,
            state: slot.terminal_state,
            generation: slot.terminal_generation,
        });
    }
    if slot.terminal_sequence_overflowed {
        return Some(PhysicalCompletionTerminalEvent {
            sequence: slot.terminal_sequence,
            consumed_sequence: slot.terminal_consumed_sequence,
            state: PHYSICAL_COMPLETION_TERMINAL_QUARANTINED,
            generation,
        });
    }
    let state = match outcome {
        BlockResetOutcome::Quiesced => PHYSICAL_COMPLETION_TERMINAL_QUIESCED,
        BlockResetOutcome::Retired => PHYSICAL_COMPLETION_TERMINAL_RETIRED,
        BlockResetOutcome::Quarantined => PHYSICAL_COMPLETION_TERMINAL_QUARANTINED,
    };
    Some(PhysicalCompletionTerminalEvent {
        sequence: slot.terminal_sequence,
        consumed_sequence: slot.terminal_consumed_sequence,
        state,
        generation,
    })
}

pub(super) fn physical_completion_terminal_event_for_reset(
    outcome: BlockResetOutcome,
    generation: u64,
) -> PhysicalCompletionTerminalEvent {
    let _event = PHYSICAL_COMPLETION_TERMINAL_EVENT_LOCK.lock();
    let sequence = PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE.load(Ordering::Acquire);
    let consumed_sequence = PHYSICAL_COMPLETION_TERMINAL_CONSUMED_SEQUENCE.load(Ordering::Acquire);
    let state = PHYSICAL_COMPLETION_TERMINAL_STATE.load(Ordering::Acquire);
    if state != PHYSICAL_COMPLETION_TERMINAL_NONE && sequence != consumed_sequence {
        return PhysicalCompletionTerminalEvent {
            sequence,
            consumed_sequence,
            state,
            generation: PHYSICAL_COMPLETION_TERMINAL_GENERATION.load(Ordering::Acquire),
        };
    }
    if PHYSICAL_COMPLETION_TERMINAL_SEQUENCE_OVERFLOWED.load(Ordering::Acquire) {
        return PhysicalCompletionTerminalEvent {
            sequence,
            consumed_sequence,
            state: PHYSICAL_COMPLETION_TERMINAL_QUARANTINED,
            generation,
        };
    }
    let state = match outcome {
        BlockResetOutcome::Quiesced => PHYSICAL_COMPLETION_TERMINAL_QUIESCED,
        BlockResetOutcome::Retired => PHYSICAL_COMPLETION_TERMINAL_RETIRED,
        BlockResetOutcome::Quarantined => PHYSICAL_COMPLETION_TERMINAL_QUARANTINED,
    };
    PhysicalCompletionTerminalEvent {
        sequence,
        consumed_sequence,
        state,
        generation,
    }
}

/// Receives a lower transport reset/retirement event without taking any
/// upper ownership lock.  The shared-device implementation invokes this
/// callback synchronously from its reset path, including when the reset was
/// initiated by this module; taking the admission lock here would deadlock
/// that path.  The upper worker later consumes the exact sequence snapshot
/// and performs route/work retirement after admitted submitters have left the
/// publication fence.
pub(super) fn physical_completion_terminal_notifier(
    context: usize,
    availability: BlockCompletionAvailability,
) {
    let (state, generation) = physical_completion_terminal_code(availability);
    // Production callbacks carry the exact slot incarnation token. Keep
    // the old process-lifetime mailbox as a test-only/legacy fallback when a
    // caller uses the historical terminal context.
    let device_context = {
        let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        if let Some(slot) = registry
            .slots
            .iter_mut()
            .flatten()
            .find(|slot| slot.callback_context == context)
        {
            slot.active = false;
            slot.admission_open = false;
            if slot.terminal_sequence_overflowed {
                // A terminal proof can no longer be represented without
                // risking sequence aliasing.  Keep this slot fenced and wait
                // for exact removal/reinstall; do not manufacture an event.
                Some((slot.identity, false))
            } else if let Ok(next) =
                advance_physical_completion_terminal_sequence(slot.terminal_sequence)
            {
                slot.terminal_generation = generation;
                slot.terminal_state = state;
                slot.terminal_sequence = next;
                slot.reset_pending = false;
                Some((slot.identity, true))
            } else {
                slot.terminal_sequence_overflowed = true;
                slot.reset_pending = false;
                Some((slot.identity, false))
            }
        } else {
            None
        }
    };
    if let Some((device_identity, published)) = device_context {
        // A terminal notification is also a lower progress edge.  Publish it
        // through the same exact-device sequence as an IRQ callback so a
        // worker snapshot cannot clear it merely because the transport
        // generation stayed unchanged.
        if published {
            let _ = mark_physical_completion_device_progress_from_callback(context);
        }
        if device_identity == physical_completion_default_identity() {
            PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
            if published {
                PHYSICAL_COMPLETION_RESET_PENDING.store(true, Ordering::Release);
            }
        }
        // A valid terminal edge is an actual liveness event even when the
        // progress marker is already fenced.  The wake occurs after releasing
        // the registry lock and is emitted once for this callback only.
        if published {
            wake_physical_completion_worker();
        }
        return;
    }
    if context != physical_completion_terminal_context() {
        return;
    }
    let terminal_overflow_wake = {
        let _event = PHYSICAL_COMPLETION_TERMINAL_EVENT_LOCK.lock();
        let sequence = PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE.load(Ordering::Acquire);
        match advance_physical_completion_terminal_sequence(sequence) {
            Ok(sequence) => {
                PHYSICAL_COMPLETION_TERMINAL_GENERATION.store(generation, Ordering::Release);
                PHYSICAL_COMPLETION_TERMINAL_STATE.store(state, Ordering::Release);
                PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE.store(sequence, Ordering::Release);
                false
            }
            Err(_) => {
                PHYSICAL_COMPLETION_TERMINAL_SEQUENCE_OVERFLOWED.store(true, Ordering::Release);
                PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
                PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
                let pending = PHYSICAL_COMPLETION_TERMINAL_STATE.load(Ordering::Acquire)
                    != PHYSICAL_COMPLETION_TERMINAL_NONE
                    && PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE.load(Ordering::Acquire)
                        != PHYSICAL_COMPLETION_TERMINAL_CONSUMED_SEQUENCE.load(Ordering::Acquire);
                PHYSICAL_COMPLETION_WORK_PENDING.store(pending, Ordering::Release);
                pending
            }
        }
    };
    if terminal_overflow_wake {
        // Preserve the already-published proof; only its existing event is
        // actionable once sequence space is exhausted.
        crate::deferred_work::wake_physical_completion_worker();
        return;
    }
    PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
    PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
    // Keep pending asserted while upper route/effect custody is unresolved;
    // `has_physical_completion_work` recognizes the terminal marker even
    // though the lower queue is no longer live.
    PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
    crate::deferred_work::wake_physical_completion_worker();
}

pub(super) fn physical_completion_terminal_outcome(state: u8) -> Option<BlockResetOutcome> {
    match state {
        PHYSICAL_COMPLETION_TERMINAL_QUIESCED => {
            Some(axdriver::prelude::BlockResetOutcome::Quiesced)
        }
        PHYSICAL_COMPLETION_TERMINAL_RETIRED => Some(axdriver::prelude::BlockResetOutcome::Retired),
        _ => None,
    }
}

/// A production physical admission guard.  It is deliberately held by the
/// fixed worker reservation, not by a temporary submitter local, so the
/// stop/generation gate covers the complete publish-to-route-commit window.
pub(super) struct PhysicalCompletionAdmissionGuard {
    pub(super) device_identity: usize,
    /// Whether this guard mirrored the legacy root admission counter when it
    /// was acquired. The registry slot can disappear during teardown, and
    /// the current default identity may already be zero by then; cleanup
    /// must still release the counter it actually charged.
    pub(super) mirrors_global: bool,
}

impl PhysicalCompletionAdmissionGuard {
    pub(super) fn begin() -> AxResult<Option<Self>> {
        Self::begin_for(physical_completion_default_identity())
    }

    pub(super) fn begin_for(device_identity: usize) -> AxResult<Option<Self>> {
        // Keep admission before registry in this path. Worker lifecycle
        // publication takes the same order, so a submitter cannot deadlock
        // with a stop/start transition while mirroring the root counters.
        let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
        let mirrors_global = device_identity == physical_completion_default_identity();
        if admission_state.install_in_progress {
            return Err(AxError::ResourceBusy);
        }
        let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        if let Some(slot) = registry
            .slots
            .iter_mut()
            .flatten()
            .find(|slot| slot.identity == device_identity)
        {
            if !slot.configured
                || !slot.active
                || !slot.admission_open
                || slot.reset_pending
                || slot.progress_overflowed
                || slot.terminal_sequence_overflowed
                || PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
                || !PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire)
            {
                return Err(AxError::BadState);
            }
            if mirrors_global {
                admission_state.in_flight = admission_state
                    .in_flight
                    .checked_add(1)
                    .ok_or(AxError::BadState)?;
            }
            slot.in_flight = match slot.in_flight.checked_add(1) {
                Some(in_flight) => in_flight,
                None => {
                    if mirrors_global {
                        admission_state.in_flight = admission_state.in_flight.saturating_sub(1);
                    }
                    return Err(AxError::BadState);
                }
            };
            drop(registry);
            drop(admission_state);
            return Ok(Some(Self {
                device_identity,
                mirrors_global,
            }));
        }
        drop(registry);
        // A production identity must never fall through to the legacy test
        // admission state after its exact registry slot has disappeared.
        // Production identities are registry-authorized only. The
        // zero-identity branch is retained solely for allocation-free unit
        // tests that model the old root gate without a real device slot.
        if device_identity != 0 {
            return Err(AxError::BadState);
        }
        if !admission_state.configured {
            // No installed production owner: direct ring capacity tests and
            // non-production callers may still exercise local reservations,
            // but they cannot publish a physical effect without readiness.
            return Ok(None);
        }
        if !admission_state.open
            || !PHYSICAL_COMPLETION_DEVICE_ACTIVE.load(Ordering::Acquire)
            || PHYSICAL_COMPLETION_TERMINAL_SEQUENCE_OVERFLOWED.load(Ordering::Acquire)
            || PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
            || !PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire)
        {
            return Err(AxError::BadState);
        }
        admission_state.in_flight = admission_state
            .in_flight
            .checked_add(1)
            .ok_or(AxError::BadState)?;
        drop(admission_state);
        Ok(Some(Self {
            device_identity,
            mirrors_global,
        }))
    }

    /// Check-and-call is kept under the same lifecycle lock used by stop and
    /// worker-failure notification.  This closes the last check-then-publish
    /// window: once publication starts, teardown cannot invalidate its owner
    /// before the reservation commits the route and worker item.
    pub(super) fn with_publish<T>(f: impl FnOnce() -> T) -> Option<T> {
        Self::with_publish_for(physical_completion_default_identity(), f)
    }

    pub(super) fn with_publish_for<T>(device_identity: usize, f: impl FnOnce() -> T) -> Option<T> {
        let registered_ready = {
            let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
            registry
                .slots
                .iter()
                .flatten()
                .find(|slot| slot.identity == device_identity)
                .map(|slot| {
                    slot.configured
                        && slot.active
                        && slot.admission_open
                        && !slot.reset_pending
                        && !slot.progress_overflowed
                        && !slot.terminal_sequence_overflowed
                        && !PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
                        && PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire)
                })
        };
        if let Some(ready) = registered_ready {
            // The counted admission guard prevents teardown/reset from
            // retiring this device while publication is in flight.  Do not
            // retain the IRQ-disabling registry lock across filesystem and
            // driver publication: that path may legitimately contend on a
            // sleeping mutex.  The lower queue's generation/availability
            // gate remains the final pre-descriptor publication check if a
            // terminal notification races this snapshot.
            return ready.then(f);
        }
        if device_identity != 0 {
            return None;
        }
        let (configured, ready) = {
            let state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            (
                state.configured,
                state.configured
                    && state.open
                    && PHYSICAL_COMPLETION_DEVICE_ACTIVE.load(Ordering::Acquire)
                    && !PHYSICAL_COMPLETION_TERMINAL_SEQUENCE_OVERFLOWED.load(Ordering::Acquire)
                    && !PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
                    && PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire),
            )
        };
        if ready {
            Some(f())
        } else if configured {
            None
        } else {
            // This branch is reachable only for test/non-production local
            // reservations.  The real physical path is gated by readiness,
            // but retaining the permissive behavior keeps the reservation
            // helper independently testable.
            Some(f())
        }
    }
}

impl Drop for PhysicalCompletionAdmissionGuard {
    fn drop(&mut self) {
        // Match begin_for's admission -> registry order. This is also the
        // order used by worker lifecycle transitions.
        let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
        let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        if registry
            .slots
            .iter()
            .flatten()
            .any(|slot| slot.identity == self.device_identity)
        {
            let mut wake_reset = false;
            if let Some(slot) = registry
                .slots
                .iter_mut()
                .flatten()
                .find(|slot| slot.identity == self.device_identity)
            {
                slot.in_flight = slot.in_flight.saturating_sub(1);
                wake_reset = slot.in_flight == 0 && (slot.reset_pending || slot.removal_pending);
            }
            if self.mirrors_global {
                admission_state.in_flight = admission_state.in_flight.saturating_sub(1);
                if admission_state.in_flight == 0 && admission_state.generation_bump_pending {
                    admission_state.generation_bump_pending = false;
                    PHYSICAL_COMPLETION_DEVICE_GENERATION.fetch_add(1, Ordering::AcqRel);
                }
            }
            drop(registry);
            drop(admission_state);
            if wake_reset {
                PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
                crate::deferred_work::wake_physical_completion_worker();
            }
            return;
        }
        drop(registry);
        admission_state.in_flight = admission_state.in_flight.saturating_sub(1);
        if admission_state.in_flight == 0 && admission_state.generation_bump_pending {
            admission_state.generation_bump_pending = false;
            PHYSICAL_COMPLETION_DEVICE_GENERATION.fetch_add(1, Ordering::AcqRel);
        }
        let wake_reset = admission_state.in_flight == 0
            && (PHYSICAL_COMPLETION_RESET_PENDING.load(Ordering::Acquire)
                || physical_completion_terminal_event().is_some());
        drop(admission_state);
        if wake_reset {
            PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
            crate::deferred_work::wake_physical_completion_worker();
        }
    }
}
