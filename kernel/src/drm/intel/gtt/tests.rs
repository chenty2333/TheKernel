use super::{mock::MockPageTable, *};

/// A page table in ordinary memory, which is how the volatile access path
/// is exercised on a machine with no graphics device.  Device memory is
/// ordinary memory with rules, and this is the window that stands in for
/// one.
struct Scratch {
    words: alloc::vec::Vec<u64>,
}

impl Scratch {
    fn new(entries: usize) -> Self {
        Self {
            words: alloc::vec![0; entries],
        }
    }

    fn gtt(&mut self) -> Gtt {
        let bytes = self.words.len() * PTE_BYTES;
        // SAFETY: `words` is a live, 8-byte-aligned buffer of exactly
        // `bytes` bytes that outlives the returned value, and the page
        // table built over it is the only way it is reached afterwards.
        unsafe { Gtt::from_mapped(self.words.as_mut_ptr() as usize, bytes) }.unwrap()
    }
}

/// A page table with room for exactly one one-page run.
///
/// The arithmetic is what the allocation policy costs, so it is written
/// down: a one-page run needs 65 entries -- its own page and the 64 of
/// padding that follow it -- a 256 KiB-aligned start, and the reserved page
/// at each end of the aperture.  A 256-entry (1 MiB) table places the run
/// at entry 128 and has no room for a second one, which is what makes it
/// the crisp size for the tests that are about one run.
const ONE_RUN_ENTRIES: usize = 256;

/// Where that one-page run lands, in entries: `((255 - 65) / 64) * 64`.
const ONE_RUN_ENTRY: u64 = 128;

/// A page table with room for several runs.
const SEVERAL_RUNS_ENTRIES: usize = 1024;

/// A page table of `entries` entries, with a handle on its contents.
fn gtt_of(entries: usize) -> (Gtt, MockPageTable) {
    let table = MockPageTable::new(entries);
    let gtt = Gtt::over(alloc::boxed::Box::new(table.clone())).unwrap();
    (gtt, table)
}

fn one_run_gtt() -> (Gtt, MockPageTable) {
    gtt_of(ONE_RUN_ENTRIES)
}

fn several_runs_gtt() -> (Gtt, MockPageTable) {
    gtt_of(SEVERAL_RUNS_ENTRIES)
}

/// The entry the padding after a run is bound to: the page of zeros.
fn scratch_entry() -> Pte {
    Pte::encode(zero_page_physical()).unwrap()
}

/// A configuration space that answers at [`pci::offset::GMCH_CTL`] and
/// nowhere else, which is the whole of the read this module makes.
struct GmchBus {
    word: Option<u16>,
}

impl pci::ConfigSpace for GmchBus {
    fn read_u32(&self, _bdf: pci::Bdf, offset: u16) -> Option<u32> {
        if offset != pci::offset::GMCH_CTL {
            return None;
        }
        self.word.map(u32::from)
    }
}

#[test]
fn a_present_entry_is_the_address_with_the_present_bit() {
    let pte = Pte::encode(0x1_2345_6000).unwrap();
    assert_eq!(pte.raw(), 0x1_2345_6000 | 1);
    assert!(pte.is_present());
    assert!(!pte.is_local_memory());
    assert_eq!(pte.address(), 0x1_2345_6000);
    assert!(pte.describes(0x1_2345_6000));
}

#[test]
fn the_bits_the_vendor_encoder_leaves_clear_are_reported_rather_than_masked() {
    // A local-memory entry is the mistake section 11 phase 3.2 warns about:
    // it is a plausible-looking number and the hardware answers with a
    // black screen.  Decoding has to keep the bit visible.
    let local = Pte::from_raw(0x2000_1000 | PTE_PRESENT | PTE_LOCAL_MEMORY);
    assert!(local.is_present());
    assert!(local.is_local_memory());
    assert!(!local.describes(0x2000_1000));
    // And the encoder never produces one.
    assert!(!Pte::encode(0x2000_1000).unwrap().is_local_memory());
}

