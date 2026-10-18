// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Temporary allocation entry point for Linux `drivers/dma-buf/dma-fence-chain.c`.

#![allow(dead_code, unsafe_code)]

use tk_linux_kapi::config::GFP_KERNEL;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_chain_alloc() -> *mut tk_linux_kapi::types::DmaFenceChain {
    tk_linux_kapi::memory::kzalloc(
        core::mem::size_of::<tk_linux_kapi::types::DmaFenceChain>(),
        GFP_KERNEL,
    )
    .cast()
}
