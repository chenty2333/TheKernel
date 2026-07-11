use alloc::{alloc::AllocError, boxed::Box, sync::Arc};
use core::{
    array,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{SignalInfo, SignalSet, Signo};

const STANDARD_SIGNAL_SLOTS: usize = Signo::SIGRTMIN as usize;
const REALTIME_SIGNAL_QUEUES: usize = Signo::SIGRT32 as usize - Signo::SIGRTMIN as usize + 1;

/// A shared atomic budget for queued signal records.
///
/// Kernel integrations normally key one account by user namespace and real
/// UID. The hard limit remains effective even when RLIMIT_SIGPENDING is
/// infinity, so a single identity can never grow the queue without bound.
pub struct SignalQueueAccount {
    queued: AtomicUsize,
    hard_limit: usize,
}

impl SignalQueueAccount {
    /// Fallibly creates a shared queue account.
    pub fn try_new(hard_limit: usize) -> Result<Arc<Self>, AllocError> {
        Arc::try_new(Self {
            queued: AtomicUsize::new(0),
            hard_limit,
        })
    }

    /// Returns the number of records currently charged to this account.
    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }

    fn try_charge(
        self: &Arc<Self>,
        limit: u64,
    ) -> Result<SingleSignalQueueCharge, SignalQueueError> {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let limit = self.hard_limit.min(limit);
        self.queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued.checked_add(1).filter(|next| *next <= limit)
            })
            .map_err(|_| SignalQueueError::LimitExceeded)?;
        Ok(SingleSignalQueueCharge {
            account: self.clone(),
        })
    }
}

struct SingleSignalQueueCharge {
    account: Arc<SignalQueueAccount>,
}

impl Drop for SingleSignalQueueCharge {
    fn drop(&mut self) {
        let previous = self.account.queued.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "signal queue charge underflow");
    }
}

struct SignalQueueCharge {
    _per_user: SingleSignalQueueCharge,
    _global: SingleSignalQueueCharge,
}

impl SignalQueueCharge {
    fn try_new(
        per_user: &Arc<SignalQueueAccount>,
        rlimit: u64,
        global: &Arc<SignalQueueAccount>,
    ) -> Result<Self, SignalQueueError> {
        let per_user = per_user.try_charge(rlimit)?;
        let global = match global.try_charge(u64::MAX) {
            Ok(global) => global,
            Err(err) => {
                drop(per_user);
                return Err(err);
            }
        };
        Ok(Self {
            _per_user: per_user,
            _global: global,
        })
    }
}

/// Why a fully queued signal record could not be prepared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalQueueError {
    /// The target identity reached RLIMIT_SIGPENDING or the account hard limit.
    LimitExceeded,
    /// Allocating the real-time queue node failed.
    NoMemory,
}

struct StandardSignal {
    info: SignalInfo,
}

struct RealtimeSignalNode {
    info: SignalInfo,
    _charge: SignalQueueCharge,
    next: Option<NonNull<RealtimeSignalNode>>,
}

#[derive(Default)]
struct RealtimeSignalQueue {
    head: Option<NonNull<RealtimeSignalNode>>,
    tail: Option<NonNull<RealtimeSignalNode>>,
    len: usize,
}

// SAFETY: every pointer in RealtimeSignalQueue originates from Box::into_raw,
// is uniquely owned by exactly one queue, and is accessed only through the
// mutable queue behind its PendingSignals lock. RealtimeSignalNode is Send.
unsafe impl Send for RealtimeSignalQueue {}

impl RealtimeSignalQueue {
    fn push_back(&mut self, node: Box<RealtimeSignalNode>) {
        debug_assert!(node.next.is_none());
        let ptr = NonNull::new(Box::into_raw(node)).expect("Box never yields a null pointer");

        if let Some(mut tail) = self.tail {
            // SAFETY: tail is a live node uniquely owned by this queue, and
            // &mut self serializes mutation of its link.
            unsafe { tail.as_mut().next = Some(ptr) };
        } else {
            debug_assert!(self.head.is_none());
            self.head = Some(ptr);
        }
        self.tail = Some(ptr);
        self.len += 1;
    }