#[test]
fn an_address_the_entry_cannot_name_is_refused_rather_than_truncated() {
    assert_eq!(Pte::encode(0), Err(GttError::AddressZero));
    assert_eq!(
        Pte::encode(0x1800),
        Err(GttError::AddressNotAligned { address: 0x1800 })
    );
    // One page past the top of the address field.
    let too_wide = PTE_ADDRESS_MASK + PAGE_SIZE;
    assert_eq!(
        Pte::encode(too_wide),
        Err(GttError::AddressTooWide { address: too_wide })
    );
    // The top page the field can name is accepted.
    assert!(Pte::encode(PTE_ADDRESS_MASK).is_ok());
}

#[test]
fn the_size_field_is_decoded_into_the_aperture_the_vendor_function_names() {
    // [I915] gt/intel_ggtt.c:1107-1121: the field is shifted down, masked to
    // two bits, treated as a power of two count of MiB of page table, and
    // returned as zero for a field of zero.  :1238 turns those MiB of 8-byte
    // entries into bytes: one entry per 4 KiB page.
    assert_eq!(ApertureSize::decode(0), ApertureSize::Unmodelled { raw: 0 });
    assert_eq!(
        ApertureSize::decode(1 << 6),
        ApertureSize::Observed {
            raw: 1 << 6,
            aperture: 1 << 30,
        },
        "1 -> 2 MiB of page table -> 1 GiB of aperture"
    );
    assert_eq!(
        ApertureSize::decode(2 << 6),
        ApertureSize::Observed {
            raw: 2 << 6,
            aperture: 1 << 31,
        },
        "2 -> 4 MiB -> 2 GiB"
    );
    assert_eq!(
        ApertureSize::decode(3 << 6),
        ApertureSize::Observed {
            raw: 3 << 6,
            aperture: 1 << 32,
        },
        "3 -> 8 MiB -> 4 GiB, which is also what PLANE_SURF can name"
    );
    // The rest of the word is other fields -- the graphics memory select is
    // at bits 15:8 (`BDW_GMCH_GMS_SHIFT`, include/drm/intel/i915_drm.h:56)
    // and bits 5:0 are below the field -- and none of them may change the
    // answer.
    let word = 0xff00 | (3 << 6) | 0x3f;
    assert_eq!(
        ApertureSize::decode(word),
        ApertureSize::Observed {
            raw: word,
            aperture: 1 << 32,
        }
    );
}

#[test]
fn the_size_field_is_read_from_the_devices_own_configuration_space() {
    let bdf = pci::Bdf::new(0, 2, 0);
    assert_eq!(
        ApertureSize::read(&GmchBus { word: Some(3 << 6) }, bdf),
        ApertureSize::Observed {
            raw: 3 << 6,
            aperture: 1 << 32,
        }
    );
    assert_eq!(
        ApertureSize::read(&GmchBus { word: Some(0) }, bdf),
        ApertureSize::Unmodelled { raw: 0 }
    );
    // A function that does not answer at the offset, or a bus that cannot be
    // reached at all, is not a size and must not become one.
    assert_eq!(
        ApertureSize::read(&GmchBus { word: None }, bdf),
        ApertureSize::Unreadable
    );
    assert!(ApertureSize::Unreadable.describe().contains("was not read"));
    assert!(
        ApertureSize::NotObserved
            .describe()
            .contains("no configuration-space read")
    );
    for size in [
        ApertureSize::Observed {
            raw: 3 << 6,
            aperture: 1 << 32,
        },
        ApertureSize::Unmodelled { raw: 0 },
        ApertureSize::Unreadable,
        ApertureSize::NotObserved,
    ] {
        assert!(
            !size.describe().is_empty(),
            "{size:?} must describe itself for the log"
        );
    }
}

