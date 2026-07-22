//! Opt-in MM lock diagnostics for performance gates.
//!
//! This module deliberately aggregates into a fixed global atomic table. Both
//! the Cargo feature and the runtime switch must be enabled before any clock is
//! read. A feature build with runtime diagnostics disabled performs one
//! packed-state atomic load before taking the unchanged underlying lock.
//!
//! The table is diagnostic-build-only. A per-CPU table can replace it later if
//! measured diagnostic contention warrants the extra initialization and
//! snapshot complexity.

use core::{
    array,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use axhal::time::monotonic_time_nanos;

/// Bucket 0 represents 0 ns, bucket 1 represents 1 ns, and bucket `n`
/// represents `[2^(n - 1), 2^n - 1]` ns. The last bucket also absorbs larger
/// values, including `u64::MAX`.
pub const MM_LOCK_HISTOGRAM_BUCKETS: usize = 64;

/// Stable phase identifiers for the MM critical sections that currently have
/// the highest-priority performance evidence gaps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MmLockStage {
    UserPinAdmission,
    UserPinExpectation,
    UserPinCollectOwners,
    UserPinRevalidate,
    UserPinCommit,
    UserPinRelease,
    MremapOptimisticPlan,
    MremapOptimisticCommit,
    MremapSerialized,
    PhysPinRegistryShard,
    PhysPinPublishShard,
    PhysPinReleaseShard,
    PhysPinDeallocProbeShard,
}

