// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/intel_ring.c
// Copyright © 2019 Intel Corporation. Full MIT grant: LICENSE-MIT.
//! Gen12 ring lifecycle and space management translated from `intel_ring.c`.
//! `RingBackend` is the narrow GEM/VMA/timeline boundary; engine submission
//! owns when these operations run and supplies the real pinned VMA semantics.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

pub const PAGE_SIZE: usize = 4096;
pub const CACHELINE_BYTES: usize = 64;
pub const MI_NOOP: u32 = 0;
pub const RING_NR_PAGES: usize = 0x001f_f000;
pub const PIN_OFFSET_BIAS: u32 = 1 << 0;
pub const PIN_MAPPABLE: u32 = 1 << 1;
pub const PIN_HIGH: u32 = 1 << 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RingError {
    NoMemory,
    Interrupted,
    NoSpace,
    Invalid,
    Backend(i32),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VmaFlags {
    pub offset_bias: bool,
    pub bias: u32,
    pub mappable: bool,
    pub high: bool,
}

/// The backing object/VMA facts that affect upstream pin/map choice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RingVma {
    pub id: u64,
    pub size: usize,
    pub stolen: bool,
    pub map_and_fenceable: bool,
    pub has_llc: bool,
    pub has_aperture: bool,
    pub has_read_only: bool,
    pub i830_or_i845g: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineRequest {
    pub ring_id: u64,
    pub postfix: usize,
    pub retire_cookie: u64,
}

#[derive(Default)]
pub struct Timeline {
    pub requests: Vec<TimelineRequest>,
}

pub struct Ring {
    pub id: u64,
    pub vma: RingVma,
    pub head: usize,
    pub tail: usize,
    pub emit: usize,
    pub size: usize,
    pub effective_size: usize,
    pub space: usize,
    pub wrap: u32,
    pub pin_count: AtomicUsize,
}

pub struct Request {
    pub ring_id: u64,
    pub reserved_space: usize,
}

/// Safe Rust equivalent of the pointer returned by `intel_ring_begin()`.
/// The adapter writes commands to the mapped GEM object using this span.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingSpan {
    pub byte_offset: usize,
    pub dwords: usize,
}

/// Adapter for i915 GEM, GGTT, timeline, and request operations.
pub trait RingBackend {
    fn ggtt_pin_bias(&mut self, vma: &RingVma) -> u32;
    fn ggtt_pin(&mut self, vma: &mut RingVma, flags: VmaFlags) -> Result<(), RingError>;
    fn map_iomap(&mut self, vma: &RingVma) -> Result<(), RingError>;
    fn coherent_map(&mut self, vma: &RingVma) -> Result<(), RingError>;
    fn fill_ring_words(
        &mut self,
        vma: &RingVma,
        byte_offset: usize,
        dwords: usize,
        value: u32,
    ) -> Result<(), RingError>;
    fn write_ring_words(
        &mut self,
        vma: &RingVma,
        byte_offset: usize,
        words: &[u32],
    ) -> Result<(), RingError>;
    fn unmap_iomap(&mut self, vma: &RingVma);
    fn unmap_object(&mut self, vma: &RingVma);
    fn make_unshrinkable(&mut self, vma: &RingVma);
    fn make_purgeable(&mut self, vma: &RingVma);
    fn unset_ggtt_write(&mut self, vma: &RingVma);
    fn vma_unpin(&mut self, vma: &mut RingVma);
    fn ggtt_has_aperture_without_llc(&self) -> bool;

    fn create_lmem(&mut self, size: usize) -> Result<RingVma, RingError>;
    fn create_stolen(&mut self, size: usize) -> Result<RingVma, RingError>;
    fn create_internal(&mut self, size: usize) -> Result<RingVma, RingError>;
    fn set_readonly(&mut self, vma: &mut RingVma);
    fn vma_instance(&mut self, vma: RingVma) -> Result<RingVma, RingError>;
    fn vma_put(&mut self, vma: RingVma);

    fn request_wait_interruptible_max(&mut self, target: TimelineRequest) -> Result<(), RingError>;
    fn request_retire_upto(&mut self, target: TimelineRequest);
}

/// `__intel_ring_space()` — the head/tail cacheline guard is not optional.
fn ring_space(head: usize, tail: usize, size: usize) -> usize {
    head.wrapping_sub(tail).wrapping_sub(CACHELINE_BYTES) & (size - 1)
}

// upstream: intel_ring.c intel_ring_update_space()
pub fn intel_ring_update_space(ring: &mut Ring) -> usize {
    let space = ring_space(ring.head, ring.emit, ring.size);
    ring.space = space;
    space
}

