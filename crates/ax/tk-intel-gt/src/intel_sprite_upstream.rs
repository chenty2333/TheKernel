// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! ADL-P build selection for Linux v7.2.3 `intel_sprite.c`.
//!
//! This file's legacy generation callbacks and `intel_sprite_plane_create()`
//! are not reachable for display version 13: Linux `intel_crtc_init()` takes
//! `skl_universal_plane_create()` for both primary and overlay planes when
//! `DISPLAY_VER >= 9`. The active ADL-P overlay path is therefore implemented
//! by B4a's `skl_universal_plane_upstream.rs`; the still-registered sprite
//! color-key UAPI is translated in `intel_sprite_uapi_upstream.rs`.
#![allow(dead_code, non_snake_case, unsafe_code)]
