// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Copyright © 2019 Intel Corporation.
//
//! x86 cache maintenance primitives for non-coherent DMA devices, shared from
//! the Linux 7.2.3 i915 CLFLUSH implementation.

const CLFLUSH: u32 = 1 << 19;
const CLFLUSHOPT: u32 = 1 << 23;

/// Failure while deriving the cache-line span of a virtual range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheFlushError {
    /// The range or CPU-reported cache-line geometry was not representable.
    InvalidRange,
}

/// CPU-supported cache-line invalidation instructions and line geometry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheFlushCaps {
    line_size: usize,
    clflushopt: bool,
}

impl CacheFlushCaps {
    /// Discover the local CPU cache-maintenance instruction and line size.
    ///
    /// Returns `None` instead of attempting an unsupported instruction.
    pub fn discover() -> Option<Self> {
        let max_leaf = core::arch::x86_64::__cpuid(0).eax;
        let leaf1 = core::arch::x86_64::__cpuid(1);
        let leaf7_ebx = if max_leaf >= 7 {
            core::arch::x86_64::__cpuid_count(7, 0).ebx
        } else {
            0
        };
        Self::from_cpuid(leaf1.ebx, leaf1.edx, leaf7_ebx)
    }

    fn from_cpuid(leaf1_ebx: u32, leaf1_edx: u32, leaf7_ebx: u32) -> Option<Self> {
        let clflushopt = leaf7_ebx & CLFLUSHOPT != 0;
        if leaf1_edx & CLFLUSH == 0 && !clflushopt {
            return None;
        }
        let line_size = (((leaf1_ebx >> 8) & 0xff) as usize) * 8;
        if line_size == 0 || !line_size.is_power_of_two() {
            return None;
        }
        Some(Self {
            line_size,
            clflushopt,
        })
    }

    /// Cache-line size reported by CPUID, in bytes.
    pub const fn line_size(self) -> usize {
        self.line_size
    }

    /// Order prior memory writes and subsequent device commands around cache
    /// maintenance performed by [`Self::flush_range_unfenced`].
    #[inline]
    pub fn barrier(self) {
        // SAFETY: MFENCE is available in x86-64 long mode and has no operands.
        unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
    }

    /// Write back and invalidate every cache line intersecting a virtual range.
    ///
    /// # Safety
    /// `address..address+length` must be a valid mapped virtual range for this
    /// CPU. The function rounds the endpoints to cache-line boundaries, so the
    /// containing lines must also be mapped.
    pub unsafe fn flush_range(self, address: usize, length: usize) -> Result<(), CacheFlushError> {
        self.barrier();
        // SAFETY: forwarded from this function's caller contract.
        let result = unsafe { self.flush_range_unfenced(address, length) };
        self.barrier();
        result
    }

    /// Flush one range without fences for callers batching several ranges.
    ///
    /// The caller must execute [`Self::barrier`] before and after a batch.
    ///
    /// # Safety
    /// `address..address+length` and the intersecting cache lines must be
    /// valid mapped virtual memory for this CPU.
    pub unsafe fn flush_range_unfenced(
        self,
        address: usize,
        length: usize,
    ) -> Result<(), CacheFlushError> {
        let Some((first, end)) = cache_line_window(address, length, self.line_size) else {
            return Err(CacheFlushError::InvalidRange);
        };
        if first == end {
            return Ok(());
        }
        let mut line = first;
        while line < end {
            if self.clflushopt {
                // SAFETY: the caller guarantees the rounded cache line is mapped.
                unsafe {
                    core::arch::asm!(
                        "clflushopt [{line}]",
                        line = in(reg) line,
                        options(nostack, preserves_flags)
                    )
                };
            } else {
                // SAFETY: CPUID advertised CLFLUSH and the line is mapped.
                unsafe {
                    core::arch::asm!(
                        "clflush [{line}]",
                        line = in(reg) line,
                        options(nostack, preserves_flags)
                    )
                };
            }
            line = line
                .checked_add(self.line_size)
                .ok_or(CacheFlushError::InvalidRange)?;
        }
        Ok(())
    }
}

fn cache_line_window(address: usize, length: usize, line_size: usize) -> Option<(usize, usize)> {
    if length == 0 {
        return Some((0, 0));
    }
    if !line_size.is_power_of_two() {
        return None;
    }
    let last = address.checked_add(length)?.checked_sub(1)?;
    let mask = !(line_size - 1);
    let first = address & mask;
    let end = (last & mask).checked_add(line_size)?;
    Some((first, end))
}

#[cfg(test)]
mod tests {
    use super::{CLFLUSH, CLFLUSHOPT, CacheFlushCaps, cache_line_window};

    #[test]
    fn cache_line_window_rounds_both_ends_and_rejects_wrap() {
        assert_eq!(cache_line_window(0x103f, 2, 64), Some((0x1000, 0x1080)));
        assert_eq!(cache_line_window(0x1040, 64, 64), Some((0x1040, 0x1080)));
        assert_eq!(cache_line_window(0x1000, 0, 64), Some((0, 0)));
        assert_eq!(cache_line_window(usize::MAX, 2, 64), None);
        assert_eq!(cache_line_window(0x1000, 1, 48), None);
    }

    #[test]
    fn flush_capability_is_cpuid_gated_and_reports_line_size() {
        if let Some(caps) = CacheFlushCaps::discover() {
            assert!(caps.line_size().is_power_of_two());
            assert!(caps.line_size() >= 8);
        }
    }

    #[test]
    fn flush_executes_on_a_mapped_aligned_page_when_supported() {
        let Some(caps) = CacheFlushCaps::discover() else {
            return;
        };
        let layout = std::alloc::Layout::from_size_align(4096, 4096).unwrap();
        // SAFETY: the allocation uses a valid layout and is released below.
        let page = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!page.is_null());
        // SAFETY: `page` is a live, uniquely owned 4 KiB allocation.
        unsafe { page.add(128).write_volatile(0x5a) };
        // SAFETY: the full aligned allocation is mapped and readable/writable.
        unsafe { caps.flush_range(page as usize, 4096) }.unwrap();
        // SAFETY: byte 128 remains inside the live allocation.
        assert_eq!(unsafe { page.add(128).read_volatile() }, 0x5a);
        // SAFETY: `page` was allocated with this exact layout above.
        unsafe { std::alloc::dealloc(page, layout) };
    }

    #[test]
    fn invalid_cpuid_cache_line_geometry_is_not_guessed() {
        assert!(CacheFlushCaps::from_cpuid(0, CLFLUSH, 0).is_none());
        assert!(CacheFlushCaps::from_cpuid(6 << 8, CLFLUSH, 0).is_none());
        assert!(CacheFlushCaps::from_cpuid(8 << 8, CLFLUSH, 0).is_some());
        assert!(CacheFlushCaps::from_cpuid(8 << 8, 0, CLFLUSHOPT).is_some());
        assert!(CacheFlushCaps::from_cpuid(8 << 8, 0, 0).is_none());
    }
}
