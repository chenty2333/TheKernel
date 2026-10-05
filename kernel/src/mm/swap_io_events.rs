//! Regular-file swap events follow Linux 7.2.3 mm/page_io.c boundaries:
//! writes count submission, reads count full-page successful completion.
use core::sync::atomic::{AtomicU64, Ordering};

struct Events {
    reads: AtomicU64,
    writes: AtomicU64,
}
impl Events {
    const fn new() -> Self {
        Self {
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
        }
    }
    fn write_submitted(&self) {
        self.writes.fetch_add(1, Ordering::Relaxed);
    }
    fn read_completed(&self, bytes: usize) {
        if bytes == memory_addr::PAGE_SIZE_4K {
            self.reads.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn snapshot(&self) -> (u64, u64) {
        (
            self.reads.load(Ordering::Relaxed),
            self.writes.load(Ordering::Relaxed),
        )
    }
}
static EVENTS: Events = Events::new();

pub(super) fn write_submitted() {
    EVENTS.write_submitted();
}
pub(super) fn read_completed(bytes: usize) {
    EVENTS.read_completed(bytes);
}
pub(crate) fn snapshot() -> (u64, u64) {
    EVENTS.snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_swap_counts_write_attempts_but_only_full_completed_reads() {
        let events = Events::new();
        events.write_submitted();
        // A submitted write stays counted even if its later I/O fails.
        assert_eq!(events.snapshot(), (0, 1));
        events.read_completed(0);
        events.read_completed(4095);
        assert_eq!(events.snapshot(), (0, 1));
        events.read_completed(4096);
        assert_eq!(events.snapshot(), (1, 1));
    }
}