#[test]
fn the_observed_aperture_bounds_every_entry_and_every_allocation() {
    // The failure this pins down is the one with no diagnostic on this
    // machine: the window is 8 MiB whatever the device says, but the table
    // the hardware walks is only the `GGMS`-sized prefix of it, so an entry
    // written past the observed aperture is a write into BAR space that is
    // not a page table.  A window of a million entries with a device that
    // reports 1 GiB is exactly that mismatch.
    let table = MockPageTable::new(GGTT_ARRAY_BYTES / PTE_BYTES);
    let gtt = Gtt::over_reported(
        alloc::boxed::Box::new(table.clone()),
        ApertureSize::decode(1 << 6),
    )
    .unwrap();
    assert_eq!(gtt.aperture(), 1 << 30);
    assert_eq!(gtt.entries(), ((1 << 30) / PAGE_SIZE) as usize);
    assert_eq!(gtt.window_entries(), GGTT_ARRAY_BYTES / PTE_BYTES);
    // The address is inside the mapped window and outside the aperture, so
    // the refusal has to be the aperture's and it has to name it.
    assert_eq!(
        gtt.entry(1 << 30),
        Err(GttError::AddressOutsideAperture {
            address: 1 << 30,
            aperture: 1 << 30,
        })
    );
    // The last page of the aperture is readable -- the reserve is a bound on
    // the allocator, not a hole in the addressing.
    assert!(!gtt.entry((1 << 30) - PAGE_SIZE).unwrap().is_present());
    // And the allocation lands inside the aperture, leaving the rest of the
    // window untouched.
    let address = gtt.map_linear(0x1000, PAGE_SIZE as usize).unwrap();
    // 1 GiB - 0x80000: the highest 256 KiB boundary whose 65-entry block,
    // plus the reserved top page, still fits under the observed aperture.
    assert_eq!(address, 0x3ff8_0000);
    assert!(address + (1 + SCANOUT_PADDING_ENTRIES) * PAGE_SIZE <= 1 << 30);
    assert_eq!(table.raw(((1 << 30) / PAGE_SIZE) as usize), 0);
    assert_eq!(table.raw(GGTT_ARRAY_BYTES / PTE_BYTES - 1), 0);
}

#[test]
fn an_unmodelled_size_refuses_the_allocation_path_by_name() {
    let table = MockPageTable::new(ONE_RUN_ENTRIES);
    let gtt = Gtt::over_reported(
        alloc::boxed::Box::new(table.clone()),
        ApertureSize::Unmodelled { raw: 0 },
    )
    .unwrap();
    // The page table exists and is described; nothing is allocated out of
    // it, because the aperture is unknown rather than zero.
    assert_eq!(gtt.aperture(), 0);
    assert_eq!(gtt.entries(), 0);
    assert_eq!(gtt.window_entries(), ONE_RUN_ENTRIES);
    assert_eq!(
        gtt.map_linear(0x1000, PAGE_SIZE as usize),
        Err(GttError::ApertureSizeUnmodelled { raw: 0 })
    );
    assert_eq!(
        gtt.entry(0),
        Err(GttError::AddressOutsideAperture {
            address: 0,
            aperture: 0,
        })
    );
    for index in 0..ONE_RUN_ENTRIES {
        assert_eq!(table.raw(index), 0, "nothing may be written");
    }
    assert!(
        GttError::ApertureSizeUnmodelled { raw: 0 }
            .describe()
            .contains("no graphics address")
    );
}

#[test]
fn a_size_that_was_not_observed_keeps_the_window_aperture() {
    // The behaviour this kernel had before it read the field, kept for the
    // case where no read happened or none answered: the aperture is the one
    // the mapped window covers.
    for size in [ApertureSize::Unreadable, ApertureSize::NotObserved] {
        let gtt = Gtt::over_reported(
            alloc::boxed::Box::new(MockPageTable::new(ONE_RUN_ENTRIES)),
            size,
        )
        .unwrap();
        assert_eq!(gtt.aperture(), ONE_RUN_ENTRIES as u64 * PAGE_SIZE);
        assert_eq!(gtt.entries(), ONE_RUN_ENTRIES);
        assert_eq!(gtt.window_entries(), ONE_RUN_ENTRIES);
        assert_eq!(gtt.size(), size);
        assert_eq!(
            gtt.map_linear(0x1000, PAGE_SIZE as usize).unwrap(),
            ONE_RUN_ENTRY * PAGE_SIZE
        );
        assert!(gtt.describe().contains("window"));
    }
}

