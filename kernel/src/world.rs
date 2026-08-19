//! The first local Semantic World authority.
//!
//! This is a deliberately small, kernel-internal authority.  It is not a
//! plugin loader, a resolver, or a wire format.  The UTS pilot is the first
//! real vertical slice: an authority generation entry owns the same execution
//! gate as its `UtsNamespace`, so a fence closes the path used by ordinary
//! hostname/domainname mutation as well as the authority path.

#[cfg(test)]
extern crate std;

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    num::NonZeroU64,
    sync::atomic::{AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult};
use axsync::spin::SpinNoIrq;
use spin::Once;

use crate::task::{UTS_FIELD_LEN, UserNamespace, UtsNamespace};

mod visa_uts;

const DEFAULT_WORLDS: usize = 8;
const DEFAULT_PROVIDERS: usize = 32;
const DEFAULT_BINDINGS: usize = 32;
const DEFAULT_TERMINAL_OPERATIONS: usize = 64;
const CLOSED_BIT: u64 = 1_u64 << 63;
const ACTIVE_MASK: u64 = !CLOSED_BIT;

static NEXT_AUTHORITY: AtomicU64 = AtomicU64::new(0);
static NEXT_WORLD: AtomicU64 = AtomicU64::new(0);
static NEXT_PROVIDER: AtomicU64 = AtomicU64::new(0);
static NEXT_EXECUTION_EPOCH: AtomicU64 = AtomicU64::new(0);
static NEXT_FENCE_EPOCH: AtomicU64 = AtomicU64::new(0);
static NEXT_BINDING: AtomicU64 = AtomicU64::new(0);
static NEXT_BINDING_EPOCH: AtomicU64 = AtomicU64::new(0);
static NEXT_OPERATION: AtomicU64 = AtomicU64::new(0);

static LOCAL_AUTHORITY: Once<Arc<Authority>> = Once::new();
static LOCAL_WORLD: Once<SemanticWorld> = Once::new();
static LOCAL_UTS_PROVIDER: Once<GenerationHandle> = Once::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuthorityInstanceId(NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorldId(NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderId(NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderGeneration(NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderCoordinate {
    world: WorldId,
    provider: ProviderId,
    generation: ProviderGeneration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BindingId(NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BindingEpoch(NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExecutionEpoch(NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FenceEpoch(NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuthorityOperationId(NonZeroU64);

fn next_id(counter: &AtomicU64) -> Option<NonZeroU64> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let next = current.checked_add(1)?;
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return NonZeroU64::new(next),
            Err(observed) => current = observed,
        }
    }
}

fn id<T>(counter: &AtomicU64, wrap: fn(NonZeroU64) -> T) -> AxResult<T> {
    next_id(counter).map(wrap).ok_or(AxError::OutOfRange)
}

/// A fixed-size logical snapshot of Linux UTS provider state.
///
/// This value contains no `Arc`, namespace owner, capability, pointer, or
/// authority handle.  It is intentionally not a serialization or wire ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UtsProviderSnapshot {
    schema_digest: [u8; 32],
    nodename: [u8; UTS_FIELD_LEN],
    nodename_len: u8,
    domainname: [u8; UTS_FIELD_LEN],
    domainname_len: u8,
}

impl UtsProviderSnapshot {
    const SCHEMA_DIGEST: [u8; 32] = *b"thekernel.uts.provider.v1.......";

    pub(crate) fn from_fields(nodename: &[u8], domainname: &[u8]) -> AxResult<Self> {
        if nodename.len() > UTS_FIELD_LEN || domainname.len() > UTS_FIELD_LEN {
            return Err(AxError::InvalidInput);
        }
        let mut snapshot = Self {
            schema_digest: Self::SCHEMA_DIGEST,
            nodename: [0; UTS_FIELD_LEN],
            nodename_len: nodename.len() as u8,
            domainname: [0; UTS_FIELD_LEN],
            domainname_len: domainname.len() as u8,
        };
        snapshot.nodename[..nodename.len()].copy_from_slice(nodename);
        snapshot.domainname[..domainname.len()].copy_from_slice(domainname);
        Ok(snapshot)
    }

    pub(crate) fn schema_digest(&self) -> [u8; 32] {
        self.schema_digest
    }

    pub(crate) fn nodename(&self) -> &[u8] {
        &self.nodename[..self.nodename_len as usize]
    }

    pub(crate) fn domainname(&self) -> &[u8] {
        &self.domainname[..self.domainname_len as usize]
    }
}

/// Explicit capacity for a local authority.  Provider, binding, and operation
/// rows are live-capacity slots, not boot-lifetime quotas.  A terminal
/// operation row is reclaimed once its binding has quiesced; callers retaining
/// an old operation id then observe `Absent` rather than a recycled operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuthorityCapacity {
    pub(crate) worlds: usize,
    pub(crate) providers: usize,
    pub(crate) bindings: usize,
    pub(crate) terminal_operations: usize,
}

impl Default for AuthorityCapacity {
    fn default() -> Self {
        Self {
            worlds: DEFAULT_WORLDS,
            providers: DEFAULT_PROVIDERS,
            bindings: DEFAULT_BINDINGS,
            terminal_operations: DEFAULT_TERMINAL_OPERATIONS,
        }
    }
}

/// A shared CAS gate used by both UtsNamespace and its authority generation.
/// The production wait path uses a check-arm-check WaitQueue; host tests use a
/// Condvar because they do not initialize the kernel scheduler. A source gate
/// that completed activation is permanently retired, so a retained UTS Arc
/// cannot be reopened as a new logical provider.
pub(crate) struct ExecutionGate {
    state: AtomicU64,
    fence_owner: AtomicU64,
    retired: core::sync::atomic::AtomicBool,
    epoch: ExecutionEpoch,
    /// Serializes the owner transition with the CLOSED-bit transition.  The
    /// active-lease counter remains lock-free, but every operation that can
    /// reopen or close a gate takes this short protocol lock so an activation
    /// cannot observe an unowned gate and race a new fence owner.
    protocol: SpinNoIrq<()>,
    waiters: axtask::WaitQueue,
    #[cfg(test)]
    host_wait: (std::sync::Mutex<()>, std::sync::Condvar),
}

impl ExecutionGate {
    pub(crate) fn try_new() -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            state: AtomicU64::new(0),
            fence_owner: AtomicU64::new(0),
            retired: core::sync::atomic::AtomicBool::new(false),
            epoch: id(&NEXT_EXECUTION_EPOCH, ExecutionEpoch)?,
            protocol: SpinNoIrq::new(()),
            waiters: axtask::WaitQueue::new(),
            #[cfg(test)]
            host_wait: (std::sync::Mutex::new(()), std::sync::Condvar::new()),
        })
        .map_err(|_| AxError::NoMemory)
    }

    fn active(&self) -> u64 {
        self.state.load(Ordering::Acquire) & ACTIVE_MASK
    }

    fn closed_and_drained(&self) -> bool {
        self.state.load(Ordering::Acquire) == CLOSED_BIT
    }

    fn epoch(&self) -> ExecutionEpoch {
        self.epoch
    }

    pub(crate) fn activate(&self) -> AxResult<()> {
        let result = {
            let _protocol = self.protocol.lock();
            if self.retired.load(Ordering::Acquire) || self.fence_owner.load(Ordering::Acquire) != 0
            {
                Err(AxError::ResourceBusy)
            } else {
                self.state
                    .compare_exchange(CLOSED_BIT, 0, Ordering::AcqRel, Ordering::Acquire)
                    .map(|_| ())
                    .map_err(|_| AxError::ResourceBusy)
            }
        };
        if result.is_ok() {
            self.notify();
        }
        result
    }

    pub(crate) fn enter(self: &Arc<Self>) -> Option<ExecutionLease> {
        if self.retired.load(Ordering::Acquire) {
            return None;
        }
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & CLOSED_BIT != 0 {
                return None;
            }
            let active = state & ACTIVE_MASK;
            if active == ACTIVE_MASK {
                return None;
            }
            let next = state + 1;
            match self
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Some(ExecutionLease {
                        gate: self.clone(),
                        epoch: self.epoch,
                        provider_pin: None,
                    });
                }
                Err(observed) => state = observed,
            }
        }
    }

    fn notify(&self) {
        #[cfg(not(test))]
        self.waiters.notify_all(true);
        #[cfg(test)]
        self.host_wait.1.notify_all();
    }

    fn wait_drained(&self) -> AxResult<()> {
        #[cfg(not(test))]
        {
            self.waiters
                .wait_until(|| self.active() == 0)
                .map_err(|_| AxError::Interrupted)
        }
        #[cfg(test)]
        {
            let mut guard = self.host_wait.0.lock().unwrap();
            while self.active() != 0 {
                guard = self.host_wait.1.wait(guard).unwrap();
            }
            Ok(())
        }
    }

    pub(crate) fn begin_fence(self: &Arc<Self>) -> AxResult<ExecutionFence> {
        let fence_epoch = id(&NEXT_FENCE_EPOCH, FenceEpoch)?;
        let _protocol = self.protocol.lock();
        if self.retired.load(Ordering::Acquire) {
            return Err(AxError::ResourceBusy);
        }
        // Reserve the owner before closing the state.  This prevents a
        // concurrent activation from observing an unowned CLOSED bit and
        // reopening a fence between its state CAS and owner publication.
        self.fence_owner
            .compare_exchange(0, fence_epoch.0.get(), Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| AxError::ResourceBusy)?;
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & CLOSED_BIT != 0 {
                let _ = self.clear_owner_unlocked(fence_epoch);
                return Err(AxError::ResourceBusy);
            }
            match self.state.compare_exchange_weak(
                state,
                state | CLOSED_BIT,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(ExecutionFence {
                        rollback: Some(RollbackFence {
                            gate: self.clone(),
                            epoch: fence_epoch,
                        }),
                        drained: core::sync::atomic::AtomicBool::new(false),
                    });
                }
                Err(observed) => {
                    state = observed;
                    if state & CLOSED_BIT != 0 {
                        let _ = self.clear_owner_unlocked(fence_epoch);
                        return Err(AxError::ResourceBusy);
                    }
                }
            }
        }
    }

    fn owner_matches(&self, epoch: FenceEpoch) -> bool {
        self.fence_owner.load(Ordering::Acquire) == epoch.0.get()
    }

    /// Clears an owner while the caller holds `protocol`.
    fn clear_owner_unlocked(&self, epoch: FenceEpoch) -> AxResult<()> {
        self.fence_owner
            .compare_exchange(epoch.0.get(), 0, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| AxError::BadState)
    }

    fn reopen_exact(&self, epoch: FenceEpoch) -> AxResult<()> {
        let result = {
            let _protocol = self.protocol.lock();
            if self.retired.load(Ordering::Acquire) || !self.owner_matches(epoch) {
                Err(AxError::BadState)
            } else {
                self.state
                    .compare_exchange(CLOSED_BIT, 0, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| AxError::BadState)
                    .and_then(|_| self.clear_owner_unlocked(epoch))
            }
        };
        if result.is_ok() {
            self.notify();
        }
        result
    }

    /// Reopens a pre-commit fence without waiting for leases to drain.
    ///
    /// This is only used by `Drop`: dropping an RAII guard must not sleep in
    /// an arbitrary caller or while an authority lock is held. Existing
    /// leases remain counted and will release normally after the gate is
    /// reopened.
    fn reopen_nonblocking(&self, epoch: FenceEpoch) -> AxResult<()> {
        let result = {
            let _protocol = self.protocol.lock();
            if self.retired.load(Ordering::Acquire) || !self.owner_matches(epoch) {
                Err(AxError::BadState)
            } else {
                let mut state = self.state.load(Ordering::Acquire);
                loop {
                    if state & CLOSED_BIT == 0 {
                        break Err(AxError::BadState);
                    }
                    let reopened = state & ACTIVE_MASK;
                    match self.state.compare_exchange_weak(
                        state,
                        reopened,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => break self.clear_owner_unlocked(epoch),
                        Err(observed) => state = observed,
                    }
                }
            }
        };
        if result.is_ok() {
            self.notify();
        }
        result
    }

    /// Seals a drained fence and releases its owner without reopening the
    /// gate. This is used for a destination that is not yet associated with a
    /// cancellable source transaction: it remains closed, but the normal
    /// activation path may later open it.
    fn seal_exact(&self, epoch: FenceEpoch) -> AxResult<()> {
        let _protocol = self.protocol.lock();
        if self.retired.load(Ordering::Acquire)
            || !self.owner_matches(epoch)
            || !self.closed_and_drained()
        {
            return Err(AxError::BadState);
        }
        self.clear_owner_unlocked(epoch)
    }

    /// Seals a drained source fence while retaining its exact owner. A
    /// committed-but-not-activated binding must not be reopenable through the
    /// generic `activate` operation; cancellation and activation use the
    /// binding's recorded fence epoch to perform the next exact transition.
    /// This is atomic-only because commit already owns the closed gate; no
    /// protocol spin lock is taken beneath the authority's no-IRQ lock.
    fn seal_for_rollback(&self, epoch: FenceEpoch) -> AxResult<()> {
        if self.retired.load(Ordering::Acquire)
            || !self.owner_matches(epoch)
            || !self.closed_and_drained()
        {
            return Err(AxError::BadState);
        }
        Ok(())
    }

    /// Permanently retires a committed source generation. The owner is
    /// cleared only as part of this exact transition, so a retained UTS Arc
    /// cannot later be reopened and registered as a fresh Runnable provider.
    fn retire_exact(&self, epoch: FenceEpoch) -> AxResult<()> {
        let _protocol = self.protocol.lock();
        if self.retired.load(Ordering::Acquire)
            || !self.owner_matches(epoch)
            || !self.closed_and_drained()
        {
            return Err(AxError::BadState);
        }
        self.clear_owner_unlocked(epoch)?;
        self.retired.store(true, Ordering::Release);
        Ok(())
    }
}

/// A single-use execution lease from the shared UTS/generation gate.
pub(crate) struct ExecutionLease {
    gate: Arc<ExecutionGate>,
    epoch: ExecutionEpoch,
    provider_pin: Option<ProviderLeasePin>,
}

impl ExecutionLease {
    fn execution_epoch(&self) -> ExecutionEpoch {
        self.epoch
    }

    fn with_provider_pin(mut self, pin: ProviderLeasePin) -> Self {
        debug_assert!(self.provider_pin.is_none());
        self.provider_pin = Some(pin);
        self
    }
}

impl Drop for ExecutionLease {
    fn drop(&mut self) {
        let previous = self
            .gate
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                let active = state & ACTIVE_MASK;
                if active == 0 {
                    None
                } else {
                    Some((state & CLOSED_BIT) | (active - 1))
                }
            })
            .expect("UTS execution lease underflow");
        debug_assert_ne!(previous & ACTIVE_MASK, 0);
        self.gate.notify();
    }
}

/// A rollbackable single-fence receipt.  Drop rolls back a precommit fence;
/// commit consumes it while keeping the shared gate permanently closed.
pub(crate) struct ExecutionFence {
    rollback: Option<RollbackFence>,
    drained: core::sync::atomic::AtomicBool,
}

struct RollbackFence {
    gate: Arc<ExecutionGate>,
    epoch: FenceEpoch,
}

/// Typed ownership of a source fence after commit. It deliberately has no
/// `Drop` rollback: the canonical BindingRecord owns this token until exact
/// activation or cancellation consumes it. Dropping an acknowledgement or a
/// temporary rollback guard therefore cannot reopen the committed source.
struct SealedExecutionFence {
    gate: Arc<ExecutionGate>,
    epoch: FenceEpoch,
}

impl SealedExecutionFence {
    fn epoch(&self) -> FenceEpoch {
        self.epoch
    }

    fn proof_valid(&self) -> bool {
        !self.gate.retired.load(Ordering::Acquire)
            && self.gate.owner_matches(self.epoch)
            && self.gate.closed_and_drained()
    }

    fn reopen_exact(&self) -> AxResult<()> {
        self.gate.reopen_exact(self.epoch)
    }

    fn retire_exact(self) -> AxResult<()> {
        self.gate.retire_exact(self.epoch)
    }
}

impl ExecutionFence {
    fn epoch(&self) -> FenceEpoch {
        self.rollback
            .as_ref()
            .expect("sealed fence no longer has rollback ownership")
            .epoch
    }

