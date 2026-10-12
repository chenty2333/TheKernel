// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Kernel owner of the Linux MM shim's `current->mm`.
//!
//! `tk-intel-gt` resolves the calling task's user memory through the
//! `UserAddressSpace` trait. This module implements that trait over the
//! process's own `AddrSpace` (the one `proc_data.aspace()` returns), so the
//! shim's usercopy, mappings, pins and interval notifiers act on the same
//! address space that the core mm invalidates.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::ffi::c_int;
use core::sync::atomic::{AtomicBool, Ordering};

use axerrno::AxError;
use axhal::paging::MappingFlags;
use axsync::Mutex;
use tk_linux_kapi::mm_native::{
    CurrentUserAddressSpace, PinnedFrames, UserAddressSpace, install_current_user_address_space,
};
use memory_addr::{PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};

use crate::{
    mm::{
        AddrSpace, UserIoPinSegment, UserMemoryCapability,
        pin_user_segments_from_user_longterm_with, pin_user_segments_to_user_longterm_with,
    },
    task::AsThread,
};

const EFAULT: c_int = 14;
const ENOMEM: c_int = 12;

/// A process address space held weakly, so the Linux MM shim never keeps
/// process memory alive. `AddrSpace::drop` runs `mmu_notifier::release`, which
/// invalidates every interval subscriber before the mappings go away.
struct ProcessAddressSpace {
    id: u64,
    aspace: alloc::sync::Weak<Mutex<AddrSpace>>,
}

/// Holds the core long-term pin of one `pin_user_pages_fast` call. The pin owner
/// contains raw user pointers used only for accounting. The frames are
/// reached through the captured physical segments.
struct HeldUserPin(#[allow(dead_code)] Box<dyn core::any::Any>);

// SAFETY: the held owner is never dereferenced; it only keeps the core frame
// pins alive and is dropped on the thread that unpins.
unsafe impl Send for HeldUserPin {}

impl ProcessAddressSpace {
    fn with<R>(&self, operation: impl FnOnce(&mut AddrSpace) -> R) -> Option<R> {
        let aspace = self.aspace.upgrade()?;
        let mut guard = aspace.lock();
        Some(operation(&mut guard))
    }
}

impl UserAddressSpace for ProcessAddressSpace {
    fn id(&self) -> u64 {
        self.id
    }

    fn alive(&self) -> bool {
        self.aspace.strong_count() > 0
    }

    fn bounds(&self) -> (usize, usize) {
        self.with(|aspace| (aspace.base().as_usize(), aspace.end().as_usize()))
            .unwrap_or((0, 0))
    }

    fn contains_range(&self, addr: usize, size: usize) -> bool {
        self.with(|aspace| aspace.contains_range(VirtAddr::from(addr), size))
            .unwrap_or(false)
    }

    fn find_free_area(&self, hint: usize, size: usize) -> Option<usize> {
        self.with(|aspace| {
            let hint = if hint == 0 {
                aspace.base()
            } else {
                VirtAddr::from(hint)
            };
            let limit = VirtAddrRange::new(aspace.base(), aspace.end());
            aspace
                .find_free_area(hint, size, limit, PAGE_SIZE_4K)
                .map(|start| start.as_usize())
        })
        .flatten()
    }

    fn query(&self, addr: usize) -> Option<(usize, MappingFlags)> {
        self.with(|aspace| {
            aspace
                .user_leaf(VirtAddr::from(addr))
                .map(|(paddr, flags)| (paddr.as_usize(), flags))
        })
        .flatten()
    }

    fn read(&self, addr: usize, dst: &mut [u8]) -> bool {
        self.with(|aspace| aspace.read(VirtAddr::from(addr), dst).is_ok())
            .unwrap_or(false)
    }

    fn write(&self, addr: usize, src: &[u8]) -> bool {
        if src.is_empty() {
            return true;
        }
        self.with(|aspace| aspace.write(VirtAddr::from(addr), src).is_ok())
            .unwrap_or(false)
    }

    fn map_linear(&self, addr: usize, paddr: usize, size: usize, flags: MappingFlags) -> bool {
        self.with(|aspace| {
            aspace
                .map_linear(VirtAddr::from(addr), PhysAddr::from(paddr), size, flags)
                .is_ok()
        })
        .unwrap_or(false)
    }

    fn unmap(&self, addr: usize, size: usize) -> bool {
        self.with(|aspace| match aspace.unmap(VirtAddr::from(addr), size) {
            Ok(wake) => {
                wake.finish();
                true
            }
            Err(_) => false,
        })
        .unwrap_or(false)
    }

    fn pin(&self, addr: usize, size: usize, write: bool) -> Result<PinnedFrames, c_int> {
        let aspace = self.aspace.upgrade().ok_or(-EFAULT)?;
        let capability = UserMemoryCapability::new(aspace);
        let (segments, hold): (Vec<UserIoPinSegment>, Box<dyn core::any::Any>) = if write {
            let pin = pin_user_segments_to_user_longterm_with(&capability, addr as *mut u8, size)
                .map_err(pin_error)?;
            (pin.segments().to_vec(), Box::new(pin))
        } else {
            let pin = pin_user_segments_from_user_longterm_with(&capability, addr as *const u8, size)
                .map_err(pin_error)?;
            (pin.segments().to_vec(), Box::new(pin))
        };
        let frames = frames_of_segments(&segments, size).ok_or(-EFAULT)?;
        Ok(PinnedFrames {
            frames,
            hold: Box::new(HeldUserPin(hold)),
        })
    }
}

fn pin_error(err: AxError) -> c_int {
    match err {
        AxError::NoMemory => -ENOMEM,
        _ => -EFAULT,
    }
}

/// Physical 4 KiB frame of every page in `[start, start + size)`. The
/// segments cover the pinned byte range in user order, each one physically
/// contiguous, so the frame of page `k` is found at user offset `k * 4096`.
fn frames_of_segments(segments: &[UserIoPinSegment], size: usize) -> Option<Vec<usize>> {
    let pages = size / PAGE_SIZE_4K;
    let mut frames = Vec::new();
    frames.try_reserve_exact(pages).ok()?;
    let mut segment_index = 0usize;
    let mut segment_start = 0usize;
    for page in 0..pages {
        let offset = page * PAGE_SIZE_4K;
        loop {
            let segment = segments.get(segment_index)?;
            if offset < segment_start + segment.len {
                frames.push(segment.paddr + (offset - segment_start));
                break;
            }
            segment_start += segment.len;
            segment_index += 1;
        }
    }
    Some(frames)
}

/// The calling task's process address space, or `None` for kernel threads.
fn current_process_address_space() -> Option<Arc<dyn UserAddressSpace>> {
    let current = axtask::current_may_uninit()?;
    let thread = current.try_as_thread()?;
    let aspace = thread.proc_data.aspace();
    let id = aspace.lock().address_space_id().get();
    Some(Arc::new(ProcessAddressSpace {
        id,
        aspace: Arc::downgrade(&aspace),
    }))
}

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Installs the `current->mm` provider for the Linux MM shim. Idempotent.
pub(super) fn install() -> Result<(), &'static str> {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    let provider: CurrentUserAddressSpace = current_process_address_space;
    install_current_user_address_space(provider)
}