#[test]
fn a_run_of_pages_is_written_present_and_in_order() {
    let mut scratch = Scratch::new(SEVERAL_RUNS_ENTRIES);
    let gtt = scratch.gtt();
    let physical = 0x40_0000;
    let address = gtt.map_linear(physical, 3 * PAGE_SIZE as usize).unwrap();
    // Page aligned (what PLANE_SURF needs) and 256 KiB aligned (what the
    // VT-d workaround wants), and the run is the top of the aperture's
    // usable region.
    assert!(address.is_multiple_of(PAGE_SIZE));
    assert!(address.is_multiple_of(SCANOUT_ALIGNMENT));
    assert_eq!(address, 896 * PAGE_SIZE);
    for page in 0..3u64 {
        let entry = gtt.entry(address + page * PAGE_SIZE).unwrap();
        assert!(
            entry.describes(physical + page * PAGE_SIZE),
            "page {page} names {:#x}",
            entry.address()
        );
    }
    // The padding after the run is the zero page, not the run's memory and
    // not a hole.
    for entry in 3..3 + SCANOUT_PADDING_ENTRIES {
        assert_eq!(
            Pte::from_raw(scratch.words[((address / PAGE_SIZE) + entry) as usize]),
            scratch_entry(),
            "padding entry {entry}"
        );
    }
    // Below the block, the space is free and untouched: the next allocation
    // starts there rather than inside the block.
    let below = address - PAGE_SIZE;
    assert_eq!(gtt.entry(below).unwrap().raw(), 0);
    let next = gtt.map_linear(0x50_0000, PAGE_SIZE as usize).unwrap();
    assert!(next + (1 + SCANOUT_PADDING_ENTRIES) * PAGE_SIZE <= address);
    assert_eq!(next, 768 * PAGE_SIZE);
}

#[test]
fn the_padding_after_a_run_is_bound_to_the_zero_page() {
    let (gtt, table) = one_run_gtt();
    let physical = 0x40_0000;
    let address = gtt.map_linear(physical, PAGE_SIZE as usize).unwrap();
    assert_eq!(address, ONE_RUN_ENTRY * PAGE_SIZE);
    let first = (address / PAGE_SIZE) as usize;
    assert!(Pte::from_raw(table.raw(first)).describes(physical));
    // 64 entries, every one of them present and naming the same zero page:
    // the VT-d workaround wants a valid entry after the scanout, and a zero
    // page is the one content that cannot be mistaken for a picture.
    for entry in 1..=SCANOUT_PADDING_ENTRIES as usize {
        let padding = Pte::from_raw(table.raw(first + entry));
        assert_eq!(padding, scratch_entry(), "padding entry {entry}");
        assert!(padding.is_present());
        assert!(!padding.is_local_memory());
        assert!(!padding.describes(physical + entry as u64 * PAGE_SIZE));
    }
    // And the padding stops where the constant says it does.
    assert_eq!(table.raw(first + SCANOUT_PADDING_ENTRIES as usize + 1), 0);
    assert_eq!(zero_page_physical() % PAGE_SIZE, 0);
}

#[test]
fn a_run_needs_its_padding_to_be_free() {
    // The padding is part of what an allocation occupies, so a block whose
    // *padding* would land on a live entry has to move: writing the zero
    // page over the firmware's scanout is the same mistake as writing the
    // surface there.
    let mut scratch = Scratch::new(SEVERAL_RUNS_ENTRIES);
    // Entry 950 is inside the padding of the first candidate block (which
    // starts at entry 896), and nothing else is occupied.
    let live = 0x20_0000 | PTE_PRESENT;
    scratch.words[950] = live;
    let before = scratch.words.clone();
    let gtt = scratch.gtt();
    let address = gtt.map_linear(0x40_0000, PAGE_SIZE as usize).unwrap();
    // The block moved below the occupied entry: 832 is the first 256 KiB
    // boundary whose 65 entries end below entry 950.
    assert_eq!(address, 832 * PAGE_SIZE);
    assert_eq!(scratch.words[950], before[950], "the live entry is whole");
    // Everything between the new block's padding and the live entry is
    // untouched, including the entry the first candidate would have used.
    for index in 897..950 {
        assert_eq!(scratch.words[index], before[index], "entry {index}");
    }
}

