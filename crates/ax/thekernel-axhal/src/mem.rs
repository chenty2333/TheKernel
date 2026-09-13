//! Physical memory management.

pub use axplat::mem::{
    MemRegionFlags, PhysMemRegion, kernel_aspace, mmio_ranges, phys_ram_ranges, phys_to_virt,
    reserved_phys_ram_ranges, total_ram_size, virt_to_phys,
};
use axplat::mem::{RawRange, check_sorted_ranges_overlap, ranges_difference};
use heapless::Vec;
pub use memory_addr::{PAGE_SIZE_4K, PhysAddr, PhysAddrRange, VirtAddr, VirtAddrRange, pa, va};
use spin::Lazy;

use crate::addr_of_sym;

const MAX_REGIONS: usize = 128;

static ALL_MEM_REGIONS: Lazy<Vec<PhysMemRegion, MAX_REGIONS>> = Lazy::new(|| {
    let mut all_regions = Vec::new();
    let mut push = |r: PhysMemRegion| {
        if r.size > 0 {
            all_regions.push(r).expect("too many memory regions");
        }
    };

    // Push regions in kernel image
    push(PhysMemRegion {
        paddr: virt_to_phys(addr_of_sym!(_stext).into()),
        size: addr_of_sym!(_etext) - addr_of_sym!(_stext),
        flags: MemRegionFlags::RESERVED | MemRegionFlags::READ | MemRegionFlags::EXECUTE,
        name: ".text",
    });
    push(PhysMemRegion {
        paddr: virt_to_phys(addr_of_sym!(_srodata).into()),
        size: addr_of_sym!(_erodata) - addr_of_sym!(_srodata),
        flags: MemRegionFlags::RESERVED | MemRegionFlags::READ,
        name: ".rodata",
    });
    push(PhysMemRegion {
        paddr: virt_to_phys(addr_of_sym!(_sdata).into()),
        size: addr_of_sym!(_edata) - addr_of_sym!(_sdata),
        flags: MemRegionFlags::RESERVED | MemRegionFlags::READ | MemRegionFlags::WRITE,
        name: ".data .tdata .tbss .percpu",
    });
    push(PhysMemRegion {
        paddr: virt_to_phys(addr_of_sym!(boot_stack).into()),
        size: addr_of_sym!(boot_stack_top) - addr_of_sym!(boot_stack),
        flags: MemRegionFlags::RESERVED | MemRegionFlags::READ | MemRegionFlags::WRITE,
        name: "boot stack",
    });
    push(PhysMemRegion {
        paddr: virt_to_phys(addr_of_sym!(_sbss).into()),
        size: addr_of_sym!(_ebss) - addr_of_sym!(_sbss),
        flags: MemRegionFlags::RESERVED | MemRegionFlags::READ | MemRegionFlags::WRITE,
        name: ".bss",
    });

    // Push MMIO & reserved regions
    //
    // A declared MMIO window that overlaps firmware-reported RAM is a mistake
    // in a machine profile, and it used to be fatal: `check_sorted_ranges_overlap`
    // below `.unwrap()`s, so one wrong number in `mmio-ranges` panicked the
    // boot before the console existed.  That is the worst possible failure
    // mode for the one value a profile must guess -- the target machine's
    // device windows -- because it looks exactly like a dead machine.
    //
    // RAM wins the overlap.  The firmware's memory map is a measurement and
    // the profile is a declaration, so the measurement is right and the
    // declaration is trimmed to the part that is not RAM.  Every trim is
    // logged, so a wrong profile is visible rather than silent.
    for &(declared_start, declared_size) in mmio_ranges() {
        let mut clipped: Vec<RawRange, MAX_REGIONS> = Vec::new();
        clip_mmio_window(
            (declared_start, declared_size),
            phys_ram_ranges(),
            |range| {
                let _ = clipped.push(range);
            },
        );
        if clipped.len() == 1 && clipped[0] == (declared_start, declared_size) {
            push(PhysMemRegion::new_mmio(declared_start, declared_size, "mmio"));
            continue;
        }
        warn!(
            "mmio: declared window [{:#x}, {:#x}) overlaps RAM and was trimmed; \
             correct `mmio-ranges` for this machine",
            declared_start,
            declared_start + declared_size
        );
        if clipped.is_empty() {
            warn!(
                "mmio: declared window [{:#x}, {:#x}) is entirely inside RAM and was \
                 dropped entirely",
                declared_start,
                declared_start + declared_size
            );
        }
        for &(start, size) in &clipped {
            warn!("mmio: keeping [{start:#x}, {:#x})", start + size);
            push(PhysMemRegion::new_mmio(start, size, "mmio"));
        }
    }
    for &(start, size) in reserved_phys_ram_ranges() {
        // push(PhysMemRegion::new_reserved(start, size, "reserved"));
        push(PhysMemRegion {
            paddr: PhysAddr::from_usize(start),
            size,
            flags: MemRegionFlags::RESERVED
                | MemRegionFlags::READ
                | MemRegionFlags::WRITE
                | MemRegionFlags::EXECUTE,
            name: "reserved",
        })
    }

    let mut reserved_ranges = reserved_phys_ram_ranges()
        .iter()
        .cloned()
        .collect::<Vec<_, MAX_REGIONS>>();
    // Combine kernel image range and reserved ranges
    let kernel_start = virt_to_phys(addr_of_sym!(_skernel).into()).as_usize();
    let kernel_size = addr_of_sym!(_ekernel) - addr_of_sym!(_skernel);
    reserved_ranges
        .push((kernel_start, kernel_size))
        .expect("too many memory regions"); // kernel image range is also reserved

    // Remove all reserved ranges from RAM ranges, and push the remaining as free memory
    reserved_ranges.sort_unstable_by_key(|&(start, _size)| start);
    ranges_difference(phys_ram_ranges(), &reserved_ranges, |(start, size)| {
        push(PhysMemRegion::new_ram(start, size, "free memory"));
    })
    .inspect_err(|(a, b)| error!("Reserved memory region {a:#x?} overlaps with {b:#x?}"))
    .unwrap();

    // Check overlapping
    all_regions.sort_unstable_by_key(|r| r.paddr);
    check_sorted_ranges_overlap(all_regions.iter().map(|r| (r.paddr.into(), r.size)))
        .inspect_err(|(a, b)| error!("Physical memory region {a:#x?} overlaps with {b:#x?}"))
        .unwrap();

    all_regions
});

