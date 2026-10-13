// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Compatibility re-exports for callers that still use the pending-stub path.

pub use crate::drm_dumb_buffers_upstream::{
    drm_mode_create_dumb_ioctl, drm_mode_destroy_dumb_ioctl, drm_mode_mmap_dumb_ioctl,
};