    fn pop_front(&mut self) -> Option<Box<RealtimeSignalNode>> {
        let head = self.head?;
        // SAFETY: head is a live node uniquely owned by this queue.
        let next = unsafe { head.as_ref().next };
        self.head = next;
        if next.is_none() {
            self.tail = None;
        }
        self.len -= 1;

        // SAFETY: this removes head from the queue before reconstructing the
        // unique Box that was transferred with Box::into_raw in push_back.
        let mut node = unsafe { Box::from_raw(head.as_ptr()) };
        node.next = None;
        Some(node)
    }

    fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    fn append(&mut self, mut other: Self) {
        let Some(other_head) = other.head else {
            return;
        };

        if let Some(mut tail) = self.tail {
            // SAFETY: tail is a live node uniquely owned by this queue, and
            // other_head is the first node uniquely owned by `other`.
            unsafe { tail.as_mut().next = Some(other_head) };
        } else {
            debug_assert!(self.head.is_none());
            self.head = Some(other_head);
        }
        self.tail = other.tail;
        self.len += other.len;

        other.head = None;
        other.tail = None;
        other.len = 0;
    }
}

impl Drop for RealtimeSignalQueue {
    fn drop(&mut self) {
        while let Some(node) = self.pop_front() {
            drop(node);
        }
    }
}

enum PreparedSignalKind {
    Standard(StandardSignal),
    Realtime(Box<RealtimeSignalNode>),
    RealtimeFallback(SignalInfo),
}

/// Signal state prepared before taking a pending-queue spin lock.
///
/// Account acquisition, Arc cloning, and RT node allocation all happen in the
/// constructors. Publishing this value only moves fixed storage or links an
/// already-owned node.
pub struct PreparedSignal {
    kind: PreparedSignalKind,
}

// SAFETY: a prepared signal exclusively owns either fixed inline state or an
// unlinked Box whose `next` pointer is `None`. Moving that ownership to the
// thread which eventually publishes it cannot create aliasing. Once
// published, the pending-queue lock owns and serializes the intrusive node.
unsafe impl Send for PreparedSignal {}

impl PreparedSignal {
    /// Prepares a fully accounted signal record.
    pub fn try_accounted(
        info: SignalInfo,
        per_user: &Arc<SignalQueueAccount>,
        rlimit: u64,
        global: &Arc<SignalQueueAccount>,
    ) -> Result<Self, SignalQueueError> {
        Self::try_accounted_with(info, per_user, rlimit, global, Box::try_new)
    }

    fn try_accounted_with(
        info: SignalInfo,
        per_user: &Arc<SignalQueueAccount>,
        rlimit: u64,
        global: &Arc<SignalQueueAccount>,
        allocate: impl FnOnce(RealtimeSignalNode) -> Result<Box<RealtimeSignalNode>, AllocError>,
    ) -> Result<Self, SignalQueueError> {
        if !info.signo().is_realtime() {
            return Ok(Self {
                kind: PreparedSignalKind::Standard(StandardSignal { info }),
            });
        }

        let charge = SignalQueueCharge::try_new(per_user, rlimit, global)?;
        let node = allocate(RealtimeSignalNode {
            info,
            _charge: charge,
            next: None,
        })
        .map_err(|_| SignalQueueError::NoMemory)?;
        Ok(Self {
            kind: PreparedSignalKind::Realtime(node),
        })
    }

    /// Prepares an allocation-free, unaccounted fallback.
    ///
    /// Standard signals retain one fixed info slot. Real-time fallback only
    /// records the pending bit, matching Linux's low-resource loss-of-info
    /// path while still guaranteeing one deliverable instance.
    pub fn unqueued(info: SignalInfo) -> Self {
        let kind = if info.signo().is_realtime() {
            PreparedSignalKind::RealtimeFallback(info)
        } else {
            PreparedSignalKind::Standard(StandardSignal { info })
        };
        Self { kind }
    }

    /// Returns the signal number carried by this prepared state.
    pub fn signo(&self) -> Signo {
        match &self.kind {
            PreparedSignalKind::Standard(signal) => signal.info.signo(),
            PreparedSignalKind::Realtime(node) => node.info.signo(),
            PreparedSignalKind::RealtimeFallback(info) => info.signo(),
        }
    }

    /// Returns the complete siginfo retained before publication.
    pub fn info(&self) -> &SignalInfo {
        match &self.kind {
            PreparedSignalKind::Standard(signal) => &signal.info,
            PreparedSignalKind::Realtime(node) => &node.info,
            PreparedSignalKind::RealtimeFallback(info) => info,
        }
    }