// upstream: intel_ring.c __intel_ring_pin()
pub fn __intel_ring_pin(ring: &Ring) {
    assert!(ring.pin_count.load(Ordering::Relaxed) > 0);
    ring.pin_count.fetch_add(1, Ordering::Relaxed);
}

// upstream: intel_ring.c intel_ring_pin()
pub fn intel_ring_pin(ring: &mut Ring, backend: &mut impl RingBackend) -> Result<(), RingError> {
    if ring.pin_count.fetch_add(1, Ordering::AcqRel) != 0 {
        return Ok(());
    }

    // Ring wraparound at offset 0 sometimes hangs. No idea why.
    let mut flags = VmaFlags {
        offset_bias: true,
        bias: backend.ggtt_pin_bias(&ring.vma),
        ..VmaFlags::default()
    };
    if ring.vma.stolen {
        flags.mappable = true;
    } else {
        flags.high = true;
    }

    if let Err(error) = backend.ggtt_pin(&mut ring.vma, flags) {
        ring.pin_count.fetch_sub(1, Ordering::AcqRel);
        return Err(error);
    }

    let mapping = if ring.vma.map_and_fenceable && !ring.vma.has_llc {
        backend.map_iomap(&ring.vma)
    } else {
        // intel_gt_coherent_map_type() is resolved by the GGTT adapter.
        backend.coherent_map(&ring.vma)
    };
    match mapping {
        Ok(()) => {
            backend.make_unshrinkable(&ring.vma);
            // Discard unused bytes beyond that submitted to hw.
            intel_ring_reset(ring, ring.emit);
            Ok(())
        }
        Err(error) => {
            backend.vma_unpin(&mut ring.vma);
            ring.pin_count.fetch_sub(1, Ordering::AcqRel);
            Err(error)
        }
    }
}

// upstream: intel_ring.c intel_ring_reset()
pub fn intel_ring_reset(ring: &mut Ring, tail: usize) {
    let tail = tail & (ring.size - 1);
    ring.tail = tail;
    ring.head = tail;
    ring.emit = tail;
    intel_ring_update_space(ring);
}

// upstream: intel_ring.c intel_ring_unpin()
pub fn intel_ring_unpin(ring: &mut Ring, backend: &mut impl RingBackend) {
    let old = ring.pin_count.fetch_sub(1, Ordering::AcqRel);
    assert!(old > 0);
    if old != 1 {
        return;
    }

    backend.unset_ggtt_write(&ring.vma);
    if ring.vma.map_and_fenceable && !ring.vma.has_llc {
        backend.unmap_iomap(&ring.vma);
    } else {
        backend.unmap_object(&ring.vma);
    }
    backend.make_purgeable(&ring.vma);
    backend.vma_unpin(&mut ring.vma);
}

// upstream: intel_ring.c create_ring_vma()
fn create_ring_vma(
    backend: &mut impl RingBackend,
    size: usize,
    mut vma: RingVma,
) -> Result<RingVma, RingError> {
    vma.size = size;
    // Mark ring buffers read-only from GPU if supported by GGTT.
    if vma.has_read_only {
        backend.set_readonly(&mut vma);
    }
    backend.vma_instance(vma)
}

// upstream: intel_ring.c intel_engine_create_ring()
pub fn intel_engine_create_ring(
    backend: &mut impl RingBackend,
    engine_id: u64,
    size: usize,
    platform_i830_or_i845g: bool,
) -> Result<Ring, RingError> {
    if !size.is_power_of_two()
        || size < PAGE_SIZE
        || size
            .checked_sub(PAGE_SIZE)
            .is_none_or(|ctl| ctl & !RING_NR_PAGES != 0)
    {
        return Err(RingError::Invalid);
    }

    let mut ring_vma = match backend.create_lmem(size) {
        Ok(vma) => vma,
        Err(_) if backend.ggtt_has_aperture_without_llc() => backend
            .create_stolen(size)
            .or_else(|_| backend.create_internal(size))?,
        Err(_) => backend.create_internal(size)?,
    };
    ring_vma.i830_or_i845g = platform_i830_or_i845g;
    let ring_vma = create_ring_vma(backend, size, ring_vma)?;
    let mut ring = Ring {
        id: engine_id,
        vma: ring_vma,
        head: 0,
        tail: 0,
        emit: 0,
        size,
        effective_size: size,
        space: 0,
        wrap: 32 - size.trailing_zeros(),
        pin_count: AtomicUsize::new(0),
    };

    // Workaround an erratum on i830/i845G: tail in the final two cachelines hangs.
    if ring.vma.i830_or_i845g {
        ring.effective_size -= 2 * CACHELINE_BYTES;
    }
    intel_ring_update_space(&mut ring);
    Ok(ring)
}

