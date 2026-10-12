// SPDX-License-Identifier: MIT
//! The firmware's initial scanout, read the way Linux reads it before i915
//! takes the GGTT: `display/skl_universal_plane.c` skl_get_initial_plane_config
//! for the plane geometry and `display/intel_plane_initial.c`
//! initial_plane_phys_smem for the backing pages behind each GGTT entry.
//!
//! The i915 port owns the whole GGTT. Until the Linux display code is ported,
//! the firmware framebuffer stays the console, so its GGTT range is reserved
//! in i915's DRM-MM before `init_ggtt()` clears every other hole, which is what
//! Linux's initial-plane vma pin achieves. Everything here only reads.
use alloc::vec::Vec;
use core::ops::Range;

use super::{pci, regs::RegisterWindow};

const PAGE_SIZE: u64 = 4096;
/// Gen12 GGTT entry array: the upper 8 MiB of the 16 MiB GTTMMADR BAR
/// (`gt/intel_ggtt.c` gen8_gmch_probe: `phys_addr = pci_resource_start + SZ_8M`
/// for GRAPHICS_VER >= 9 integrated parts).
const GSM_OFFSET: u64 = 0x80_0000;
const GSM_BYTES: usize = 0x80_0000;
/// `GEN8_PAGE_PRESENT` and the 4 KiB address field of a gen8 GGTT PTE.
const PTE_PRESENT: u64 = 1;
const PTE_ADDRESS_MASK: u64 = 0x0000_7fff_ffff_f000;

/// `GEN6_DSMBASE`/`GEN11_BDSM_MASK` from `i915_gem_stolen.c`.
const DSMBASE: u32 = 0x1080c0;
const BDSM_MASK: u64 = 0xffff_ffff_fff0_0000;

/// Gen9+ universal plane 1 registers of pipe A; pipe n adds 0x1000.
const PLANE_CTL: u32 = 0x70180;
const PLANE_STRIDE: u32 = 0x70188;
const PLANE_SIZE: u32 = 0x70190;
const PLANE_SURF: u32 = 0x7019c;
const PIPE_STRIDE: u32 = 0x1000;
/// ADL-P (display 13) has four pipes.
const PIPES: u32 = 4;

const PLANE_CTL_ENABLE: u32 = 1 << 31;
/// `PLANE_CTL_TILED_MASK` (bits 12:10) values for display 9+.
const TILED_SHIFT: u32 = 10;
const TILED_LINEAR: u32 = 0;
const TILED_X: u32 = 1;
const TILED_Y: u32 = 4;
const TILED_4: u32 = 5;
const PLANE_STRIDE_MASK: u32 = 0x7ff;

/// One firmware-enabled primary plane, as Linux records it in
/// `intel_initial_plane_config`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InitialPlane {
    pub(crate) pipe: u32,
    /// GGTT range of the surface, page aligned.
    pub(crate) ggtt: Range<u64>,
    /// Physical page behind each GGTT page of `ggtt`, all in stolen memory.
    pub(crate) pages: Vec<u64>,
}

fn read32(window: &RegisterWindow, offset: u32) -> Option<u32> {
    if offset as usize + 4 > window.len() {
        return None;
    }
    // SAFETY: the window is a live uncached mapping of at least `len` bytes,
    // and `offset` is dword aligned and inside it.
    Some(unsafe { core::ptr::read_volatile((window.base() + offset as usize) as *const u32) })
}

/// Stolen memory (DSM) as `i915_gem_stolen.c` i915_gem_init_stolen finds it on
/// a Gen11+ integrated part: base from `GEN6_DSMBASE`, size from the GMS field
/// of `GMCH_CTRL` (PCI 0x50 bits 15:8, gen8_get_stolen_size).
pub(crate) fn stolen_range(
    ecam: &impl pci::ConfigSpace,
    window: &RegisterWindow,
    bdf: pci::Bdf,
) -> Option<Range<u64>> {
    let low = u64::from(read32(window, DSMBASE)?);
    let high = u64::from(read32(window, DSMBASE + 4)?);
    let base = ((high << 32) | low) & BDSM_MASK;
    let gms = (ecam.read_u32(bdf, 0x50)? >> 8) & 0xff;
    let mib = match gms {
        1..=0xef => u64::from(gms) * 32,
        0xf0..=0xfe => u64::from(gms - 0xef) * 4,
        _ => return None,
    };
    let end = base.checked_add(mib * 1024 * 1024)?;
    (base != 0).then_some(base..end)
}

