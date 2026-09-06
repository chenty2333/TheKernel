//! Disabled-by-default VirtIO I/O counters.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static ENABLE_IO_COUNTERS: AtomicBool = AtomicBool::new(false);
static QUEUE_SYNC_WAITS: AtomicU64 = AtomicU64::new(0);
static QUEUE_SYNC_WAIT_POLLS: AtomicU64 = AtomicU64::new(0);
static QUEUE_SYNC_WAIT_IMMEDIATE: AtomicU64 = AtomicU64::new(0);
static QUEUE_NOTIFY_CALLS: AtomicU64 = AtomicU64::new(0);
static BLK_REQUESTS: AtomicU64 = AtomicU64::new(0);
static BLK_READ_REQUESTS: AtomicU64 = AtomicU64::new(0);
static BLK_WRITE_REQUESTS: AtomicU64 = AtomicU64::new(0);
static BLK_FLUSH_REQUESTS: AtomicU64 = AtomicU64::new(0);
static BLK_DATA_FENCES: AtomicU64 = AtomicU64::new(0);
static BLK_METADATA_FENCES: AtomicU64 = AtomicU64::new(0);
static BLK_FLUSH_UNSUPPORTED: AtomicU64 = AtomicU64::new(0);
static BLK_READ_BYTES: AtomicU64 = AtomicU64::new(0);
static BLK_WRITE_BYTES: AtomicU64 = AtomicU64::new(0);
static BLK_VECTORED_READ_REQUESTS: AtomicU64 = AtomicU64::new(0);
static BLK_VECTORED_WRITE_REQUESTS: AtomicU64 = AtomicU64::new(0);
static BLK_VECTORED_SEGMENTS: AtomicU64 = AtomicU64::new(0);
static BLK_PENDING_MAX_DEPTH: AtomicU64 = AtomicU64::new(0);
static BLK_PENDING_QUEUE_FULL: AtomicU64 = AtomicU64::new(0);
static BLK_PENDING_DRAIN_BATCHES: AtomicU64 = AtomicU64::new(0);
static BLK_PENDING_DRAINED_REQUESTS: AtomicU64 = AtomicU64::new(0);
static ASYNC_BLOCK_ENABLED: AtomicBool = AtomicBool::new(false);
static ASYNC_BLOCK_DEPTH: AtomicU64 = AtomicU64::new(0);
static ASYNC_BLOCK_WAIT_POLICY: AtomicU64 = AtomicU64::new(AsyncBlockWaitPolicy::Hybrid as u64);
static ASYNC_BLOCK_ADAPTIVE_ENABLED: AtomicBool = AtomicBool::new(false);
static ASYNC_BLOCK_ADAPTIVE_CURRENT_DEPTH: AtomicU64 = AtomicU64::new(1);
static ASYNC_BLOCK_ADAPTIVE_GOOD_STREAK: AtomicU64 = AtomicU64::new(0);
static ASYNC_BLOCK_MERGE_WRITE_ENABLED: AtomicBool = AtomicBool::new(false);
static BLK_ASYNC_ADAPTIVE_INCREASES: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_ADAPTIVE_DECREASES: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_ADAPTIVE_GOOD_EVENTS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_ADAPTIVE_PRESSURE_EVENTS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_MERGE_WRITE_CALLS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_MERGE_WRITE_INPUT_SEGMENTS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_MERGE_WRITE_OUTPUT_REQUESTS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_MERGE_WRITE_SAVED_REQUESTS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_MERGE_WRITE_MAX_SEGMENTS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_FLUSH_REQUESTS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_FLUSH_COMPLETIONS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_FALLBACK_SYNC: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_SUBMIT_BATCHES: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_SUBMIT_REQUESTS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_SUBMIT_BYTES: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_SUBMIT_PARTIAL_BATCHES: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_COMPLETION_BATCHES: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_COMPLETED_REQUESTS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_COMPLETED_BYTES: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_MAX_DEPTH: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_CURRENT_DEPTH: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_DESC_IN_USE_MAX: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_DESC_BUDGET: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_ADMISSION_STALLS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_QUEUE_FULL: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_NOTIFY_CALLS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_WAIT_SPINS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_WAIT_SPIN_HITS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_WAIT_YIELDS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_WAIT_SLEEPS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_WAIT_WAKEUPS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_WAIT_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_INTERRUPT_DRAINS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_IRQ_FIRST_ARMS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_IRQ_FIRST_WAITS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_IRQ_FIRST_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_IRQ_FIRST_FALLBACK_UNARMED: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_IRQ_FIRST_FALLBACK_CANNOT_BLOCK: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_IRQ_FIRST_FALLBACK_NO_IRQ: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_IRQ_FIRST_FALLBACK_REGISTER_FAILED: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_IRQ_FIRST_FALLBACK_FEATURE_DISABLED: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_SUBMIT_ERRORS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_COMPLETION_ERRORS: AtomicU64 = AtomicU64::new(0);
static BLK_ASYNC_RESOURCE_LEAKS: AtomicU64 = AtomicU64::new(0);

const ASYNC_BLOCK_ADAPTIVE_INCREASE_WINDOW: u64 = 2;

/// Runtime wait policy for the async block queue.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[repr(u64)]
pub enum AsyncBlockWaitPolicy {
    /// Drain, spin briefly, then yield/sleep through the shared completion path.
    Hybrid = 0,
    /// Force submit-one/wait-one fallback through the owned-request path.
    Sync = 1,
    /// Prefer IRQ wakeups and use the hybrid fallback unless IRQ wait is armed.
    InterruptFirst = 2,
}

