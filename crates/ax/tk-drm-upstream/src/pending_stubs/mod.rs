// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batches whose `*_upstream.rs`
//! files have not landed. Each exported symbol is defined in exactly one
//! file here; the module header of each file names the Linux source that
//! replaces it. Delete an entry when its owner lands.

pub mod atomic_b2a;
pub mod bridge;
pub mod debugfs;
pub mod dma_buf;
pub mod dp_b2d;
pub mod drm_client_probe;
pub mod dumb_buffers;
pub mod fbdev_cmdline;
pub mod hrtimer;
#[cfg(test)]
pub mod host_test_links;
pub mod kthread;
pub mod panel_quirks;
pub mod privacy_screen;
pub mod syncobj;
pub mod test_records;

#[cfg(test)]
pub use test_records::{pending_stubs_delivered_events, pending_stubs_queued_works, pending_stubs_reset_records};
#[cfg(test)]
pub use host_test_links::{
    dma_fence_get_count, dma_fence_init_count, dma_fence_put_count, reset_dma_fence_counts,
};
