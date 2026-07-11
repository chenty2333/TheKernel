//! Resource limits.

use core::{
    ops::{Index, IndexMut},
    sync::atomic::{AtomicU64, Ordering},
};

use linux_raw_sys::general::{
    RLIM_INFINITY, RLIM_NLIMITS, RLIMIT_CORE, RLIMIT_NOFILE, RLIMIT_SIGPENDING, RLIMIT_STACK,
};

/// The maximum number of open files
pub const AX_FILE_LIMIT: usize = 1024;

/// Linux's default system-wide ceiling for RLIMIT_NOFILE.
pub const NR_OPEN_MAX: u64 = 1_048_576;

static NR_OPEN_LIMIT: AtomicU64 = AtomicU64::new(NR_OPEN_MAX);

pub fn nr_open_limit() -> u64 {
    NR_OPEN_LIMIT.load(Ordering::Relaxed)
}

pub fn set_nr_open_limit(value: u64) -> bool {
    if value < AX_FILE_LIMIT as u64 {
        return false;
    }
    NR_OPEN_LIMIT.store(value, Ordering::Relaxed);
    true
}

/// The limit for a specific resource
#[derive(Clone, Copy, Default)]
pub struct Rlimit {
    /// The current limit for the resource (soft)
    pub current: u64,
    /// The maximum limit for the resource (hard)
    pub max: u64,
}

impl Rlimit {
    /// Creates a new `Rlimit` with the specified soft and hard limits.
    pub fn new(soft: u64, hard: u64) -> Self {
        Self {
            current: soft,
            max: hard,
        }
    }
}

impl From<u64> for Rlimit {
    fn from(value: u64) -> Self {
        Self {
            current: value,
            max: value,
        }
    }
}

/// Process resource limits
#[derive(Clone)]
pub struct Rlimits([Rlimit; RLIM_NLIMITS as usize]);

impl Default for Rlimits {
    fn default() -> Self {
        let mut result = Self(core::array::from_fn(|_| {
            Rlimit::new(RLIM_INFINITY as i64 as u64, RLIM_INFINITY as i64 as u64)
        }));
        result[RLIMIT_STACK] = Rlimit::new(
            crate::config::USER_STACK_SIZE as u64,
            RLIM_INFINITY as i64 as u64,
        );
        result[RLIMIT_CORE] = Rlimit::new(0, RLIM_INFINITY as i64 as u64);
        result[RLIMIT_NOFILE] = (AX_FILE_LIMIT as u64).into();
        // Explicit bounded default for per-(user namespace, real UID) queued
        // real-time signals. The global implementation ceiling is enforced by
        // the shared signal queue account independently of this rlimit.
        result[RLIMIT_SIGPENDING] = 1_024.into();
        result
    }
}

impl Index<u32> for Rlimits {
    type Output = Rlimit;

    fn index(&self, index: u32) -> &Self::Output {
        &self.0[index as usize]
    }
}

impl IndexMut<u32> for Rlimits {
    fn index_mut(&mut self, index: u32) -> &mut Self::Output {
        &mut self.0[index as usize]
    }
}

#[cfg(test)]
mod tests {
    use linux_raw_sys::general::{RLIMIT_CORE, RLIMIT_NOFILE, RLIMIT_SIGPENDING};

    use super::{Rlimit, Rlimits};

    #[test]
    fn sigpending_default_is_explicitly_bounded() {
        let limits = Rlimits::default();
        assert_eq!(limits[RLIMIT_SIGPENDING].current, 1_024);
        assert_eq!(limits[RLIMIT_SIGPENDING].max, 1_024);
    }

    #[test]
    fn cloning_preserves_every_soft_and_hard_value() {
        let mut parent = Rlimits::default();
        parent[RLIMIT_CORE] = Rlimit::new(17, 23);
        parent[RLIMIT_NOFILE] = Rlimit::new(41, 47);
        parent[RLIMIT_SIGPENDING] = Rlimit::new(53, 59);

        let child = parent.clone();
        for resource in 0..linux_raw_sys::general::RLIM_NLIMITS {
            assert_eq!(child[resource].current, parent[resource].current);
            assert_eq!(child[resource].max, parent[resource].max);
        }
    }
}