    /// Replaces retained siginfo without changing the reserved signal number.
    ///
    /// Ptrace uses this to implement `PTRACE_SETSIGINFO` while preserving the
    /// exact queue node and account charge that were admitted at generation.
    pub fn replace_info(&mut self, info: SignalInfo) -> Option<SignalInfo> {
        if info.signo() != self.signo() {
            return None;
        }
        let old = match &mut self.kind {
            PreparedSignalKind::Standard(signal) => core::mem::replace(&mut signal.info, info),
            PreparedSignalKind::Realtime(node) => core::mem::replace(&mut node.info, info),
            PreparedSignalKind::RealtimeFallback(current) => core::mem::replace(current, info),
        };
        Some(old)
    }
}

/// An owned signal removed from a pending queue.
///
/// Convert this into SignalInfo only after releasing the pending spin lock;
/// doing so may release an account Arc and deallocate an RT node.
pub struct DequeuedSignal {
    kind: DequeuedSignalKind,
}

enum DequeuedSignalKind {
    Standard(StandardSignal),
    Realtime(Box<RealtimeSignalNode>),
    RealtimeFallback(Signo),
}

impl DequeuedSignal {
    /// Returns the signal number without releasing queue ownership.
    pub fn signo(&self) -> Signo {
        match &self.kind {
            DequeuedSignalKind::Standard(signal) => signal.info.signo(),
            DequeuedSignalKind::Realtime(node) => node.info.signo(),
            DequeuedSignalKind::RealtimeFallback(signo) => *signo,
        }
    }

    /// Extracts SignalInfo and releases all queue resources.
    pub fn into_info(self) -> SignalInfo {
        match self.kind {
            DequeuedSignalKind::Standard(signal) => signal.info.clone(),
            DequeuedSignalKind::Realtime(node) => node.info.clone(),
            DequeuedSignalKind::RealtimeFallback(signo) => SignalInfo::new_user(signo, 0, 0),
        }
    }
}

pub(crate) struct PublishOutcome {
    pub(crate) added: bool,
    unused: Option<PreparedSignal>,
}

impl PublishOutcome {
    pub(crate) fn finish(self) -> bool {
        drop(self.unused);
        self.added
    }
}

/// Structure to record pending signals.
pub struct PendingSignals {
    /// Signals with at least one deliverable instance.
    pub set: SignalSet,
    standard: [Option<StandardSignal>; STANDARD_SIGNAL_SLOTS],
    realtime: [RealtimeSignalQueue; REALTIME_SIGNAL_QUEUES],
}

/// Queue storage detached while holding a pending lock and destroyed after
/// that lock is released.
pub(crate) struct DetachedSignal {
    realtime: RealtimeSignalQueue,
}

impl DetachedSignal {
    pub(crate) fn empty() -> Self {
        Self {
            realtime: RealtimeSignalQueue::default(),
        }
    }
}

impl Default for PendingSignals {
    fn default() -> Self {
        Self {
            set: SignalSet::default(),
            standard: array::from_fn(|_| None),
            realtime: array::from_fn(|_| RealtimeSignalQueue::default()),
        }
    }
}

impl PendingSignals {
    pub(crate) fn publish(&mut self, prepared: PreparedSignal) -> PublishOutcome {
        let signo = prepared.signo();
        match prepared.kind {
            PreparedSignalKind::Standard(signal) => {
                if self.set.has(signo) {
                    return PublishOutcome {
                        added: false,
                        unused: Some(PreparedSignal {
                            kind: PreparedSignalKind::Standard(signal),
                        }),
                    };
                }
                self.set.add(signo);
                let slot = &mut self.standard[signo as usize];
                debug_assert!(slot.is_none());
                *slot = Some(signal);
                PublishOutcome {
                    added: true,
                    unused: None,
                }
            }
            PreparedSignalKind::Realtime(node) => {
                self.set.add(signo);
                self.realtime[signo as usize - Signo::SIGRTMIN as usize].push_back(node);
                PublishOutcome {
                    added: true,
                    unused: None,
                }
            }
            PreparedSignalKind::RealtimeFallback(info) => {
                let signo = info.signo();
                PublishOutcome {
                    added: self.set.add(signo),
                    unused: None,
                }
            }
        }
    }

