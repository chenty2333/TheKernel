//! Linux-style global load averages derived from the scheduler's lock-free
//! runnable snapshots.  The scheduler currently has no uninterruptible-task
//! counter, so this accounts the runnable component it does expose.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use axconfig::plat::MAX_CPU_NUM;
use axhal::time::monotonic_time;
use axsync::spin::SpinNoIrq;
use axtask::scheduler_load_snapshot;

const FSHIFT: u32 = 11;
const FIXED_1: u64 = 1 << FSHIFT;
const SAMPLE_SECS: u64 = 5;
const EXP_1: u64 = 1_884;
const EXP_5: u64 = 2_014;
const EXP_15: u64 = 2_037;

struct LoadAverageClock {
    last_sample_secs: u64,
}

static LOAD_CLOCK: SpinNoIrq<LoadAverageClock> = SpinNoIrq::new(LoadAverageClock {
    last_sample_secs: u64::MAX,
});
static LOAD_1: AtomicU64 = AtomicU64::new(0);
static LOAD_5: AtomicU64 = AtomicU64::new(0);
static LOAD_15: AtomicU64 = AtomicU64::new(0);
static NR_UNINTERRUPTIBLE: AtomicUsize = AtomicUsize::new(0);

/// Records a transition into or out of the real D-state hint.  This counter
/// is maintained at the state-transition point, not by allocating a task
/// snapshot in the timer IRQ path.
pub(crate) fn account_uninterruptible_transition(was: bool, is: bool) {
    match (was, is) {
        (false, true) => {
            NR_UNINTERRUPTIBLE.fetch_add(1, Ordering::Relaxed);
        }
        (true, false) => {
            let _ = NR_UNINTERRUPTIBLE
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
        }
        _ => {}
    }
}

#[inline]
const fn calc_load(load: u64, exp: u64, active: u64) -> u64 {
    let value = load
        .saturating_mul(exp)
        .saturating_add(active.saturating_mul(FIXED_1.saturating_sub(exp)));
    // Match Linux's bias toward convergence when the active count rises.
    (value.saturating_add((active >= load) as u64 * (FIXED_1 - 1))) >> FSHIFT
}

/// Linux's fixed_power_int(), used by calc_load_n for a delayed sampling
/// interval.  Rounding at each multiply keeps the fixed-point error bounded.
fn fixed_power_int(mut x: u64, mut n: u64) -> u64 {
    let mut result = FIXED_1;
    while n != 0 {
        if n & 1 != 0 {
            result = result.saturating_mul(x).saturating_add(FIXED_1 / 2) >> FSHIFT;
        }
        n >>= 1;
        if n != 0 {
            x = x.saturating_mul(x).saturating_add(FIXED_1 / 2) >> FSHIFT;
        }
    }
    result
}

#[inline]
fn runnable_tasks() -> u64 {
    (0..axhal::cpu_num().min(MAX_CPU_NUM))
        .filter_map(scheduler_load_snapshot)
        .fold(
            (NR_UNINTERRUPTIBLE.load(Ordering::Relaxed) as u64).saturating_mul(FIXED_1),
            |count, load| {
                count.saturating_add((load.runnable_tasks() as u64).saturating_mul(FIXED_1))
            },
        )
}

fn update(active: u64, intervals: u64) {
    LOAD_1.store(
        calc_load(
            LOAD_1.load(Ordering::Relaxed),
            fixed_power_int(EXP_1, intervals),
            active,
        ),
        Ordering::Relaxed,
    );
    LOAD_5.store(
        calc_load(
            LOAD_5.load(Ordering::Relaxed),
            fixed_power_int(EXP_5, intervals),
            active,
        ),
        Ordering::Relaxed,
    );
    LOAD_15.store(
        calc_load(
            LOAD_15.load(Ordering::Relaxed),
            fixed_power_int(EXP_15, intervals),
            active,
        ),
        Ordering::Relaxed,
    );
}

/// Advances global 1/5/15-minute averages at Linux's five-second cadence.
/// It is safe from the periodic tick and syscall read path; one caller wins
/// each sampling interval and readers remain lock-free.
pub(crate) fn load_average_sample_now() {
    let now = monotonic_time().as_secs() / SAMPLE_SECS * SAMPLE_SECS;
    let mut clock = LOAD_CLOCK.lock();
    let previous = clock.last_sample_secs;
    if previous == u64::MAX {
        clock.last_sample_secs = now;
        return;
    }
    if now <= previous {
        return;
    }
    // Serialize timestamp advancement and stores: a late sampler cannot
    // publish an older sample after a newer interval has already won.
    clock.last_sample_secs = now;
    update(runnable_tasks(), (now - previous) / SAMPLE_SECS);
}

/// Returns the ABI `SI_LOAD_SHIFT == 16` representation of the three Linux
/// internal FSHIFT==11 averages.
pub(crate) fn load_average_sysinfo() -> [u64; 3] {
    [
        LOAD_1.load(Ordering::Relaxed) << (16 - FSHIFT),
        LOAD_5.load(Ordering::Relaxed) << (16 - FSHIFT),
        LOAD_15.load(Ordering::Relaxed) << (16 - FSHIFT),
    ]
}

#[cfg(test)]
mod tests {
    use super::{FIXED_1, calc_load};

    #[test]
    fn load_update_uses_linux_fixed_point_bias() {
        assert_eq!(calc_load(0, FIXED_1, 0), 0);
        assert_eq!(calc_load(FIXED_1, FIXED_1, FIXED_1), FIXED_1);
        assert!(calc_load(0, 1_884, FIXED_1) > 0);
    }

    #[test]
    fn sysinfo_shift_is_16_16() {
        assert_eq!(FIXED_1 << (16 - 11), 65_536);
        assert_eq!((FIXED_1 * 3 / 2) << (16 - 11), 98_304);
    }
}
