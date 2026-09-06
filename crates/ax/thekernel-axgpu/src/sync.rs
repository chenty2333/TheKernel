use core::sync::atomic::{AtomicU64, Ordering};

/// A generation-labelled binary synchronization object.
pub struct SyncObject {
    state: AtomicU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncSnapshot {
    pub generation: u64,
    pub signaled: bool,
}

impl SyncObject {
    pub const fn new(signaled: bool) -> Self {
        Self {
            state: AtomicU64::new(if signaled { 1 } else { 0 }),
        }
    }
    pub fn snapshot(&self) -> SyncSnapshot {
        let state = self.state.load(Ordering::Acquire);
        SyncSnapshot {
            generation: state >> 1,
            signaled: state & 1 != 0,
        }
    }
    pub fn signal(&self) {
        self.state.fetch_or(1, Ordering::Release);
    }
    /// Starts a new unsignaled generation. Overflow leaves the object unchanged.
    pub fn reset(&self) -> Option<u64> {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let generation = current >> 1;
            let next_generation = generation.checked_add(1)?;
            match self.state.compare_exchange_weak(
                current,
                next_generation << 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(next_generation),
                Err(observed) => current = observed,
            }
        }
    }
}

impl Default for SyncObject {
    fn default() -> Self {
        Self::new(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};

    #[test]
    fn reset_is_a_single_atomic_state_transition() {
        let sync = SyncObject::new(true);
        assert_eq!(
            sync.snapshot(),
            SyncSnapshot {
                generation: 0,
                signaled: true
            }
        );
        assert_eq!(sync.reset(), Some(1));
        assert_eq!(
            sync.snapshot(),
            SyncSnapshot {
                generation: 1,
                signaled: false
            }
        );
    }

    #[test]
    fn concurrent_resets_assign_distinct_generations() {
        let sync = Arc::new(SyncObject::default());
        let mut workers = std::vec::Vec::new();
        for _ in 0..8 {
            let sync = Arc::clone(&sync);
            workers.push(thread::spawn(move || sync.reset().unwrap()));
        }
        let mut generations: std::vec::Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        generations.sort_unstable();
        assert_eq!(generations, (1..=8).collect::<std::vec::Vec<_>>());
        assert_eq!(sync.snapshot().generation, 8);
    }
}