    pub(crate) fn dequeue_signal(&mut self, mask: &SignalSet) -> Option<DequeuedSignal> {
        let signo = self.set.dequeue(mask)?;
        let kind = if signo.is_realtime() {
            let queue = &mut self.realtime[signo as usize - Signo::SIGRTMIN as usize];
            if let Some(node) = queue.pop_front() {
                if !queue.is_empty() {
                    self.set.add(signo);
                }
                DequeuedSignalKind::Realtime(node)
            } else {
                DequeuedSignalKind::RealtimeFallback(signo)
            }
        } else {
            let signal = self.standard[signo as usize]
                .take()
                .expect("standard pending bit must own a fixed signal slot");
            DequeuedSignalKind::Standard(signal)
        };
        Some(DequeuedSignal { kind })
    }

    pub(crate) fn take_signal(&mut self, signo: Signo) -> DetachedSignal {
        let mut detached = DetachedSignal::empty();
        self.detach_signal_into(signo, &mut detached);
        detached
    }

    pub(crate) fn detach_signal_into(&mut self, signo: Signo, detached: &mut DetachedSignal) {
        if !self.set.remove(signo) {
            return;
        }

        if signo.is_realtime() {
            let queue =
                core::mem::take(&mut self.realtime[signo as usize - Signo::SIGRTMIN as usize]);
            detached.realtime.append(queue);
        } else {
            let removed = self.standard[signo as usize].take();
            debug_assert!(
                removed.is_some(),
                "standard pending bit must own a fixed slot"
            );
            // SignalInfo is fixed POD storage and has no allocator-backed
            // destructor, so discarding it under the short pending lock is
            // allocation-free.
        }
    }

    pub(crate) fn take_all(&mut self) -> Self {
        core::mem::take(self)
    }
}

#[cfg(test)]
mod tests {
    use alloc::alloc::AllocError;

    use super::*;

    fn accounts(
        per_user: usize,
        global: usize,
    ) -> (Arc<SignalQueueAccount>, Arc<SignalQueueAccount>) {
        (
            SignalQueueAccount::try_new(per_user).unwrap(),
            SignalQueueAccount::try_new(global).unwrap(),
        )
    }

    fn all_signals() -> SignalSet {
        !SignalSet::default()
    }

    #[test]
    fn standard_slots_coalesce_without_charging() {
        let (user, global) = accounts(1, 1);
        let mut pending = PendingSignals::default();
        let first = PreparedSignal::try_accounted(
            SignalInfo::new_user(Signo::SIGTERM, 7, 11),
            &user,
            0,
            &global,
        )
        .unwrap();
        assert!(pending.publish(first).finish());

        let duplicate = PreparedSignal::try_accounted(
            SignalInfo::new_user(Signo::SIGTERM, 8, 12),
            &user,
            0,
            &global,
        )
        .unwrap();
        assert!(!pending.publish(duplicate).finish());
        assert_eq!(user.queued(), 0);
        assert_eq!(global.queued(), 0);

        let signal = pending.dequeue_signal(&all_signals()).unwrap();
        assert_eq!(signal.into_info().code(), 7);
        assert!(pending.set.is_empty());
    }

    #[test]
    fn realtime_fifo_and_signal_number_priority() {
        let (user, global) = accounts(8, 8);
        let mut pending = PendingSignals::default();
        for (signo, code) in [
            (Signo::SIGRT2, 20),
            (Signo::SIGRTMIN, 10),
            (Signo::SIGRTMIN, 11),
            (Signo::SIGRT1, 15),
        ] {
            let prepared = PreparedSignal::try_accounted(
                SignalInfo::new_user(signo, code, 1),
                &user,
                8,
                &global,
            )
            .unwrap();
            assert!(pending.publish(prepared).finish());
        }
        assert_eq!(user.queued(), 4);
        assert_eq!(global.queued(), 4);

        let delivered: alloc::vec::Vec<_> = (0..4)
            .map(|_| pending.dequeue_signal(&all_signals()).unwrap().into_info())
            .map(|info| (info.signo(), info.code()))
            .collect();
        assert_eq!(
            delivered,
            [
                (Signo::SIGRTMIN, 10),
                (Signo::SIGRTMIN, 11),
                (Signo::SIGRT1, 15),
                (Signo::SIGRT2, 20),
            ]
        );
        assert_eq!(user.queued(), 0);
        assert_eq!(global.queued(), 0);
    }