// upstream: intel_ring.c intel_ring_free()
pub fn intel_ring_free(ring: Ring, backend: &mut impl RingBackend) {
    assert_eq!(ring.pin_count.load(Ordering::Acquire), 0);
    backend.vma_put(ring.vma);
}

// upstream: intel_ring.c wait_for_space()
fn wait_for_space(
    ring: &mut Ring,
    timeline: &Timeline,
    bytes: usize,
    backend: &mut impl RingBackend,
) -> Result<(), RingError> {
    if intel_ring_update_space(ring) >= bytes {
        return Ok(());
    }

    let target = timeline
        .requests
        .iter()
        .copied()
        .find(|request| {
            request.ring_id == ring.id && bytes <= ring_space(request.postfix, ring.emit, ring.size)
        })
        .ok_or(RingError::NoSpace)?;
    backend.request_wait_interruptible_max(target)?;
    backend.request_retire_upto(target);
    intel_ring_update_space(ring);
    if ring.space < bytes {
        return Err(RingError::NoSpace);
    }
    Ok(())
}

// upstream: intel_ring.c intel_ring_begin()
pub fn intel_ring_begin(
    ring: &mut Ring,
    request: &Request,
    timeline: &Timeline,
    num_dwords: usize,
    backend: &mut impl RingBackend,
) -> Result<RingSpan, RingError> {
    if ring.pin_count.load(Ordering::Acquire) == 0 {
        return Err(RingError::Invalid);
    }
    if request.ring_id != ring.id || num_dwords & 1 != 0 {
        return Err(RingError::Invalid);
    }
    let remain_usable = ring
        .effective_size
        .checked_sub(ring.emit)
        .ok_or(RingError::Invalid)?;
    let bytes = num_dwords
        .checked_mul(core::mem::size_of::<u32>())
        .ok_or(RingError::Invalid)?;
    let mut need_wrap = 0usize;
    let mut total_bytes = bytes
        .checked_add(request.reserved_space)
        .ok_or(RingError::Invalid)?;
    if total_bytes > ring.effective_size {
        return Err(RingError::Invalid);
    }

    if total_bytes > remain_usable {
        let remain_actual = ring.size - ring.emit;
        if bytes > remain_usable {
            total_bytes = total_bytes
                .checked_add(remain_actual)
                .ok_or(RingError::Invalid)?;
            need_wrap = remain_actual | 1;
        } else {
            total_bytes = request
                .reserved_space
                .checked_add(remain_actual)
                .ok_or(RingError::Invalid)?;
        }
    }

    if total_bytes > ring.space {
        if request.reserved_space == 0 {
            return Err(RingError::NoSpace);
        }
        wait_for_space(ring, timeline, total_bytes, backend)?;
    }

    if need_wrap != 0 {
        need_wrap &= !1;
        if need_wrap > ring.space || ring.emit + need_wrap > ring.size {
            return Err(RingError::Invalid);
        }
        // Fill the ring tail with MI_NOOP qwords before wrapping to offset 0.
        backend.fill_ring_words(&ring.vma, ring.emit, need_wrap / 4, MI_NOOP)?;
        ring.space -= need_wrap;
        ring.emit = 0;
    }

    if ring.emit > ring.size - bytes || ring.space < bytes {
        return Err(RingError::Invalid);
    }
    let start = ring.emit;
    ring.emit += bytes;
    ring.space -= bytes;
    Ok(RingSpan {
        byte_offset: start,
        dwords: num_dwords,
    })
}