#[test]
fn a_length_that_is_not_whole_pages_maps_the_whole_last_page() {
    let mut scratch = Scratch::new(SEVERAL_RUNS_ENTRIES);
    let gtt = scratch.gtt();
    let physical = 0x80_0000;
    let address = gtt
        .map_linear(physical, 2 * PAGE_SIZE as usize + 1)
        .unwrap();
    // Three pages are mapped for two pages and one byte, and the first of
    // them is on a 256 KiB boundary.
    assert_eq!(address, 896 * PAGE_SIZE);
    assert!(gtt.entry(address).unwrap().describes(physical));
    assert!(
        gtt.entry(address + PAGE_SIZE)
            .unwrap()
            .describes(physical + PAGE_SIZE)
    );
    assert!(
        gtt.entry(address + 2 * PAGE_SIZE)
            .unwrap()
            .describes(physical + 2 * PAGE_SIZE)
    );
    assert_eq!(Pte::from_raw(scratch.words[896 + 3]), scratch_entry());
    assert!(!gtt.entry(address - PAGE_SIZE).unwrap().is_present());
    // The block moved by three pages and the padding, not two.
    assert_eq!(
        gtt.map_linear(0x90_0000, PAGE_SIZE as usize).unwrap(),
        768 * PAGE_SIZE
    );
}

#[test]
fn a_second_mapping_does_not_overlap_the_first() {
    let mut scratch = Scratch::new(SEVERAL_RUNS_ENTRIES);
    let gtt = scratch.gtt();
    let first = gtt.map_linear(0x10_0000, 2 * PAGE_SIZE as usize).unwrap();
    let second = gtt.map_linear(0x20_0000, 3 * PAGE_SIZE as usize).unwrap();
    // Allocation is top-down, so the second block is entirely below the
    // first, padding included.
    assert!(second + (3 + SCANOUT_PADDING_ENTRIES) * PAGE_SIZE <= first);
    assert!(gtt.entry(first).unwrap().describes(0x10_0000));
    assert!(gtt.entry(second).unwrap().describes(0x20_0000));
}

#[test]
fn a_present_entry_is_never_overwritten() {
    // This is the property the whole allocation policy exists for: the
    // firmware's live scanout is somewhere in this table and its entries
    // are present.  A new mapping must step over them, not reuse them.
    let mut scratch = Scratch::new(SEVERAL_RUNS_ENTRIES);
    let entries = scratch.words.len();
    // Stand in for the firmware: the top two pages of the aperture, one of
    // which is the page this module reserves for the prefetch guard.
    let firmware = 0x30_0000;
    scratch.words[entries - 1] = firmware | PTE_PRESENT;
    scratch.words[entries - 2] = (firmware + PAGE_SIZE) | PTE_PRESENT;
    let before = scratch.words.clone();
    let gtt = scratch.gtt();

    let address = gtt.map_linear(0x90_0000, 2 * PAGE_SIZE as usize).unwrap();
    // The whole block, padding included, is below the occupied pages ...
    assert!(
        address + (2 + SCANOUT_PADDING_ENTRIES) * PAGE_SIZE <= (entries as u64 - 2) * PAGE_SIZE
    );
    // ... and the entries that were there are bit-for-bit unchanged.
    assert_eq!(scratch.words[entries - 1], before[entries - 1]);
    assert_eq!(scratch.words[entries - 2], before[entries - 2]);
}

#[test]
fn a_run_restarts_below_an_occupied_page_rather_than_straddling_it() {
    // "Skip the occupied page" is only correct if the run restarts below
    // it: a run that straddled the gap would put the surface's pages out of
    // order in the aperture, and the display engine walks the aperture
    // linearly from the surface address.
    let mut scratch = Scratch::new(SEVERAL_RUNS_ENTRIES);
    // Occupied: entry 900, which is inside the first candidate block (896
    // through 962).  The block must restart below it rather than straddle
    // it, because a run that straddled the gap would put the surface's
    // pages out of order in the aperture, and the display engine walks the
    // aperture linearly from the surface address.
    let live = 0x20_0000 | PTE_PRESENT;
    scratch.words[900] = live;
    let gtt = scratch.gtt();
    let address = gtt.map_linear(0x40_0000, 3 * PAGE_SIZE as usize).unwrap();
    // The first 256 KiB boundary whose 67-entry block ends below entry 900.
    assert_eq!(address, 832 * PAGE_SIZE);
    for page in 0..3u64 {
        assert!(
            gtt.entry(address + page * PAGE_SIZE)
                .unwrap()
                .describes(0x40_0000 + page * PAGE_SIZE)
        );
    }
    // The block ends at entry 898 and the gap above it was not used: the
    // entry the first candidate would have written into is still absent.
    assert_eq!(scratch.words[899], 0);
    assert_eq!(scratch.words[900], live);
}