    fn execution_epoch(&self) -> ExecutionEpoch {
        self.rollback
            .as_ref()
            .expect("sealed fence no longer has rollback ownership")
            .gate
            .epoch()
    }

    pub(crate) fn wait_for_drain(&self) -> AxResult<()> {
        let Some(rollback) = self.rollback.as_ref() else {
            return Err(AxError::BadState);
        };
        rollback.gate.wait_drained()?;
        self.drained.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn rollback(self) -> AxResult<()> {
        let mut fence = self;
        fence.rollback_in_place()
    }

    fn rollback_in_place(&mut self) -> AxResult<()> {
        self.wait_for_drain()?;
        let rollback = self
            .rollback
            .as_ref()
            .expect("drained fence lost rollback ownership");
        rollback.gate.reopen_exact(rollback.epoch)?;
        self.rollback.take();
        Ok(())
    }

    /// Seals the fence before the authority publishes its cancellable commit.
    ///
    /// The destination preparation checks the drained proof before publishing
    /// its Closed provider row. Since the gate is closed, no new lease can be
    /// admitted between that proof and this bookkeeping step.
    fn seal_after_drained(&mut self) {
        assert!(self.drained.load(Ordering::Acquire));
        let rollback = self
            .rollback
            .as_ref()
            .expect("sealed fence no longer has rollback ownership");
        rollback
            .gate
            .seal_exact(rollback.epoch)
            .expect("execution fence owner lost before seal");
        self.rollback.take();
    }

    /// Seals a source fence for a committed binding while retaining the
    /// owner epoch. The authority must later either reopen that exact epoch
    /// on cancellation or clear it after successful activation.
    fn seal_for_rollback(mut self) -> SealedExecutionFence {
        assert!(self.drained.load(Ordering::Acquire));
        let rollback = self
            .rollback
            .as_ref()
            .expect("sealed fence no longer has rollback ownership");
        rollback
            .gate
            .seal_for_rollback(rollback.epoch)
            .expect("execution fence owner lost before rollback seal");
        let rollback = self
            .rollback
            .take()
            .expect("sealed fence lost rollback ownership");
        SealedExecutionFence {
            gate: rollback.gate,
            epoch: rollback.epoch,
        }
    }

    fn is_drained(&self) -> bool {
        self.drained.load(Ordering::Acquire)
    }

    pub(crate) fn matches_gate(&self, gate: &Arc<ExecutionGate>) -> bool {
        self.rollback
            .as_ref()
            .is_some_and(|rollback| Arc::ptr_eq(&rollback.gate, gate))
    }
}

impl Drop for ExecutionFence {
    fn drop(&mut self) {
        if let Some(rollback) = self.rollback.take() {
            let _ = rollback.gate.reopen_nonblocking(rollback.epoch);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RequestDigest([u8; 32]);

#[derive(Clone)]
pub(crate) struct OperationToken {
    authority: Arc<Authority>,
    id: AuthorityOperationId,
    digest: RequestDigest,
}

impl OperationToken {
    fn operation(&self) -> AuthorityOperationId {
        self.id
    }
}

struct AuthorityState {
    limits: AuthorityCapacity,
    worlds: Vec<WorldRecord>,
    providers: Vec<Option<ProviderRecord>>,
    generation_high_water: Vec<ProviderHighWater>,
    bindings: Vec<Option<BindingRecord>>,
    operations: Vec<Option<OperationRecord>>,
    /// Highest operation identity admitted by this authority. Operation rows
    /// may be recycled, but an old caller-supplied id must never be admitted
    /// again after its row has quiesced.
    operation_high_water: u64,
}

pub(crate) struct Authority {
    id: AuthorityInstanceId,
    state: SpinNoIrq<AuthorityState>,
}

#[derive(Clone)]
pub(crate) struct SemanticWorld {
    authority: Arc<Authority>,
    id: WorldId,
}

struct WorldRecord {
    id: WorldId,
}

pub(crate) struct GenerationHandle {
    authority: Arc<Authority>,
    coordinate: ProviderCoordinate,
    uts: Arc<UtsNamespace>,
    counted: bool,
}

/// A counted ProcessData consumer lease for one provider generation. The
/// lease is deliberately non-serializable and releases its count on Drop;
/// provider retirement waits for this count to reach zero.
pub(crate) struct UtsConsumerLease {
    authority: Arc<Authority>,
    coordinate: ProviderCoordinate,
    uts: Arc<UtsNamespace>,
}

struct ProviderRecord {
    coordinate: ProviderCoordinate,
    lifecycle: ProviderLifecycle,
    uts: Arc<UtsNamespace>,
    reclaimable: bool,
    consumer_count: usize,
    handle_count: usize,
    execution_lease_count: usize,
    fence_count: usize,
}

#[derive(Clone, Copy)]
enum ProviderPinKind {
    ExecutionLease,
    Fence,
}

struct ProviderLeasePin {
    authority: Arc<Authority>,
    coordinate: ProviderCoordinate,
    uts: Arc<UtsNamespace>,
    kind: ProviderPinKind,
}

impl Drop for ProviderLeasePin {
    fn drop(&mut self) {
        self.authority
            .release_provider_pin(self.coordinate, &self.uts, self.kind);
    }
}

/// Permanent high-water metadata for a logical `(world, provider)` identity.
/// Provider slots may be reused after abort, but this record is never removed,
/// so a generation can never be issued twice.
struct ProviderHighWater {
    world: WorldId,
    provider: ProviderId,
    generation: ProviderGeneration,
    /// The last generation row reclaimed for this logical provider. A live
    /// stale UTS Arc may not be admitted as a fresh generation after its row
    /// disappears; the weak pointer keeps this guard bounded and does not
    /// keep the namespace alive.
    last_reclaimed_uts: Weak<UtsNamespace>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderLifecycle {
    Runnable,
    Closed,
    Fenced,
    Retired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BindingLifecycle {
    Prepared,
    /// The authority has sealed the source and applied the canonical
    /// operation, but the vISA native ledger has not yet published the
    /// returned destination capability.  Cancellation is deliberately
    /// rejected in this phase: allowing it to complete would let the world
    /// release the binding while a concurrent native publisher still owns a
    /// handle that has not reached the terminal row.
    Committing,
    Committed,
    Activating,
    Cancelling,
    Active,
    Aborted,
}

struct BindingRecord {
    id: BindingId,
    epoch: BindingEpoch,
    execution_epoch: ExecutionEpoch,
    /// The source fence owner is retained through Committed so cancellation
    /// and activation can prove the exact closed generation.
    fence_epoch: FenceEpoch,
    sealed_source: Option<SealedExecutionFence>,
    source: ProviderCoordinate,
    destination: ProviderCoordinate,
    operation: AuthorityOperationId,
    lifecycle: BindingLifecycle,
}

struct OperationRecord {
    id: AuthorityOperationId,
    digest: RequestDigest,
    status: OperationStatus,
    binding: Option<BindingId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationStatus {
    Pending,
    Applied,
    Rejected,
    Conflict,
}

pub(crate) enum PrepareOutcome {
    Prepared(PreparedBinding),
    AlreadyPending,
    Applied(ActivationReceipt),
    Rejected,
    Conflict,
}

pub(crate) enum AuthorityQuery {
    Applied(ActivationReceipt),
    Rejected,
    Pending,
    Absent,
    Conflict,
    AuthorityGone,
}

#[derive(Clone)]
pub(crate) struct AuthorityQueryHandle {
    authority: Weak<Authority>,
}

pub(crate) struct CapturedProviderState {
    authority: Arc<Authority>,
    source: ProviderCoordinate,
    owner_user_ns: Arc<UserNamespace>,
    snapshot: UtsProviderSnapshot,
    fence: Option<GenerationFence>,
}

pub(crate) struct GenerationFence {
    authority: Arc<Authority>,
    source: ProviderCoordinate,
    uts: Arc<UtsNamespace>,
    execution: Option<ExecutionFence>,
    provider_pin: Option<ProviderLeasePin>,
}

impl GenerationFence {
    /// Returns the execution epoch owned by this pre-commit fence.
    ///
    /// The execution fence itself remains private to the world authority; the
    /// UTS adapter only needs the opaque epoch to bind a restoration receipt
    /// to the exact source cut.
    pub(crate) fn execution_epoch(&self) -> Option<u64> {
        self.execution
            .as_ref()
            .map(ExecutionFence::execution_epoch)
            .map(|epoch| epoch.0.get())
    }

    pub(crate) fn is_drained(&self) -> bool {
        self.execution
            .as_ref()
            .is_some_and(ExecutionFence::is_drained)
    }

    /// Reopens this exact pre-commit source fence.
    ///
    /// This consumes the typed owner and delegates to `ExecutionFence`'s
    /// exact rollback transition.  Dropping a `GenerationFence` is not a
    /// substitute: its best-effort Drop path is intentionally only for RAII
    /// cleanup, while restoration must prove and reopen the recorded owner.
    pub(crate) fn rollback(&mut self) -> AxResult<()> {
        let execution = self.execution.as_mut().ok_or(AxError::BadState)?;
        execution.rollback_in_place()?;
        self.execution.take();
        self.provider_pin.take();
        Ok(())
    }
}

pub(crate) struct PreparedBinding {
    authority: Arc<Authority>,
    binding: BindingId,
    source: ProviderCoordinate,
    destination: ProviderCoordinate,
    operation: AuthorityOperationId,
    captured: Option<CapturedProviderState>,
    armed: bool,
}

pub(crate) struct CommittedBinding {
    authority: Arc<Authority>,
    binding: BindingId,
    destination: GenerationHandle,
    operation: AuthorityOperationId,
    source_fence_epoch: FenceEpoch,
    execution_epoch: ExecutionEpoch,
}

pub(crate) struct ActiveBinding {
    destination: GenerationHandle,
    operation: AuthorityOperationId,
    /// Authority-only activation has no ProcessData owner, so the returned
    /// binding itself is the counted consumer until it is dropped. Process
    /// activation stores the lease in ProcessData instead.
    consumer: Option<UtsConsumerLease>,
}

/// Resources reserved before a destination activation leaves the authority
/// state machine.  The reservation owns the destination handle, the
/// destination consumer count, and the exact sealed source fence while the
/// ProcessData compare-and-replace runs outside the authority lock.
struct ActivationReservation {
    authority: Arc<Authority>,
    binding: BindingId,
    operation: AuthorityOperationId,
    destination: ProviderCoordinate,
    destination_uts: Arc<UtsNamespace>,
    destination_handle: Option<GenerationHandle>,
    sealed_source: Option<SealedExecutionFence>,
    new_consumer: Option<UtsConsumerLease>,
    active_consumer: Option<UtsConsumerLease>,
}

impl ActivationReservation {
    fn retire_source(&mut self) {
        let sealed = self
            .sealed_source
            .take()
            .expect("activation reservation lost sealed source");
        sealed
            .retire_exact()
            .expect("validated source fence owner lost during activation");
    }

    fn rollback(mut self) -> AxResult<()> {
        let sealed = self.sealed_source.take().ok_or(AxError::BadState)?;
        {
            let mut state = self.authority.state.lock();
            let binding_index = Authority::binding_index(&state, self.binding)
                .expect("activation binding reservation disappeared during rollback");
            let binding = state.bindings[binding_index]
                .as_mut()
                .expect("activation binding row disappeared during rollback");
            assert_eq!(binding.operation, self.operation);
            assert_eq!(binding.lifecycle, BindingLifecycle::Activating);
            binding.sealed_source = Some(sealed);
            binding.lifecycle = BindingLifecycle::Committed;
        }

        // The authority lock is deliberately out of scope before any of the
        // reserved RAII owners are dropped: both drops reacquire it.
        let new_consumer = self.new_consumer.take();
        let active_consumer = self.active_consumer.take();
        let destination_handle = self.destination_handle.take();
        drop(new_consumer);
        drop(active_consumer);
        drop(destination_handle);
        Ok(())
    }

    fn finish(mut self) -> ActiveBinding {
        {
            let mut state = self.authority.state.lock();
            let binding_index = Authority::binding_index(&state, self.binding)
                .expect("activation binding reservation disappeared");
            assert!(self.sealed_source.is_none());
            let source = {
                let binding = state.bindings[binding_index]
                    .as_ref()
                    .expect("activation binding row disappeared");
                assert_eq!(binding.operation, self.operation);
                assert_eq!(binding.lifecycle, BindingLifecycle::Activating);
                binding.source
            };

            let destination_index = Authority::provider_index(&state, self.destination)
                .expect("activation destination disappeared");
            state.providers[destination_index]
                .as_mut()
                .expect("activation destination row disappeared")
                .lifecycle = ProviderLifecycle::Runnable;
            let source_index =
                Authority::provider_index(&state, source).expect("activation source disappeared");
            state.providers[source_index]
                .as_mut()
                .expect("activation source row disappeared")
                .lifecycle = ProviderLifecycle::Retired;
            state.bindings[binding_index]
                .as_mut()
                .expect("activation binding row disappeared")
                .lifecycle = BindingLifecycle::Active;
        }

        let destination = self
            .destination_handle
            .take()
            .expect("activation destination handle missing");
        assert!(self.new_consumer.is_none());
        ActiveBinding {
            destination,
            operation: self.operation,
            consumer: self.active_consumer.take(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ActivationReceipt {
    authority: Arc<Authority>,
    binding: BindingId,
    operation: AuthorityOperationId,
    digest: RequestDigest,
}

impl Authority {
    pub(crate) fn try_new(capacity: AuthorityCapacity) -> AxResult<Arc<Self>> {
        let mut worlds = Vec::new();
        worlds
            .try_reserve_exact(capacity.worlds)
            .map_err(|_| AxError::NoMemory)?;
        let mut providers = Vec::new();
        providers
            .try_reserve_exact(capacity.providers)
            .map_err(|_| AxError::NoMemory)?;
        let mut generation_high_water = Vec::new();
        generation_high_water
            .try_reserve_exact(capacity.providers)
            .map_err(|_| AxError::NoMemory)?;
        let mut bindings = Vec::new();
        bindings
            .try_reserve_exact(capacity.bindings)
            .map_err(|_| AxError::NoMemory)?;
        let mut operations = Vec::new();
        operations
            .try_reserve_exact(capacity.terminal_operations)
            .map_err(|_| AxError::NoMemory)?;
        Arc::try_new(Self {
            id: id(&NEXT_AUTHORITY, AuthorityInstanceId)?,
            state: SpinNoIrq::new(AuthorityState {
                limits: capacity,
                worlds,
                providers,
                generation_high_water,
                bindings,
                operations,
                operation_high_water: 0,
            }),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn query_handle(self: &Arc<Self>) -> AuthorityQueryHandle {
        AuthorityQueryHandle {
            authority: Arc::downgrade(self),
        }
    }

    pub(crate) const fn instance_id(&self) -> AuthorityInstanceId {
        self.id
    }

    /// Allocates a private, non-reconstructible operation identity before a
    /// request is submitted. A caller can retain this value across a lost
    /// reserve acknowledgement and retry with the exact identity/digest.
    pub(crate) fn allocate_operation_id() -> AxResult<AuthorityOperationId> {
        id(&NEXT_OPERATION, AuthorityOperationId)
    }

    /// Reserves a caller-owned operation identity. Same identity plus digest
    /// is idempotent and returns the canonical token; a different digest is a
    /// conflict and never mutates the canonical row.
    pub(crate) fn reserve_operation_with_id(
        self: &Arc<Self>,
        operation: AuthorityOperationId,
        digest: [u8; 32],
    ) -> AxResult<OperationToken> {
        let mut state = self.state.lock();
        if let Some(index) = Self::operation_index(&state, operation) {
            let row = state.operations[index].as_ref().ok_or(AxError::BadState)?;
            if row.digest != RequestDigest(digest) {
                return Err(AxError::InvalidInput);
            }
            return Ok(OperationToken {
                authority: self.clone(),
                id: operation,
                digest: row.digest,
            });
        }
        if operation.0.get() <= state.operation_high_water {
            return Err(AxError::InvalidInput);
        }
        let slot = Self::operation_slot(&mut state).ok_or(AxError::ResourceBusy)?;
        let record = OperationRecord {
            id: operation,
            digest: RequestDigest(digest),
            status: OperationStatus::Pending,
            binding: None,
        };
        if slot == state.operations.len() {
            state.operations.push(None);
        }
        state.operations[slot] = Some(record);
        state.operation_high_water = operation.0.get();
        Ok(OperationToken {
            authority: self.clone(),
            id: operation,
            digest: RequestDigest(digest),
        })
    }

    /// Convenience form for local callers that do not need to recover a lost
    /// reserve acknowledgement. Adapters requiring that property should call
    /// `allocate_operation_id` first and then `reserve_operation_with_id`.
    pub(crate) fn reserve_operation(
        self: &Arc<Self>,
        digest: [u8; 32],
    ) -> AxResult<OperationToken> {
        self.reserve_operation_with_id(Self::allocate_operation_id()?, digest)
    }

    pub(crate) fn try_new_world(self: &Arc<Self>) -> AxResult<SemanticWorld> {
        let world = id(&NEXT_WORLD, WorldId)?;
        let mut state = self.state.lock();
        if state.worlds.len() >= state.limits.worlds {
            return Err(AxError::ResourceBusy);
        }
        state.worlds.push(WorldRecord { id: world });
        Ok(SemanticWorld {
            authority: self.clone(),
            id: world,
        })
    }

    fn provider_index(state: &AuthorityState, coordinate: ProviderCoordinate) -> Option<usize> {
        state.providers.iter().position(|provider| {
            provider
                .as_ref()
                .is_some_and(|provider| provider.coordinate == coordinate)
        })
    }

    fn binding_index(state: &AuthorityState, binding: BindingId) -> Option<usize> {
        state
            .bindings
            .iter()
            .position(|record| record.as_ref().is_some_and(|record| record.id == binding))
    }

    fn operation_index(state: &AuthorityState, operation: AuthorityOperationId) -> Option<usize> {
        state
            .operations
            .iter()
            .position(|record| record.as_ref().is_some_and(|record| record.id == operation))
    }

    fn operation_slot(state: &mut AuthorityState) -> Option<usize> {
        if let Some(index) = state.operations.iter().position(Option::is_none) {
            return Some(index);
        }
        if let Some(index) = state.operations.iter().position(|record| {
            record.as_ref().is_some_and(|record| {
                matches!(
                    record.status,
                    OperationStatus::Rejected
                        | OperationStatus::Conflict
                        | OperationStatus::Applied
                ) && record.binding.is_none()
            })
        }) {
            state.operations[index] = None;
            return Some(index);
        }
        (state.operations.len() < state.limits.terminal_operations)
            .then_some(state.operations.len())
    }

    fn provider_slot(state: &mut AuthorityState) -> Option<usize> {
        if let Some(index) = state.providers.iter().position(Option::is_none) {
            return Some(index);
        }
        (state.providers.len() < state.limits.providers).then_some(state.providers.len())
    }

    /// Returns a logical provider high-water slot which is not currently
    /// represented by a live provider generation.  The physical provider
    /// table and the logical-id table are deliberately separate: a prepared
    /// migration may occupy a second physical slot while advancing the same
    /// logical provider's generation.  Once all generations for a logical id
    /// quiesce, the id can be reused with a strictly larger generation.
    fn free_logical_provider_slot(state: &AuthorityState, world: WorldId) -> Option<usize> {
        state.generation_high_water.iter().position(|high_water| {
            high_water.world == world
                && !state
                    .providers
                    .iter()
                    .flatten()
                    .any(|provider| provider.coordinate.provider == high_water.provider)
        })
    }

    fn note_reclaimed_provider_locked(state: &mut AuthorityState, coordinate: ProviderCoordinate) {
        let Some(provider_index) = Self::provider_index(state, coordinate) else {
            return;
        };
        let Some(provider) = state.providers[provider_index].as_ref() else {
            return;
        };
        let uts = provider.uts.clone();
        if let Some(high_water) = state.generation_high_water.iter_mut().find(|record| {
            record.world == coordinate.world && record.provider == coordinate.provider
        }) {
            high_water.last_reclaimed_uts = Arc::downgrade(&uts);
        }
    }

    fn binding_slot(state: &mut AuthorityState) -> Option<usize> {
        if let Some(index) = state.bindings.iter().position(Option::is_none) {
            return Some(index);
        }
        (state.bindings.len() < state.limits.bindings).then_some(state.bindings.len())
    }

    fn find_operation<'a>(
        state: &'a AuthorityState,
        operation: AuthorityOperationId,
    ) -> Option<&'a OperationRecord> {
        Self::operation_index(state, operation).and_then(|index| state.operations[index].as_ref())
    }

    fn destination_handle(
        authority: &Arc<Self>,
        state: &mut AuthorityState,
        coordinate: ProviderCoordinate,
    ) -> AxResult<GenerationHandle> {
        let index = Self::provider_index(state, coordinate).ok_or(AxError::BadState)?;
        let provider = state.providers[index].as_mut().ok_or(AxError::BadState)?;
        provider.handle_count = provider
            .handle_count
            .checked_add(1)
            .ok_or(AxError::OutOfRange)?;
        Ok(GenerationHandle {
            authority: authority.clone(),
            coordinate,
            uts: provider.uts.clone(),
            counted: true,
        })
    }

    fn remove_active_binding_locked(state: &mut AuthorityState, binding_index: usize) {
        let Some(binding) = state.bindings[binding_index].take() else {
            return;
        };
        if binding.lifecycle != BindingLifecycle::Active {
            state.bindings[binding_index] = Some(binding);
            return;
        }
        // Once the last live consumer has quiesced, the Applied receipt is no
        // longer an activation capability. Reclaim the operation row with
        // the binding so the bounded table can serve a later request.
        if let Some(operation_index) = Self::operation_index(state, binding.operation) {
            if state.operations[operation_index]
                .as_ref()
                .is_some_and(|operation| operation.binding == Some(binding.id))
            {
                state.operations[operation_index] = None;
            }
        }
    }

    fn provider_has_live_owner(provider: &ProviderRecord) -> bool {
        provider.consumer_count != 0
            || provider.handle_count != 0
            || provider.execution_lease_count != 0
            || provider.fence_count != 0
    }

    /// Reclaims quiescent generations and their active relations to a fixed
    /// point. A source may also be the destination of an older migration, so
    /// a single `binding_index` lookup is insufficient: every active relation
    /// is examined on each pass. The scan is bounded by the authority limits.
    fn reap_quiescent_locked(state: &mut AuthorityState) {
        loop {
            let mut changed = false;
            for binding_index in 0..state.bindings.len() {
                let Some(binding) = state.bindings[binding_index].as_ref() else {
                    continue;
                };
                if binding.lifecycle != BindingLifecycle::Active {
                    continue;
                }
                let source_coordinate = binding.source;
                let destination_coordinate = binding.destination;
                let source_live = Self::provider_index(state, source_coordinate)
                    .and_then(|index| state.providers[index].as_ref())
                    .is_some_and(Self::provider_has_live_owner);
                let destination_live = Self::provider_index(state, destination_coordinate)
                    .and_then(|index| state.providers[index].as_ref())
                    .is_some_and(Self::provider_has_live_owner);

                if !source_live {
                    if let Some(index) = Self::provider_index(state, source_coordinate) {
                        Self::note_reclaimed_provider_locked(state, source_coordinate);
                        state.providers[index] = None;
                        changed = true;
                    }
                }
                if !destination_live {
                    if let Some(index) = Self::provider_index(state, destination_coordinate) {
                        Self::note_reclaimed_provider_locked(state, destination_coordinate);
                        state.providers[index] = None;
                    }
                    Self::remove_active_binding_locked(state, binding_index);
                    changed = true;
                }
                // If the destination is still consumed, retain the exact
                // Active binding as the retirement edge for that consumer.
            }

            // Standalone reclaimable providers (including a destination
            // activated without a ProcessData owner) do not need a
            // nonterminal binding relation to keep their generation alive
            // after their lease is gone. Prepared and Committed relations do
            // need that protection until their explicit terminal transition.
            for index in 0..state.providers.len() {
                let remove = state.providers[index].as_ref().is_some_and(|provider| {
                    !Self::provider_has_live_owner(provider)
                        && (provider.reclaimable
                            || matches!(
                                provider.lifecycle,
                                ProviderLifecycle::Closed
                                    | ProviderLifecycle::Fenced
                                    | ProviderLifecycle::Retired
                            ))
                        && !state.bindings.iter().any(|binding| {
                            binding.as_ref().is_some_and(|binding| {
                                binding.lifecycle != BindingLifecycle::Aborted
                                    && (binding.source == provider.coordinate
                                        || binding.destination == provider.coordinate)
                            })
                        })
                });
                if remove {
                    let coordinate = state.providers[index]
                        .as_ref()
                        .expect("reclaimable provider disappeared")
                        .coordinate;
                    Self::note_reclaimed_provider_locked(state, coordinate);
                    state.providers[index] = None;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    fn release_provider_pin(
        &self,
        coordinate: ProviderCoordinate,
        uts: &Arc<UtsNamespace>,
        kind: ProviderPinKind,
    ) {
        let mut state = self.state.lock();
        let Some(provider_index) = Self::provider_index(&state, coordinate) else {
            return;
        };
        let Some(provider) = state.providers[provider_index].as_mut() else {
            return;
        };
        if !Arc::ptr_eq(&provider.uts, uts) {
            return;
        }
        match kind {
            ProviderPinKind::ExecutionLease => {
                provider.execution_lease_count = provider
                    .execution_lease_count
                    .checked_sub(1)
                    .expect("provider execution lease underflow");
            }
            ProviderPinKind::Fence => {
                provider.fence_count = provider
                    .fence_count
                    .checked_sub(1)
                    .expect("provider fence underflow");
            }
        }
        Self::reap_quiescent_locked(&mut state);
    }

    fn release_uts_consumer(&self, coordinate: ProviderCoordinate, uts: &Arc<UtsNamespace>) {
        let mut state = self.state.lock();
        let Some(provider_index) = Self::provider_index(&state, coordinate) else {
            return;
        };
        let Some(provider) = state.providers[provider_index].as_mut() else {
            return;
        };
        if !Arc::ptr_eq(&provider.uts, uts) {
            return;
        }
        provider.consumer_count = provider
            .consumer_count
            .checked_sub(1)
            .expect("UTS consumer lease underflow");
        Self::reap_quiescent_locked(&mut state);
    }

    fn query_operation(
        self: &Arc<Self>,
        operation: AuthorityOperationId,
        digest: RequestDigest,
    ) -> AuthorityQuery {
        let state = self.state.lock();
        let Some(row) = Self::find_operation(&state, operation) else {
            return AuthorityQuery::Absent;
        };
        // A mismatching request is a conflict view only. Never poison the
        // canonical operation row: the owner carrying the original digest
        // must still be able to observe Pending/Applied and recover a lost
        // acknowledgement.
        if row.digest != digest {
            return AuthorityQuery::Conflict;
        }
        match row.status {
            OperationStatus::Pending => AuthorityQuery::Pending,
            OperationStatus::Rejected => AuthorityQuery::Rejected,
            OperationStatus::Conflict => AuthorityQuery::Conflict,
            OperationStatus::Applied => {
                let Some(binding) = row.binding else {
                    return AuthorityQuery::Conflict;
                };
                AuthorityQuery::Applied(ActivationReceipt {
                    authority: self.clone(),
                    binding,
                    operation,
                    digest: row.digest,
                })
            }
        }
    }

    fn retry_operation(
        self: &Arc<Self>,
        token: &OperationToken,
        digest: RequestDigest,
    ) -> AxResult<PrepareOutcome> {
        if !Arc::ptr_eq(self, &token.authority) {
            return Err(AxError::InvalidInput);
        }
        let state = self.state.lock();
        let index = Self::operation_index(&state, token.id).ok_or(AxError::BadState)?;
        let row = state.operations[index].as_ref().ok_or(AxError::BadState)?;
        if row.digest != token.digest || digest != row.digest {
            return Ok(PrepareOutcome::Conflict);
        }
        Ok(match row.status {
            OperationStatus::Pending => PrepareOutcome::AlreadyPending,
            OperationStatus::Applied => {
                let binding = row.binding.ok_or(AxError::BadState)?;
                PrepareOutcome::Applied(ActivationReceipt {
                    authority: self.clone(),
                    binding,
                    operation: token.id,
                    digest: row.digest,
                })
            }
            OperationStatus::Rejected => PrepareOutcome::Rejected,
            OperationStatus::Conflict => PrepareOutcome::Conflict,
        })
    }

    fn cancel_operation(
        self: &Arc<Self>,
        token: &OperationToken,
        digest: RequestDigest,
    ) -> AxResult<()> {
        if !Arc::ptr_eq(self, &token.authority) {
            return Err(AxError::InvalidInput);
        }
        let mut state = self.state.lock();
        let index = Self::operation_index(&state, token.id).ok_or(AxError::BadState)?;
        let (row_digest, row_status, row_binding) = {
            let row = state.operations[index].as_ref().ok_or(AxError::BadState)?;
            (row.digest, row.status, row.binding)
        };
        if row_digest != token.digest || row_digest != digest {
            return Err(AxError::InvalidInput);
        }
        if row_status == OperationStatus::Applied {
            let binding = row_binding.ok_or(AxError::BadState)?;
            drop(state);
            return self.abort_committed_binding(binding, token.id, row_digest);
        }
        match row_status {
            OperationStatus::Pending if row_binding.is_none() => {
                let row = state.operations[index].as_mut().ok_or(AxError::BadState)?;
                row.status = OperationStatus::Rejected;
                Ok(())
            }
            OperationStatus::Pending => {
                let binding = row_binding.ok_or(AxError::BadState)?;
                let binding_index =
                    Self::binding_index(&state, binding).ok_or(AxError::BadState)?;
                let Some(binding_record) = state.bindings[binding_index].as_ref() else {
                    return Err(AxError::BadState);
                };
                if binding_record.lifecycle != BindingLifecycle::Prepared {
                    return Err(AxError::ResourceBusy);
                }
                let source = binding_record.source;
                let fence_epoch = binding_record.fence_epoch;
                let source_uts = Self::provider_index(&state, source)
                    .and_then(|index| state.providers[index].as_ref())
                    .map(|provider| provider.uts.clone())
                    .ok_or(AxError::BadState)?;
                let source_gate = source_uts.execution_gate();
                if !source_gate.owner_matches(fence_epoch) || !source_gate.closed_and_drained() {
                    return Err(AxError::ResourceBusy);
                }
                state.bindings[binding_index]
                    .as_mut()
                    .expect("validated pending binding")
                    .lifecycle = BindingLifecycle::Cancelling;
                drop(state);

                // Reopen the exact pending fence outside the authority lock;
                // the gate protocol is allowed to spin briefly, while the
                // authority lock must never nest over that path.
                if source_gate.reopen_nonblocking(fence_epoch).is_err() {
                    let mut state = self.state.lock();
                    let binding_index = Self::binding_index(&state, binding)
                        .expect("cancelling binding disappeared during reopen");
                    let record = state.bindings[binding_index]
                        .as_mut()
                        .expect("cancelling binding row disappeared");
                    assert_eq!(record.lifecycle, BindingLifecycle::Cancelling);
                    record.lifecycle = BindingLifecycle::Prepared;
                    return Err(AxError::ResourceBusy);
                }

                let mut state = self.state.lock();
                let binding_index = Self::binding_index(&state, binding)
                    .expect("cancelling binding disappeared after reopen");
                let operation_index = Self::operation_index(&state, token.id)
                    .expect("cancelling operation disappeared after reopen");
                let record = state.bindings[binding_index]
                    .as_ref()
                    .expect("cancelling binding row disappeared after reopen");
                assert_eq!(record.lifecycle, BindingLifecycle::Cancelling);
                state.bindings[binding_index] = None;
                let row = state.operations[operation_index]
                    .as_mut()
                    .expect("validated pending operation");
                row.status = OperationStatus::Rejected;
                row.binding = None;
                Self::reap_quiescent_locked(&mut state);
                Ok(())
            }
            OperationStatus::Applied | OperationStatus::Rejected | OperationStatus::Conflict => {
                Err(AxError::BadState)
            }
        }
    }

    fn prepare_binding(
        authority: &Arc<Self>,
        token: &OperationToken,
        digest: RequestDigest,
        captured: &mut Option<CapturedProviderState>,
    ) -> AxResult<PrepareOutcome> {
        let captured_ref = captured.as_ref().ok_or(AxError::BadState)?;
        if !Arc::ptr_eq(authority, &token.authority)
            || !Arc::ptr_eq(authority, &captured_ref.authority)
        {
            return Err(AxError::InvalidInput);
        }
        let source = captured_ref.source;
        let source_fence_uts = captured_ref
            .fence
            .as_ref()
            .ok_or(AxError::BadState)?
            .uts
            .clone();

        // Observe the canonical operation and source before doing any
        // fallible preparation. The same checks are repeated at publication
        // because another owner may have completed the operation meanwhile.
        {
            let state = authority.state.lock();
            let op_index = Self::operation_index(&state, token.id).ok_or(AxError::BadState)?;
            let operation = state.operations[op_index]
                .as_ref()
                .ok_or(AxError::BadState)?;
            let operation_digest = operation.digest;
            let operation_status = operation.status;
            let operation_binding = operation.binding;
            if operation_digest != token.digest || digest != operation_digest {
                return Ok(PrepareOutcome::Conflict);
            }
            if operation_status != OperationStatus::Pending {
                return Ok(match operation_status {
                    OperationStatus::Applied => PrepareOutcome::Applied(ActivationReceipt {
                        authority: authority.clone(),
                        binding: operation_binding.ok_or(AxError::BadState)?,
                        operation: token.id,
                        digest: operation_digest,
                    }),
                    OperationStatus::Rejected => PrepareOutcome::Rejected,
                    OperationStatus::Conflict => PrepareOutcome::Conflict,
                    OperationStatus::Pending => unreachable!(),
                });
            }
            if operation_binding.is_some() {
                return Ok(PrepareOutcome::AlreadyPending);
            }
            let source_index = Self::provider_index(&state, source).ok_or(AxError::BadState)?;
            let source_provider = state.providers[source_index]
                .as_ref()
                .ok_or(AxError::BadState)?;
            if source_provider.lifecycle != ProviderLifecycle::Runnable
                || !Arc::ptr_eq(&source_provider.uts, &source_fence_uts)
            {
                return Err(AxError::ResourceBusy);
            }
            // The pilot has no bounded consumer registry yet.  Refuse to
            // fence a source with a cohort larger than one rather than
            // committing a migration that would strand sibling ProcessData
            // instances on a permanently closed source gate.  Zero is kept
            // available for authority-only activation.
            if source_provider.consumer_count > 1 {
                return Err(AxError::ResourceBusy);
            }
        }

        let binding = id(&NEXT_BINDING, BindingId)?;
        let epoch = id(&NEXT_BINDING_EPOCH, BindingEpoch)?;
        let fence_epoch = captured_ref
            .fence
            .as_ref()
            .and_then(|fence| fence.execution.as_ref())
            .map(ExecutionFence::epoch)
            .ok_or(AxError::BadState)?;
        // All potentially failing allocations and gate transitions happen
        // before the publication lock. The lock below only reserves an
        // already-capacitated slot and moves prepared values into records.
        let destination_uts = UtsNamespace::try_from_provider_snapshot(
            captured_ref.owner_user_ns.clone(),
            captured_ref.snapshot,
        )?;
        let mut destination_fence = destination_uts.freeze_execution()?;
        destination_fence.wait_for_drain()?;
        destination_fence.seal_after_drained();

        let mut state = authority.state.lock();
        let op_index = Self::operation_index(&state, token.id).ok_or(AxError::BadState)?;
        let operation = state.operations[op_index]
            .as_ref()
            .ok_or(AxError::BadState)?;
        let operation_digest = operation.digest;
        let operation_status = operation.status;
        let operation_binding = operation.binding;
        if operation_digest != token.digest || digest != operation_digest {
            return Ok(PrepareOutcome::Conflict);
        }
        if operation_status != OperationStatus::Pending {
            return Ok(match operation_status {
                OperationStatus::Applied => PrepareOutcome::Applied(ActivationReceipt {
                    authority: authority.clone(),
                    binding: operation_binding.ok_or(AxError::BadState)?,
                    operation: token.id,
                    digest: operation_digest,
                }),
                OperationStatus::Rejected => PrepareOutcome::Rejected,
                OperationStatus::Conflict => PrepareOutcome::Conflict,
                OperationStatus::Pending => unreachable!(),
            });
        }
        if operation_binding.is_some() {
            return Ok(PrepareOutcome::AlreadyPending);
        }
        let source_index = Self::provider_index(&state, source).ok_or(AxError::BadState)?;
        let source_provider = state.providers[source_index]
            .as_ref()
            .ok_or(AxError::BadState)?;
        if source_provider.lifecycle != ProviderLifecycle::Runnable
            || !Arc::ptr_eq(&source_provider.uts, &source_fence_uts)
        {
            return Err(AxError::ResourceBusy);
        }
        if source_provider.consumer_count > 1 {
            return Err(AxError::ResourceBusy);
        }
        let provider_slot = Self::provider_slot(&mut state).ok_or(AxError::ResourceBusy)?;
        let binding_slot = Self::binding_slot(&mut state).ok_or(AxError::ResourceBusy)?;
        let generation = next_generation(&mut state, source)?;
        let destination = ProviderCoordinate {
            world: source.world,
            provider: source.provider,
            generation,
        };
        if provider_slot == state.providers.len() {
            state.providers.push(None);
        }
        state.providers[provider_slot] = Some(ProviderRecord {
            coordinate: destination,
            lifecycle: ProviderLifecycle::Closed,
            uts: destination_uts,
            reclaimable: true,
            consumer_count: 0,
            handle_count: 0,
            execution_lease_count: 0,
            fence_count: 0,
        });
        if binding_slot == state.bindings.len() {
            state.bindings.push(None);
        }
        let captured = captured
            .take()
            .expect("validated captured provider state before publication");
        state.bindings[binding_slot] = Some(BindingRecord {
            id: binding,
            epoch,
            execution_epoch: captured.source_epoch(),
            fence_epoch,
            sealed_source: None,
            source,
            destination,
            operation: token.id,
            lifecycle: BindingLifecycle::Prepared,
        });
        state.operations[op_index]
            .as_mut()
            .expect("validated pending operation")
            .binding = Some(binding);
        Ok(PrepareOutcome::Prepared(PreparedBinding {
            authority: authority.clone(),
            binding,
            source,
            destination,
            operation: token.id,
            captured: Some(captured),
            armed: true,
        }))
    }

    fn abort_binding(&self, binding: BindingId, operation: AuthorityOperationId) {
        let mut state = self.state.lock();
        let Some(binding_index) = Self::binding_index(&state, binding) else {
            return;
        };
        let Some(record) = state.bindings[binding_index].take() else {
            return;
        };
        if record.lifecycle != BindingLifecycle::Prepared {
            state.bindings[binding_index] = Some(record);
            return;
        }
        if let Some(operation_index) = Self::operation_index(&state, operation) {
            if let Some(operation) = state.operations[operation_index].as_mut() {
                operation.status = OperationStatus::Rejected;
                operation.binding = None;
            }
        }
        Self::reap_quiescent_locked(&mut state);
    }

    fn commit_binding(
        &self,
        prepared: &mut PreparedBinding,
        defer_publication: bool,
    ) -> AxResult<CommittedBinding> {
        let mut state = self.state.lock();
        let binding_index =
            Self::binding_index(&state, prepared.binding).ok_or(AxError::BadState)?;
        let source_index =
            Self::provider_index(&state, prepared.source).ok_or(AxError::BadState)?;
        let destination_index =
            Self::provider_index(&state, prepared.destination).ok_or(AxError::BadState)?;
        let operation_index =
            Self::operation_index(&state, prepared.operation).ok_or(AxError::BadState)?;
        let valid = {
            let source = state.providers[source_index]
                .as_ref()
                .ok_or(AxError::BadState)?;
            let destination = state.providers[destination_index]
                .as_ref()
                .ok_or(AxError::BadState)?;
            let binding = state.bindings[binding_index]
                .as_ref()
                .ok_or(AxError::BadState)?;
            let valid = binding.lifecycle == BindingLifecycle::Prepared
                && state.operations[operation_index]
                    .as_ref()
                    .is_some_and(|operation| operation.status == OperationStatus::Pending)
                && source.lifecycle == ProviderLifecycle::Runnable
                && destination.lifecycle == ProviderLifecycle::Closed
                && binding.sealed_source.is_none()
                && binding.execution_epoch == source.uts.execution_gate().epoch()
                && source
                    .uts
                    .execution_gate()
                    .owner_matches(binding.fence_epoch)
                && destination.uts.execution_gate().closed_and_drained()
                && prepared.captured.as_ref().is_some_and(|captured| {
                    captured.fence.as_ref().is_some_and(|fence| {
                        fence.execution.as_ref().is_some_and(|execution| {
                            execution.matches_gate(&source.uts.execution_gate())
                                && execution.epoch() == binding.fence_epoch
                        }) && fence
                            .execution
                            .as_ref()
                            .is_some_and(|execution| execution.is_drained())
                            && source.uts.execution_gate().closed_and_drained()
                    })
                });
            valid
        };
        if !valid {
            state.bindings[binding_index] = None;
            let operation = state.operations[operation_index]
                .as_mut()
                .ok_or(AxError::BadState)?;
            operation.status = OperationStatus::Conflict;
            operation.binding = None;
            Self::reap_quiescent_locked(&mut state);
            return Err(AxError::ResourceBusy);
        }
        let destination_handle =
            Authority::destination_handle(&prepared.authority, &mut state, prepared.destination)?;
        // Seal the drained source fence before publishing any cancellable
        // Committed/Applied state.  The authority lock remains held, so a
        // receipt cannot observe the row between publication and sealing and
        // reopen a source that still has its fence owner installed.
        let execution = prepared
            .captured
            .as_mut()
            .and_then(|captured| captured.fence.as_mut())
            .and_then(|fence| fence.execution.take())
            .expect("prepared binding lost its drained execution fence");
        let sealed_source = execution.seal_for_rollback();

        // Everything below is an infallible move into already validated
        // slots.  Do not introduce a fallible operation after the source
        // fence has been sealed.
        state.providers[source_index]
            .as_mut()
            .expect("validated UTS source provider")
            .lifecycle = ProviderLifecycle::Fenced;
        state.bindings[binding_index]
            .as_mut()
            .expect("validated UTS binding")
            .sealed_source = Some(sealed_source);
        state.bindings[binding_index]
            .as_mut()
            .expect("validated UTS binding")
            .lifecycle = if defer_publication {
            BindingLifecycle::Committing
        } else {
            BindingLifecycle::Committed
        };
        state.operations[operation_index]
            .as_mut()
            .expect("validated UTS operation")
            .status = OperationStatus::Applied;
        Ok(CommittedBinding {
            authority: prepared.authority.clone(),
            binding: prepared.binding,
            destination: destination_handle,
            operation: prepared.operation,
            source_fence_epoch: {
                state.bindings[binding_index]
                    .as_ref()
                    .expect("validated committed binding")
                    .fence_epoch
            },
            execution_epoch: {
                state.bindings[binding_index]
                    .as_ref()
                    .expect("validated committed binding")
                    .execution_epoch
            },
        })
    }

    /// Completes the short publication handshake used by the vISA adapter.
    ///
    /// `commit_binding(..., true)` has already made the canonical operation
    /// Applied and sealed the source, but leaves the binding in Committing
    /// until the native row owns the returned destination handle.  This
    /// transition is intentionally infallible for a validated publisher and
    /// idempotent for a retry: cancellation can only proceed after this
    /// method changes the lifecycle to Committed, so it cannot miss a handle
    /// that is still between the world and native ledgers.
    fn finish_commit_publication(
        &self,
        binding: BindingId,
        operation: AuthorityOperationId,
    ) -> AxResult<()> {
        let mut state = self.state.lock();
        let binding_index = Self::binding_index(&state, binding).ok_or(AxError::BadState)?;
        let operation_index = Self::operation_index(&state, operation).ok_or(AxError::BadState)?;
        let operation_record = state.operations[operation_index]
            .as_ref()
            .ok_or(AxError::BadState)?;
        if operation_record.status != OperationStatus::Applied
            || operation_record.binding != Some(binding)
        {
            return Err(AxError::BadState);
        }
        let record = state.bindings[binding_index]
            .as_mut()
            .ok_or(AxError::BadState)?;
        if record.operation != operation {
            return Err(AxError::InvalidInput);
        }
        match record.lifecycle {
            BindingLifecycle::Committing => {
                record.lifecycle = BindingLifecycle::Committed;
                Ok(())
            }
            // A concurrent activation may have advanced the world relation
            // after the native row received its capability but before the
            // publisher (or a retry) reached this handshake.  Those states
            // already prove publication; the native activation transaction
            // still owns the row capability until it consumes it.
            BindingLifecycle::Committed
            | BindingLifecycle::Activating
            | BindingLifecycle::Active
            | BindingLifecycle::Cancelling => Ok(()),
            BindingLifecycle::Prepared | BindingLifecycle::Aborted => Err(AxError::ResourceBusy),
        }
    }

    /// Rolls a committed-but-never-activated transaction back. The source
    /// fence has already been sealed by `PreparedBinding::commit`, so the
    /// rollback reopens that exact source gate before returning it to the
    /// Runnable provider state. Activation races are serialized by the
    /// authority lock: an Active binding is left untouched.
    fn abort_committed_binding(
        &self,
        binding: BindingId,
        operation: AuthorityOperationId,
        digest: RequestDigest,
    ) -> AxResult<()> {
        let mut state = self.state.lock();
        let binding_index = Self::binding_index(&state, binding).ok_or(AxError::BadState)?;
        let record = state.bindings[binding_index]
            .as_ref()
            .ok_or(AxError::BadState)?;
        if record.lifecycle != BindingLifecycle::Committed || record.operation != operation {
            return Err(AxError::ResourceBusy);
        }
        let source_coordinate = record.source;
        let fence_epoch = record.fence_epoch;
        if !record
            .sealed_source
            .as_ref()
            .is_some_and(|sealed| sealed.epoch() == fence_epoch && sealed.proof_valid())
        {
            return Err(AxError::ResourceBusy);
        }
        let operation_index = Self::operation_index(&state, operation).ok_or(AxError::BadState)?;
        let operation_record = state.operations[operation_index]
            .as_ref()
            .ok_or(AxError::BadState)?;
        if operation_record.digest != digest
            || operation_record.status != OperationStatus::Applied
            || operation_record.binding != Some(binding)
        {
            return Err(AxError::InvalidInput);
        }
        let source_index =
            Self::provider_index(&state, source_coordinate).ok_or(AxError::BadState)?;
        let source_uts = state.providers[source_index]
            .as_ref()
            .filter(|provider| provider.lifecycle == ProviderLifecycle::Fenced)
            .map(|provider| provider.uts.clone())
            .ok_or(AxError::BadState)?;
        let source_gate = source_uts.execution_gate();
        if !source_gate.owner_matches(fence_epoch) || !source_gate.closed_and_drained() {
            return Err(AxError::ResourceBusy);
        }

        // Reserve cancellation under the authority lock, then drop that lock
        // before touching the UTS gate. Reopening takes its protocol spin lock
        // and must never nest beneath the authority's no-IRQ lock.
        let sealed_source = state.bindings[binding_index]
            .as_mut()
            .expect("validated committed binding")
            .sealed_source
            .take()
            .expect("validated sealed source token");
        state.bindings[binding_index]
            .as_mut()
            .expect("validated committed binding")
            .lifecycle = BindingLifecycle::Cancelling;
        drop(state);

        if sealed_source.reopen_exact().is_err() {
            let mut state = self.state.lock();
            let binding_index = Self::binding_index(&state, binding)
                .expect("cancelling binding disappeared during reopen");
            let record = state.bindings[binding_index]
                .as_mut()
                .expect("cancelling binding row disappeared");
            assert_eq!(record.operation, operation);
            assert_eq!(record.lifecycle, BindingLifecycle::Cancelling);
            record.sealed_source = Some(sealed_source);
            record.lifecycle = BindingLifecycle::Committed;
            return Err(AxError::ResourceBusy);
        }

        let mut state = self.state.lock();
        let binding_index = Self::binding_index(&state, binding)
            .expect("cancelling binding disappeared after reopen");
        let operation_index = Self::operation_index(&state, operation)
            .expect("cancelling operation disappeared after reopen");
        let source_index = Self::provider_index(&state, source_coordinate)
            .expect("cancelling source disappeared after reopen");
        let record = state.bindings[binding_index]
            .as_ref()
            .expect("cancelling binding row disappeared after reopen");
        assert_eq!(record.operation, operation);
        assert_eq!(record.lifecycle, BindingLifecycle::Cancelling);
        state.bindings[binding_index] = None;
        state.operations[operation_index]
            .as_mut()
            .expect("validated Applied operation")
            .status = OperationStatus::Rejected;
        state.operations[operation_index]
            .as_mut()
            .expect("validated Applied operation")
            .binding = None;
        state.providers[source_index]
            .as_mut()
            .expect("validated fenced source provider")
            .lifecycle = ProviderLifecycle::Runnable;
        Self::reap_quiescent_locked(&mut state);
        drop(state);
        // vISA keeps the exact commit receipt for replay, but its native
        // destination handle is a capability and must be released at the
        // cancellation boundary so provider reaping can make progress.
        visa_uts::release_cancelled_commit_for_operation(self, operation);
        Ok(())
    }

    fn activate_binding(self: &Arc<Self>, receipt: &ActivationReceipt) -> AxResult<ActiveBinding> {
        self.activate_binding_inner(receipt, None)
    }

    fn activate_binding_for_process(
        self: &Arc<Self>,
        receipt: &ActivationReceipt,
        process: &crate::task::ProcessData,
    ) -> AxResult<ActiveBinding> {
        self.activate_binding_inner(receipt, Some(process))
    }

    fn activate_binding_inner(
        self: &Arc<Self>,
        receipt: &ActivationReceipt,
        process: Option<&crate::task::ProcessData>,
    ) -> AxResult<ActiveBinding> {
        // Read ProcessData state before the authority lock. The actual
        // compare-and-replace below revalidates the pointer while holding the
        // process UTS write lock, so no authority -> ProcessData lock nesting
        // is needed here.
        let process_current_uts = process.map(crate::task::ProcessData::uts_ns);
        let process_has_consumer_lease =
            process.map(crate::task::ProcessData::has_uts_consumer_lease);
        let mut state = self.state.lock();
        let operation_index =
            Self::operation_index(&state, receipt.operation).ok_or(AxError::InvalidInput)?;
        let operation = state.operations[operation_index]
            .as_ref()
            .ok_or(AxError::BadState)?;
        if operation.digest != receipt.digest
            || operation.status != OperationStatus::Applied
            || operation.binding != Some(receipt.binding)
        {
            return Err(AxError::InvalidInput);
        }
        let binding_index =
            Self::binding_index(&state, receipt.binding).ok_or(AxError::BadState)?;
        let binding = state.bindings[binding_index]
            .as_ref()
            .ok_or(AxError::BadState)?;
        if binding.operation != receipt.operation {
            return Err(AxError::InvalidInput);
        }
        let destination_coordinate = binding.destination;
        let source_coordinate = binding.source;
        let source_fence_epoch = binding.fence_epoch;
        let source_index =
            Self::provider_index(&state, source_coordinate).ok_or(AxError::BadState)?;
        let destination_index =
            Self::provider_index(&state, destination_coordinate).ok_or(AxError::BadState)?;
        let source_provider = state.providers[source_index]
            .as_ref()
            .ok_or(AxError::BadState)?;
        let destination_provider = state.providers[destination_index]
            .as_ref()
            .ok_or(AxError::BadState)?;
        let source_uts = source_provider.uts.clone();
        let destination_uts = destination_provider.uts.clone();

        match binding.lifecycle {
            BindingLifecycle::Committed => {
                let source_count = source_provider.consumer_count;
                let expected_source_count = match (process, process_has_consumer_lease) {
                    (Some(_), Some(has_lease)) => usize::from(has_lease),
                    (None, None) => 0,
                    _ => return Err(AxError::ResourceBusy),
                };
                if source_count != expected_source_count
                    || source_provider.lifecycle != ProviderLifecycle::Fenced
                    || destination_provider.lifecycle != ProviderLifecycle::Closed
                {
                    return Err(AxError::ResourceBusy);
                }
                if !binding.sealed_source.as_ref().is_some_and(|sealed| {
                    sealed.epoch() == source_fence_epoch && sealed.proof_valid()
                }) {
                    return Err(AxError::ResourceBusy);
                }
                if !source_uts
                    .execution_gate()
                    .owner_matches(source_fence_epoch)
                    || !source_uts.execution_gate().closed_and_drained()
                {
                    return Err(AxError::ResourceBusy);
                }
                destination_provider
                    .consumer_count
                    .checked_add(1)
                    .ok_or(AxError::OutOfRange)?;
                destination_provider
                    .handle_count
                    .checked_add(1)
                    .ok_or(AxError::OutOfRange)?;

                let destination_handle = GenerationHandle {
                    authority: self.clone(),
                    coordinate: destination_coordinate,
                    uts: destination_uts.clone(),
                    counted: true,
                };
                let sealed_source = state.bindings[binding_index]
                    .as_mut()
                    .expect("validated activation binding")
                    .sealed_source
                    .take()
                    .expect("validated activation source fence");
                state.providers[destination_index]
                    .as_mut()
                    .expect("validated activation destination")
                    .consumer_count += 1;
                state.providers[destination_index]
                    .as_mut()
                    .expect("validated activation destination")
                    .handle_count += 1;
                state.bindings[binding_index]
                    .as_mut()
                    .expect("validated activation binding")
                    .lifecycle = BindingLifecycle::Activating;
                let reservation = ActivationReservation {
                    authority: self.clone(),
                    binding: receipt.binding,
                    operation: receipt.operation,
                    destination: destination_coordinate,
                    destination_uts: destination_uts.clone(),
                    destination_handle: Some(destination_handle),
                    sealed_source: Some(sealed_source),
                    new_consumer: Some(UtsConsumerLease {
                        authority: self.clone(),
                        coordinate: destination_coordinate,
                        uts: destination_uts,
                    }),
                    active_consumer: None,
                };
                drop(state);

                let mut reservation = reservation;
                let mut old_consumer_lease = None;
                if let Some(process) = process {
                    let switched = match process.activate_and_compare_replace_uts_ns(
                        &source_uts,
                        reservation.destination_uts.clone(),
                        |_had_consumer_lease| {
                            reservation.retire_source();
                            let new_lease = reservation
                                .new_consumer
                                .take()
                                .expect("activation destination lease missing");
                            // The ProcessData slot is the owner of the
                            // destination consumer after a process-bound
                            // activation, including processes created before
                            // Semantic World installed an initial lease. A
                            // terminal native row must not retain this lease
                            // merely because the source had no counted
                            // consumer at reservation time.
                            old_consumer_lease =
                                process.install_uts_consumer_lease(Some(new_lease));
                        },
                    ) {
                        Ok(switched) => switched,
                        Err(_) => {
                            let _ = reservation.rollback();
                            return Err(AxError::ResourceBusy);
                        }
                    };
                    if !switched {
                        let _ = reservation.rollback();
                        return Err(AxError::ResourceBusy);
                    }
                } else {
                    if reservation.destination_uts.activate_execution().is_err() {
                        let _ = reservation.rollback();
                        return Err(AxError::ResourceBusy);
                    }
                    reservation.retire_source();
                    reservation.active_consumer = reservation.new_consumer.take();
                }
                let active = reservation.finish();
                drop(old_consumer_lease);
                Ok(active)
            }
            BindingLifecycle::Active => {
                if let Some(current) = process_current_uts {
                    if !Arc::ptr_eq(&current, &destination_uts) {
                        return Err(AxError::ResourceBusy);
                    }
                    if destination_provider.lifecycle != ProviderLifecycle::Runnable {
                        return Err(AxError::ResourceBusy);
                    }
                    destination_provider
                        .handle_count
                        .checked_add(1)
                        .ok_or(AxError::OutOfRange)?;
                    let destination = GenerationHandle {
                        authority: self.clone(),
                        coordinate: destination_coordinate,
                        uts: destination_uts,
                        counted: true,
                    };
                    state.providers[destination_index]
                        .as_mut()
                        .expect("validated active destination")
                        .handle_count += 1;
                    drop(state);
                    Ok(ActiveBinding {
                        destination,
                        operation: receipt.operation,
                        consumer: None,
                    })
                } else {
                    if destination_provider.lifecycle != ProviderLifecycle::Runnable {
                        return Err(AxError::ResourceBusy);
                    }
                    destination_provider
                        .consumer_count
                        .checked_add(1)
                        .ok_or(AxError::OutOfRange)?;
                    destination_provider
                        .handle_count
                        .checked_add(1)
                        .ok_or(AxError::OutOfRange)?;
                    let destination = GenerationHandle {
                        authority: self.clone(),
                        coordinate: destination_coordinate,
                        uts: destination_uts.clone(),
                        counted: true,
                    };
                    let consumer = UtsConsumerLease {
                        authority: self.clone(),
                        coordinate: destination_coordinate,
                        uts: destination_uts,
                    };
                    state.providers[destination_index]
                        .as_mut()
                        .expect("validated active destination")
                        .consumer_count += 1;
                    state.providers[destination_index]
                        .as_mut()
                        .expect("validated active destination")
                        .handle_count += 1;
                    drop(state);
                    Ok(ActiveBinding {
                        destination,
                        operation: receipt.operation,
                        consumer: Some(consumer),
                    })
                }
            }
            BindingLifecycle::Committing
            | BindingLifecycle::Activating
            | BindingLifecycle::Cancelling
            | BindingLifecycle::Prepared
            | BindingLifecycle::Aborted => Err(AxError::ResourceBusy),
        }
    }
}

fn next_generation(
    state: &mut AuthorityState,
    source: ProviderCoordinate,
) -> AxResult<ProviderGeneration> {
    let high_water = state
        .generation_high_water
        .iter_mut()
        .find(|record| record.world == source.world && record.provider == source.provider)
        .ok_or(AxError::BadState)?;
    let next = high_water
        .generation
        .0
        .get()
        .checked_add(1)
        .ok_or(AxError::OutOfRange)?;
    high_water.generation = ProviderGeneration(NonZeroU64::new(next).unwrap());
    Ok(high_water.generation)
}

impl SemanticWorld {
    fn register_uts_provider_with_policy(
        &self,
        uts: Arc<UtsNamespace>,
        reclaimable: bool,
    ) -> AxResult<GenerationHandle> {
        let mut state = self.authority.state.lock();
        if !state.worlds.iter().any(|world| world.id == self.id) {
            return Err(AxError::BadState);
        }
        if let Some(index) = state.providers.iter().position(|provider| {
            provider.as_ref().is_some_and(|provider| {
                provider.coordinate.world == self.id && Arc::ptr_eq(&provider.uts, &uts)
            })
        }) {
            let provider = state.providers[index].as_ref().ok_or(AxError::BadState)?;
            if provider.lifecycle != ProviderLifecycle::Runnable {
                return Err(AxError::ResourceBusy);
            }
            let coordinate = provider.coordinate;
            // Hold an execution admission through the publication while the
            // authority lock is held. A concurrent fence therefore either
            // closes before registration (and this fails) or observes the
            // newly registered Runnable provider after this section.
            let registration_lease = uts.try_enter_execution().ok_or(AxError::ResourceBusy)?;
            let handle = Authority::destination_handle(&self.authority, &mut state, coordinate)?;
            drop(state);
            drop(registration_lease);
            return Ok(handle);
        }
        if state.providers.iter().any(|provider| {
            provider.as_ref().is_some_and(|provider| {
                provider.coordinate.world != self.id && Arc::ptr_eq(&provider.uts, &uts)
            })
        }) {
            return Err(AxError::InvalidInput);
        }
        if state.generation_high_water.iter().any(|high_water| {
            high_water.world == self.id
                && high_water
                    .last_reclaimed_uts
                    .upgrade()
                    .is_some_and(|previous| Arc::ptr_eq(&previous, &uts))
        }) {
            return Err(AxError::ResourceBusy);
        }
        let registration_lease = uts.try_enter_execution().ok_or(AxError::ResourceBusy)?;
        let slot = Authority::provider_slot(&mut state).ok_or(AxError::ResourceBusy)?;

        // A provider slot is a concurrent/live capacity, not a boot-lifetime
        // quota.  Reuse a logical ProviderId after every old generation has
        // retired, but advance its high-water generation before publishing
        // the new record.  Thus churn can reuse the bounded tables while any
        // stale coordinate or handle still fails exact lookup.
        let (provider, generation) = if let Some(high_water_index) =
            Authority::free_logical_provider_slot(&state, self.id)
        {
            let high_water = &mut state.generation_high_water[high_water_index];
            let next = high_water
                .generation
                .0
                .get()
                .checked_add(1)
                .ok_or(AxError::OutOfRange)?;
            high_water.world = self.id;
            high_water.generation = ProviderGeneration(
                NonZeroU64::new(next).expect("checked provider generation is non-zero"),
            );
            (high_water.provider, high_water.generation)
        } else {
            if state.generation_high_water.len() >= state.limits.providers {
                return Err(AxError::ResourceBusy);
            }
            // The vector was pre-reserved by Authority::try_new and its
            // explicit length is bounded above, so this push cannot allocate
            // while the authority lock is held.
            let provider = id(&NEXT_PROVIDER, ProviderId)?;
            let generation = ProviderGeneration(NonZeroU64::new(1).unwrap());
            state.generation_high_water.push(ProviderHighWater {
                world: self.id,
                provider,
                generation,
                last_reclaimed_uts: Weak::new(),
            });
            (provider, generation)
        };
        let coordinate = ProviderCoordinate {
            world: self.id,
            provider,
            generation,
        };
        if slot == state.providers.len() {
            state.providers.push(None);
        }
        if let Some(high_water) = state
            .generation_high_water
            .iter_mut()
            .find(|record| record.world == self.id && record.provider == provider)
        {
            high_water.last_reclaimed_uts = Weak::new();
        }
        state.providers[slot] = Some(ProviderRecord {
            coordinate,
            lifecycle: ProviderLifecycle::Runnable,
            uts: uts.clone(),
            reclaimable,
            consumer_count: 0,
            handle_count: 1,
            execution_lease_count: 0,
            fence_count: 0,
        });
        let handle = GenerationHandle {
            authority: self.authority.clone(),
            coordinate,
            uts,
            counted: true,
        };
        drop(state);
        drop(registration_lease);
        Ok(handle)
    }

    pub(crate) fn register_uts_provider(
        &self,
        uts: Arc<UtsNamespace>,
    ) -> AxResult<GenerationHandle> {
        self.register_uts_provider_with_policy(uts, true)
    }

    fn register_initial_uts_provider(&self, uts: Arc<UtsNamespace>) -> AxResult<GenerationHandle> {
        self.register_uts_provider_with_policy(uts, false)
    }

    fn acquire_uts_consumer(&self, uts: Arc<UtsNamespace>) -> AxResult<UtsConsumerLease> {
        // Registration and lease publication are normally one short lock
        // section. If the namespace was not registered yet, allocation must
        // happen outside that lock; retry once if a concurrent quiescence
        // retired the just-registered slot before the count was published.
        for _ in 0..2 {
            let mut state = self.authority.state.lock();
            if let Some(index) = state.providers.iter().position(|provider| {
                provider.as_ref().is_some_and(|provider| {
                    provider.coordinate.world == self.id && Arc::ptr_eq(&provider.uts, &uts)
                })
            }) {
                let provider = state.providers[index].as_mut().ok_or(AxError::BadState)?;
                if provider.lifecycle != ProviderLifecycle::Runnable {
                    return Err(AxError::ResourceBusy);
                }
                let provider_uts = provider.uts.clone();
                // This admission is the linearization point for the
                // canonical consumer lease. It is serialized with a fence's
                // CLOSED transition by the execution gate; a closed gate
                // cannot gain a new consumer merely because its lifecycle row
                // is still Runnable during preparation.
                let admission = provider_uts
                    .try_enter_execution()
                    .ok_or(AxError::ResourceBusy)?;
                provider.consumer_count = provider
                    .consumer_count
                    .checked_add(1)
                    .ok_or(AxError::OutOfRange)?;
                drop(admission);
                return Ok(UtsConsumerLease {
                    authority: self.authority.clone(),
                    coordinate: provider.coordinate,
                    uts,
                });
            }
            if state.providers.iter().any(|provider| {
                provider.as_ref().is_some_and(|provider| {
                    provider.coordinate.world != self.id && Arc::ptr_eq(&provider.uts, &uts)
                })
            }) {
                return Err(AxError::InvalidInput);
            }
            drop(state);
            let provider = self.register_uts_provider_with_policy(uts.clone(), true)?;
            let mut state = self.authority.state.lock();
            let Some(index) = Authority::provider_index(&state, provider.coordinate) else {
                continue;
            };
            let record = state.providers[index].as_mut().ok_or(AxError::BadState)?;
            if record.lifecycle != ProviderLifecycle::Runnable {
                return Err(AxError::ResourceBusy);
            }
            let admission = record
                .uts
                .try_enter_execution()
                .ok_or(AxError::ResourceBusy)?;
            record.consumer_count = record
                .consumer_count
                .checked_add(1)
                .ok_or(AxError::OutOfRange)?;
            drop(admission);
            return Ok(UtsConsumerLease {
                authority: self.authority.clone(),
                coordinate: provider.coordinate,
                uts,
            });
        }
        Err(AxError::ResourceBusy)
    }

    pub(crate) fn prepare_uts_binding(
        &self,
        token: &OperationToken,
        digest: [u8; 32],
        captured: CapturedProviderState,
    ) -> AxResult<PrepareOutcome> {
        if captured.source.world != self.id {
            return Err(AxError::InvalidInput);
        }
        let (result, _captured) =
            self.prepare_uts_binding_preserving_capture(token, digest, captured);
        result
    }

    /// Runs preparation while retaining the captured source token until the
    /// caller has durably recorded either a prepared binding or an exact
    /// source-restoration obligation.  The ordinary world wrapper drops the
    /// returned token on non-prepared outcomes; the vISA adapter puts it back
    /// into its queryable capture row instead.
    pub(crate) fn prepare_uts_binding_preserving_capture(
        &self,
        token: &OperationToken,
        digest: [u8; 32],
        captured: CapturedProviderState,
    ) -> (AxResult<PrepareOutcome>, Option<CapturedProviderState>) {
        if captured.source.world != self.id {
            return (Err(AxError::InvalidInput), Some(captured));
        }
        let mut captured = Some(captured);
        let result = Authority::prepare_binding(
            &self.authority,
            token,
            RequestDigest(digest),
            &mut captured,
        );
        (result, captured)
    }

    pub(crate) fn retry_operation(
        &self,
        token: &OperationToken,
        digest: [u8; 32],
    ) -> AxResult<PrepareOutcome> {
        if !Arc::ptr_eq(&self.authority, &token.authority) {
            return Err(AxError::InvalidInput);
        }
        self.authority.retry_operation(token, RequestDigest(digest))
    }

    /// Explicitly terminates an abandoned request. The operation id and full
    /// digest must match exactly; a prepared binding is revoked with its
    /// recorded fence epoch, so a lost RAII owner cannot strand the source.
    pub(crate) fn cancel_operation(
        &self,
        token: &OperationToken,
        digest: [u8; 32],
    ) -> AxResult<()> {
        if !Arc::ptr_eq(&self.authority, &token.authority) {
            return Err(AxError::InvalidInput);
        }
        self.authority
            .cancel_operation(token, RequestDigest(digest))
    }
}

impl Clone for GenerationHandle {
    fn clone(&self) -> Self {
        let counted = if self.counted {
            let mut state = self.authority.state.lock();
            match Authority::provider_index(&state, self.coordinate)
                .and_then(|index| state.providers[index].as_mut())
            {
                Some(provider) if Arc::ptr_eq(&provider.uts, &self.uts) => {
                    provider.handle_count = provider
                        .handle_count
                        .checked_add(1)
                        .expect("generation handle count overflow");
                    true
                }
                _ => false,
            }
        } else {
            false
        };
        Self {
            authority: self.authority.clone(),
            coordinate: self.coordinate,
            uts: self.uts.clone(),
            counted,
        }
    }
}

impl Drop for GenerationHandle {
    fn drop(&mut self) {
        if !self.counted {
            return;
        }
        let mut state = self.authority.state.lock();
        let Some(index) = Authority::provider_index(&state, self.coordinate) else {
            return;
        };
        let Some(provider) = state.providers[index].as_mut() else {
            return;
        };
        if !Arc::ptr_eq(&provider.uts, &self.uts) {
            return;
        }
        provider.handle_count = provider
            .handle_count
            .checked_sub(1)
            .expect("generation handle underflow");
        Authority::reap_quiescent_locked(&mut state);
    }
}

impl GenerationHandle {
    pub(crate) fn enter(&self) -> Option<ExecutionLease> {
        let mut state = self.authority.state.lock();
        let index = Authority::provider_index(&state, self.coordinate)?;
        let provider = state.providers[index].as_mut()?;
        if provider.lifecycle != ProviderLifecycle::Runnable
            || !Arc::ptr_eq(&provider.uts, &self.uts)
        {
            return None;
        }
        // Keep the authority row pinned while the execution admission is
        // taken. This prevents a concurrent fence/retirement from making a
        // stale handle appear Runnable between the two checks.
        let lease = self.uts.try_enter_execution()?;
        provider.execution_lease_count = provider.execution_lease_count.checked_add(1)?;
        drop(state);
        Some(lease.with_provider_pin(ProviderLeasePin {
            authority: self.authority.clone(),
            coordinate: self.coordinate,
            uts: self.uts.clone(),
            kind: ProviderPinKind::ExecutionLease,
        }))
    }

    pub(crate) fn begin_fence(&self) -> AxResult<GenerationFence> {
        {
            let state = self.authority.state.lock();
            let index =
                Authority::provider_index(&state, self.coordinate).ok_or(AxError::BadState)?;
            let provider = state.providers[index].as_ref().ok_or(AxError::BadState)?;
            if provider.lifecycle != ProviderLifecycle::Runnable
                || !Arc::ptr_eq(&provider.uts, &self.uts)
            {
                return Err(AxError::ResourceBusy);
            }
            // A multi-consumer source would require a bounded cohort switch.
            // Do not even close its gate: the pilot's migration contract is
            // one canonical movable consumer (or an authority-only source).
            if provider.consumer_count > 1 {
                return Err(AxError::ResourceBusy);
            }
        }

        // The gate protocol is a separate lock domain. Never take it while
        // holding the authority's SpinNoIrq state lock; this path may contend
        // with ordinary UTS operations and must not create an authority ->
        // UTS lock inversion.
        let execution = self.uts.freeze_execution()?;
        let mut state = self.authority.state.lock();
        let Some(index) = Authority::provider_index(&state, self.coordinate) else {
            drop(state);
            let _ = execution.rollback();
            return Err(AxError::BadState);
        };
        let Some(provider) = state.providers[index].as_mut() else {
            drop(state);
            let _ = execution.rollback();
            return Err(AxError::BadState);
        };
        if provider.lifecycle != ProviderLifecycle::Runnable
            || !Arc::ptr_eq(&provider.uts, &self.uts)
            || provider.consumer_count > 1
        {
            drop(state);
            let _ = execution.rollback();
            return Err(AxError::ResourceBusy);
        }
        if let Err(error) = provider
            .fence_count
            .checked_add(1)
            .ok_or(AxError::OutOfRange)
        {
            drop(state);
            let _ = execution.rollback();
            return Err(error);
        }
        provider.fence_count += 1;
        drop(state);
        Ok(GenerationFence {
            authority: self.authority.clone(),
            source: self.coordinate,
            uts: self.uts.clone(),
            execution: Some(execution),
            provider_pin: Some(ProviderLeasePin {
                authority: self.authority.clone(),
                coordinate: self.coordinate,
                uts: self.uts.clone(),
                kind: ProviderPinKind::Fence,
            }),
        })
    }

    pub(crate) fn capture_after_fence(
        &self,
        fence: GenerationFence,
    ) -> AxResult<CapturedProviderState> {
        if fence.source != self.coordinate || !Arc::ptr_eq(&fence.uts, &self.uts) {
            return Err(AxError::InvalidInput);
        }
        let execution = fence.execution.as_ref().ok_or(AxError::BadState)?;
        execution.wait_for_drain()?;
        let snapshot = self.uts.try_provider_snapshot_after_fence(execution)?;
        Ok(CapturedProviderState {
            authority: self.authority.clone(),
            source: self.coordinate,
            owner_user_ns: self.uts.owner_user_ns().clone(),
            snapshot,
            fence: Some(fence),
        })
    }

    pub(crate) fn snapshot(&self) -> AxResult<UtsProviderSnapshot> {
        {
            let state = self.authority.state.lock();
            let index =
                Authority::provider_index(&state, self.coordinate).ok_or(AxError::BadState)?;
            let provider = state.providers[index].as_ref().ok_or(AxError::BadState)?;
            if provider.lifecycle != ProviderLifecycle::Runnable
                || !Arc::ptr_eq(&provider.uts, &self.uts)
            {
                return Err(AxError::ResourceBusy);
            }
        }

        // Snapshotting takes the UTS execution/state locks. Keep the
        // authority lock dropped, then revalidate the exact generation row so
        // a stale handle cannot publish a snapshot from a replaced provider.
        let snapshot = self.uts.try_provider_snapshot()?;
        let state = self.authority.state.lock();
        let index = Authority::provider_index(&state, self.coordinate).ok_or(AxError::BadState)?;
        let provider = state.providers[index].as_ref().ok_or(AxError::BadState)?;
        if provider.lifecycle != ProviderLifecycle::Runnable
            || !Arc::ptr_eq(&provider.uts, &self.uts)
        {
            return Err(AxError::ResourceBusy);
        }
        Ok(snapshot)
    }
}

impl CapturedProviderState {
    fn source_epoch(&self) -> ExecutionEpoch {
        self.fence
            .as_ref()
            .and_then(|fence| fence.execution.as_ref())
            .map(ExecutionFence::execution_epoch)
            .expect("captured provider state lost its fence")
    }
}

impl PreparedBinding {
    /// Returns the exact source-fence epoch that `commit` will seal.  The
    /// value is available before the irreversible transition, so callers can
    /// construct and validate durable commit receipts before consuming the
    /// prepared binding.
    pub(crate) fn source_fence_epoch(&self) -> Option<u64> {
        self.captured
            .as_ref()?
            .fence
            .as_ref()?
            .execution
            .as_ref()
            .map(ExecutionFence::epoch)
            .map(|epoch| epoch.0.get())
    }

    /// Returns the execution epoch bound to the captured source cut.  This
    /// is likewise stable across the pre-commit transition.
    pub(crate) fn execution_epoch(&self) -> Option<u64> {
        self.captured
            .as_ref()?
            .fence
            .as_ref()?
            .execution
            .as_ref()
            .map(ExecutionFence::execution_epoch)
            .map(|epoch| epoch.0.get())
    }

    /// Commits while retaining the prepared object on an error.  The vISA
    /// adapter uses this form while a native commit-row reservation is in
    /// flight; a failed pre-commit validation can then restore the exact
    /// prepared token instead of losing it through `Drop`.
    pub(crate) fn commit_in_place(&mut self) -> AxResult<CommittedBinding> {
        self.commit_in_place_with_publication(false)
    }

    /// Commits the canonical operation while holding it in the world-side
    /// publication phase.  vISA uses this only after reserving its native
    /// commit row; the row is filled with the returned handle before the
    /// authority transitions Committing -> Committed.
    pub(crate) fn commit_in_place_pending(&mut self) -> AxResult<CommittedBinding> {
        self.commit_in_place_with_publication(true)
    }

    fn commit_in_place_with_publication(
        &mut self,
        defer_publication: bool,
    ) -> AxResult<CommittedBinding> {
        let authority = self.authority.clone();
        let result = authority.commit_binding(self, defer_publication);
        if result.is_ok() {
            // `commit_binding` moved the rollback guard into the canonical
            // sealed binding before exposing success.
            self.captured.take();
            self.armed = false;
        }
        result
    }

    pub(crate) fn commit(mut self) -> AxResult<CommittedBinding> {
        self.commit_in_place()
    }

    pub(crate) fn abort(mut self) -> Option<CapturedProviderState> {
        if self.armed {
            self.authority.abort_binding(self.binding, self.operation);
            self.armed = false;
        }
        self.captured.take()
    }

    /// Aborts a prepared binding after proving that its opaque capture token
    /// is still present. The vISA authority uses this form after reserving its
    /// durable abort row, so an impossible missing-token state cannot silently
    /// consume the preparation without an exact source fact.
    pub(crate) fn abort_prepared(mut self) -> CapturedProviderState {
        let captured = self
            .captured
            .take()
            .expect("prepared binding lost its capture token");
        if self.armed {
            self.authority.abort_binding(self.binding, self.operation);
            self.armed = false;
        }
        captured
    }

    fn operation(&self) -> AuthorityOperationId {
        self.operation
    }
}

impl Drop for PreparedBinding {
    fn drop(&mut self) {
        if self.armed {
            self.authority.abort_binding(self.binding, self.operation);
            self.armed = false;
        }
    }
}

impl CommittedBinding {
    pub(crate) fn destination(&self) -> &GenerationHandle {
        &self.destination
    }

    fn operation(&self) -> AuthorityOperationId {
        self.operation
    }

    fn binding(&self) -> BindingId {
        self.binding
    }

    pub(crate) fn source_fence_epoch(&self) -> u64 {
        self.source_fence_epoch.0.get()
    }

    pub(crate) fn execution_epoch(&self) -> u64 {
        self.execution_epoch.0.get()
    }
}

impl ActivationReceipt {
    /// Activates the authority binding without selecting a process consumer.
    /// Call `activate_for_process` when a live ProcessData owner is part of
    /// the transaction.
    pub(crate) fn activate(&self) -> AxResult<ActiveBinding> {
        self.authority.activate_binding(self)
    }

    /// Activates and atomically switches one canonical ProcessData consumer
    /// from the exact fenced source generation to the destination. A stale
    /// process binding is rejected instead of silently replacing its current
    /// namespace.
    pub(crate) fn activate_for_process(
        &self,
        process: &crate::task::ProcessData,
    ) -> AxResult<ActiveBinding> {
        self.authority.activate_binding_for_process(self, process)
    }

    /// Cancels an Applied receipt that has not yet been activated. This is
    /// the exact cleanup edge for a caller that retained only a recovered
    /// receipt after losing its `CommittedBinding` acknowledgement.
    pub(crate) fn cancel(&self) -> AxResult<()> {
        self.authority
            .abort_committed_binding(self.binding, self.operation, self.digest)
    }
}

impl ActiveBinding {
    pub(crate) fn destination(&self) -> &GenerationHandle {
        &self.destination
    }

    /// Releases the capability-like generation handle while retaining only
    /// the optional consumer lease needed by a ProcessData that predates
    /// Semantic World lease initialization. The handle is never retained in
    /// a terminal vISA row after activation; dropping it here occurs outside
    /// the native ledger lock.
    pub(crate) fn into_consumer(self) -> Option<UtsConsumerLease> {
        let Self {
            destination,
            operation: _,
            consumer,
        } = self;
        drop(destination);
        consumer
    }

    fn operation(&self) -> AuthorityOperationId {
        self.operation
    }

    pub(crate) fn destination_uts(&self) -> Arc<UtsNamespace> {
        self.destination.uts.clone()
    }
}

impl Drop for UtsConsumerLease {
    fn drop(&mut self) {
        self.authority
            .release_uts_consumer(self.coordinate, &self.uts);
    }
}

impl AuthorityQueryHandle {
    pub(crate) fn query_operation(
        &self,
        operation: AuthorityOperationId,
        digest: [u8; 32],
    ) -> AuthorityQuery {
        self.authority
            .upgrade()
            .map_or(AuthorityQuery::AuthorityGone, |authority| {
                authority.query_operation(operation, RequestDigest(digest))
            })
    }
}

impl SemanticWorld {
    /// Resolves the current host-local generation handle for an exact UTS
    /// object. This is a control-path lookup used by the vISA adapter; normal
    /// UTS reads and writes do not consult the authority.
    pub(crate) fn generation_for_uts(&self, uts: &Arc<UtsNamespace>) -> AxResult<GenerationHandle> {
        let mut state = self.authority.state.lock();
        let provider = state.providers.iter().flatten().find(|provider| {
            provider.coordinate.world == self.id && Arc::ptr_eq(&provider.uts, uts)
        });
        let Some(provider) = provider else {
            if state.providers.iter().flatten().any(|provider| {
                provider.coordinate.world != self.id && Arc::ptr_eq(&provider.uts, uts)
            }) {
                return Err(AxError::InvalidInput);
            }
            return Err(AxError::BadState);
        };
        if provider.lifecycle != ProviderLifecycle::Runnable {
            return Err(AxError::ResourceBusy);
        }
        let coordinate = provider.coordinate;
        Authority::destination_handle(&self.authority, &mut state, coordinate)
    }
}

/// Initializes the one local authority used by the first Semantic World pilot.
pub(crate) fn init() -> AxResult<()> {
    LOCAL_AUTHORITY
        .try_call_once(|| Authority::try_new(AuthorityCapacity::default()))
        .map(|_| ())
}

pub(crate) fn register_initial_uts(uts: &Arc<UtsNamespace>) -> AxResult<()> {
    if LOCAL_UTS_PROVIDER.get().is_some() {
        return Ok(());
    }
    let authority = LOCAL_AUTHORITY.get().ok_or(AxError::BadState)?.clone();
    let world = authority.try_new_world()?;
    let provider = world.register_initial_uts_provider(uts.clone())?;
    LOCAL_WORLD
        .try_call_once(|| Ok::<SemanticWorld, AxError>(world))
        .map_err(|_| AxError::BadState)?;
    LOCAL_UTS_PROVIDER
        .try_call_once(|| Ok::<GenerationHandle, AxError>(provider))
        .map(|_| ())
        .map_err(|_| AxError::BadState)
}

/// Registers one ProcessData UTS consumer with the local authority when the
/// Semantic World pilot is initialized. Ordinary UTS reads/writes never call
/// this resolver; only namespace lifecycle publication does.
pub(crate) fn acquire_uts_consumer(uts: &Arc<UtsNamespace>) -> AxResult<Option<UtsConsumerLease>> {
    let Some(world) = LOCAL_WORLD.get() else {
        return Ok(None);
    };
    world.acquire_uts_consumer(uts.clone()).map(Some)
}

pub(crate) fn generation_for_uts(uts: &Arc<UtsNamespace>) -> AxResult<GenerationHandle> {
    LOCAL_WORLD
        .get()
        .ok_or(AxError::BadState)?
        .generation_for_uts(uts)
}

pub(crate) fn continue_uts_provider_state(
    process: &crate::task::ProcessData,
    operation: visa_core::OperationId,
) -> AxResult<()> {
    visa_uts::continue_uts_provider_state(process, operation)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    const OLD: [u8; 32] = *b"uts-pilot-request-old...........";
    const NEW: [u8; 32] = *b"uts-pilot-request-new...........";
    const OTHER: [u8; 32] = *b"uts-pilot-request-other.........";

    fn authority(capacity: AuthorityCapacity) -> Arc<Authority> {
        Authority::try_new(capacity).unwrap()
    }

    fn root(capacity: AuthorityCapacity) -> (Arc<Authority>, SemanticWorld, GenerationHandle) {
        let authority = authority(capacity);
        let world = authority.try_new_world().unwrap();
        let owner = UserNamespace::try_new_root().unwrap();
        let uts = UtsNamespace::try_new_root(owner).unwrap();
        let provider = world.register_uts_provider(uts).unwrap();
        (authority, world, provider)
    }

    fn captured(provider: &GenerationHandle) -> CapturedProviderState {
        provider
            .capture_after_fence(provider.begin_fence().unwrap())
            .unwrap()
    }

    #[test]
    fn real_uts_vertical_slice_fences_mutation_captures_new_state_and_activates() {
        let (authority, world, source) = root(AuthorityCapacity {
            worlds: 1,
            providers: 2,
            bindings: 1,
            terminal_operations: 2,
        });
        source.uts.set_nodename(b"new-host").unwrap();
        source.uts.set_domainname(b"new-domain").unwrap();
        let lease = source.enter().unwrap();
        let fence = source.begin_fence().unwrap();
        assert_eq!(
            source.uts.set_nodename(b"blocked"),
            Err(AxError::ResourceBusy)
        );
        assert!(matches!(source.uts.nodename(), Err(AxError::ResourceBusy)));
        drop(lease);
        let captured = source.capture_after_fence(fence).unwrap();
        assert_eq!(captured.snapshot.nodename(), b"new-host");
        let token = authority.reserve_operation(OLD).unwrap();
        let prepared = match world.prepare_uts_binding(&token, OLD, captured).unwrap() {
            PrepareOutcome::Prepared(binding) => binding,
            _ => panic!("expected prepared binding"),
        };
        let operation = prepared.operation();
        let _committed = prepared.commit().unwrap();
        assert!(source.uts.set_nodename(b"after-commit").is_err());
        assert!(matches!(source.uts.nodename(), Err(AxError::ResourceBusy)));
        let receipt = match authority.query_handle().query_operation(operation, OLD) {
            AuthorityQuery::Applied(receipt) => receipt,
            _ => panic!("expected applied receipt"),
        };
        let active = receipt.activate().unwrap();
        assert_eq!(
            active.destination().snapshot().unwrap().nodename(),
            b"new-host"
        );
        let destination = active.destination_uts();
        assert_eq!(destination.nodename().unwrap(), b"new-host");
        assert_eq!(active.operation(), operation);
    }

    #[test]
    fn deferred_commit_publication_is_idempotent_after_activation() {
        let (authority, world, source) = root(AuthorityCapacity {
            worlds: 1,
            providers: 2,
            bindings: 1,
            terminal_operations: 2,
        });
        let token = authority.reserve_operation(OLD).unwrap();
        let mut prepared = match world
            .prepare_uts_binding(&token, OLD, captured(&source))
            .unwrap()
        {
            PrepareOutcome::Prepared(binding) => binding,
            _ => panic!("expected prepared binding"),
        };
        let operation = prepared.operation();
        let binding = prepared.binding;
        let committed = prepared.commit_in_place_pending().unwrap();

        // Model the native-row publication before activation, then repeat
        // the same handshake after the world has advanced through Active.
        // A concurrent commit-row retry in this interval must observe an
        // already-published relation, not turn a recoverable state into a
        // panic while the committed capability is still native-owned.
        authority
            .finish_commit_publication(binding, operation)
            .unwrap();
        let receipt = match authority.query_handle().query_operation(operation, OLD) {
            AuthorityQuery::Applied(receipt) => receipt,
            _ => panic!("expected applied receipt"),
        };
        let active = receipt.activate().unwrap();
        authority
            .finish_commit_publication(binding, operation)
            .unwrap();
        drop(active);
        drop(committed);
    }

    #[test]
    fn double_fence_and_fence_drop_are_safe() {
        let (_, _, provider) = root(AuthorityCapacity::default());
        let fence = provider.begin_fence().unwrap();
        assert!(matches!(provider.begin_fence(), Err(AxError::ResourceBusy)));
        drop(fence);
        assert!(provider.enter().is_some());
    }

    #[test]
    fn fence_drop_is_nonblocking_and_fork_is_fenced() {
        let (_, _, provider) = root(AuthorityCapacity::default());
        let owner = provider.uts.owner_user_ns().clone();
        let lease = provider.enter().unwrap();
        let fence = provider.begin_fence().unwrap();
        assert!(matches!(
            provider.uts.try_fork(owner.clone()),
            Err(AxError::ResourceBusy)
        ));

        // A dropped RAII fence must reopen atomically even while an old
        // execution lease is still active; otherwise Drop would block.
        drop(fence);
        let replacement_lease = provider.enter().unwrap();
        drop(replacement_lease);
        drop(lease);
        assert!(provider.uts.try_fork(owner).is_ok());
    }

    #[test]
    fn stale_fence_cannot_reopen_a_new_owner() {
        let (_, _, provider) = root(AuthorityCapacity::default());
        let first = provider.begin_fence().unwrap();
        let gate = provider.uts.execution_gate();
        let first_epoch = first
            .execution
            .as_ref()
            .expect("generation fence owns execution fence")
            .epoch();
        gate.reopen_nonblocking(first_epoch).unwrap();
        let second = provider.begin_fence().unwrap();
        drop(first);
        assert!(provider.enter().is_none());
        drop(second);
        assert!(provider.enter().is_some());
    }

    #[test]
    fn activation_cannot_reopen_an_owned_fence() {
        let (_, _, provider) = root(AuthorityCapacity::default());
        let fence = provider.uts.freeze_execution().unwrap();
        assert_eq!(
            provider.uts.activate_execution(),
            Err(AxError::ResourceBusy)
        );
        drop(fence);
        assert!(provider.enter().is_some());
    }

    #[test]
    fn gate_protocol_orders_closed_bit_before_owner_observation() {
        let (_, _, provider) = root(AuthorityCapacity::default());
        let gate = provider.uts.execution_gate();
        let protocol = gate.protocol.lock();
        // Model the dangerous publication window directly: the CLOSED bit is
        // visible first, while the fence owner is still being published. The
        // activation thread must wait for the protocol lock and then observe
        // the owner instead of reopening the gate.
        gate.state.store(CLOSED_BIT, Ordering::Release);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let activation_gate = gate.clone();
        let worker = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            activation_gate.activate()
        });
        ready_rx.recv().unwrap();
        gate.fence_owner.store(1, Ordering::Release);
        drop(protocol);
        assert_eq!(worker.join().unwrap(), Err(AxError::ResourceBusy));
        gate.fence_owner.store(0, Ordering::Release);
        gate.state.store(0, Ordering::Release);
    }

    #[test]
    fn same_operation_is_idempotent_and_different_digest_conflicts() {
        let (authority, world, provider) = root(AuthorityCapacity::default());
        let token = authority.reserve_operation(OLD).unwrap();
        let captured = captured(&provider);
        let _prepared = match world.prepare_uts_binding(&token, OLD, captured).unwrap() {
            PrepareOutcome::Prepared(binding) => binding,
            _ => panic!("expected prepared binding"),
        };
        assert!(matches!(
            world.retry_operation(&token, OLD).unwrap(),
            PrepareOutcome::AlreadyPending
        ));
        assert!(matches!(
            world.retry_operation(&token, OTHER).unwrap(),
            PrepareOutcome::Conflict
        ));
        // A wrong-digest retry is only a conflict view; it must not poison
        // the canonical Pending row for the original owner.
        assert!(matches!(
            authority
                .query_handle()
                .query_operation(token.operation(), OLD),
            AuthorityQuery::Pending
        ));
        assert!(matches!(
            authority
                .query_handle()
                .query_operation(token.operation(), OTHER),
            AuthorityQuery::Conflict
        ));
    }

    #[test]
    fn lost_commit_ack_is_recoverable_from_applied_receipt() {
        let (authority, world, provider) = root(AuthorityCapacity::default());
        let operation_id = Authority::allocate_operation_id().unwrap();
        let token = authority
            .reserve_operation_with_id(operation_id, NEW)
            .unwrap();
        // A caller that retained the private id can recover a lost reserve
        // acknowledgement without any numeric id reconstruction.
        let _retry_token = authority
            .reserve_operation_with_id(operation_id, NEW)
            .unwrap();
        assert!(matches!(
            authority.reserve_operation_with_id(operation_id, OLD),
            Err(AxError::InvalidInput)
        ));
        let prepared = match world
            .prepare_uts_binding(&token, NEW, captured(&provider))
            .unwrap()
        {
            PrepareOutcome::Prepared(binding) => binding,
            _ => panic!("expected prepared binding"),
        };
        let operation = prepared.operation();
        drop(prepared.commit().unwrap());
        assert!(matches!(
            authority.query_handle().query_operation(operation, OLD),
            AuthorityQuery::Conflict
        ));
        let receipt = match authority.query_handle().query_operation(operation, NEW) {
            AuthorityQuery::Applied(receipt) => receipt,
            _ => panic!("expected applied receipt"),
        };
        let wrong_receipt = ActivationReceipt {
            authority: authority.clone(),
            binding: receipt.binding,
            operation: receipt.operation,
            digest: RequestDigest(OLD),
        };
        assert!(matches!(
            wrong_receipt.activate(),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(wrong_receipt.cancel(), Err(AxError::InvalidInput)));
        assert!(receipt.activate().is_ok());
    }

    #[test]
    fn aborted_binding_reuses_provider_and_binding_slots() {
        let (authority, world, provider) = root(AuthorityCapacity {
            worlds: 1,
            providers: 2,
            bindings: 1,
            terminal_operations: 8,
        });
        let mut previous_generation = provider.coordinate.generation.0.get();
        for index in 0..8 {
            let mut digest = [0; 32];
            digest[0] = index;
            let token = authority.reserve_operation(digest).unwrap();
            let prepared = match world
                .prepare_uts_binding(&token, digest, captured(&provider))
                .unwrap()
            {
                PrepareOutcome::Prepared(binding) => binding,
                _ => panic!("expected prepared binding"),
            };
            assert!(prepared.destination.generation.0.get() > previous_generation);
            previous_generation = prepared.destination.generation.0.get();
            let stale_destination = prepared.destination;
            prepared.abort();
            {
                let state = authority.state.lock();
                assert!(Authority::provider_index(&state, stale_destination).is_none());
            }
            assert!(matches!(
                authority
                    .query_handle()
                    .query_operation(token.operation(), digest),
                AuthorityQuery::Rejected
            ));
        }
    }

    #[test]
    fn explicit_capacity_limits_admission_not_allocator_capacity() {
        let (authority, world, _provider) = root(AuthorityCapacity {
            worlds: 1,
            providers: 1,
            bindings: 1,
            terminal_operations: 1,
        });
        assert!(matches!(
            authority.try_new_world(),
            Err(AxError::ResourceBusy)
        ));
        let replacement =
            UtsNamespace::try_new_root(UserNamespace::try_new_root().unwrap()).unwrap();
        assert!(matches!(
            world.register_uts_provider(replacement),
            Err(AxError::ResourceBusy)
        ));
        let digest = [7; 32];
        let _operation = authority.reserve_operation(digest).unwrap();
        assert!(matches!(
            authority.reserve_operation([8; 32]),
            Err(AxError::ResourceBusy)
        ));
    }

    #[test]
    fn failed_prepare_can_be_exactly_cancelled_instead_of_staying_pending() {
        let (authority, world, provider) = root(AuthorityCapacity {
            worlds: 1,
            providers: 1,
            bindings: 1,
            terminal_operations: 2,
        });
        let token = authority.reserve_operation(OLD).unwrap();
        assert!(matches!(
            world.prepare_uts_binding(&token, OLD, captured(&provider)),
            Err(AxError::ResourceBusy)
        ));
        world.cancel_operation(&token, OLD).unwrap();
        assert!(matches!(
            authority
                .query_handle()
                .query_operation(token.operation(), OLD),
            AuthorityQuery::Rejected
        ));
        let _next = authority.reserve_operation(NEW).unwrap();
    }

    #[test]
    fn prepared_relation_survives_unrelated_consumer_reaping() {
        let (authority, world, source) = root(AuthorityCapacity {
            worlds: 1,
            providers: 3,
            bindings: 1,
            terminal_operations: 2,
        });
        let extra_uts = UtsNamespace::try_new_root(UserNamespace::try_new_root().unwrap()).unwrap();
        let extra = world.register_uts_provider(extra_uts.clone()).unwrap();
        let extra_lease = world.acquire_uts_consumer(extra_uts).unwrap();
        let token = authority.reserve_operation(OLD).unwrap();
        let prepared = match world
            .prepare_uts_binding(&token, OLD, captured(&source))
            .unwrap()
        {
            PrepareOutcome::Prepared(binding) => binding,
            _ => panic!("expected prepared binding"),
        };
        let destination = prepared.destination;
        drop(extra_lease);
        let state = authority.state.lock();
        assert!(Authority::provider_index(&state, destination).is_some());
        assert!(Authority::binding_index(&state, prepared.binding).is_some());
        drop(state);
        prepared.abort();
        assert!(matches!(
            authority
                .query_handle()
                .query_operation(token.operation(), OLD),
            AuthorityQuery::Rejected
        ));
        // Keep the extra handle live until after the assertion so the test
        // also proves that reaping was driven by consumer quiescence rather
        // than handle destruction.
        drop(extra);
    }

    #[test]
    fn committed_relation_survives_unrelated_consumer_reaping() {
        let (authority, world, source) = root(AuthorityCapacity {
            worlds: 1,
            providers: 3,
            bindings: 1,
            terminal_operations: 2,
        });
        let extra_uts = UtsNamespace::try_new_root(UserNamespace::try_new_root().unwrap()).unwrap();
        let extra = world.register_uts_provider(extra_uts.clone()).unwrap();
        let extra_lease = world.acquire_uts_consumer(extra_uts).unwrap();
        let token = authority.reserve_operation(OLD).unwrap();
        let prepared = match world
            .prepare_uts_binding(&token, OLD, captured(&source))
            .unwrap()
        {
            PrepareOutcome::Prepared(binding) => binding,
            _ => panic!("expected prepared binding"),
        };
        let committed = prepared.commit().unwrap();
        let destination = committed.destination.coordinate;
        drop(extra_lease);
        let state = authority.state.lock();
        assert!(Authority::provider_index(&state, source.coordinate).is_some());
        assert!(Authority::provider_index(&state, destination).is_some());
        assert!(Authority::binding_index(&state, committed.binding).is_some());
        assert!(matches!(
            authority
                .query_handle()
                .query_operation(token.operation(), OLD),
            AuthorityQuery::Applied(_)
        ));
        drop(state);
        let receipt = match authority
            .query_handle()
            .query_operation(token.operation(), OLD)
        {
            AuthorityQuery::Applied(receipt) => receipt,
            _ => panic!("expected applied receipt"),
        };
        receipt.cancel().unwrap();
        assert!(source.enter().is_some());
        drop(committed);
        drop(extra);
    }

    #[test]
    fn consumer_admission_and_registration_reject_a_closed_gate() {
        let (_, world, provider) = root(AuthorityCapacity::default());
        let mut fence = provider.uts.freeze_execution().unwrap();
        fence.wait_for_drain().unwrap();
        assert!(matches!(
            world.acquire_uts_consumer(provider.uts.clone()),
            Err(AxError::ResourceBusy)
        ));
        assert!(matches!(
            world.register_uts_provider(provider.uts.clone()),
            Err(AxError::ResourceBusy)
        ));
        fence.seal_after_drained();
        assert!(matches!(
            world.register_uts_provider(provider.uts.clone()),
            Err(AxError::ResourceBusy)
        ));
        assert!(matches!(
            world.acquire_uts_consumer(provider.uts.clone()),
            Err(AxError::ResourceBusy)
        ));
    }

    #[test]
    fn multi_consumer_source_is_rejected_before_fencing() {
        let (authority, world, source) = root(AuthorityCapacity {
            worlds: 1,
            providers: 2,
            bindings: 1,
            terminal_operations: 2,
        });
        let first = world.acquire_uts_consumer(source.uts.clone()).unwrap();
        let second = world.acquire_uts_consumer(source.uts.clone()).unwrap();
        let token = authority.reserve_operation(OLD).unwrap();
        assert!(matches!(source.begin_fence(), Err(AxError::ResourceBusy)));
        assert!(source.enter().is_some());
        drop(first);
        drop(second);
        assert!(matches!(
            authority
                .query_handle()
                .query_operation(token.operation(), OLD),
            AuthorityQuery::Pending
        ));
        world.cancel_operation(&token, OLD).unwrap();
    }

    #[test]
    fn authority_only_activation_rejects_an_unmovable_consumer() {
        let (authority, world, source) = root(AuthorityCapacity {
            worlds: 1,
            providers: 2,
            bindings: 1,
            terminal_operations: 2,
        });
        let source_lease = world.acquire_uts_consumer(source.uts.clone()).unwrap();
        let token = authority.reserve_operation(OLD).unwrap();
        let prepared = match world
            .prepare_uts_binding(&token, OLD, captured(&source))
            .unwrap()
        {
            PrepareOutcome::Prepared(binding) => binding,
            _ => panic!("expected prepared binding"),
        };
        let committed = prepared.commit().unwrap();
        let receipt = match authority
            .query_handle()
            .query_operation(token.operation(), OLD)
        {
            AuthorityQuery::Applied(receipt) => receipt,
            _ => panic!("expected applied receipt"),
        };
        assert!(matches!(receipt.activate(), Err(AxError::ResourceBusy)));
        receipt.cancel().unwrap();
        assert!(source.enter().is_some());
        drop(source_lease);
        drop(committed);
    }

    #[test]
    fn prepared_pending_operation_can_be_cancelled_by_exact_token() {
        let (authority, world, provider) = root(AuthorityCapacity {
            worlds: 1,
            providers: 2,
            bindings: 1,
            terminal_operations: 1,
        });
        let token = authority.reserve_operation(OLD).unwrap();
        let prepared = match world
            .prepare_uts_binding(&token, OLD, captured(&provider))
            .unwrap()
        {
            PrepareOutcome::Prepared(binding) => binding,
            _ => panic!("expected prepared binding"),
        };
        world.cancel_operation(&token, OLD).unwrap();
        assert!(provider.enter().is_some());
        drop(prepared);
        assert!(matches!(
            authority
                .query_handle()
                .query_operation(token.operation(), OLD),
            AuthorityQuery::Rejected
        ));
    }

    #[test]
    fn consumer_quiescence_retires_reclaimable_provider_slot() {
        let (_, world, provider) = root(AuthorityCapacity {
            worlds: 1,
            providers: 1,
            bindings: 1,
            terminal_operations: 1,
        });
        let lease = world.acquire_uts_consumer(provider.uts.clone()).unwrap();
        drop(lease);
        // A live generation handle pins its row after the ProcessData
        // consumer quiesces. Same-Arc registration resolves to that exact
        // generation instead of issuing a replacement generation.
        assert!(provider.enter().is_some());
        let same_generation = world.register_uts_provider(provider.uts.clone()).unwrap();
        assert_eq!(same_generation.coordinate, provider.coordinate);
        drop(same_generation);
        drop(provider);
        let owner = UserNamespace::try_new_root().unwrap();
        let replacement = UtsNamespace::try_new_root(owner).unwrap();
        assert!(world.register_uts_provider(replacement).is_ok());
    }

    #[test]
    fn reclaimed_generation_rejects_same_arc_as_a_new_generation() {
        let (_, world, provider) = root(AuthorityCapacity {
            worlds: 1,
            providers: 2,
            bindings: 1,
            terminal_operations: 2,
        });
        let uts = provider.uts.clone();
        let coordinate = provider.coordinate;
        drop(provider);
        assert!(
            world
                .authority
                .state
                .lock()
                .providers
                .iter()
                .flatten()
                .all(|record| record.coordinate != coordinate)
        );
        assert!(matches!(
            world.register_uts_provider(uts),
            Err(AxError::ResourceBusy)
        ));
    }

    #[test]
    fn cross_world_arc_handles_are_rejected_without_rebinding() {
        let authority = authority(AuthorityCapacity {
            worlds: 2,
            providers: 2,
            bindings: 1,
            terminal_operations: 2,
        });
        let world_a = authority.try_new_world().unwrap();
        let world_b = authority.try_new_world().unwrap();
        let owner = UserNamespace::try_new_root().unwrap();
        let uts = UtsNamespace::try_new_root(owner).unwrap();
        let source = world_a.register_uts_provider(uts.clone()).unwrap();
        assert!(matches!(
            world_b.register_uts_provider(uts.clone()),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            world_b.acquire_uts_consumer(uts.clone()),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            world_b.generation_for_uts(&uts),
            Err(AxError::InvalidInput)
        ));

        let captured = captured(&source);
        let token = authority.reserve_operation(OLD).unwrap();
        assert!(matches!(
            world_b.prepare_uts_binding(&token, OLD, captured),
            Err(AxError::InvalidInput)
        ));
        assert!(source.enter().is_some());
    }

    #[test]
    fn execution_lease_pins_generation_after_handle_drop() {
        let (_, world, provider) = root(AuthorityCapacity {
            worlds: 1,
            providers: 1,
            bindings: 1,
            terminal_operations: 1,
        });
        let uts = provider.uts.clone();
        let coordinate = provider.coordinate;
        let lease = provider.enter().unwrap();
        drop(provider);

        let same_generation = world.register_uts_provider(uts.clone()).unwrap();
        assert_eq!(same_generation.coordinate, coordinate);
        drop(same_generation);
        drop(lease);

        let replacement =
            UtsNamespace::try_new_root(UserNamespace::try_new_root().unwrap()).unwrap();
        assert!(world.register_uts_provider(replacement).is_ok());
    }

    #[test]
    fn provider_slot_churn_reuses_logical_id_without_generation_reuse() {
        let (authority, world, root_provider) = root(AuthorityCapacity {
            worlds: 1,
            providers: 2,
            bindings: 1,
            terminal_operations: 1,
        });
        // The root provider occupies one live slot.  Repeatedly create and
        // quiesce the other provider well beyond the concurrent capacity;
        // this must remain admitted instead of turning capacity into a
        // boot-lifetime registration quota.
        let mut stale_coordinates = Vec::new();
        let mut previous_generation = 0;
        for _ in 0..(2 * 8) {
            let owner = UserNamespace::try_new_root().unwrap();
            let uts = UtsNamespace::try_new_root(owner).unwrap();
            let provider = world.register_uts_provider(uts.clone()).unwrap();
            assert!(provider.coordinate.provider != root_provider.coordinate.provider);
            assert!(provider.coordinate.generation.0.get() > previous_generation);
            previous_generation = provider.coordinate.generation.0.get();
            let lease = world.acquire_uts_consumer(uts).unwrap();
            drop(lease);
            assert!(provider.enter().is_some());
            stale_coordinates.push(provider.coordinate);
            drop(provider);
        }
        let state = authority.state.lock();
        assert_eq!(state.generation_high_water.len(), 2);
        drop(state);
        // Every old coordinate is superseded by a larger generation, even
        // though the logical ProviderId itself is intentionally recycled.
        assert!(stale_coordinates.windows(2).all(|pair| {
            pair[0].provider == pair[1].provider && pair[0].generation != pair[1].generation
        }));
    }

    #[test]
    fn terminal_operation_slots_recycle_after_exact_quiescence() {
        let (authority, world, _provider) = root(AuthorityCapacity {
            worlds: 1,
            providers: 1,
            bindings: 1,
            terminal_operations: 1,
        });
        let mut stale = None;
        for index in 0..32u8 {
            let mut digest = [0; 32];
            digest[0] = index;
            let token = authority.reserve_operation(digest).unwrap();
            let operation = token.operation();
            world.cancel_operation(&token, digest).unwrap();
            assert!(matches!(
                authority.query_handle().query_operation(operation, digest),
                AuthorityQuery::Rejected
            ));
            if let Some((old_operation, old_digest)) = stale {
                assert!(matches!(
                    authority
                        .query_handle()
                        .query_operation(old_operation, old_digest),
                    AuthorityQuery::Absent
                ));
                assert!(matches!(
                    authority.reserve_operation_with_id(old_operation, [0xA5; 32]),
                    Err(AxError::InvalidInput)
                ));
            }
            stale = Some((operation, digest));
        }
    }

    #[test]
    fn authority_only_active_binding_owns_and_releases_destination_lease() {
        let (authority, world, source) = root(AuthorityCapacity {
            worlds: 1,
            providers: 2,
            bindings: 1,
            terminal_operations: 1,
        });
        let token = authority.reserve_operation(OLD).unwrap();
        let prepared = match world
            .prepare_uts_binding(&token, OLD, captured(&source))
            .unwrap()
        {
            PrepareOutcome::Prepared(binding) => binding,
            _ => panic!("expected prepared binding"),
        };
        let committed = prepared.commit().unwrap();
        let operation = committed.operation();
        let receipt = match authority.query_handle().query_operation(operation, OLD) {
            AuthorityQuery::Applied(receipt) => receipt,
            _ => panic!("expected applied receipt"),
        };
        let active = receipt.activate().unwrap();
        drop(committed);
        let destination = active.destination().clone();
        assert!(destination.enter().is_some());
        drop(active);
        // The explicit generation handle clone is itself a row pin. Once it
        // is dropped, the authority can reap the quiescent destination.
        let destination_coordinate = destination.coordinate;
        drop(destination);
        let state = authority.state.lock();
        assert!(Authority::provider_index(&state, destination_coordinate).is_none());
        drop(state);
        assert!(matches!(
            authority.query_handle().query_operation(operation, OLD),
            AuthorityQuery::Absent
        ));
    }

    #[test]
    fn retired_closed_uts_cannot_be_registered_again() {
        let (authority, world, source) = root(AuthorityCapacity {
            worlds: 1,
            providers: 2,
            bindings: 1,
            terminal_operations: 1,
        });
        let token = authority.reserve_operation(OLD).unwrap();
        let prepared = match world
            .prepare_uts_binding(&token, OLD, captured(&source))
            .unwrap()
        {
            PrepareOutcome::Prepared(binding) => binding,
            _ => panic!("expected prepared binding"),
        };
        let committed = prepared.commit().unwrap();
        let receipt = match authority
            .query_handle()
            .query_operation(committed.operation(), OLD)
        {
            AuthorityQuery::Applied(receipt) => receipt,
            _ => panic!("expected applied receipt"),
        };
        let active = receipt.activate().unwrap();
        let retired_source = source.uts.clone();
        drop(active);
        assert!(source.enter().is_none());
        assert!(source.snapshot().is_err());
        assert_eq!(
            retired_source.activate_execution(),
            Err(AxError::ResourceBusy)
        );
        assert!(matches!(
            world.register_uts_provider(retired_source),
            Err(AxError::ResourceBusy)
        ));
    }

    #[test]
    fn recovered_committed_receipt_can_cancel_without_ack_owner() {
        let (authority, world, source) = root(AuthorityCapacity {
            worlds: 1,
            providers: 2,
            bindings: 1,
            terminal_operations: 1,
        });
        let token = authority.reserve_operation(OLD).unwrap();
        let prepared = match world
            .prepare_uts_binding(&token, OLD, captured(&source))
            .unwrap()
        {
            PrepareOutcome::Prepared(binding) => binding,
            _ => panic!("expected prepared binding"),
        };
        let committed = prepared.commit().unwrap();
        let receipt = match authority
            .query_handle()
            .query_operation(committed.operation(), OLD)
        {
            AuthorityQuery::Applied(receipt) => receipt,
            _ => panic!("expected applied receipt"),
        };
        let binding = committed.binding;
        let fence_epoch = {
            let state = authority.state.lock();
            let record = state.bindings[Authority::binding_index(&state, binding).unwrap()]
                .as_ref()
                .unwrap();
            assert!(record.sealed_source.is_some());
            assert_eq!(
                record.sealed_source.as_ref().unwrap().epoch(),
                record.fence_epoch
            );
            record.fence_epoch
        };
        assert!(source.uts.execution_gate().owner_matches(fence_epoch));
        drop(committed);
        // A committed source retains its fence owner until either exact
        // activation or exact cancellation; a generic activation cannot
        // reopen a stale closed generation.
        assert_eq!(source.uts.activate_execution(), Err(AxError::ResourceBusy));
        assert!(source.uts.execution_gate().owner_matches(fence_epoch));
        assert!(source.enter().is_none());
        receipt.cancel().unwrap();
        assert!(source.enter().is_some());
        assert!(matches!(
            authority
                .query_handle()
                .query_operation(token.operation(), OLD),
            AuthorityQuery::Rejected
        ));
        let _next = authority.reserve_operation(NEW).unwrap();
    }

    #[test]
    fn snapshot_is_fixed_logical_bytes_without_pointer_ownership() {
        let snapshot = UtsProviderSnapshot::from_fields(b"node", b"domain").unwrap();
        assert_eq!(snapshot.nodename(), b"node");
        assert_eq!(snapshot.domainname(), b"domain");
        assert_eq!(snapshot.schema_digest(), UtsProviderSnapshot::SCHEMA_DIGEST);
        assert_eq!(
            core::mem::size_of::<UtsProviderSnapshot>(),
            2 * UTS_FIELD_LEN + 34
        );
    }
}
