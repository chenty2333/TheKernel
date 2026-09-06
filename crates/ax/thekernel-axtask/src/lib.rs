//! TheKernel generic task-management mechanisms, derived from the
//! [ArceOS `axtask`](https://github.com/arceos-org/arceos/tree/main/modules/axtask)
//! module.
//!
//! This module provides primitives for task management, including task
//! creation, scheduling, sleeping, termination, etc. The scheduler algorithm
//! is configurable by cargo features.
//!
//! # Cargo Features
//!
//! - `multitask`: Enable multi-task support. If it's enabled, complex task
//!   management and scheduling is used, as well as more task-related APIs.
//!   Otherwise, only a few APIs with naive implementation is available.
//! - `irq`: Interrupts are enabled. If this feature is enabled, timer-based
//!   APIs can be used, such as [`sleep`], [`sleep_until`], and
//!   [`WaitQueue::wait_timeout`].
//! - `preempt`: Enable preemptive scheduling.
//! - `sched-fifo`: Use the [FIFO cooperative scheduler][1]. It also enables the
//!   `multitask` feature. FIFO is also the fallback when `multitask` is enabled
//!   without selecting another scheduler feature.
//! - `sched-rr`: Use the [Round-robin preemptive scheduler][2]. It also enables
//!   the `multitask` and `preempt` features if it is enabled.
//! - `sched-eevdf`: Use the [EEVDF scheduler][3]. It also enables the
//!   `multitask` and `preempt` features if it is enabled.
//! - `idle-steal`: Enable the bounded pull-only idle-stealing path. This
//!   feature is only available for x86_64 and automatically enables `smp` and
//!   `sched-eevdf`.
//!
//! [1]: axsched::FifoScheduler
//! [2]: axsched::RRScheduler
//! [3]: axsched::EEVDFScheduler

#![cfg_attr(not(test), no_std)]
#![feature(doc_cfg)]
#![cfg_attr(feature = "multitask", feature(allocator_api))]

#[cfg(feature = "tls")]
compile_error!(
    "thekernel-axtask 0.1.0 does not support TLS tasks: axhal must first expose fallible TLS allocation"
);

#[cfg(all(feature = "idle-steal", not(target_arch = "x86_64")))]
compile_error!("the idle-steal task path is x86_64-only");

#[cfg(all(test, feature = "multitask"))]
mod tests;

#[cfg(any(
    all(feature = "sched-fifo", feature = "sched-rr"),
    all(feature = "sched-fifo", feature = "sched-eevdf"),
    all(feature = "sched-rr", feature = "sched-eevdf"),
))]
compile_error!("select at most one of sched-fifo, sched-rr, or sched-eevdf");

#[cfg(any(
    all(feature = "eevdf-balanced", feature = "eevdf-latency"),
    all(feature = "eevdf-balanced", feature = "eevdf-throughput"),
    all(feature = "eevdf-latency", feature = "eevdf-throughput"),
))]
compile_error!(
    "select at most one EEVDF profile: eevdf-balanced, eevdf-latency, or eevdf-throughput"
);

cfg_if::cfg_if! {
    if #[cfg(feature = "multitask")] {
        #[macro_use]
        extern crate log;
        extern crate alloc;

        #[macro_use]
        mod run_queue;
        #[cfg(feature = "scheduler-observer")]
        mod scheduler_observer;
        mod task;
        mod api;
        mod wait_queue;
        pub mod event_listener;

        #[cfg(all(feature = "fp-simd", target_arch = "x86_64"))]
        mod pkru_xsave;

        #[cfg(feature = "irq-exit")]
        mod irq_exit;
        #[cfg(feature = "irq-exit")]
        #[doc(hidden)]
        pub use self::irq_exit::IrqExitIf;

        #[cfg(feature = "irq-continuation-diagnostics")]
        #[cfg_attr(not(target_os = "none"), allow(dead_code))]
        mod irq_continuation_diagnostics;

        #[cfg(feature = "irq")]
        mod timers;

        #[cfg(feature = "multitask")]
        pub mod future;

        #[doc(cfg(feature = "multitask"))]
        pub use self::api::*;
        #[cfg(feature = "scheduler-observer")]
        #[doc(hidden)]
        pub use self::scheduler_observer::SchedulerObserver;
        pub use self::api::{sleep, sleep_until, yield_now};
        #[cfg(all(feature = "fp-simd", target_arch = "x86_64"))]
        pub use self::pkru_xsave::{
            XsaveCommit, XsaveImage, XsaveImageError, XsaveTaskError,
            prepare_current_task_xsave_commit, prepare_current_task_xsave_reset,
            snapshot_current_task_xsave,
        };
        #[cfg(feature = "irq-continuation-diagnostics")]
        pub use self::irq_continuation_diagnostics::{
            IrqContinuationDiagnosticEvent, IrqContinuationDiagnosticSnapshot,
            irq_continuation_diagnostic_event, irq_continuation_diagnostic_snapshot,
        };
    } else {
        mod api_s;
        pub use self::api_s::{can_block_current, sleep, sleep_until, yield_now};
    }
}