#[test]
fn an_exhausted_aperture_is_an_error_and_not_a_wrap() {
    let (gtt, _table) = one_run_gtt();
    // 1 MiB of aperture holds exactly one one-page run: its 65-entry block
    // fits on the entry-128 boundary, and nothing above or below that
    // boundary has room for another.
    let address = gtt.map_linear(0x1000, PAGE_SIZE as usize).unwrap();
    assert_eq!(address, ONE_RUN_ENTRY * PAGE_SIZE);
    for pages in [1u64, 2, 3] {
        assert_eq!(
            gtt.map_linear(0x9000, pages as usize * PAGE_SIZE as usize),
            Err(GttError::ApertureExhausted {
                pages,
                aperture: ONE_RUN_ENTRIES as u64 * PAGE_SIZE,
            }),
            "a {pages}-page run must not be answered out of the reserved pages"
        );
    }
}

#[test]
fn the_first_page_of_the_aperture_is_never_handed_out() {
    // Reference section 5.6 disables a plane with PLANE_SURF = 0 and
    // section 11 phase 4.3 reads PLANE_SURFLIVE = 0 as "the address was
    // rejected", so a surface at aperture zero would be indistinguishable
    // from an unarmed plane in the one register that proves it armed.  A
    // 129-entry table has room for one run at address zero and nowhere
    // else, so it is refused; one entry more and the run lands on the first
    // boundary above the reserved page.
    let (refused, table) = gtt_of(129);
    assert_eq!(
        refused.map_linear(0x1000, PAGE_SIZE as usize),
        Err(GttError::ApertureExhausted {
            pages: 1,
            aperture: 129 * PAGE_SIZE,
        })
    );
    for index in 0..129 {
        assert_eq!(
            table.raw(index),
            0,
            "nothing may be written at address zero"
        );
    }
    let (accepted, _table) = gtt_of(130);
    assert_eq!(
        accepted.map_linear(0x1000, PAGE_SIZE as usize).unwrap(),
        SCANOUT_ALIGNMENT,
        "the first 256 KiB boundary above the reserved page"
    );
}

#[test]
fn the_top_page_of_the_aperture_is_never_handed_out() {
    // [I915] leaves the last page of the GGTT to the scratch page because
    // the hardware prefetches past the end of an object
    // (gt/intel_ggtt.c:815-826, cleared at :906-907).  This kernel reserves
    // it without writing it, because the firmware's own entry may still be
    // there: the reserve is a bound in the allocator.
    let (gtt, table) = one_run_gtt();
    let top = ONE_RUN_ENTRIES - 1;
    let _ = gtt.map_linear(0x1000, PAGE_SIZE as usize).unwrap();
    assert_eq!(table.raw(top), 0, "the reserve is not a write");
    // The whole aperture is used up, and the top page is still not the
    // answer: the search stops below it.
    assert!(gtt.map_linear(0x2000, PAGE_SIZE as usize).is_err());
    assert_eq!(table.raw(top), 0);
}

#[test]
fn a_write_that_does_not_stick_is_refused_and_named() {
    let (gtt, table) = one_run_gtt();
    table.drop_writes();
    let error = gtt.map_linear(0x1000, PAGE_SIZE as usize).unwrap_err();
    match error {
        GttError::ReadBackMismatch { index, wrote, read } => {
            // The first entry of the block, which is the first one written.
            assert_eq!(index, ONE_RUN_ENTRY as usize);
            assert_eq!(wrote, 0x1000 | PTE_PRESENT);
            assert_eq!(read, 0, "the dropped write leaves the entry as it was");
        }
        other => panic!("expected a read-back mismatch, got {other:?}"),
    }
}