impl AsyncBlockWaitPolicy {
    fn from_raw(value: u64) -> Self {
        match value {
            1 => Self::Sync,
            2 => Self::InterruptFirst,
            _ => Self::Hybrid,
        }
    }
}

/// Snapshot of VirtIO I/O counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct VirtioIoCounters {
    /// Number of synchronous queue waits.
    pub queue_sync_waits: u64,
    /// Number of `can_pop` polling iterations in synchronous queue waits.
    pub queue_sync_wait_polls: u64,
    /// Number of synchronous queue waits that completed without polling.
    pub queue_sync_wait_immediate: u64,
    /// Number of explicit queue notify calls.
    pub queue_notify_calls: u64,
    /// Total VirtIO block requests.
    pub blk_requests: u64,
    /// VirtIO block read requests.
    pub blk_read_requests: u64,
    /// VirtIO block write requests.
    pub blk_write_requests: u64,
    /// VirtIO block flush requests.
    pub blk_flush_requests: u64,
    /// Block-layer data fences that wait for earlier async writes.
    pub blk_data_fences: u64,
    /// Metadata or persistence fences that require a device flush boundary.
    pub blk_metadata_fences: u64,
    /// Flush boundaries skipped because the device did not negotiate FLUSH.
    pub blk_flush_unsupported: u64,
    /// Bytes requested through VirtIO block reads.
    pub blk_read_bytes: u64,
    /// Bytes requested through VirtIO block writes.
    pub blk_write_bytes: u64,
    /// VirtIO block vectored read requests.
    pub blk_vectored_read_requests: u64,
    /// VirtIO block vectored write requests.
    pub blk_vectored_write_requests: u64,
    /// Non-empty data segments in VirtIO block vectored requests.
    pub blk_vectored_segments: u64,
    /// Maximum number of outstanding pending block requests observed.
    pub blk_pending_max_depth: u64,
    /// Number of pending-submit attempts that found the VirtQueue full.
    pub blk_pending_queue_full: u64,
    /// Number of non-empty pending completion drain batches.
    pub blk_pending_drain_batches: u64,
    /// Number of pending requests completed by drain batches.
    pub blk_pending_drained_requests: u64,
    /// Whether the async block queue is enabled at runtime.
    pub blk_async_enabled: u64,
    /// Runtime async queue depth cap.
    pub blk_async_depth: u64,
    /// Runtime wait policy encoded as [`AsyncBlockWaitPolicy`].
    pub blk_async_wait_policy: u64,
    /// Whether adaptive queue-depth tuning is enabled.
    pub blk_async_adaptive_enabled: u64,
    /// Current adaptive queue-depth cap.
    pub blk_async_adaptive_depth: u64,
    /// Number of adaptive depth increases.
    pub blk_async_adaptive_increases: u64,
    /// Number of adaptive depth decreases.
    pub blk_async_adaptive_decreases: u64,
    /// Number of successful completion events considered by adaptive tuning.
    pub blk_async_adaptive_good_events: u64,
    /// Number of queue-pressure events considered by adaptive tuning.
    pub blk_async_adaptive_pressure_events: u64,
    /// Whether async vectored-write request merging is enabled.
    pub blk_async_merge_write_enabled: u64,
    /// Number of vectored-write calls observed by the merge path.
    pub blk_async_merge_write_calls: u64,
    /// Number of input data segments offered to the merge path.
    pub blk_async_merge_write_input_segments: u64,
    /// Number of output block requests produced by the merge path.
    pub blk_async_merge_write_output_requests: u64,
    /// Number of block requests avoided versus one segment per request.
    pub blk_async_merge_write_saved_requests: u64,
    /// Maximum data segments allowed in one merged write request.
    pub blk_async_merge_write_max_segments: u64,
    /// Flush requests submitted through the async queue.
    pub blk_async_flush_requests: u64,
    /// Flush requests completed through the async queue.
    pub blk_async_flush_completions: u64,
    /// Number of async-capable operations forced through synchronous fallback.
    pub blk_async_fallback_sync: u64,
    /// Number of async submit batches.
    pub blk_async_submit_batches: u64,
    /// Number of requests submitted through async batches.
    pub blk_async_submit_requests: u64,
    /// Bytes submitted through async batches.
    pub blk_async_submit_bytes: u64,
    /// Number of batches that submitted only a prefix of requested work.
    pub blk_async_submit_partial_batches: u64,
    /// Number of completion drain batches for async requests.
    pub blk_async_completion_batches: u64,
    /// Number of async requests completed.
    pub blk_async_completed_requests: u64,
    /// Bytes completed through async requests.
    pub blk_async_completed_bytes: u64,
    /// Maximum observed async queue depth.
    pub blk_async_max_depth: u64,
    /// Current async queue depth at snapshot time.
    pub blk_async_current_depth: u64,
    /// Maximum observed VirtIO descriptor use by async requests.
    pub blk_async_desc_in_use_max: u64,
    /// Current descriptor budget exposed to async admission.
    pub blk_async_desc_budget: u64,
    /// Number of descriptor/request admission stalls.
    pub blk_async_admission_stalls: u64,
    /// Number of async queue-full events.
    pub blk_async_queue_full: u64,
    /// Number of async queue notify calls.
    pub blk_async_notify_calls: u64,
    /// Number of short-spin iterations in async waits.
    pub blk_async_wait_spins: u64,
    /// Number of async waits satisfied during the short-spin phase.
    pub blk_async_wait_spin_hits: u64,
    /// Number of async waits that yielded.
    pub blk_async_wait_yields: u64,
    /// Number of async waits that slept on a completion event.
    pub blk_async_wait_sleeps: u64,
    /// Number of async completion wakeups.
    pub blk_async_wait_wakeups: u64,
    /// Number of async wait timeout/fallback wakeups.
    pub blk_async_wait_timeouts: u64,
    /// Number of completion drains from the interrupt path.
    pub blk_async_interrupt_drains: u64,
    /// Number of devices armed for IRQ-first waits.
    pub blk_async_irq_first_arms: u64,
    /// Number of no-timeout IRQ-first waits entered.
    pub blk_async_irq_first_waits: u64,
    /// Number of IRQ-first waits that fell back to the hybrid policy.
    pub blk_async_irq_first_fallbacks: u64,
    /// Number of IRQ-first fallbacks because no usable IRQ wait was armed.
    pub blk_async_irq_first_fallback_unarmed: u64,
    /// Number of IRQ-first fallbacks because the current context cannot block.
    pub blk_async_irq_first_fallback_cannot_block: u64,
    /// Number of IRQ-first fallbacks because the block device has no IRQ.
    pub blk_async_irq_first_fallback_no_irq: u64,
    /// Number of IRQ-first fallbacks because IRQ handler registration failed.
    pub blk_async_irq_first_fallback_register_failed: u64,
    /// Number of IRQ-first fallbacks because the driver was built without IRQ support.
    pub blk_async_irq_first_fallback_feature_disabled: u64,
    /// Number of async submit errors.
    pub blk_async_submit_errors: u64,
    /// Number of async completion errors.
    pub blk_async_completion_errors: u64,
    /// Number of leaked async request resources detected.
    pub blk_async_resource_leaks: u64,
}