/// Commit the command payload into the mapped ring reservation returned by
/// `intel_ring_begin()`. The C API returns `u32 *`; the Rust adapter keeps the
/// same byte offset/length but performs the write through the GEM mapping.
pub fn intel_ring_emit(
    ring: &Ring,
    span: RingSpan,
    words: &[u32],
    backend: &mut impl RingBackend,
) -> Result<(), RingError> {
    let bytes = span.dwords.checked_mul(4).ok_or(RingError::Invalid)?;
    let end = span
        .byte_offset
        .checked_add(bytes)
        .ok_or(RingError::Invalid)?;
    if ring.pin_count.load(Ordering::Acquire) == 0 || words.len() != span.dwords || end > ring.size
    {
        return Err(RingError::Invalid);
    }
    backend.write_ring_words(&ring.vma, span.byte_offset, words)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    struct Backend {
        words: Vec<u32>,
    }
    impl RingBackend for Backend {
        fn ggtt_pin_bias(&mut self, _: &RingVma) -> u32 {
            0
        }
        fn ggtt_pin(&mut self, _: &mut RingVma, _: VmaFlags) -> Result<(), RingError> {
            Ok(())
        }
        fn map_iomap(&mut self, _: &RingVma) -> Result<(), RingError> {
            Ok(())
        }
        fn coherent_map(&mut self, _: &RingVma) -> Result<(), RingError> {
            Ok(())
        }
        fn fill_ring_words(
            &mut self,
            _: &RingVma,
            byte_offset: usize,
            dwords: usize,
            value: u32,
        ) -> Result<(), RingError> {
            let start = byte_offset / 4;
            self.words
                .get_mut(start..start + dwords)
                .ok_or(RingError::Invalid)?
                .fill(value);
            Ok(())
        }
        fn write_ring_words(
            &mut self,
            _: &RingVma,
            byte_offset: usize,
            words: &[u32],
        ) -> Result<(), RingError> {
            let start = byte_offset / 4;
            self.words
                .get_mut(start..start + words.len())
                .ok_or(RingError::Invalid)?
                .copy_from_slice(words);
            Ok(())
        }
        fn unmap_iomap(&mut self, _: &RingVma) {}
        fn unmap_object(&mut self, _: &RingVma) {}
        fn make_unshrinkable(&mut self, _: &RingVma) {}
        fn make_purgeable(&mut self, _: &RingVma) {}
        fn unset_ggtt_write(&mut self, _: &RingVma) {}
        fn vma_unpin(&mut self, _: &mut RingVma) {}
        fn ggtt_has_aperture_without_llc(&self) -> bool {
            false
        }
        fn create_lmem(&mut self, _: usize) -> Result<RingVma, RingError> {
            Err(RingError::NoMemory)
        }
        fn create_stolen(&mut self, _: usize) -> Result<RingVma, RingError> {
            Err(RingError::NoMemory)
        }
        fn create_internal(&mut self, _: usize) -> Result<RingVma, RingError> {
            Err(RingError::NoMemory)
        }
        fn set_readonly(&mut self, _: &mut RingVma) {}
        fn vma_instance(&mut self, vma: RingVma) -> Result<RingVma, RingError> {
            Ok(vma)
        }
        fn vma_put(&mut self, _: RingVma) {}
        fn request_wait_interruptible_max(&mut self, _: TimelineRequest) -> Result<(), RingError> {
            Ok(())
        }
        fn request_retire_upto(&mut self, _: TimelineRequest) {}
    }

    #[test]
    fn ring_space_reserves_cacheline_and_wraps() {
        assert_eq!(ring_space(0, 0, 4096), 4096 - CACHELINE_BYTES);
        assert_eq!(ring_space(64, 4096 - 32, 4096), 32);
    }

    #[test]
    fn reset_wraps_and_recomputes_space() {
        let ring = Ring {
            id: 1,
            vma: RingVma {
                id: 1,
                size: 4096,
                stolen: false,
                map_and_fenceable: false,
                has_llc: true,
                has_aperture: false,
                has_read_only: false,
                i830_or_i845g: false,
            },
            head: 0,
            tail: 0,
            emit: 0,
            size: 4096,
            effective_size: 4096,
            space: 0,
            wrap: 20,
            pin_count: AtomicUsize::new(0),
        };
        let mut ring = ring;
        intel_ring_reset(&mut ring, 4097);
        assert_eq!((ring.head, ring.tail, ring.emit), (1, 1, 1));
        assert_eq!(ring.space, ring_space(1, 1, 4096));
    }

    #[test]
    fn begin_and_emit_write_through_the_backend_mapping() {
        let mut ring = Ring {
            id: 9,
            vma: RingVma {
                id: 9,
                size: 4096,
                stolen: false,
                map_and_fenceable: false,
                has_llc: true,
                has_aperture: false,
                has_read_only: false,
                i830_or_i845g: false,
            },
            head: 0,
            tail: 0,
            emit: 0,
            size: 4096,
            effective_size: 4096,
            space: 4096 - CACHELINE_BYTES,
            wrap: 20,
            pin_count: AtomicUsize::new(1),
        };
        let request = Request {
            ring_id: 9,
            reserved_space: 0,
        };
        let mut backend = Backend {
            words: vec![0; 1024],
        };
        let span =
            intel_ring_begin(&mut ring, &request, &Timeline::default(), 2, &mut backend).unwrap();
        assert_eq!(
            span,
            RingSpan {
                byte_offset: 0,
                dwords: 2
            }
        );
        intel_ring_emit(&ring, span, &[0xdead_beef, 0x1234_5678], &mut backend).unwrap();
        assert_eq!(&backend.words[..2], &[0xdead_beef, 0x1234_5678]);
    }
}