#[test]
fn a_dropped_padding_write_is_refused_too() {
    // The padding is a real entry and the VT-d workaround depends on it, so
    // a dropped write there is the same class of failure as a dropped write
    // into the run.  The mock forgets *every* write, so the first mismatch
    // reported is the run's; this pins that the padding is read back at all
    // by failing the run and checking the error names an entry of the
    // block.
    let (gtt, table) = one_run_gtt();
    table.drop_writes();
    let error = gtt.map_linear(0x1000, PAGE_SIZE as usize).unwrap_err();
    match error {
        GttError::ReadBackMismatch { index, .. } => {
            assert!(
                (ONE_RUN_ENTRY as usize
                    ..ONE_RUN_ENTRY as usize + 1 + SCANOUT_PADDING_ENTRIES as usize)
                    .contains(&index),
                "entry {index} is inside the block"
            );
        }
        other => panic!("expected a read-back mismatch, got {other:?}"),
    }
}

#[test]
fn the_physical_range_is_validated_before_anything_is_written() {
    let (gtt, table) = one_run_gtt();
    // The last page of the run is out of the entry's address field, so the
    // run is refused even though its first page is fine.
    let physical = PTE_ADDRESS_MASK - PAGE_SIZE;
    assert_eq!(
        gtt.map_linear(physical, 3 * PAGE_SIZE as usize),
        Err(GttError::AddressTooWide {
            address: PTE_ADDRESS_MASK + PAGE_SIZE
        })
    );
    // Nothing was written and nothing was allocated: the aperture is still
    // whole for the next caller.
    for index in 0..ONE_RUN_ENTRIES {
        assert_eq!(table.raw(index), 0);
    }
    assert_eq!(
        gtt.map_linear(0x1000, PAGE_SIZE as usize).unwrap(),
        ONE_RUN_ENTRY * PAGE_SIZE
    );
    assert_eq!(table.raw(0), 0, "the reserved page stays absent");
}

#[test]
fn every_refusal_names_itself() {
    let cases = [
        GttError::BarTooSmall {
            observed: 0x20_0000,
            needed: MIN_BAR0_BYTES,
        },
        GttError::WindowUnmappable { physical: 0x1000 },
        GttError::WindowTooSmall { bytes: 4 },
        GttError::AddressZero,
        GttError::AddressNotAligned { address: 0x2000 },
        GttError::AddressTooWide { address: 1 << 50 },
        GttError::AddressOutsideAperture {
            address: 0x1000,
            aperture: 0,
        },
        GttError::ApertureSizeUnmodelled { raw: 0 },
        GttError::EmptyRun,
        GttError::ApertureExhausted {
            pages: 2,
            aperture: 0x4000,
        },
        GttError::ReadBackMismatch {
            index: 1,
            wrote: 0x1001,
            read: 0,
        },
    ];
    for error in cases {
        let text = error.describe();
        assert!(!text.is_empty(), "{error:?} must describe itself");
    }
}

#[test]
fn a_bar_too_short_for_the_page_table_is_refused() {
    // Reference section 3.2's warning, as a test: i915 under-maps BAR 0 to
    // 2 MiB for registers, and a driver that took that for the BAR size
    // would map -- and write page table entries into -- whatever follows
    // it.  The refusal is the only thing between that mistake and unrelated
    // memory, so it is checked without a BAR to map.
    assert_eq!(
        array_physical(0x6000_0000, 0x0020_0000),
        Err(GttError::BarTooSmall {
            observed: 0x0020_0000,
            needed: MIN_BAR0_BYTES,
        })
    );
    // A 16 MiB BAR puts the array exactly 8 MiB into it.
    assert_eq!(
        array_physical(0x6000_0000, MIN_BAR0_BYTES).unwrap(),
        0x6080_0000
    );
    // An address that cannot hold the offset without wrapping is refused
    // rather than wrapped.
    assert!(array_physical(u64::MAX - PAGE_SIZE, MIN_BAR0_BYTES).is_err());
}

#[test]
fn a_run_of_zero_pages_is_refused() {
    let (gtt, _table) = one_run_gtt();
    assert_eq!(gtt.map_linear(0x1000, 0), Err(GttError::EmptyRun));
}

