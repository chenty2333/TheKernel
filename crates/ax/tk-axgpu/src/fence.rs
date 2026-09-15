use core::sync::atomic::{AtomicBool, Ordering};

/// A one-shot completion indicator with release/acquire publication.
pub struct Fence {
    signaled: AtomicBool,
}

impl Fence {
    pub const fn new(signaled: bool) -> Self {
        Self {
            signaled: AtomicBool::new(signaled),
        }
    }

    pub fn is_signaled(&self) -> bool {
        self.signaled.load(Ordering::Acquire)
    }

    /// Signals the fence and returns whether this call changed its state.
    pub fn signal(&self) -> bool {
        !self.signaled.swap(true, Ordering::AcqRel)
    }
}

/// One exclusive completion slot, represented by a monotonically increasing
/// sequence.  Adapters retain the actual fence object in their own bounded
/// submission table and use this sequence to reject stale publication.
pub struct Reservation {
    sequence: core::sync::atomic::AtomicU64,
}

impl Reservation {
    pub const fn new() -> Self {
        Self {
            sequence: core::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::Acquire)
    }

    pub fn publish_next(&self) -> Option<u64> {
        let mut current = self.sequence.load(Ordering::Acquire);
        loop {
            let next = current.checked_add(1)?;
            match self.sequence.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(next),
                Err(observed) => current = observed,
            }
        }
    }
}

impl Default for Reservation {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};

    #[test]
    fn first_signal_wins_across_threads() {
        let fence = Arc::new(Fence::new(false));
        let mut workers = std::vec::Vec::new();
        for _ in 0..8 {
            let fence = Arc::clone(&fence);
            workers.push(thread::spawn(move || fence.signal()));
        }
        assert_eq!(
            workers
                .into_iter()
                .filter_map(|worker| worker.join().ok())
                .filter(|changed| *changed)
                .count(),
            1
        );
        assert!(fence.is_signaled());
    }
}