    #[test]
    fn queued_node_and_fallback_bit_never_create_an_extra_instance() {
        let (user, global) = accounts(2, 2);
        let mut pending = PendingSignals::default();
        let queued = PreparedSignal::try_accounted(
            SignalInfo::new_user(Signo::SIGRTMIN, 1, 1),
            &user,
            2,
            &global,
        )
        .unwrap();
        pending.publish(queued).finish();
        assert!(
            !pending
                .publish(PreparedSignal::unqueued(SignalInfo::new_user(
                    Signo::SIGRTMIN,
                    2,
                    2,
                )))
                .finish()
        );

        assert_eq!(
            pending
                .dequeue_signal(&all_signals())
                .unwrap()
                .into_info()
                .code(),
            1
        );
        assert!(pending.dequeue_signal(&all_signals()).is_none());

        pending
            .publish(PreparedSignal::unqueued(SignalInfo::new_user(
                Signo::SIGRTMIN,
                3,
                3,
            )))
            .finish();
        let queued = PreparedSignal::try_accounted(
            SignalInfo::new_user(Signo::SIGRTMIN, 4, 4),
            &user,
            2,
            &global,
        )
        .unwrap();
        pending.publish(queued).finish();
        assert_eq!(
            pending
                .dequeue_signal(&all_signals())
                .unwrap()
                .into_info()
                .code(),
            4
        );
        assert!(pending.dequeue_signal(&all_signals()).is_none());
    }

    #[test]
    fn allocation_failure_rolls_back_both_charges() {
        let (user, global) = accounts(1, 1);
        let result = PreparedSignal::try_accounted_with(
            SignalInfo::new_user(Signo::SIGRTMIN, 1, 1),
            &user,
            1,
            &global,
            |node| {
                drop(node);
                Err(AllocError)
            },
        );
        assert!(matches!(result, Err(SignalQueueError::NoMemory)));
        assert_eq!(user.queued(), 0);
        assert_eq!(global.queued(), 0);
    }

    #[test]
    fn global_limit_rolls_back_per_user_charge() {
        let (user, global) = accounts(2, 0);
        let result = PreparedSignal::try_accounted(
            SignalInfo::new_user(Signo::SIGRTMIN, 1, 1),
            &user,
            2,
            &global,
        );
        assert!(matches!(result, Err(SignalQueueError::LimitExceeded)));
        assert_eq!(user.queued(), 0);
        assert_eq!(global.queued(), 0);
    }

    #[test]
    fn detached_queue_refunds_on_drop() {
        let (user, global) = accounts(4, 4);
        let mut pending = PendingSignals::default();
        for code in 0..3 {
            pending
                .publish(
                    PreparedSignal::try_accounted(
                        SignalInfo::new_user(Signo::SIGRTMIN, code, 1),
                        &user,
                        4,
                        &global,
                    )
                    .unwrap(),
                )
                .finish();
        }
        let detached = pending.take_all();
        assert!(pending.set.is_empty());
        assert_eq!(user.queued(), 3);
        drop(detached);
        assert_eq!(user.queued(), 0);
        assert_eq!(global.queued(), 0);
    }

    #[test]
    fn detaching_one_signal_refunds_only_its_queue() {
        let (user, global) = accounts(4, 4);
        let mut pending = PendingSignals::default();
        for (signo, code) in [
            (Signo::SIGRTMIN, 1),
            (Signo::SIGRTMIN, 2),
            (Signo::SIGRT1, 3),
        ] {
            pending
                .publish(
                    PreparedSignal::try_accounted(
                        SignalInfo::new_user(signo, code, 1),
                        &user,
                        4,
                        &global,
                    )
                    .unwrap(),
                )
                .finish();
        }

        let detached = pending.take_signal(Signo::SIGRTMIN);
        assert!(!pending.set.has(Signo::SIGRTMIN));
        assert!(pending.set.has(Signo::SIGRT1));
        assert_eq!(user.queued(), 3);
        drop(detached);
        assert_eq!(user.queued(), 1);
        assert_eq!(global.queued(), 1);

        let remaining = pending.dequeue_signal(&all_signals()).unwrap().into_info();
        assert_eq!((remaining.signo(), remaining.code()), (Signo::SIGRT1, 3));
        assert_eq!(user.queued(), 0);
        assert_eq!(global.queued(), 0);
    }
}