/// Enables or disables VirtIO I/O counters.
pub fn set_io_counters_enabled(enabled: bool) {
    ENABLE_IO_COUNTERS.store(enabled, Ordering::Relaxed);
}

/// Resets VirtIO I/O counters.
pub fn reset_io_counters() {
    for counter in [
        &QUEUE_SYNC_WAITS,
        &QUEUE_SYNC_WAIT_POLLS,
        &QUEUE_SYNC_WAIT_IMMEDIATE,
        &QUEUE_NOTIFY_CALLS,
        &BLK_REQUESTS,
        &BLK_READ_REQUESTS,
        &BLK_WRITE_REQUESTS,
        &BLK_FLUSH_REQUESTS,
        &BLK_DATA_FENCES,
        &BLK_METADATA_FENCES,
        &BLK_FLUSH_UNSUPPORTED,
        &BLK_READ_BYTES,
        &BLK_WRITE_BYTES,
        &BLK_VECTORED_READ_REQUESTS,
        &BLK_VECTORED_WRITE_REQUESTS,
        &BLK_VECTORED_SEGMENTS,
        &BLK_PENDING_MAX_DEPTH,
        &BLK_PENDING_QUEUE_FULL,
        &BLK_PENDING_DRAIN_BATCHES,
        &BLK_PENDING_DRAINED_REQUESTS,
        &BLK_ASYNC_ADAPTIVE_INCREASES,
        &BLK_ASYNC_ADAPTIVE_DECREASES,
        &BLK_ASYNC_ADAPTIVE_GOOD_EVENTS,
        &BLK_ASYNC_ADAPTIVE_PRESSURE_EVENTS,
        &BLK_ASYNC_MERGE_WRITE_CALLS,
        &BLK_ASYNC_MERGE_WRITE_INPUT_SEGMENTS,
        &BLK_ASYNC_MERGE_WRITE_OUTPUT_REQUESTS,
        &BLK_ASYNC_MERGE_WRITE_SAVED_REQUESTS,
        &BLK_ASYNC_MERGE_WRITE_MAX_SEGMENTS,
        &BLK_ASYNC_FLUSH_REQUESTS,
        &BLK_ASYNC_FLUSH_COMPLETIONS,
        &BLK_ASYNC_FALLBACK_SYNC,
        &BLK_ASYNC_SUBMIT_BATCHES,
        &BLK_ASYNC_SUBMIT_REQUESTS,
        &BLK_ASYNC_SUBMIT_BYTES,
        &BLK_ASYNC_SUBMIT_PARTIAL_BATCHES,
        &BLK_ASYNC_COMPLETION_BATCHES,
        &BLK_ASYNC_COMPLETED_REQUESTS,
        &BLK_ASYNC_COMPLETED_BYTES,
        &BLK_ASYNC_MAX_DEPTH,
        &BLK_ASYNC_CURRENT_DEPTH,
        &BLK_ASYNC_DESC_IN_USE_MAX,
        &BLK_ASYNC_DESC_BUDGET,
        &BLK_ASYNC_ADMISSION_STALLS,
        &BLK_ASYNC_QUEUE_FULL,
        &BLK_ASYNC_NOTIFY_CALLS,
        &BLK_ASYNC_WAIT_SPINS,
        &BLK_ASYNC_WAIT_SPIN_HITS,
        &BLK_ASYNC_WAIT_YIELDS,
        &BLK_ASYNC_WAIT_SLEEPS,
        &BLK_ASYNC_WAIT_WAKEUPS,
        &BLK_ASYNC_WAIT_TIMEOUTS,
        &BLK_ASYNC_INTERRUPT_DRAINS,
        &BLK_ASYNC_IRQ_FIRST_ARMS,
        &BLK_ASYNC_IRQ_FIRST_WAITS,
        &BLK_ASYNC_IRQ_FIRST_FALLBACKS,
        &BLK_ASYNC_IRQ_FIRST_FALLBACK_UNARMED,
        &BLK_ASYNC_IRQ_FIRST_FALLBACK_CANNOT_BLOCK,
        &BLK_ASYNC_IRQ_FIRST_FALLBACK_NO_IRQ,
        &BLK_ASYNC_IRQ_FIRST_FALLBACK_REGISTER_FAILED,
        &BLK_ASYNC_IRQ_FIRST_FALLBACK_FEATURE_DISABLED,
        &BLK_ASYNC_SUBMIT_ERRORS,
        &BLK_ASYNC_COMPLETION_ERRORS,
        &BLK_ASYNC_RESOURCE_LEAKS,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
    reset_async_block_adaptive_depth();
}

/// Enables or disables async block-queue behavior.
pub fn set_async_block_enabled(enabled: bool) {
    ASYNC_BLOCK_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Returns whether async block-queue behavior is enabled.
pub fn async_block_enabled() -> bool {
    ASYNC_BLOCK_ENABLED.load(Ordering::Relaxed)
}

/// Sets the async block queue depth cap.
pub fn set_async_block_depth(depth: u64) {
    ASYNC_BLOCK_DEPTH.store(depth, Ordering::Relaxed);
}

/// Returns the configured async block queue depth cap.
pub fn async_block_depth() -> u64 {
    ASYNC_BLOCK_DEPTH.load(Ordering::Relaxed)
}

/// Sets the async block wait policy.
pub fn set_async_block_wait_policy(policy: AsyncBlockWaitPolicy) {
    ASYNC_BLOCK_WAIT_POLICY.store(policy as u64, Ordering::Relaxed);
}

/// Returns the async block wait policy.
pub fn async_block_wait_policy() -> AsyncBlockWaitPolicy {
    AsyncBlockWaitPolicy::from_raw(ASYNC_BLOCK_WAIT_POLICY.load(Ordering::Relaxed))
}

/// Enables or disables adaptive async block queue-depth tuning.
pub fn set_async_block_adaptive_enabled(enabled: bool) {
    ASYNC_BLOCK_ADAPTIVE_ENABLED.store(enabled, Ordering::Relaxed);
    reset_async_block_adaptive_depth();
}

/// Returns whether adaptive async block queue-depth tuning is enabled.
pub fn async_block_adaptive_enabled() -> bool {
    ASYNC_BLOCK_ADAPTIVE_ENABLED.load(Ordering::Relaxed)
}

/// Enables or disables async vectored-write request merging.
pub fn set_async_block_merge_write_enabled(enabled: bool) {
    ASYNC_BLOCK_MERGE_WRITE_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Returns whether async vectored-write request merging is enabled.
pub fn async_block_merge_write_enabled() -> bool {
    ASYNC_BLOCK_MERGE_WRITE_ENABLED.load(Ordering::Relaxed)
}

/// Resets adaptive queue-depth state to the conservative starting point.
pub fn reset_async_block_adaptive_depth() {
    ASYNC_BLOCK_ADAPTIVE_CURRENT_DEPTH.store(1, Ordering::Relaxed);
    ASYNC_BLOCK_ADAPTIVE_GOOD_STREAK.store(0, Ordering::Relaxed);
}

/// Returns the effective async queue depth after optional adaptive tuning.
pub(crate) fn async_block_effective_depth(configured_cap: usize) -> usize {
    let configured_cap = configured_cap.max(1) as u64;
    if !async_block_adaptive_enabled() {
        return configured_cap as usize;
    }
    adaptive_depth_clamped(configured_cap) as usize
}

/// Returns a snapshot of VirtIO I/O counters.
pub fn io_counters_snapshot() -> VirtioIoCounters {
    VirtioIoCounters {
        queue_sync_waits: QUEUE_SYNC_WAITS.load(Ordering::Relaxed),
        queue_sync_wait_polls: QUEUE_SYNC_WAIT_POLLS.load(Ordering::Relaxed),
        queue_sync_wait_immediate: QUEUE_SYNC_WAIT_IMMEDIATE.load(Ordering::Relaxed),
        queue_notify_calls: QUEUE_NOTIFY_CALLS.load(Ordering::Relaxed),
        blk_requests: BLK_REQUESTS.load(Ordering::Relaxed),
        blk_read_requests: BLK_READ_REQUESTS.load(Ordering::Relaxed),
        blk_write_requests: BLK_WRITE_REQUESTS.load(Ordering::Relaxed),
        blk_flush_requests: BLK_FLUSH_REQUESTS.load(Ordering::Relaxed),
        blk_data_fences: BLK_DATA_FENCES.load(Ordering::Relaxed),
        blk_metadata_fences: BLK_METADATA_FENCES.load(Ordering::Relaxed),
        blk_flush_unsupported: BLK_FLUSH_UNSUPPORTED.load(Ordering::Relaxed),
        blk_read_bytes: BLK_READ_BYTES.load(Ordering::Relaxed),
        blk_write_bytes: BLK_WRITE_BYTES.load(Ordering::Relaxed),
        blk_vectored_read_requests: BLK_VECTORED_READ_REQUESTS.load(Ordering::Relaxed),
        blk_vectored_write_requests: BLK_VECTORED_WRITE_REQUESTS.load(Ordering::Relaxed),
        blk_vectored_segments: BLK_VECTORED_SEGMENTS.load(Ordering::Relaxed),
        blk_pending_max_depth: BLK_PENDING_MAX_DEPTH.load(Ordering::Relaxed),
        blk_pending_queue_full: BLK_PENDING_QUEUE_FULL.load(Ordering::Relaxed),
        blk_pending_drain_batches: BLK_PENDING_DRAIN_BATCHES.load(Ordering::Relaxed),
        blk_pending_drained_requests: BLK_PENDING_DRAINED_REQUESTS.load(Ordering::Relaxed),
        blk_async_enabled: if ASYNC_BLOCK_ENABLED.load(Ordering::Relaxed) {
            1
        } else {
            0
        },
        blk_async_depth: ASYNC_BLOCK_DEPTH.load(Ordering::Relaxed),
        blk_async_wait_policy: ASYNC_BLOCK_WAIT_POLICY.load(Ordering::Relaxed),
        blk_async_adaptive_enabled: if ASYNC_BLOCK_ADAPTIVE_ENABLED.load(Ordering::Relaxed) {
            1
        } else {
            0
        },
        blk_async_adaptive_depth: ASYNC_BLOCK_ADAPTIVE_CURRENT_DEPTH.load(Ordering::Relaxed),
        blk_async_adaptive_increases: BLK_ASYNC_ADAPTIVE_INCREASES.load(Ordering::Relaxed),
        blk_async_adaptive_decreases: BLK_ASYNC_ADAPTIVE_DECREASES.load(Ordering::Relaxed),
        blk_async_adaptive_good_events: BLK_ASYNC_ADAPTIVE_GOOD_EVENTS.load(Ordering::Relaxed),
        blk_async_adaptive_pressure_events: BLK_ASYNC_ADAPTIVE_PRESSURE_EVENTS
            .load(Ordering::Relaxed),
        blk_async_merge_write_enabled: if ASYNC_BLOCK_MERGE_WRITE_ENABLED.load(Ordering::Relaxed) {
            1
        } else {
            0
        },
        blk_async_merge_write_calls: BLK_ASYNC_MERGE_WRITE_CALLS.load(Ordering::Relaxed),
        blk_async_merge_write_input_segments: BLK_ASYNC_MERGE_WRITE_INPUT_SEGMENTS
            .load(Ordering::Relaxed),
        blk_async_merge_write_output_requests: BLK_ASYNC_MERGE_WRITE_OUTPUT_REQUESTS
            .load(Ordering::Relaxed),
        blk_async_merge_write_saved_requests: BLK_ASYNC_MERGE_WRITE_SAVED_REQUESTS
            .load(Ordering::Relaxed),
        blk_async_merge_write_max_segments: BLK_ASYNC_MERGE_WRITE_MAX_SEGMENTS
            .load(Ordering::Relaxed),
        blk_async_flush_requests: BLK_ASYNC_FLUSH_REQUESTS.load(Ordering::Relaxed),
        blk_async_flush_completions: BLK_ASYNC_FLUSH_COMPLETIONS.load(Ordering::Relaxed),
        blk_async_fallback_sync: BLK_ASYNC_FALLBACK_SYNC.load(Ordering::Relaxed),
        blk_async_submit_batches: BLK_ASYNC_SUBMIT_BATCHES.load(Ordering::Relaxed),
        blk_async_submit_requests: BLK_ASYNC_SUBMIT_REQUESTS.load(Ordering::Relaxed),
        blk_async_submit_bytes: BLK_ASYNC_SUBMIT_BYTES.load(Ordering::Relaxed),
        blk_async_submit_partial_batches: BLK_ASYNC_SUBMIT_PARTIAL_BATCHES.load(Ordering::Relaxed),
        blk_async_completion_batches: BLK_ASYNC_COMPLETION_BATCHES.load(Ordering::Relaxed),
        blk_async_completed_requests: BLK_ASYNC_COMPLETED_REQUESTS.load(Ordering::Relaxed),
        blk_async_completed_bytes: BLK_ASYNC_COMPLETED_BYTES.load(Ordering::Relaxed),
        blk_async_max_depth: BLK_ASYNC_MAX_DEPTH.load(Ordering::Relaxed),
        blk_async_current_depth: BLK_ASYNC_CURRENT_DEPTH.load(Ordering::Relaxed),
        blk_async_desc_in_use_max: BLK_ASYNC_DESC_IN_USE_MAX.load(Ordering::Relaxed),
        blk_async_desc_budget: BLK_ASYNC_DESC_BUDGET.load(Ordering::Relaxed),
        blk_async_admission_stalls: BLK_ASYNC_ADMISSION_STALLS.load(Ordering::Relaxed),
        blk_async_queue_full: BLK_ASYNC_QUEUE_FULL.load(Ordering::Relaxed),
        blk_async_notify_calls: BLK_ASYNC_NOTIFY_CALLS.load(Ordering::Relaxed),
        blk_async_wait_spins: BLK_ASYNC_WAIT_SPINS.load(Ordering::Relaxed),
        blk_async_wait_spin_hits: BLK_ASYNC_WAIT_SPIN_HITS.load(Ordering::Relaxed),
        blk_async_wait_yields: BLK_ASYNC_WAIT_YIELDS.load(Ordering::Relaxed),
        blk_async_wait_sleeps: BLK_ASYNC_WAIT_SLEEPS.load(Ordering::Relaxed),
        blk_async_wait_wakeups: BLK_ASYNC_WAIT_WAKEUPS.load(Ordering::Relaxed),
        blk_async_wait_timeouts: BLK_ASYNC_WAIT_TIMEOUTS.load(Ordering::Relaxed),
        blk_async_interrupt_drains: BLK_ASYNC_INTERRUPT_DRAINS.load(Ordering::Relaxed),
        blk_async_irq_first_arms: BLK_ASYNC_IRQ_FIRST_ARMS.load(Ordering::Relaxed),
        blk_async_irq_first_waits: BLK_ASYNC_IRQ_FIRST_WAITS.load(Ordering::Relaxed),
        blk_async_irq_first_fallbacks: BLK_ASYNC_IRQ_FIRST_FALLBACKS.load(Ordering::Relaxed),
        blk_async_irq_first_fallback_unarmed: BLK_ASYNC_IRQ_FIRST_FALLBACK_UNARMED
            .load(Ordering::Relaxed),
        blk_async_irq_first_fallback_cannot_block: BLK_ASYNC_IRQ_FIRST_FALLBACK_CANNOT_BLOCK
            .load(Ordering::Relaxed),
        blk_async_irq_first_fallback_no_irq: BLK_ASYNC_IRQ_FIRST_FALLBACK_NO_IRQ
            .load(Ordering::Relaxed),
        blk_async_irq_first_fallback_register_failed: BLK_ASYNC_IRQ_FIRST_FALLBACK_REGISTER_FAILED
            .load(Ordering::Relaxed),
        blk_async_irq_first_fallback_feature_disabled:
            BLK_ASYNC_IRQ_FIRST_FALLBACK_FEATURE_DISABLED.load(Ordering::Relaxed),
        blk_async_submit_errors: BLK_ASYNC_SUBMIT_ERRORS.load(Ordering::Relaxed),
        blk_async_completion_errors: BLK_ASYNC_COMPLETION_ERRORS.load(Ordering::Relaxed),
        blk_async_resource_leaks: BLK_ASYNC_RESOURCE_LEAKS.load(Ordering::Relaxed),
    }
}

pub(crate) fn io_counters_enabled() -> bool {
    ENABLE_IO_COUNTERS.load(Ordering::Relaxed)
}

pub(crate) fn record_queue_sync_wait(polls: u64, notified: bool) {
    if !io_counters_enabled() {
        return;
    }
    QUEUE_SYNC_WAITS.fetch_add(1, Ordering::Relaxed);
    QUEUE_SYNC_WAIT_POLLS.fetch_add(polls, Ordering::Relaxed);
    if polls == 0 {
        QUEUE_SYNC_WAIT_IMMEDIATE.fetch_add(1, Ordering::Relaxed);
    }
    if notified {
        QUEUE_NOTIFY_CALLS.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn record_blk_read(bytes: usize, vectored_segments: usize) {
    if !io_counters_enabled() {
        return;
    }
    BLK_REQUESTS.fetch_add(1, Ordering::Relaxed);
    BLK_READ_REQUESTS.fetch_add(1, Ordering::Relaxed);
    BLK_READ_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    if vectored_segments != 0 {
        BLK_VECTORED_READ_REQUESTS.fetch_add(1, Ordering::Relaxed);
        BLK_VECTORED_SEGMENTS.fetch_add(vectored_segments as u64, Ordering::Relaxed);
    }
}

pub(crate) fn record_blk_write(bytes: usize, vectored_segments: usize) {
    if !io_counters_enabled() {
        return;
    }
    BLK_REQUESTS.fetch_add(1, Ordering::Relaxed);
    BLK_WRITE_REQUESTS.fetch_add(1, Ordering::Relaxed);
    BLK_WRITE_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    if vectored_segments != 0 {
        BLK_VECTORED_WRITE_REQUESTS.fetch_add(1, Ordering::Relaxed);
        BLK_VECTORED_SEGMENTS.fetch_add(vectored_segments as u64, Ordering::Relaxed);
    }
}

pub(crate) fn record_blk_flush() {
    if !io_counters_enabled() {
        return;
    }
    BLK_REQUESTS.fetch_add(1, Ordering::Relaxed);
    BLK_FLUSH_REQUESTS.fetch_add(1, Ordering::Relaxed);
}

/// Records a block-layer data fence that waits for earlier writes to complete.
pub fn record_blk_data_fence() {
    if !io_counters_enabled() {
        return;
    }
    BLK_DATA_FENCES.fetch_add(1, Ordering::Relaxed);
}

/// Records a metadata or persistence fence that must issue a device flush.
pub fn record_blk_metadata_fence() {
    if !io_counters_enabled() {
        return;
    }
    BLK_METADATA_FENCES.fetch_add(1, Ordering::Relaxed);
}

/// Records that a requested device flush was unsupported by this block device.
pub fn record_blk_flush_unsupported() {
    if !io_counters_enabled() {
        return;
    }
    BLK_FLUSH_UNSUPPORTED.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_blk_async_flush_request() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_FLUSH_REQUESTS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_blk_async_flush_completion() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_FLUSH_COMPLETIONS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_blk_pending_depth(depth: usize) {
    if !io_counters_enabled() {
        return;
    }
    let depth = depth as u64;
    let mut current = BLK_PENDING_MAX_DEPTH.load(Ordering::Relaxed);
    while depth > current {
        match BLK_PENDING_MAX_DEPTH.compare_exchange_weak(
            current,
            depth,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

pub(crate) fn record_blk_pending_queue_full() {
    if !io_counters_enabled() {
        return;
    }
    BLK_PENDING_QUEUE_FULL.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_blk_pending_drain(drained: usize) {
    if !io_counters_enabled() || drained == 0 {
        return;
    }
    BLK_PENDING_DRAIN_BATCHES.fetch_add(1, Ordering::Relaxed);
    BLK_PENDING_DRAINED_REQUESTS.fetch_add(drained as u64, Ordering::Relaxed);
}

pub(crate) fn record_blk_async_submit_batch(
    submitted: usize,
    bytes: usize,
    partial: bool,
    depth: usize,
    desc_in_use: usize,
    desc_budget: usize,
    notified: bool,
) {
    if !io_counters_enabled() || submitted == 0 {
        return;
    }
    BLK_ASYNC_SUBMIT_BATCHES.fetch_add(1, Ordering::Relaxed);
    BLK_ASYNC_SUBMIT_REQUESTS.fetch_add(submitted as u64, Ordering::Relaxed);
    BLK_ASYNC_SUBMIT_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    if partial {
        BLK_ASYNC_SUBMIT_PARTIAL_BATCHES.fetch_add(1, Ordering::Relaxed);
    }
    if notified {
        BLK_ASYNC_NOTIFY_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    BLK_ASYNC_CURRENT_DEPTH.store(depth as u64, Ordering::Relaxed);
    BLK_ASYNC_DESC_BUDGET.store(desc_budget as u64, Ordering::Relaxed);
    update_max(&BLK_ASYNC_MAX_DEPTH, depth as u64);
    update_max(&BLK_ASYNC_DESC_IN_USE_MAX, desc_in_use as u64);
}

pub(crate) fn record_blk_async_completion(drained: usize, bytes: usize, depth: usize) {
    if !io_counters_enabled() || drained == 0 {
        return;
    }
    BLK_ASYNC_COMPLETION_BATCHES.fetch_add(1, Ordering::Relaxed);
    BLK_ASYNC_COMPLETED_REQUESTS.fetch_add(drained as u64, Ordering::Relaxed);
    BLK_ASYNC_COMPLETED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    BLK_ASYNC_CURRENT_DEPTH.store(depth as u64, Ordering::Relaxed);
}

pub(crate) fn record_blk_async_adaptive_completion(drained: usize, configured_cap: usize) {
    if drained == 0 || !async_block_adaptive_enabled() {
        return;
    }
    let configured_cap = configured_cap.max(1) as u64;
    if io_counters_enabled() {
        BLK_ASYNC_ADAPTIVE_GOOD_EVENTS.fetch_add(drained as u64, Ordering::Relaxed);
    }

    let current = adaptive_depth_clamped(configured_cap);
    if current >= configured_cap {
        ASYNC_BLOCK_ADAPTIVE_GOOD_STREAK.store(0, Ordering::Relaxed);
        return;
    }

    let good = ASYNC_BLOCK_ADAPTIVE_GOOD_STREAK
        .fetch_add(drained as u64, Ordering::Relaxed)
        .saturating_add(drained as u64);
    if good < ASYNC_BLOCK_ADAPTIVE_INCREASE_WINDOW {
        return;
    }

    ASYNC_BLOCK_ADAPTIVE_GOOD_STREAK.store(0, Ordering::Relaxed);
    let mut observed = ASYNC_BLOCK_ADAPTIVE_CURRENT_DEPTH.load(Ordering::Relaxed);
    loop {
        let next = observed.clamp(1, configured_cap).saturating_add(1);
        if next > configured_cap {
            return;
        }
        match ASYNC_BLOCK_ADAPTIVE_CURRENT_DEPTH.compare_exchange_weak(
            observed,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                if io_counters_enabled() {
                    BLK_ASYNC_ADAPTIVE_INCREASES.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
            Err(actual) => observed = actual,
        }
    }
}

/// Records one async vectored-write request merging decision.
pub fn record_blk_async_merge_write(
    input_segments: usize,
    output_requests: usize,
    max_segments_per_request: usize,
) {
    if !io_counters_enabled() || input_segments == 0 || output_requests == 0 {
        return;
    }
    BLK_ASYNC_MERGE_WRITE_CALLS.fetch_add(1, Ordering::Relaxed);
    BLK_ASYNC_MERGE_WRITE_INPUT_SEGMENTS.fetch_add(input_segments as u64, Ordering::Relaxed);
    BLK_ASYNC_MERGE_WRITE_OUTPUT_REQUESTS.fetch_add(output_requests as u64, Ordering::Relaxed);
    BLK_ASYNC_MERGE_WRITE_SAVED_REQUESTS.fetch_add(
        input_segments.saturating_sub(output_requests) as u64,
        Ordering::Relaxed,
    );
    update_max(
        &BLK_ASYNC_MERGE_WRITE_MAX_SEGMENTS,
        max_segments_per_request as u64,
    );
}

/// Records one short spin iteration while waiting for async block completion.
pub fn record_blk_async_wait_spin() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_WAIT_SPINS.fetch_add(1, Ordering::Relaxed);
}

/// Records that a pending async block request completed during the short spin window.
pub fn record_blk_async_wait_spin_hit() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_WAIT_SPIN_HITS.fetch_add(1, Ordering::Relaxed);
}

/// Records one scheduler yield in the async block completion fallback path.
pub fn record_blk_async_wait_yield() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_WAIT_YIELDS.fetch_add(1, Ordering::Relaxed);
}

/// Records one sleep in the async block completion path.
pub fn record_blk_async_wait_sleep() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_WAIT_SLEEPS.fetch_add(1, Ordering::Relaxed);
}

/// Records one wakeup from the async block completion path.
pub fn record_blk_async_wait_wakeup() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_WAIT_WAKEUPS.fetch_add(1, Ordering::Relaxed);
}

/// Records one timeout fallback while waiting for async block completion.
pub fn record_blk_async_wait_timeout() {
    record_blk_async_adaptive_pressure();
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_WAIT_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
}

/// Records one completion drain reached from a VirtIO interrupt path.
pub fn record_blk_async_interrupt_drain() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_INTERRUPT_DRAINS.fetch_add(1, Ordering::Relaxed);
}

/// Records one device armed for IRQ-first wait experiments.
pub fn record_blk_async_irq_first_arm() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_IRQ_FIRST_ARMS.fetch_add(1, Ordering::Relaxed);
}

/// Records one wait that used the no-timeout IRQ-first completion path.
pub fn record_blk_async_irq_first_wait() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_IRQ_FIRST_WAITS.fetch_add(1, Ordering::Relaxed);
}

/// Records one IRQ-first wait that could not safely block on IRQ completion.
pub fn record_blk_async_irq_first_fallback() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_IRQ_FIRST_FALLBACKS.fetch_add(1, Ordering::Relaxed);
}

/// Records one IRQ-first fallback because no usable IRQ wait was armed.
pub fn record_blk_async_irq_first_fallback_unarmed() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_IRQ_FIRST_FALLBACK_UNARMED.fetch_add(1, Ordering::Relaxed);
}

/// Records one IRQ-first fallback because the current context cannot block.
pub fn record_blk_async_irq_first_fallback_cannot_block() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_IRQ_FIRST_FALLBACK_CANNOT_BLOCK.fetch_add(1, Ordering::Relaxed);
}

/// Records one IRQ-first fallback because the block device has no IRQ.
pub fn record_blk_async_irq_first_fallback_no_irq() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_IRQ_FIRST_FALLBACK_NO_IRQ.fetch_add(1, Ordering::Relaxed);
}

