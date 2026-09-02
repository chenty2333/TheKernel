//! Bounded transparent-huge-page promotion worker.
//!
//! This is deliberately independent from memory-pressure reclaim: explicit
//! MADV_HUGEPAGE and the mm default must make progress while memory is healthy.
//! Each wake retains at most one address-space lock and scans a fixed PMD
//! budget, so a large mm cannot monopolize the worker.

use alloc::string::String;
use core::time::Duration;

use kspin::SpinNoIrq;
use memory_addr::VirtAddr;

const KHUGEPAGED_INTERVAL_MS: u64 = 500;
const KHUGEPAGED_PMD_BUDGET: usize = 64;

#[derive(Clone, Copy)]
struct KhugepagedCursor {
    mm_index: usize,
    address: usize,
}

static KHUGEPAGED_CURSOR: SpinNoIrq<KhugepagedCursor> = SpinNoIrq::new(KhugepagedCursor {
    mm_index: 0,
    address: 0,
});

fn scan_once() {
    let spaces = crate::mm::live_address_spaces();
    if spaces.is_empty() {
        *KHUGEPAGED_CURSOR.lock() = KhugepagedCursor {
            mm_index: 0,
            address: 0,
        };
        return;
    }

    let snapshot = *KHUGEPAGED_CURSOR.lock();
    let mm_index = snapshot.mm_index.min(spaces.len() - 1);
    let next = spaces[mm_index]
        .lock()
        .collapse_background_thp_budget(VirtAddr::from(snapshot.address), KHUGEPAGED_PMD_BUDGET);

    let mut cursor = KHUGEPAGED_CURSOR.lock();
    if let Some(next) = next {
        cursor.mm_index = mm_index;
        cursor.address = next.as_usize();
    } else {
        cursor.mm_index = (mm_index + 1) % spaces.len();
        cursor.address = 0;
    }
}

fn khugepaged_worker() {
    loop {
        if let Err(error) = axtask::sleep(Duration::from_millis(KHUGEPAGED_INTERVAL_MS)) {
            error!("khugepaged worker stopped: {error}");
            return;
        }
        scan_once();
        axtask::yield_now();
    }
}

pub(crate) fn init_khugepaged() {
    let mut name = String::new();
    name.try_reserve_exact("khugepaged".len())
        .expect("failed to allocate khugepaged worker name");
    name.push_str("khugepaged");
    axtask::try_spawn_with_name(khugepaged_worker, name)
        .expect("failed to start khugepaged worker");
}
