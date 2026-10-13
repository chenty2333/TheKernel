// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batches whose `*_upstream.rs`
//! files have not landed. Each exported symbol is defined in exactly one
//! file here; the module header of each file names the Linux source that
//! replaces it. Delete an entry when its owner lands.

pub mod b3b;
pub mod b3c;
pub mod b3d;
pub mod b3e;
pub mod b4;
pub mod b5;
pub mod b6;
pub mod b7;
pub mod opt;
pub mod kapi_display;