/// Records one IRQ-first fallback because IRQ handler registration failed.
pub fn record_blk_async_irq_first_fallback_register_failed() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_IRQ_FIRST_FALLBACK_REGISTER_FAILED.fetch_add(1, Ordering::Relaxed);
}

/// Records one IRQ-first fallback because the driver was built without IRQ support.
pub fn record_blk_async_irq_first_fallback_feature_disabled() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_IRQ_FIRST_FALLBACK_FEATURE_DISABLED.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_blk_async_queue_full() {
    record_blk_async_adaptive_pressure();
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_QUEUE_FULL.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_blk_async_admission_stall() {
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_ADMISSION_STALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_blk_async_completion_error() {
    record_blk_async_adaptive_pressure();
    if !io_counters_enabled() {
        return;
    }
    BLK_ASYNC_COMPLETION_ERRORS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_blk_async_resource_leaks(leaks: usize) {
    if !io_counters_enabled() || leaks == 0 {
        return;
    }
    BLK_ASYNC_RESOURCE_LEAKS.fetch_add(leaks as u64, Ordering::Relaxed);
}

fn update_max(counter: &AtomicU64, value: u64) {
    let mut current = counter.load(Ordering::Relaxed);
    while value > current {
        match counter.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

fn adaptive_depth_clamped(configured_cap: u64) -> u64 {
    let configured_cap = configured_cap.max(1);
    let current = ASYNC_BLOCK_ADAPTIVE_CURRENT_DEPTH.load(Ordering::Relaxed);
    let clamped = current.clamp(1, configured_cap);
    if clamped != current {
        ASYNC_BLOCK_ADAPTIVE_CURRENT_DEPTH.store(clamped, Ordering::Relaxed);
    }
    clamped
}

fn record_blk_async_adaptive_pressure() {
    if !async_block_adaptive_enabled() {
        return;
    }
    ASYNC_BLOCK_ADAPTIVE_GOOD_STREAK.store(0, Ordering::Relaxed);
    if io_counters_enabled() {
        BLK_ASYNC_ADAPTIVE_PRESSURE_EVENTS.fetch_add(1, Ordering::Relaxed);
    }

    let mut observed = ASYNC_BLOCK_ADAPTIVE_CURRENT_DEPTH.load(Ordering::Relaxed);
    loop {
        let current = observed.max(1);
        if current <= 1 {
            if observed != 1 {
                let _ = ASYNC_BLOCK_ADAPTIVE_CURRENT_DEPTH.compare_exchange_weak(
                    observed,
                    1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
            }
            return;
        }
        let next = current - 1;
        match ASYNC_BLOCK_ADAPTIVE_CURRENT_DEPTH.compare_exchange_weak(
            observed,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                if io_counters_enabled() {
                    BLK_ASYNC_ADAPTIVE_DECREASES.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
            Err(actual) => observed = actual,
        }
    }
}