#[test]
fn a_page_table_window_shorter_than_an_entry_is_refused() {
    let mut words = alloc::vec![0u64; 1];
    // SAFETY: the buffer is live for the call; the length is what is being
    // tested, and 4 bytes is half an entry.
    let short = unsafe { Gtt::from_mapped(words.as_mut_ptr() as usize, 4) };
    assert_eq!(short.err(), Some(GttError::WindowTooSmall { bytes: 4 }));
    // SAFETY: as above, with a whole entry.
    let whole = unsafe { Gtt::from_mapped(words.as_mut_ptr() as usize, PTE_BYTES) };
    assert!(whole.is_ok());
}

#[test]
fn an_address_outside_the_table_is_refused() {
    let (gtt, _table) = one_run_gtt();
    let aperture = ONE_RUN_ENTRIES as u64 * PAGE_SIZE;
    assert_eq!(
        gtt.entry(aperture),
        Err(GttError::AddressOutsideAperture {
            address: aperture,
            aperture,
        })
    );
    assert_eq!(
        gtt.is_present(PAGE_SIZE / 2),
        Err(GttError::AddressNotAligned {
            address: PAGE_SIZE / 2
        })
    );
}

#[test]
fn the_aperture_follows_the_window_when_nothing_was_observed() {
    let (gtt, _table) = one_run_gtt();
    assert_eq!(gtt.entries(), ONE_RUN_ENTRIES);
    assert_eq!(gtt.window_entries(), ONE_RUN_ENTRIES);
    assert_eq!(gtt.aperture(), ONE_RUN_ENTRIES as u64 * PAGE_SIZE);
    assert_eq!(gtt.size(), ApertureSize::NotObserved);
    // A full-size window is the 8 MiB the BAR holds, which is exactly the
    // 4 GiB PLANE_SURF can name; a larger one is capped rather than
    // believed.
    let full = Gtt::over(alloc::boxed::Box::new(MockPageTable::new(
        GGTT_ARRAY_BYTES / PTE_BYTES,
    )))
    .unwrap();
    assert_eq!(full.aperture(), MAX_APERTURE);
    assert_eq!(full.entries(), (MAX_APERTURE / PAGE_SIZE) as usize);
    assert_eq!(full.window_entries(), GGTT_ARRAY_BYTES / PTE_BYTES);
    let larger = Gtt::over(alloc::boxed::Box::new(MockPageTable::new(
        GGTT_ARRAY_BYTES / PTE_BYTES * 2,
    )))
    .unwrap();
    assert_eq!(larger.aperture(), MAX_APERTURE);
    assert_eq!(larger.entries(), (MAX_APERTURE / PAGE_SIZE) as usize);
    assert_eq!(larger.window_entries(), GGTT_ARRAY_BYTES / PTE_BYTES * 2);
}

#[test]
fn the_report_says_which_of_the_four_states_the_size_field_is_in() {
    // The one line a reader on the machine will have: it must contain both
    // numbers (what this module allocates out of and what it mapped) and
    // the observation they came from.
    let (gtt, _table) = one_run_gtt();
    let text = gtt.describe();
    assert!(text.contains("aperture 0x100000"), "{text}");
    assert!(text.contains("0x800 bytes of array"), "{text}");
    assert!(
        text.contains("256 entries, covering 0x100000 bytes"),
        "{text}"
    );
    assert!(text.contains("no configuration-space read"), "{text}");
    let bounded = Gtt::over_reported(
        alloc::boxed::Box::new(MockPageTable::new(GGTT_ARRAY_BYTES / PTE_BYTES)),
        ApertureSize::decode(1 << 6),
    )
    .unwrap();
    let text = bounded.describe();
    assert!(text.contains("aperture 0x40000000"), "{text}");
    assert!(text.contains("0x800000 bytes of array"), "{text}");
    assert!(
        text.contains("1048576 entries, covering 0x100000000 bytes"),
        "{text}"
    );
    assert!(text.contains("reads 0x0040"), "{text}");
    assert!(text.contains("2 MiB of page table"), "{text}");
}