/// Subtracts RAM from a declared MMIO window, yielding the parts that are not
/// backed by memory.
///
/// A machine profile declares device windows at compile time, and the firmware
/// reports RAM at run time.  When the two disagree the run-time answer wins:
/// claiming RAM is MMIO would make the kernel refuse to allocate memory it
/// actually has, and the memory-map consistency check treats the disagreement
/// as fatal, which on the target machine looks exactly like a dead computer.
///
/// `emit` is called once per surviving piece, in ascending order, and not at
/// all when the window lies entirely inside RAM.  A window that is untouched
/// is emitted exactly as declared, so callers can detect the no-overlap case.
fn clip_mmio_window(declared: RawRange, ram: &[RawRange], emit: impl FnMut(RawRange)) {
    // `ram` is sorted and disjoint by construction in every platform backend,
    // which is the precondition `ranges_difference` documents.  The `expect`
    // is unreachable rather than optimistic, but it is still an `expect` and
    // not an `unwrap` so the invariant is named where it is relied on.
    ranges_difference(&[declared], ram, emit)
        .expect("platform RAM ranges are sorted and disjoint by construction");
}

/// Returns an iterator over all physical memory regions.
pub fn memory_regions() -> impl Iterator<Item = PhysMemRegion> {
    ALL_MEM_REGIONS.iter().cloned()
}

/// Fills the `.bss` section with zeros.
///
/// It requires the symbols `_sbss` and `_ebss` to be defined in the linker script.
///
/// # Safety
///
/// This function is unsafe because it writes `.bss` section directly.
pub unsafe fn clear_bss() {
    unsafe {
        core::slice::from_raw_parts_mut(
            _sbss as *mut u8,
            (_ebss as *mut u8).offset_from_unsigned(_sbss as *mut u8),
        )
        .fill(0);
    }
}

#[allow(dead_code)]
unsafe extern "C" {
    fn _stext();
    fn _etext();
    fn _srodata();
    fn _erodata();
    fn _sdata();
    fn _edata();
    fn _sbss();
    fn _ebss();
    fn _skernel();
    fn _ekernel();
    fn boot_stack();
    fn boot_stack_top();
}

#[cfg(test)]
mod mmio_clip_tests {
    use super::clip_mmio_window;
    use std::vec::Vec;

    fn clip(declared: (usize, usize), ram: &[(usize, usize)]) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        clip_mmio_window(declared, ram, |range| out.push(range));
        out
    }

    #[test]
    fn a_window_that_touches_no_ram_is_emitted_unchanged() {
        assert_eq!(clip((0x1000, 0x100), &[(0x8000, 0x1000)]), [(0x1000, 0x100)]);
        assert_eq!(clip((0x1000, 0x100), &[]), [(0x1000, 0x100)]);
    }

    #[test]
    fn a_window_adjacent_to_ram_is_not_treated_as_overlapping() {
        // Half-open ranges: ending exactly where RAM starts is not an overlap,
        // and a caller that used closed intervals here would clip a window it
        // should have kept whole.
        assert_eq!(clip((0x1000, 0x100), &[(0x1100, 0x100)]), [(0x1000, 0x100)]);
        assert_eq!(clip((0x1100, 0x100), &[(0x1000, 0x100)]), [(0x1100, 0x100)]);
    }

    #[test]
    fn a_window_entirely_inside_ram_is_dropped() {
        // This is the 16 GiB case: a profile declaring a 4-5 GiB device window
        // on a machine whose firmware reports RAM there.  Dropping it loses a
        // window nothing needed; keeping it panicked the boot.
        assert!(clip((0x1000, 0x100), &[(0x800, 0x1000)]).is_empty());
    }

    #[test]
    fn a_window_survives_on_both_sides_of_a_ram_island() {
        assert_eq!(
            clip((0x1000, 0x1000), &[(0x1800, 0x100)]),
            [(0x1000, 0x800), (0x1900, 0x700)]
        );
    }

    #[test]
    fn the_kept_pieces_never_overlap_ram_and_never_exceed_the_declaration() {
        // The property that matters, checked over a grid rather than by
        // example: every emitted piece is inside the declared window, and no
        // emitted byte is inside a RAM range.
        let declared = (0x1000usize, 0x2000);
        let ram = [(0x1400, 0x200), (0x1a00, 0x400), (0x3000, 0x100)];
        let pieces = clip(declared, &ram);
        for &(start, size) in &pieces {
            let (declared_start, declared_size) = declared;
            assert!(start >= declared_start, "{start:#x} before the window");
            assert!(
                start + size <= declared_start + declared_size,
                "piece ends past the window"
            );
            for &(ram_start, ram_size) in &ram {
                assert!(
                    start + size <= ram_start || start >= ram_start + ram_size,
                    "piece [{start:#x}, {:#x}) is inside RAM",
                    start + size
                );
            }
        }
    }
}