/// `skl_plane_stride_mult()` and the tile height used to align the surface
/// size, for the tilings a firmware console uses. Other tilings are reported
/// and not reserved, as Linux's readout refuses formats it cannot describe.
fn stride_geometry(tiling: u32) -> Option<(u64, u64)> {
    match tiling {
        TILED_LINEAR => Some((64, 1)),
        TILED_X => Some((512, 8)),
        TILED_Y | TILED_4 => Some((128, 32)),
        _ => None,
    }
}

/// The four registers of one pipe's primary plane, as read. Compared before and
/// after the PTE walk: a plane the firmware reprogrammed meanwhile is not the
/// plane whose pages were read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PlaneRegs {
    ctl: u32,
    stride: u32,
    size: u32,
    surf: u32,
}

fn read_plane_regs(window: &RegisterWindow, pipe: u32) -> Option<PlaneRegs> {
    let base = pipe * PIPE_STRIDE;
    Some(PlaneRegs {
        ctl: read32(window, PLANE_CTL + base)?,
        stride: read32(window, PLANE_STRIDE + base)?,
        size: read32(window, PLANE_SIZE + base)?,
        surf: read32(window, PLANE_SURF + base)?,
    })
}

/// Decode one pipe's primary plane, or `None` when it is disabled or not
/// describable.
fn decode_plane(window: &RegisterWindow, pipe: u32) -> Option<(Range<u64>, u32)> {
    decode_regs(&read_plane_regs(window, pipe)?, pipe)
}

fn decode_regs(regs: &PlaneRegs, pipe: u32) -> Option<(Range<u64>, u32)> {
    if regs.ctl & PLANE_CTL_ENABLE == 0 {
        return None;
    }
    let tiling = (regs.ctl >> TILED_SHIFT) & 7;
    let Some((stride_mult, tile_height)) = stride_geometry(tiling) else {
        axlog::warn!("intel-initial-plane: pipe {pipe} tiling {tiling} not described; not reserved");
        return None;
    };
    let height = u64::from((regs.size >> 16) & 0xffff) + 1;
    let pitch = u64::from(regs.stride & PLANE_STRIDE_MASK) * stride_mult;
    let surface = u64::from(regs.surf & 0xffff_f000);
    let bytes = pitch * height.div_ceil(tile_height) * tile_height;
    if pitch == 0 || bytes == 0 {
        return None;
    }
    let end = surface.checked_add(bytes.div_ceil(PAGE_SIZE) * PAGE_SIZE)?;
    Some((surface..end, pipe))
}

/// Whether the plane read after the PTE walk is the plane read before it. An
/// unreadable register after the walk counts as a change.
fn unchanged(before: &PlaneRegs, after: Option<PlaneRegs>) -> bool {
    after.as_ref() == Some(before)
}

/// Whether `page` overlaps memory the kernel hands out as RAM. Firmware pages
/// that are also RAM would alias kernel or user memory, which the identity
/// lease must never admit.
fn overlaps_usable_ram(page: u64) -> bool {
    axhal::mem::phys_ram_ranges().iter().any(|&(base, size)| {
        page < (base as u64).saturating_add(size as u64) && page + PAGE_SIZE > base as u64
    })
}