impl MmLockStage {
    pub const ALL: [Self; 13] = [
        Self::UserPinAdmission,
        Self::UserPinExpectation,
        Self::UserPinCollectOwners,
        Self::UserPinRevalidate,
        Self::UserPinCommit,
        Self::UserPinRelease,
        Self::MremapOptimisticPlan,
        Self::MremapOptimisticCommit,
        Self::MremapSerialized,
        Self::PhysPinRegistryShard,
        Self::PhysPinPublishShard,
        Self::PhysPinReleaseShard,
        Self::PhysPinDeallocProbeShard,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserPinAdmission => "user_pin_admission",
            Self::UserPinExpectation => "user_pin_expectation",
            Self::UserPinCollectOwners => "user_pin_collect_owners",
            Self::UserPinRevalidate => "user_pin_revalidate",
            Self::UserPinCommit => "user_pin_commit",
            Self::UserPinRelease => "user_pin_release",
            Self::MremapOptimisticPlan => "mremap_optimistic_plan",
            Self::MremapOptimisticCommit => "mremap_optimistic_commit",
            Self::MremapSerialized => "mremap_serialized",
            Self::PhysPinRegistryShard => "phys_pin_registry_shard",
            Self::PhysPinPublishShard => "phys_pin_publish_shard",
            Self::PhysPinReleaseShard => "phys_pin_release_shard",
            Self::PhysPinDeallocProbeShard => "phys_pin_dealloc_probe_shard",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

const MM_LOCK_STAGE_COUNT: usize = MmLockStage::ALL.len();

struct StageCounters {
    samples: AtomicU64,
    wait_ns: AtomicU64,
    hold_ns: AtomicU64,
    max_wait_ns: AtomicU64,
    max_hold_ns: AtomicU64,
    wait_buckets: [AtomicU64; MM_LOCK_HISTOGRAM_BUCKETS],
    hold_buckets: [AtomicU64; MM_LOCK_HISTOGRAM_BUCKETS],
    saturated: AtomicBool,
}

impl StageCounters {
    const fn new() -> Self {
        Self {
            samples: AtomicU64::new(0),
            wait_ns: AtomicU64::new(0),
            hold_ns: AtomicU64::new(0),
            max_wait_ns: AtomicU64::new(0),
            max_hold_ns: AtomicU64::new(0),
            wait_buckets: [const { AtomicU64::new(0) }; MM_LOCK_HISTOGRAM_BUCKETS],
            hold_buckets: [const { AtomicU64::new(0) }; MM_LOCK_HISTOGRAM_BUCKETS],
            saturated: AtomicBool::new(false),
        }
    }

    fn reset(&self) {
        self.samples.store(0, Ordering::Relaxed);
        self.wait_ns.store(0, Ordering::Relaxed);
        self.hold_ns.store(0, Ordering::Relaxed);
        self.max_wait_ns.store(0, Ordering::Relaxed);
        self.max_hold_ns.store(0, Ordering::Relaxed);
        for bucket in &self.wait_buckets {
            bucket.store(0, Ordering::Relaxed);
        }
        for bucket in &self.hold_buckets {
            bucket.store(0, Ordering::Relaxed);
        }
        self.saturated.store(false, Ordering::Relaxed);
    }

    fn snapshot(&self, stage: MmLockStage) -> MmLockStageSnapshot {
        MmLockStageSnapshot {
            stage,
            samples: self.samples.load(Ordering::Relaxed),
            wait_ns: self.wait_ns.load(Ordering::Relaxed),
            hold_ns: self.hold_ns.load(Ordering::Relaxed),
            max_wait_ns: self.max_wait_ns.load(Ordering::Relaxed),
            max_hold_ns: self.max_hold_ns.load(Ordering::Relaxed),
            wait_buckets: array::from_fn(|index| self.wait_buckets[index].load(Ordering::Relaxed)),
            hold_buckets: array::from_fn(|index| self.hold_buckets[index].load(Ordering::Relaxed)),
            saturated: self.saturated.load(Ordering::Relaxed),
        }
    }
}

static MM_LOCK_COUNTERS: [StageCounters; MM_LOCK_STAGE_COUNT] =
    [const { StageCounters::new() }; MM_LOCK_STAGE_COUNT];
static MM_LOCK_DIAGNOSTICS_STATE: AtomicUsize = AtomicUsize::new(0);
static MM_LOCK_DIAGNOSTICS_EPOCH: AtomicU64 = AtomicU64::new(0);
static MM_LOCK_DIAGNOSTICS_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const CONTROL_ORDERING: Ordering = Ordering::SeqCst;
const STATE_ENABLED: usize = 1;
const STATE_RESETTING: usize = 1 << 1;
const STATE_ACTIVE_ONE: usize = 1 << 2;

const fn state_active_samples(state: usize) -> usize {
    state >> 2
}

const fn state_enabled(state: usize) -> bool {
    state & STATE_ENABLED != 0
}

const fn state_resetting(state: usize) -> bool {
    state & STATE_RESETTING != 0
}

const fn sequence_exhausted(sequence: u64) -> bool {
    sequence == u64::MAX
}

fn advance_sequence() {
    // Never wrap an evidence generation. Once exhausted, snapshots remain
    // explicitly invalid instead of becoming vulnerable to an ABA at zero.
    let _ =
        MM_LOCK_DIAGNOSTICS_SEQUENCE.fetch_update(CONTROL_ORDERING, CONTROL_ORDERING, |sequence| {
            sequence.checked_add(1)
        });
}

/// One non-transactional observation of a diagnostic stage.
///
/// Every cumulative field and histogram bucket saturates instead of wrapping.
/// `saturated` makes that loss of exactness explicit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmLockStageSnapshot {
    pub stage: MmLockStage,
    pub samples: u64,
    pub wait_ns: u64,
    pub hold_ns: u64,
    pub max_wait_ns: u64,
    pub max_hold_ns: u64,
    pub wait_buckets: [u64; MM_LOCK_HISTOGRAM_BUCKETS],
    pub hold_buckets: [u64; MM_LOCK_HISTOGRAM_BUCKETS],
    pub saturated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmLockDiagnosticsSnapshot {
    pub enabled: bool,
    pub resetting: bool,
    pub active_samples: usize,
    pub epoch: u64,
    pub sequence: u64,
    pub sequence_exhausted: bool,
    pub stage: MmLockStageSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmLockDiagnosticsResetError {
    Enabled,
    ConcurrentReset,
    SamplesActive,
    EpochExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmLockDiagnosticsSetError {
    ResetInProgress,
    SamplesActive,
}

fn counters(stage: MmLockStage) -> &'static StageCounters {
    &MM_LOCK_COUNTERS[stage.index()]
}

fn histogram_bucket(value_ns: u64) -> usize {
    (u64::BITS - value_ns.leading_zeros()).min((MM_LOCK_HISTOGRAM_BUCKETS - 1) as u32) as usize
}

fn saturating_add(counter: &AtomicU64, value: u64, saturated: &AtomicBool) {
    let mut observed = counter.load(Ordering::Relaxed);
    loop {
        let (next, overflowed) = observed.overflowing_add(value);
        let next = if overflowed { u64::MAX } else { next };
        if overflowed {
            saturated.store(true, Ordering::Relaxed);
        }
        if next == observed {
            return;
        }
        match counter.compare_exchange_weak(observed, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(actual) => observed = actual,
        }
    }
}

fn update_max(counter: &AtomicU64, value: u64) {
    let mut observed = counter.load(Ordering::Relaxed);
    while value > observed {
        match counter.compare_exchange_weak(observed, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(actual) => observed = actual,
        }
    }
}

fn record_sample(stage: MmLockStage, epoch: u64, wait_ns: u64, hold_ns: u64) {
    if state_resetting(MM_LOCK_DIAGNOSTICS_STATE.load(CONTROL_ORDERING)) {
        return;
    }
    if MM_LOCK_DIAGNOSTICS_EPOCH.load(CONTROL_ORDERING) != epoch {
        return;
    }

    // The active-sample reference remains held until every counter write below
    // completes, so a successful freeze observes both this sequence advance
    // and the complete publication.
    advance_sequence();
    let counters = counters(stage);
    saturating_add(&counters.samples, 1, &counters.saturated);
    saturating_add(&counters.wait_ns, wait_ns, &counters.saturated);
    saturating_add(&counters.hold_ns, hold_ns, &counters.saturated);
    update_max(&counters.max_wait_ns, wait_ns);
    update_max(&counters.max_hold_ns, hold_ns);
    saturating_add(
        &counters.wait_buckets[histogram_bucket(wait_ns)],
        1,
        &counters.saturated,
    );
    saturating_add(
        &counters.hold_buckets[histogram_bucket(hold_ns)],
        1,
        &counters.saturated,
    );
}

#[must_use]
pub fn mm_lock_diagnostics_enabled() -> bool {
    state_enabled(MM_LOCK_DIAGNOSTICS_STATE.load(CONTROL_ORDERING))
}

/// Changes runtime collection without changing any MM lock or transaction.
/// Enabling fails explicitly while reset owns the control state; it never
/// spins behind another task.
pub fn set_mm_lock_diagnostics_enabled(enabled: bool) -> Result<(), MmLockDiagnosticsSetError> {
    let mut observed = MM_LOCK_DIAGNOSTICS_STATE.load(CONTROL_ORDERING);
    loop {
        if state_resetting(observed) {
            return Err(MmLockDiagnosticsSetError::ResetInProgress);
        }
        if enabled && state_enabled(observed) {
            return Ok(());
        }
        if enabled && state_active_samples(observed) != 0 {
            return Err(MmLockDiagnosticsSetError::SamplesActive);
        }
        let desired = if enabled {
            observed | STATE_ENABLED
        } else {
            observed & !STATE_ENABLED
        };
        if desired == observed {
            return if !enabled && state_active_samples(observed) != 0 {
                Err(MmLockDiagnosticsSetError::SamplesActive)
            } else {
                Ok(())
            };
        }
        match MM_LOCK_DIAGNOSTICS_STATE.compare_exchange_weak(
            observed,
            desired,
            CONTROL_ORDERING,
            CONTROL_ORDERING,
        ) {
            Ok(_) => {
                // Snapshot reads sequence before state. Advancing immediately
                // after this linearized transition means a racing reader sees
                // either the old sequence (and a changed END) or the new state.
                advance_sequence();
                return if !enabled && state_active_samples(desired) != 0 {
                    Err(MmLockDiagnosticsSetError::SamplesActive)
                } else {
                    Ok(())
                };
            }
            Err(actual) => observed = actual,
        }
    }
}

#[must_use]
pub fn mm_lock_diagnostics_snapshot(stage: MmLockStage) -> MmLockDiagnosticsSnapshot {
    // Sequence is the read-side boundary: every writer advances it before
    // changing state or counters. Reading it first prevents a snapshot from
    // combining pre-mutation state with a post-mutation sequence and table.
    let sequence = MM_LOCK_DIAGNOSTICS_SEQUENCE.load(CONTROL_ORDERING);
    let state = MM_LOCK_DIAGNOSTICS_STATE.load(CONTROL_ORDERING);
    MmLockDiagnosticsSnapshot {
        enabled: state_enabled(state),
        resetting: state_resetting(state),
        active_samples: state_active_samples(state),
        epoch: MM_LOCK_DIAGNOSTICS_EPOCH.load(CONTROL_ORDERING),
        sequence,
        sequence_exhausted: sequence_exhausted(sequence),
        stage: counters(stage).snapshot(stage),
    }
}

/// Clears one disabled collection epoch. A caller must disable collection
/// first; an in-flight sample makes reset return `SamplesActive`, so no sample
/// can be silently split across epochs.
pub fn reset_mm_lock_diagnostics() -> Result<(), MmLockDiagnosticsResetError> {
    let mut observed = MM_LOCK_DIAGNOSTICS_STATE.load(CONTROL_ORDERING);
    loop {
        if state_enabled(observed) {
            return Err(MmLockDiagnosticsResetError::Enabled);
        }
        if state_resetting(observed) {
            return Err(MmLockDiagnosticsResetError::ConcurrentReset);
        }
        if state_active_samples(observed) != 0 {
            return Err(MmLockDiagnosticsResetError::SamplesActive);
        }
        match MM_LOCK_DIAGNOSTICS_STATE.compare_exchange_weak(
            observed,
            observed | STATE_RESETTING,
            CONTROL_ORDERING,
            CONTROL_ORDERING,
        ) {
            Ok(_) => break,
            Err(actual) => observed = actual,
        }
    }
    // Resetting is now visible. Publish the new sequence before changing the
    // epoch or table; readers that observe it must subsequently observe either
    // RESETTING or the fully completed reset.
    advance_sequence();

    if MM_LOCK_DIAGNOSTICS_EPOCH
        .fetch_update(CONTROL_ORDERING, CONTROL_ORDERING, |epoch| {
            epoch.checked_add(1)
        })
        .is_err()
    {
        MM_LOCK_DIAGNOSTICS_STATE.fetch_and(!STATE_RESETTING, CONTROL_ORDERING);
        return Err(MmLockDiagnosticsResetError::EpochExhausted);
    }
    // The packed state admits reset only after collection is frozen and every
    // admitted sample has published. No cross-atomic observation is needed.
    for counters in &MM_LOCK_COUNTERS {
        counters.reset();
    }
    MM_LOCK_DIAGNOSTICS_STATE.fetch_and(!STATE_RESETTING, CONTROL_ORDERING);
    Ok(())
}

pub(crate) struct MmLockSampleStart {
    stage: MmLockStage,
    epoch: u64,
    started_ns: u64,
    active: bool,
}

impl Drop for MmLockSampleStart {
    fn drop(&mut self) {
        if self.active {
            MM_LOCK_DIAGNOSTICS_STATE.fetch_sub(STATE_ACTIVE_ONE, CONTROL_ORDERING);
        }
    }
}

pub(crate) fn begin_mm_lock(stage: MmLockStage) -> Option<MmLockSampleStart> {
    // This is the sole runtime-disabled hot-path operation in a diagnostic
    // feature build. The epoch and clock are read only when collection is on.
    let mut observed = MM_LOCK_DIAGNOSTICS_STATE.load(CONTROL_ORDERING);
    loop {
        if !state_enabled(observed)
            || state_resetting(observed)
            || state_active_samples(observed) == (usize::MAX >> 2)
        {
            return None;
        }
        match MM_LOCK_DIAGNOSTICS_STATE.compare_exchange_weak(
            observed,
            observed + STATE_ACTIVE_ONE,
            CONTROL_ORDERING,
            CONTROL_ORDERING,
        ) {
            Ok(_) => break,
            Err(actual) => observed = actual,
        }
    }
    Some(MmLockSampleStart {
        stage,
        epoch: MM_LOCK_DIAGNOSTICS_EPOCH.load(CONTROL_ORDERING),
        started_ns: monotonic_time_nanos(),
        active: true,
    })
}

struct ActiveMmLockSample {
    stage: MmLockStage,
    epoch: u64,
    wait_ns: u64,
    acquired_ns: u64,
}

/// Generic RAII accounting preserves the exact drop boundary of both the
/// sleeping AddrSpace mutex and IRQ-safe physical-pin shard locks. It adds no
/// lock and changes no lock ordering.
pub(crate) struct DiagnosedMmLockGuard<G> {
    guard: Option<G>,
    sample: Option<ActiveMmLockSample>,
}

impl<G: Deref> Deref for DiagnosedMmLockGuard<G> {
    type Target = G::Target;

    fn deref(&self) -> &Self::Target {
        self.guard.as_deref().expect("live diagnosed MM lock guard")
    }
}

impl<G: DerefMut> DerefMut for DiagnosedMmLockGuard<G> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard
            .as_deref_mut()
            .expect("live diagnosed MM lock guard")
    }
}

impl<G> Drop for DiagnosedMmLockGuard<G> {
    fn drop(&mut self) {
        let Some(sample) = self.sample.take() else {
            return;
        };
        // Release the measured lock before reading the clock or updating the
        // table. This is especially important for IRQ-safe shard locks.
        drop(self.guard.take());
        let hold_ns = monotonic_time_nanos().saturating_sub(sample.acquired_ns);
        record_sample(sample.stage, sample.epoch, sample.wait_ns, hold_ns);
        MM_LOCK_DIAGNOSTICS_STATE.fetch_sub(STATE_ACTIVE_ONE, CONTROL_ORDERING);
    }
}

pub(crate) fn finish_mm_lock<G>(
    guard: G,
    sample: Option<MmLockSampleStart>,
) -> DiagnosedMmLockGuard<G> {
    let sample = sample.map(|mut sample| {
        let acquired_ns = monotonic_time_nanos();
        let active = ActiveMmLockSample {
            stage: sample.stage,
            epoch: sample.epoch,
            wait_ns: acquired_ns.saturating_sub(sample.started_ns),
            acquired_ns,
        };
        sample.active = false;
        active
    });
    DiagnosedMmLockGuard {
        guard: Some(guard),
        sample,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::sync::{
        Mutex,
        atomic::{AtomicBool as StdAtomicBool, Ordering as StdOrdering},
    };

    use super::*;

    static TEST_SERIAL: Mutex<()> = Mutex::new(());
    static PROBE_GUARD_DROPPED: StdAtomicBool = StdAtomicBool::new(false);
    static PROBE_TARGET: () = ();

    struct ProbeGuard;

    impl Deref for ProbeGuard {
        type Target = ();

        fn deref(&self) -> &Self::Target {
            &PROBE_TARGET
        }
    }

    impl Drop for ProbeGuard {
        fn drop(&mut self) {
            assert_eq!(
                state_active_samples(MM_LOCK_DIAGNOSTICS_STATE.load(CONTROL_ORDERING)),
                1
            );
            PROBE_GUARD_DROPPED.store(true, StdOrdering::Release);
        }
    }

    fn lock_tests() -> std::sync::MutexGuard<'static, ()> {
        TEST_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn stage_names_indices_and_histogram_boundaries_are_stable() {
        let _serial = lock_tests();
        assert_eq!(MmLockStage::ALL.len(), MM_LOCK_STAGE_COUNT);
        for (index, stage) in MmLockStage::ALL.iter().copied().enumerate() {
            assert_eq!(stage.index(), index);
            assert!(!stage.as_str().is_empty());
            assert!(
                MmLockStage::ALL[..index]
                    .iter()
                    .all(|prior| prior.as_str() != stage.as_str())
            );
        }
        assert_eq!(histogram_bucket(0), 0);
        assert_eq!(histogram_bucket(1), 1);
        assert_eq!(histogram_bucket(2), 2);
        assert_eq!(histogram_bucket(3), 2);
        assert_eq!(histogram_bucket(4), 3);
        assert_eq!(histogram_bucket(u64::MAX), 63);
    }

    #[test]
    fn runtime_collection_is_disabled_and_reset_rejects_enabled_state() {
        let _serial = lock_tests();
        set_mm_lock_diagnostics_enabled(false).unwrap();
        reset_mm_lock_diagnostics().unwrap();
        assert!(!mm_lock_diagnostics_enabled());
        assert!(begin_mm_lock(MmLockStage::UserPinAdmission).is_none());

        set_mm_lock_diagnostics_enabled(true).unwrap();
        assert!(begin_mm_lock(MmLockStage::UserPinAdmission).is_some());
        assert_eq!(
            reset_mm_lock_diagnostics(),
            Err(MmLockDiagnosticsResetError::Enabled)
        );
        set_mm_lock_diagnostics_enabled(false).unwrap();
        reset_mm_lock_diagnostics().unwrap();
    }

    #[test]
    fn control_transitions_return_busy_without_spinning() {
        let _serial = lock_tests();
        set_mm_lock_diagnostics_enabled(false).unwrap();
        reset_mm_lock_diagnostics().unwrap();

        MM_LOCK_DIAGNOSTICS_STATE.store(STATE_RESETTING, CONTROL_ORDERING);
        assert_eq!(
            set_mm_lock_diagnostics_enabled(true),
            Err(MmLockDiagnosticsSetError::ResetInProgress)
        );
        assert!(!mm_lock_diagnostics_enabled());
        MM_LOCK_DIAGNOSTICS_STATE.store(0, CONTROL_ORDERING);

        let epoch = MM_LOCK_DIAGNOSTICS_EPOCH.load(CONTROL_ORDERING);
        MM_LOCK_DIAGNOSTICS_STATE.store(STATE_ACTIVE_ONE, CONTROL_ORDERING);
        assert_eq!(
            reset_mm_lock_diagnostics(),
            Err(MmLockDiagnosticsResetError::SamplesActive)
        );
        assert_eq!(MM_LOCK_DIAGNOSTICS_EPOCH.load(CONTROL_ORDERING), epoch);
        MM_LOCK_DIAGNOSTICS_STATE.store(0, CONTROL_ORDERING);
        reset_mm_lock_diagnostics().unwrap();
    }

    #[test]
    fn snapshots_track_totals_maxima_and_log_histograms() {
        let _serial = lock_tests();
        set_mm_lock_diagnostics_enabled(false).unwrap();
        reset_mm_lock_diagnostics().unwrap();
        let epoch = MM_LOCK_DIAGNOSTICS_EPOCH.load(Ordering::Acquire);
        let stage = MmLockStage::UserPinAdmission;
        record_sample(stage, epoch, 5, 17);
        record_sample(stage, epoch, 11, 7);

        let snapshot = mm_lock_diagnostics_snapshot(stage).stage;
        assert_eq!(snapshot.stage, stage);
        assert_eq!(snapshot.samples, 2);
        assert_eq!(snapshot.wait_ns, 16);
        assert_eq!(snapshot.hold_ns, 24);
        assert_eq!(snapshot.max_wait_ns, 11);
        assert_eq!(snapshot.max_hold_ns, 17);
        assert_eq!(snapshot.wait_buckets[histogram_bucket(5)], 1);
        assert_eq!(snapshot.wait_buckets[histogram_bucket(11)], 1);
        assert_eq!(snapshot.hold_buckets[histogram_bucket(17)], 1);
        assert_eq!(snapshot.hold_buckets[histogram_bucket(7)], 1);
        assert!(!snapshot.saturated);
        reset_mm_lock_diagnostics().unwrap();
    }

    #[test]
    fn guard_release_precedes_publication_and_freeze_waits_for_active_samples() {
        let _serial = lock_tests();
        set_mm_lock_diagnostics_enabled(false).unwrap();
        reset_mm_lock_diagnostics().unwrap();
        PROBE_GUARD_DROPPED.store(false, StdOrdering::Release);

        set_mm_lock_diagnostics_enabled(true).unwrap();
        let sample = begin_mm_lock(MmLockStage::PhysPinPublishShard).unwrap();
        drop(finish_mm_lock(ProbeGuard, Some(sample)));
        assert!(PROBE_GUARD_DROPPED.load(StdOrdering::Acquire));
        assert_eq!(
            mm_lock_diagnostics_snapshot(MmLockStage::PhysPinPublishShard)
                .stage
                .samples,
            1
        );

        let active = begin_mm_lock(MmLockStage::PhysPinReleaseShard).unwrap();
        assert_eq!(
            set_mm_lock_diagnostics_enabled(false),
            Err(MmLockDiagnosticsSetError::SamplesActive)
        );
        assert_eq!(
            reset_mm_lock_diagnostics(),
            Err(MmLockDiagnosticsResetError::SamplesActive)
        );
        drop(finish_mm_lock(ProbeGuard, Some(active)));
        assert_eq!(
            mm_lock_diagnostics_snapshot(MmLockStage::PhysPinReleaseShard)
                .stage
                .samples,
            1
        );
        assert_eq!(
            state_active_samples(MM_LOCK_DIAGNOSTICS_STATE.load(CONTROL_ORDERING)),
            0
        );
        set_mm_lock_diagnostics_enabled(false).unwrap();
        reset_mm_lock_diagnostics().unwrap();
    }

    #[test]
    fn publication_sequence_detects_control_aba_and_never_wraps() {
        let _serial = lock_tests();
        set_mm_lock_diagnostics_enabled(false).unwrap();
        reset_mm_lock_diagnostics().unwrap();

        let before = mm_lock_diagnostics_snapshot(MmLockStage::UserPinAdmission);
        assert!(!before.enabled);
        assert!(!before.sequence_exhausted);

        set_mm_lock_diagnostics_enabled(true).unwrap();
        let after_enable = mm_lock_diagnostics_snapshot(MmLockStage::UserPinAdmission).sequence;
        assert!(after_enable > before.sequence);

        let sample = begin_mm_lock(MmLockStage::UserPinAdmission).unwrap();
        drop(finish_mm_lock((), Some(sample)));
        let after_publication =
            mm_lock_diagnostics_snapshot(MmLockStage::UserPinAdmission).sequence;
        assert!(after_publication > after_enable);

        set_mm_lock_diagnostics_enabled(false).unwrap();
        let after_aba = mm_lock_diagnostics_snapshot(MmLockStage::UserPinAdmission);
        assert!(!after_aba.enabled);
        assert_eq!(after_aba.active_samples, 0);
        assert_eq!(after_aba.epoch, before.epoch);
        assert!(after_aba.sequence > after_publication);

        reset_mm_lock_diagnostics().unwrap();
        let after_reset = mm_lock_diagnostics_snapshot(MmLockStage::UserPinAdmission);
        assert!(after_reset.epoch > after_aba.epoch);
        assert!(after_reset.sequence > after_aba.sequence);

        let saved_sequence = after_reset.sequence;
        MM_LOCK_DIAGNOSTICS_SEQUENCE.store(u64::MAX, CONTROL_ORDERING);
        advance_sequence();
        let exhausted = mm_lock_diagnostics_snapshot(MmLockStage::UserPinAdmission);
        assert_eq!(exhausted.sequence, u64::MAX);
        assert!(exhausted.sequence_exhausted);
        MM_LOCK_DIAGNOSTICS_SEQUENCE.store(saved_sequence, CONTROL_ORDERING);
    }

    #[test]
    fn cumulative_counters_saturate_instead_of_wrapping() {
        let _serial = lock_tests();
        set_mm_lock_diagnostics_enabled(false).unwrap();
        reset_mm_lock_diagnostics().unwrap();
        let epoch = MM_LOCK_DIAGNOSTICS_EPOCH.load(Ordering::Acquire);
        let stage = MmLockStage::MremapSerialized;
        counters(stage)
            .wait_ns
            .store(u64::MAX - 2, Ordering::Relaxed);
        record_sample(stage, epoch, 9, 0);

        let snapshot = mm_lock_diagnostics_snapshot(stage).stage;
        assert_eq!(snapshot.samples, 1);
        assert_eq!(snapshot.wait_ns, u64::MAX);
        assert_eq!(snapshot.max_wait_ns, 9);
        assert!(snapshot.saturated);
        reset_mm_lock_diagnostics().unwrap();
    }
}