/// Read every firmware-enabled primary plane and the stolen pages behind it.
///
/// Linux reads the plane once and trusts the result; here the pages are a DMA
/// identity lease, so the read is checked the way a racing reprogram would need
/// to be caught: the four plane registers are read before the PTE walk and again
/// after it, and a plane whose registers changed is dropped. A plane whose PTEs
/// are not present, leave stolen memory, or land on usable RAM is dropped too.
///
/// The DC-state request is not refused here. Linux sanitizes DC states at
/// display-power init (`gen9_sanitize_dc_state`), and this port does the same
/// at the first `POWER_DOMAIN_GT_IRQ` reference (`power.rs`), before any GT
/// work starts.
#[cfg(target_os = "none")]
pub(crate) fn read_initial_planes(
    window: &RegisterWindow,
    bar0: u64,
    stolen: &Range<u64>,
) -> Vec<InitialPlane> {
    let mut planes = Vec::new();
    let Ok(physical) = usize::try_from(bar0 + GSM_OFFSET) else {
        return planes;
    };
    let Ok(gsm) = axmm::iomap(axhal::mem::PhysAddr::from_usize(physical), GSM_BYTES) else {
        axlog::warn!("intel-initial-plane: GGTT entry array unmappable; no plane reserved");
        return planes;
    };
    let entries = GSM_BYTES / 8;
    let mut before = [None; PIPES as usize];
    for pipe in 0..PIPES {
        before[pipe as usize] = read_plane_regs(window, pipe);
    }
    for pipe in 0..PIPES {
        let Some(regs) = before[pipe as usize] else {
            continue;
        };
        let Some((ggtt, pipe)) = decode_regs(&regs, pipe) else {
            continue;
        };
        let first = (ggtt.start / PAGE_SIZE) as usize;
        let last = (ggtt.end / PAGE_SIZE) as usize;
        if last > entries {
            axlog::warn!("intel-initial-plane: pipe {pipe} surface {ggtt:#x?} outside GGTT");
            continue;
        }
        let mut pages = Vec::new();
        if pages.try_reserve_exact(last - first).is_err() {
            continue;
        }
        let mut legal = true;
        for index in first..last {
            // SAFETY: `gsm` maps GSM_BYTES of the entry array; index < entries.
            let pte = unsafe { core::ptr::read_volatile((gsm.as_usize() + index * 8) as *const u64) };
            let page = pte & PTE_ADDRESS_MASK;
            if pte & PTE_PRESENT == 0
                || page < stolen.start
                || page + PAGE_SIZE > stolen.end
                || overlaps_usable_ram(page)
            {
                axlog::warn!(
                    "intel-initial-plane: pipe {pipe} PTE[{index:#x}]={pte:#018x} is not a \
                     present stolen page outside RAM; not reserved"
                );
                legal = false;
                break;
            }
            pages.push(page);
        }
        if !legal {
            continue;
        }
        if !unchanged(&regs, read_plane_regs(window, pipe)) {
            axlog::warn!("intel-initial-plane: pipe {pipe} plane changed during readout; not reserved");
            continue;
        }
        axlog::info!(
            "intel-initial-plane: pipe {pipe} firmware surface GGTT {:#x}..{:#x} ({} stolen pages)",
            ggtt.start,
            ggtt.end,
            pages.len()
        );
        planes.push(InitialPlane { pipe, ggtt, pages });
    }
    planes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window_with(values: &[(u32, u32)]) -> (alloc::boxed::Box<[u32]>, RegisterWindow) {
        let mut memory = alloc::vec![0u32; 0x80000 / 4].into_boxed_slice();
        for &(offset, value) in values {
            memory[offset as usize / 4] = value;
        }
        // SAFETY: the boxed buffer outlives the window in each test.
        let window = unsafe { RegisterWindow::from_mapped(memory.as_ptr() as usize, memory.len() * 4) };
        (memory, window)
    }

    #[test]
    fn linear_4k_console_surface_is_decoded_like_skl_get_initial_plane_config() {
        // 3840x2160 XRGB8888 linear: stride 15360 bytes = 240 * 64.
        let (_memory, window) = window_with(&[
            (PLANE_CTL, PLANE_CTL_ENABLE | (4 << 24)),
            (PLANE_STRIDE, 240),
            (PLANE_SIZE, (2159 << 16) | 3839),
            (PLANE_SURF, 0),
        ]);
        let (ggtt, pipe) = decode_plane(&window, 0).unwrap();
        assert_eq!(pipe, 0);
        assert_eq!(ggtt, 0..0x1fa_4000);
        assert!(decode_plane(&window, 1).is_none(), "pipe B is disabled");
    }

    #[test]
    fn x_tiled_height_is_aligned_to_the_tile() {
        let (_memory, window) = window_with(&[
            (PLANE_CTL + PIPE_STRIDE, PLANE_CTL_ENABLE | (TILED_X << TILED_SHIFT)),
            (PLANE_STRIDE + PIPE_STRIDE, 8),
            (PLANE_SIZE + PIPE_STRIDE, (600 - 1) << 16 | (800 - 1)),
            (PLANE_SURF + PIPE_STRIDE, 0x10_0000),
        ]);
        let (ggtt, _) = decode_plane(&window, 1).unwrap();
        // 8 * 512 = 4096-byte pitch; 600 rows round up to 600 (multiple of 8).
        assert_eq!(ggtt, 0x10_0000..0x10_0000 + 4096 * 600);
    }

    #[test]
    fn a_plane_reprogrammed_during_the_pte_walk_is_not_admitted() {
        let before = PlaneRegs { ctl: PLANE_CTL_ENABLE | (4 << 24), stride: 240, size: 0, surf: 0 };
        assert!(unchanged(&before, Some(before)));
        let moved = PlaneRegs { surf: 0x10_0000, ..before };
        assert!(!unchanged(&before, Some(moved)));
        assert!(!unchanged(&before, None), "an unreadable plane after the walk is a change");
    }

    #[test]
    fn undescribed_tiling_is_not_reserved() {
        let (_memory, window) = window_with(&[(PLANE_CTL, PLANE_CTL_ENABLE | (2 << TILED_SHIFT))]);
        assert!(decode_plane(&window, 0).is_none());
    }
}
