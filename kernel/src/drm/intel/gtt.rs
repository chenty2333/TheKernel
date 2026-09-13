//! The global graphics translation table, and the framebuffer's place in it.
//!
//! A display engine does not read a physical address.  It reads a *graphics*
//! address, and the global graphics translation table (GGTT) is the page table
//! that turns one into the other: one 8-byte entry per 4 KiB page of aperture,
//! holding a physical page address and a handful of flag bits.  This module
//! encodes those entries, writes a contiguous run of them for a contiguous
//! physical range, reads them back, and hands back the aperture address that a
//! plane's surface register wants.
//!
//! ## Where the page table is
//!
//! `BAR 0` (`GTTMMADR`) is 16 MiB on Gen8 and later.  Reference §1.1 and §3.2:
//! the first 8 MiB is registers, the second 8 MiB is the GGTT page table
//! array.  [`regs::PROBE_WINDOW`] maps the first 2 MiB only -- it is the
//! register window, and it deliberately stops short of the page table -- so
//! this module maps [`GGTT_ARRAY_OFFSET`] into the same BAR itself, exactly as
//! `[I915]` does (`gt/intel_ggtt.c:1133-1147`, `gttadr_offset = gttmmadr_size /
//! 2`, and `ggtt_probe_common()` at `:1149-1185` ioremaps that offset).
//!
//! The firmware also leaves the array's *physical* base in `GSMBASE`
//! (`0x108100`, bits `[63:20]`, reference §1.1); `[I915]` reads it only on the
//! parts where it cannot reach the array through the BAR (`ggtt_probe_common`,
//! the `i915_direct_stolen_access()` branch).  This kernel maps the BAR window,
//! which is the route that does not need a stolen-memory manager.
//!
//! ## The entry
//!
//! **The reference document does not give the GGTT page table entry layout.**
//! Section 1.1 locates the array and `GSMBASE`, and `regs/mod.rs` declares
//! `GSMBASE` as the page table base, but no section states the bits.  The
//! layout below is therefore read from `[I915]` `gt/intel_gtt.h` and
//! `gt/intel_ggtt.c`, and every claim in it is cited to a line:
//!
//! | Bit(s) | Meaning | Source |
//! |---|---|---|
//! | 0 | present | `GEN8_PAGE_PRESENT`, `gt/intel_gtt.h:152` |
//! | 1 | local memory on Gen12 (`GEN12_GGTT_PTE_LM`, `gt/intel_gtt.h:97`) — and `GEN8_PAGE_RW` on older parts (`:153`) | same |
//! | 12..=45 | physical page address, `GEN12_GGTT_PTE_ADDR_MASK = GENMASK_ULL(45, 12)`, `gt/intel_gtt.h:100` | same |
//! | 2..=11, 46..=63 | not set for a system-memory page | inferred from the encoder below |
//!
//! The encoder that decides those bits for this part is
//!
//! ```c
//! /* gt/intel_ggtt.c:277-286 */
//! u64 gen8_ggtt_pte_encode(dma_addr_t addr, unsigned int pat_index, u32 flags)
//! {
//!         gen8_pte_t pte = addr | GEN8_PAGE_PRESENT;
//!         if (flags & PTE_LM)
//!                 pte |= GEN12_GGTT_PTE_LM;
//!         return pte;
//! }
//! ```
//!
//! Three consequences are worth stating because each one is a way to get a
//! black screen with no diagnostic:
//!
//! * **Bit 1 stays clear on this part.**  `PTE_LM` is set only for local
//!   memory, which is the discrete parts' memory; ADL-N is system memory, and
//!   bit 1 is `GEN12_GGTT_PTE_LM` there, not the read/write bit of an x86 page
//!   table entry that the older `gen8_pte_encode` set.  A PTE marking a page of
//!   system memory as local memory is exactly the class of mistake §11 phase
//!   3.2 warns about.  [`Pte::is_local_memory`] reports the bit rather than
//!   masking it away, so a wrong entry is visible in a log.
//! * **No cache-policy bits are set.**  The Gen12 GGTT encoder ignores the PAT
//!   index it is handed; on this generation the cache behaviour of a scanout
//!   read is the platform's, not the entry's.  Earlier generations did encode
//!   it (`PPAT_UNCACHED`/`PPAT_CACHED`, `:135-141`), which is why a reader who
//!   has seen those bits expects them here.
//! * **`pat_index` is ignored, so this driver cannot ask for uncached GPU
//!   reads.**  That is not a gap this kernel can close from the page table.
//!
//! ## The aperture, and where an allocation may go
//!
//! The array is 8 MiB of 8-byte entries, which is 1 048 576 entries covering
//! 4 GiB of aperture.  **That size is an inference**, not a statement in the
//! reference: it follows from the BAR split (the array is the last 8 MiB of a
//! 16 MiB BAR) and from `PLANE_SURF[31:12]` (reference §5.4) being able to name
//! exactly 4 GiB.  `[I915]` states the same total in a comment:
//! `gt/intel_ggtt.c:769-778`, "if the size of the GGTT is 4G", and clamps any
//! larger table to `1ULL << 32` at `:1471-1478`.  This module therefore derives
//! the aperture from the length of the array it was given -- the arithmetic
//! follows the mapping rather than a hard-coded size -- and caps it at
//! [`MAX_APERTURE`], which is the largest address `PLANE_SURF` can carry.
//!
//! An allocation is placed **from the top of the aperture downwards**, and
//! every candidate page is skipped when its entry is already present.  The
//! second half of that is the load-bearing one: the firmware programmed this
//! machine's current scanout into the same table, and overwriting those entries
//! corrupts the only console the machine has.  `[I915]` treats it as a real
//! hazard in the same situation -- `display/intel_plane_initial.c:220-232`
//! refuses to relocate the firmware's framebuffer onto its own PTEs, "that
//! would corrupt the original PTEs which are still being used for scanout" --
//! so writing over a present entry is refused here rather than trusted to luck.
//! The top-down direction is defence in depth, not the protection: the usual
//! place for a firmware framebuffer is low in the aperture (the PRM's own
//! worked example maps it at `0x200000`, `[PRM]` DG1 Vol 12 §"GTT mapping"),
//! but `[I915]` records that at least one GOP places it high
//! (`intel_plane_initial.c:222-225`, "MTL GOP likes to place the framebuffer
//! high up in ggtt"), so neither direction is safe on its own.
//!
//! ## What this module deliberately does not do
//!
//! * **It does not invalidate any translation cache.**  `[I915]` writes
//!   `GEN12_GUC_TLB_INV_CR` (`0xcee8`, bit 0) after every PTE update
//!   (`gt/intel_ggtt.c:237-252`); that register is below `0x40000`, the range
//!   the reference's §2.1 model assigns to `FORCEWAKE_GT`
//!   (`intel_uncore.c`, `__gen12_fw_ranges`, `GEN_FW_RANGE(0xb400, 0xcfff,
//!   FORCEWAKE_GT)`), and this kernel implements no forcewake handshake and
//!   refuses to address registers outside [`regs::FORCEWAKE_FREE_BANDS`].  A
//!   freshly written entry for an address the display engine has never walked is
//!   *expected* to be picked up without one, but no source states that, and the
//!   vendor driver does not rely on it: it invalidates after the first binding
//!   of an object as well as after an update (`gen8_ggtt_insert_entries` ends
//!   in `ggtt->invalidate(ggtt)`, `:497-501`).  See
//!   `docs/design/intel-scanout.md`.
//! * **It does not unmap.**  Nothing in this kernel frees the console's
//!   surface, and an aperture range that is released must first be cleared out
//!   of the display engine's plane register, which belongs to the module that
//!   programs the plane.  A partial teardown would be worse than none.
//! * **It does not choose the physical memory.**  [`super::fb`] does that, and
//!   says why.

use core::sync::atomic::{Ordering, compiler_fence};

use spin::Mutex;

use super::regs;

/// Where the GGTT page table array starts inside BAR 0.
///
/// `[I915]` `gt/intel_ggtt.c:1144-1147`: `gen6_gttadr_offset()` is
/// `gen6_gttmmadr_size() / 2`, and the BAR is 16 MiB on Gen8 and later
/// (`:1133-1142`).  Reference §1.1 and §3.2 give the same split.
pub(crate) const GGTT_ARRAY_OFFSET: u64 = 0x0080_0000;

/// How much of BAR 0 the page table array occupies, in bytes.
///
/// The array is the whole second half of the 16 MiB BAR.  A BAR shorter than
/// [`GGTT_ARRAY_OFFSET`] + this is refused rather than mapped: mapping past the
/// end of a BAR would map an unrelated physical range, which is a way to write
/// page table entries into something else entirely.
pub(crate) const GGTT_ARRAY_BYTES: usize = 0x0080_0000;

/// The smallest BAR 0 that can hold the register window and the page table.
pub(crate) const MIN_BAR0_BYTES: u64 = GGTT_ARRAY_OFFSET + GGTT_ARRAY_BYTES as u64;

/// One page of graphics address space.
pub(crate) const PAGE_SIZE: u64 = 0x1000;

/// Bytes one page table entry occupies.
pub(crate) const PTE_BYTES: usize = 8;

/// `GEN8_PAGE_PRESENT`: the entry names a real page.
pub(crate) const PTE_PRESENT: u64 = 1 << 0;

/// `GEN12_GGTT_PTE_LM`: the entry names a page of local memory.
///
/// Set only on the discrete parts.  On Gen12 this is the same bit position as
/// the older `GEN8_PAGE_RW`, so an entry that sets it on an integrated part is
/// asking for memory that does not exist.
pub(crate) const PTE_LOCAL_MEMORY: u64 = 1 << 1;

/// `GEN12_GGTT_PTE_ADDR_MASK`, `GENMASK_ULL(45, 12)`: the physical page
/// address an entry can name.
pub(crate) const PTE_ADDRESS_MASK: u64 = 0x0000_3fff_ffff_f000;

/// The lowest aperture address this module will hand out.
///
/// The first page is not allocatable, and that is a deliberate refusal rather
/// than an off-by-one.  Zero is the value `PLANE_SURF` and `PLANE_SURFLIVE` use
/// for "no surface": reference §5.6 disables a plane by writing `PLANE_SURF = 0`
/// and §11 phase 4.3 reads `PLANE_SURFLIVE = 0` as *the surface address was
/// rejected*.  A framebuffer placed at aperture address zero would therefore be
/// indistinguishable, in the one register that proves the plane armed, from a
/// plane that never armed at all -- and §11 phase 6.2 makes that register the
/// proof.
pub(crate) const RESERVED_LOW_APERTURE: u64 = PAGE_SIZE;

/// The largest aperture this module will hand addresses out of.
///
/// `PLANE_SURF` carries the graphics address in bits `[31:12]` (reference
/// §5.4), so no surface can be named above 4 GiB however large the table is.
pub(crate) const MAX_APERTURE: u64 = 1 << 32;

/// A GGTT page table entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Pte(u64);

impl Pte {
    /// Encode `physical` as a present entry for a page of system memory.
    ///
    /// The three refusals are the three ways an address cannot be written into
    /// an entry without the mistake being invisible:
    ///
    /// * address zero: a present entry naming page zero would have the display
    ///   engine read low memory that no allocation ever returns, and an
    ///   uninitialised address is the usual reason to see one;
    /// * an address that is not page aligned: the low twelve bits are not part
    ///   of the address field, so they would be silently dropped and the
    ///   surface would begin at a page the caller did not name;
    /// * an address with a bit above the field: `GEN12_GGTT_PTE_ADDR_MASK` is
    ///   bits `[45:12]`, and a wider address would be silently truncated to a
    ///   different page.
    pub(crate) fn encode(physical: u64) -> Result<Self, GttError> {
        if physical == 0 {
            return Err(GttError::AddressZero);
        }
        if physical & (PAGE_SIZE - 1) != 0 {
            return Err(GttError::AddressNotAligned { address: physical });
        }
        if physical & !PTE_ADDRESS_MASK != 0 {
            return Err(GttError::AddressTooWide { address: physical });
        }
        Ok(Self(physical | PTE_PRESENT))
    }

    /// The entry as it is written to the array.
    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    /// Take an entry as it was read back from the array.
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The physical page address the entry names.
    pub(crate) const fn address(self) -> u64 {
        self.0 & PTE_ADDRESS_MASK
    }

    /// Whether the entry names a real page.
    pub(crate) const fn is_present(self) -> bool {
        self.0 & PTE_PRESENT != 0
    }

    /// Whether the entry claims local memory, which on this part is wrong.
    pub(crate) const fn is_local_memory(self) -> bool {
        self.0 & PTE_LOCAL_MEMORY != 0
    }

    /// Whether the entry is exactly what this kernel writes for a physical
    /// page: present, a system-memory page, and nothing else set.
    pub(crate) fn describes(self, physical: u64) -> bool {
        self.0 == (physical & PTE_ADDRESS_MASK) | PTE_PRESENT
    }
}

/// The page table array, as something that can be read and written by index.
///
/// The real implementation is [`MappedArray`], over the mapped BAR window, and
/// is the only one that touches hardware.  Keeping the access behind a trait is
/// the same decision `regs::Registers` makes and for the same reason: the
/// interesting behaviour here is what happens when a write does not stick, when
/// an entry is already present, and when the array is smaller than the request
/// -- and none of those can be produced on demand by a real aperture, while all
/// three are reachable from a host test through a mock.
pub(crate) trait PageTable {
    /// How many entries the array holds.
    fn entries(&self) -> usize;

    /// Read entry `index`, which must be less than [`Self::entries`].
    fn read(&self, index: usize) -> u64;

    /// Write entry `index`, which must be less than [`Self::entries`].
    fn write(&self, index: usize, value: u64);
}

/// The physical address of the page table array inside a device's BAR 0.
///
/// `bar0_len` is the length the *device reports*, not the length this kernel
/// models: §3.2 warns that `[I915]` under-maps the BAR to 2 MiB for registers,
/// and a driver that assumed the BAR were 2 MiB would map whatever follows it
/// and then write page table entries into it.  The check is separate from the
/// mapping so that it can be exercised on a machine with no BAR to map, because
/// it is the one thing standing between a wrong model of the aperture and
/// unrelated memory.
pub(crate) fn array_physical(bar0_physical: u64, bar0_len: u64) -> Result<u64, GttError> {
    if bar0_len < MIN_BAR0_BYTES {
        return Err(GttError::BarTooSmall {
            observed: bar0_len,
            needed: MIN_BAR0_BYTES,
        });
    }
    bar0_physical
        .checked_add(GGTT_ARRAY_OFFSET)
        .ok_or(GttError::BarTooSmall {
            observed: bar0_len,
            needed: MIN_BAR0_BYTES,
        })
}

/// The array as a window into BAR 0.
///
/// The window is device memory, so accesses are volatile, and x86_64 orders
/// uncached loads and stores against each other in hardware -- the same
/// argument `regs` makes for the register window.  What the architecture does
/// *not* give is a compiler barrier, so both accesses carry one, and a
/// read-back of a written entry is therefore the strongest check this platform
/// offers that the write landed.  `[I915]` relies on exactly that check: "The
/// WC issue is easily caught by the readback check when writing GTT PTE
/// entries" (`gt/intel_ggtt.c:200-208`).
pub(crate) struct MappedArray {
    /// Virtual address of the first entry, kept as an integer so that this
    /// type is `Send` and `Sync` without an unsafe implementation.
    base: usize,
    entries: usize,
}

impl MappedArray {
    /// Take a page table window over memory that is already mapped.
    ///
    /// # Safety
    ///
    /// `base .. base + entries * PTE_BYTES` must be a live region that stays
    /// mapped for as long as this array is used, mapped as device memory (or,
    /// in a test, ordinary memory standing in for it), and aligned for 64-bit
    /// accesses.  Nothing else may write it while this driver owns the device:
    /// a concurrent writer is a second owner of the page table.
    pub(crate) const unsafe fn over_mapped(base: usize, entries: usize) -> Self {
        Self { base, entries }
    }

    /// Map the page table array of a Gen12 device.
    ///
    /// The mapping is uncached, which is what `[I915]` wants here on this
    /// generation: `needs_wc_ggtt_mapping()` (`gt/intel_ggtt.c:197-208`) is
    /// false from Gen11 on precisely because a write-combining mapping of this
    /// range *drops* writes larger than 64 bits, and `axmm::iomap` maps device
    /// memory uncached.
    #[cfg(target_os = "none")]
    pub(crate) fn map_bar(bar0_physical: u64, bar0_len: u64) -> Result<Self, GttError> {
        let physical = array_physical(bar0_physical, bar0_len)?;
        let address =
            usize::try_from(physical).map_err(|_| GttError::WindowUnmappable { physical })?;
        let mapped = axmm::iomap(axhal::mem::PhysAddr::from_usize(address), GGTT_ARRAY_BYTES)
            .map_err(|_| GttError::WindowUnmappable { physical })?;
        Ok(Self {
            base: mapped.as_usize(),
            entries: GGTT_ARRAY_BYTES / PTE_BYTES,
        })
    }
}

impl PageTable for MappedArray {
    fn entries(&self) -> usize {
        self.entries
    }

    fn read(&self, index: usize) -> u64 {
        debug_assert!(index < self.entries);
        let address = self.base + index * PTE_BYTES;
        // SAFETY: the constructor's contract is that the window is live,
        // mapped device memory for as long as this value exists and aligned
        // for 64-bit accesses; `index` is inside it because every caller goes
        // through `Gtt`, which checks the address against `entries`.
        let value = unsafe { core::ptr::read_volatile(address as *const u64) };
        compiler_fence(Ordering::Acquire);
        value
    }

    fn write(&self, index: usize, value: u64) {
        debug_assert!(index < self.entries);
        let address = self.base + index * PTE_BYTES;
        compiler_fence(Ordering::Release);
        // SAFETY: as for `read`.  A single naturally aligned 64-bit store is
        // the form this hardware takes: `[I915]` writes one through
        // `gen8_set_pte()` (`gt/intel_ggtt.c:429-432`, `writeq`), and the
        // 64-byte burst a write-combining mapping would produce is exactly
        // what that range drops on Gen11 and later.
        unsafe { core::ptr::write_volatile(address as *mut u64, value) };
        compiler_fence(Ordering::SeqCst);
    }
}

/// Why a page table entry could not be produced, or an aperture range not
/// taken.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GttError {
    /// BAR 0 is shorter than the register window plus the page table array.
    BarTooSmall { observed: u64, needed: u64 },
    /// The array window could not be mapped.
    WindowUnmappable { physical: u64 },
    /// A page table window smaller than one entry.
    WindowTooSmall { bytes: usize },
    /// A page table entry naming address zero.
    AddressZero,
    /// An address that is not a whole number of pages.
    AddressNotAligned { address: u64 },
    /// An address with bits outside `GEN12_GGTT_PTE_ADDR_MASK`.
    AddressTooWide { address: u64 },
    /// A graphics address outside the aperture the table covers.
    AddressOutsideAperture { address: u64, aperture: u64 },
    /// A run of zero bytes.
    EmptyRun,
    /// No free run of that many pages is left in the aperture.
    ApertureExhausted { pages: u64, aperture: u64 },
    /// An entry was written and did not read back as written.
    ///
    /// This is the one failure a real aperture can produce that no arithmetic
    /// predicts, and it is the reason the map path reads its own work back.
    ReadBackMismatch { index: usize, wrote: u64, read: u64 },
}

impl GttError {
    /// A sentence a boot log can carry.
    pub(crate) fn describe(&self) -> alloc::string::String {
        use alloc::{format, string::String};

        match self {
            Self::BarTooSmall { observed, needed } => format!(
                "BAR 0 is {observed:#x} bytes, but the page table array is at \
                 {GGTT_ARRAY_OFFSET:#x} and is {GGTT_ARRAY_BYTES:#x} bytes, so {needed:#x} are \
                 needed.  Mapping past the end of a BAR would map unrelated memory; reference \
                 section 3.2 warns that a driver must check the observed BAR length rather than \
                 assume one"
            ),
            Self::WindowUnmappable { physical } => format!(
                "the page table array at {physical:#x} could not be mapped as device memory"
            ),
            Self::WindowTooSmall { bytes } => format!(
                "a page table window of {bytes} bytes was offered, which does not hold even one \
                 8-byte entry"
            ),
            Self::AddressZero => String::from(
                "a page table entry for address zero was refused: page zero is never a scanout \
                 buffer, and an uninitialised address is the usual way one appears",
            ),
            Self::AddressNotAligned { address } => format!(
                "address {address:#x} is not 4 KiB aligned, and an entry's low twelve bits are \
                 not part of its address field"
            ),
            Self::AddressTooWide { address } => format!(
                "address {address:#x} has bits outside GEN12_GGTT_PTE_ADDR_MASK \
                 ({PTE_ADDRESS_MASK:#x}, bits 45:12), so an entry could not name it"
            ),
            Self::AddressOutsideAperture { address, aperture } => format!(
                "graphics address {address:#x} is outside the {aperture:#x}-byte aperture this \
                 table covers"
            ),
            Self::EmptyRun => String::from("a run of zero pages was requested"),
            Self::ApertureExhausted { pages, aperture } => format!(
                "no run of {pages} free pages is left in the {aperture:#x}-byte aperture: every \
                 candidate entry was already present"
            ),
            Self::ReadBackMismatch { index, wrote, read } => format!(
                "page table entry {index} did not keep what was written to it: wrote \
                 {wrote:#018x}, read {read:#018x}.  Reference section 11 phase 4.3: a surface \
                 address the hardware rejects shows up as PLANE_SURFLIVE reading zero, with no \
                 other diagnostic"
            ),
        }
    }
}

/// The mapped page table, the aperture it covers, and the allocator that hands
/// addresses out of it.
pub(crate) struct Gtt {
    array: alloc::boxed::Box<dyn PageTable + Send + Sync>,
    /// How many entries the array holds.
    entries: usize,
    /// How many bytes of graphics address space those entries cover.
    aperture: u64,
    /// The next address to hand out, counted downwards from the top.
    next: Mutex<u64>,
}

impl Gtt {
    /// Take a page table that is already mapped.
    ///
    /// The aperture follows from the number of entries rather than from a
    /// constant, so a shorter window in a test is a smaller aperture rather
    /// than a lie about the hardware.
    pub(crate) fn over(
        array: alloc::boxed::Box<dyn PageTable + Send + Sync>,
    ) -> Result<Self, GttError> {
        let entries = array.entries();
        if entries == 0 {
            return Err(GttError::WindowTooSmall { bytes: 0 });
        }
        let aperture = (entries as u64).saturating_mul(PAGE_SIZE).min(MAX_APERTURE);
        Ok(Self {
            array,
            entries,
            aperture,
            next: Mutex::new(aperture),
        })
    }

    /// Map a Gen12 device's page table array out of its BAR 0.
    ///
    /// `bar0_len` must be the length the device reports for BAR 0; this is the
    /// only constructor that touches hardware.
    #[cfg(target_os = "none")]
    pub(crate) fn map(bar0_physical: u64, bar0_len: u64) -> Result<Self, GttError> {
        Self::over(alloc::boxed::Box::new(MappedArray::map_bar(
            bar0_physical,
            bar0_len,
        )?))
    }

    /// Take a page table window over memory that is already mapped.
    ///
    /// The window must be a whole number of entries; a window that is not is an
    /// error rather than a silently shortened aperture.
    ///
    /// # Safety
    ///
    /// As [`MappedArray::over_mapped`].
    pub(crate) unsafe fn from_mapped(base: usize, len: usize) -> Result<Self, GttError> {
        if len < PTE_BYTES || !len.is_multiple_of(PTE_BYTES) {
            return Err(GttError::WindowTooSmall { bytes: len });
        }
        let entries = len / PTE_BYTES;
        // SAFETY: forwarded from this function's contract.
        let array = unsafe { MappedArray::over_mapped(base, entries) };
        Self::over(alloc::boxed::Box::new(array))
    }

    /// How many entries the page table holds.
    pub(crate) fn entries(&self) -> usize {
        self.entries
    }

    /// How many bytes of graphics address space the page table covers.
    pub(crate) fn aperture(&self) -> u64 {
        self.aperture
    }

    /// The entry at graphics address `ggtt_address`.
    ///
    /// This is the read side of the map path and the way a caller checks what
    /// the display engine will actually see, which is the only thing that
    /// matters: a write the caller believes happened and the array does not
    /// hold is a black screen.
    pub(crate) fn entry(&self, ggtt_address: u64) -> Result<Pte, GttError> {
        let index = self.index_of(ggtt_address)?;
        Ok(Pte::from_raw(self.array.read(index)))
    }

    /// Whether the entry at `ggtt_address` names a page.
    ///
    /// A zeroed table has no present entries, which is the state a machine
    /// whose firmware never programmed this address is in -- and also what a
    /// window that is not the page table reads as, which is why the mapping
    /// path asks this question about every candidate page.
    pub(crate) fn is_present(&self, ggtt_address: u64) -> Result<bool, GttError> {
        Ok(self.entry(ggtt_address)?.is_present())
    }

    /// Map a contiguous physical range into the aperture and return the
    /// graphics address a plane's surface register wants.
    ///
    /// The run is rounded up to whole pages: a length that is not a multiple of
    /// 4 KiB maps the whole of its last page, because an entry names pages and
    /// a partial page cannot be described.  The returned address is page
    /// aligned, which is what `PLANE_SURF[31:12]` requires, and the pages of
    /// the run are consecutive in the aperture as well as in memory, because
    /// the display engine walks the aperture linearly from that address.
    ///
    /// Two properties are deliberate:
    ///
    /// * **A present entry is never overwritten.**  Each candidate page is read
    ///   before it is written, and a candidate that is already present is
    ///   stepped over.  The firmware's live scanout is in this table, and on a
    ///   machine whose screen is its only output, corrupting it costs the only
    ///   diagnostic the machine has.  `[I915]` guards the same thing explicitly
    ///   in `intel_plane_initial.c:220-232`.  The cost is one extra read per
    ///   page of the allocation; the alternative is a heuristic about where
    ///   firmware puts things, and `[I915]` records that the heuristic is false
    ///   on at least one platform.
    /// * **Every entry is read back.**  A write to device memory can be
    ///   dropped, and §11 phase 4.3 states what that looks like:
    ///   `PLANE_SURFLIVE` reads zero and nothing else says why.  Failing here,
    ///   with the entry index and both values, is the difference between a
    ///   named error and a black screen.
    ///
    /// On failure the aperture range is left allocated and the entries that
    /// were written are left written.  That is stated rather than fixed: the
    /// caller must not use the surface (it has no address), and clearing the
    /// entries would be a write to the page table for a caller that is already
    /// being told the page table did not answer.
    pub(crate) fn map_linear(&self, physical: u64, len: usize) -> Result<u64, GttError> {
        if len == 0 {
            return Err(GttError::EmptyRun);
        }
        let len = len as u64;
        let pages = len.div_ceil(PAGE_SIZE);
        let bytes = pages
            .checked_mul(PAGE_SIZE)
            .ok_or(GttError::AddressTooWide { address: len })?;
        // Both ends of the run are checked before anything is written:
        // `Pte::encode` is the one place the entry's rules live, and an
        // address that is aligned at one end of a contiguous run is aligned at
        // every page of it.
        Pte::encode(physical)?;
        let last = physical
            .checked_add(bytes - PAGE_SIZE)
            .ok_or(GttError::AddressTooWide { address: physical })?;
        Pte::encode(last)?;

        // The search and the writes share one lock, so an address handed out
        // here can never be handed out again before its entries exist.  The
        // hold is bounded by the run rather than by the aperture -- at boot, on
        // one CPU, for a surface of a few thousand pages -- and it is the only
        // way to be sure two callers cannot interleave a reservation with the
        // write that makes it real.
        let mut next = self.next.lock();
        let start = self.reserve_run(&mut next, pages)?;
        for page in 0..pages {
            let index = self.index_of(start + page * PAGE_SIZE)?;
            let pte = Pte::encode(physical + page * PAGE_SIZE)?;
            self.array.write(index, pte.raw());
        }
        for page in 0..pages {
            let index = self.index_of(start + page * PAGE_SIZE)?;
            let wrote = Pte::encode(physical + page * PAGE_SIZE)?.raw();
            let read = self.array.read(index);
            if read != wrote {
                return Err(GttError::ReadBackMismatch { index, wrote, read });
            }
        }
        Ok(start)
    }

    /// Find a run of `pages` consecutive entries that are all absent, counting
    /// downwards from the cursor, and move the cursor below it.
    fn reserve_run(&self, next: &mut u64, pages: u64) -> Result<u64, GttError> {
        let bytes = pages * PAGE_SIZE;
        let mut end = *next;
        let mut start = end;
        let mut needed = pages;
        loop {
            // A run of `pages` entries ending at `end` would start at
            // `end - bytes`, and nothing below [`RESERVED_LOW_APERTURE`] is
            // handed out.  When `end` is below that there is no room left in
            // the aperture, however many entries below it happen to be free.
            if end < RESERVED_LOW_APERTURE + bytes {
                return Err(GttError::ApertureExhausted {
                    pages,
                    aperture: self.aperture,
                });
            }
            start -= PAGE_SIZE;
            // `start` is inside the aperture by the check above and by the
            // invariant that at most `pages - needed` pages have been walked
            // down from `end`, so the index is inside the array.
            let index = (start / PAGE_SIZE) as usize;
            if self.array.read(index) & PTE_PRESENT != 0 {
                // Occupied: the run restarts below this entry, which is what
                // keeps the pages of one surface consecutive in the aperture.
                end = start;
                needed = pages;
                continue;
            }
            needed -= 1;
            if needed == 0 {
                break;
            }
        }
        debug_assert_eq!(start + bytes, end);
        *next = start;
        Ok(start)
    }

    /// The array index a graphics address names.
    fn index_of(&self, ggtt_address: u64) -> Result<usize, GttError> {
        if !ggtt_address.is_multiple_of(PAGE_SIZE) {
            return Err(GttError::AddressNotAligned {
                address: ggtt_address,
            });
        }
        let index = ggtt_address / PAGE_SIZE;
        if index >= self.entries as u64 {
            return Err(GttError::AddressOutsideAperture {
                address: ggtt_address,
                aperture: self.aperture,
            });
        }
        Ok(index as usize)
    }
}

// The two numbers a reader will want to check against the reference, as
// assertions rather than comments: the page table begins after the register
// window this kernel maps, and the two together are exactly the 16 MiB BAR 0.
const _: () = assert!(GGTT_ARRAY_OFFSET >= regs::PROBE_WINDOW as u64);
const _: () = assert!(GGTT_ARRAY_OFFSET + GGTT_ARRAY_BYTES as u64 == 0x0100_0000);

#[cfg(test)]
pub(crate) mod mock {
    //! A page table a host test can drive.
    //!
    //! It is a `Vec<u64>` behind a mutex, with the two things that matter: a
    //! write that can be made not to stick, and contents the test chooses --
    //! which is how "the entry was already present" and "the write was dropped"
    //! are reached without a graphics device.

    use alloc::{sync::Arc, vec, vec::Vec};

    use spin::Mutex;

    use super::PageTable;

    struct Inner {
        entries: Mutex<Vec<u64>>,
        /// When set, every write is forgotten, as a burst the hardware drops
        /// would be.
        drop_writes: Mutex<bool>,
    }

    /// A handle on a mock page table.  Cloning shares the table, which is how a
    /// test keeps a reference to the array it handed to a [`super::Gtt`].
    #[derive(Clone)]
    pub(crate) struct MockPageTable {
        inner: Arc<Inner>,
    }

    impl MockPageTable {
        pub(crate) fn new(entries: usize) -> Self {
            Self {
                inner: Arc::new(Inner {
                    entries: Mutex::new(vec![0; entries]),
                    drop_writes: Mutex::new(false),
                }),
            }
        }

        /// Start forgetting every write.
        pub(crate) fn drop_writes(&self) {
            *self.inner.drop_writes.lock() = true;
        }

        /// The value of one entry, as a test set it or as a mapping left it.
        pub(crate) fn raw(&self, index: usize) -> u64 {
            self.inner.entries.lock()[index]
        }
    }

    impl PageTable for MockPageTable {
        fn entries(&self) -> usize {
            self.inner.entries.lock().len()
        }

        fn read(&self, index: usize) -> u64 {
            self.inner.entries.lock()[index]
        }

        fn write(&self, index: usize, value: u64) {
            if *self.inner.drop_writes.lock() {
                return;
            }
            self.inner.entries.lock()[index] = value;
        }
    }
}

#[cfg(test)]
mod tests {
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

    /// A page table with room for four pages: small enough that exhaustion is
    /// a few lines rather than a million iterations.
    const FOUR_PAGES: usize = 4;

    fn four_page_gtt() -> (Gtt, MockPageTable) {
        four_page_gtt_of(FOUR_PAGES)
    }

    /// A page table of `entries` entries, with a handle on its contents.
    fn four_page_gtt_of(entries: usize) -> (Gtt, MockPageTable) {
        let table = MockPageTable::new(entries);
        let gtt = Gtt::over(alloc::boxed::Box::new(table.clone())).unwrap();
        (gtt, table)
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
    fn a_run_of_pages_is_written_present_and_in_order() {
        let mut scratch = Scratch::new(64);
        let gtt = scratch.gtt();
        let physical = 0x40_0000;
        let address = gtt.map_linear(physical, 3 * PAGE_SIZE as usize).unwrap();
        assert!(address.is_multiple_of(PAGE_SIZE));
        for page in 0..3u64 {
            let entry = gtt.entry(address + page * PAGE_SIZE).unwrap();
            assert!(
                entry.describes(physical + page * PAGE_SIZE),
                "page {page} names {:#x}",
                entry.address()
            );
        }
        // Allocation is top-down, so the run sits at the top of the aperture
        // and the free space is below it.  The entry just below the run is
        // untouched: the run stopped where the arithmetic said it should, and
        // the next allocation starts there.
        assert_eq!(gtt.entry(address - PAGE_SIZE).unwrap().raw(), 0);
        let next = gtt.map_linear(0x50_0000, PAGE_SIZE as usize).unwrap();
        assert_eq!(next, address - PAGE_SIZE);
    }

    #[test]
    fn a_length_that_is_not_whole_pages_maps_the_whole_last_page() {
        let mut scratch = Scratch::new(64);
        let gtt = scratch.gtt();
        let physical = 0x80_0000;
        let address = gtt
            .map_linear(physical, 2 * PAGE_SIZE as usize + 1)
            .unwrap();
        // Three pages are mapped for two pages and one byte, and the pages run
        // downwards from the top of the aperture.
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
        assert!(!gtt.entry(address - PAGE_SIZE).unwrap().is_present());
        // The cursor moved by exactly three pages, not two.
        assert_eq!(
            gtt.map_linear(0x90_0000, PAGE_SIZE as usize).unwrap(),
            address - PAGE_SIZE
        );
    }

    #[test]
    fn a_second_mapping_does_not_overlap_the_first() {
        let mut scratch = Scratch::new(64);
        let gtt = scratch.gtt();
        let first = gtt.map_linear(0x10_0000, 2 * PAGE_SIZE as usize).unwrap();
        let second = gtt.map_linear(0x20_0000, 3 * PAGE_SIZE as usize).unwrap();
        // Allocation is top-down, so the second run is below the first.
        assert!(second + 3 * PAGE_SIZE <= first);
        assert!(gtt.entry(first).unwrap().describes(0x10_0000));
        assert!(gtt.entry(second).unwrap().describes(0x20_0000));
    }

    #[test]
    fn a_present_entry_is_never_overwritten() {
        // This is the property the whole allocation policy exists for: the
        // firmware's live scanout is somewhere in this table and its entries
        // are present.  A new mapping must step over them, not reuse them.
        let mut scratch = Scratch::new(16);
        let entries = scratch.words.len();
        // Stand in for the firmware: the top two pages of the aperture.
        let firmware = 0x30_0000;
        scratch.words[entries - 1] = firmware | PTE_PRESENT;
        scratch.words[entries - 2] = (firmware + PAGE_SIZE) | PTE_PRESENT;
        let before = scratch.words.clone();
        let gtt = scratch.gtt();

        let address = gtt.map_linear(0x90_0000, 2 * PAGE_SIZE as usize).unwrap();
        // The run is below the occupied pages ...
        assert!(address + 2 * PAGE_SIZE <= (entries as u64 - 2) * PAGE_SIZE);
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
        let mut scratch = Scratch::new(8);
        // Occupied: the top page (7) and page 5, leaving the free pages 6 and
        // 4..0.  A three-page run must therefore land on 4, 3, 2 -- not on the
        // six/seven pair, and not straddling five.
        scratch.words[7] = 0x10_0000 | PTE_PRESENT;
        scratch.words[5] = 0x20_0000 | PTE_PRESENT;
        let gtt = scratch.gtt();
        let address = gtt.map_linear(0x40_0000, 3 * PAGE_SIZE as usize).unwrap();
        assert_eq!(address, 2 * PAGE_SIZE);
        for page in 0..3u64 {
            assert!(
                gtt.entry(address + page * PAGE_SIZE)
                    .unwrap()
                    .describes(0x40_0000 + page * PAGE_SIZE)
            );
        }
        // The gap at page 6 was not used, and the occupied entries are whole.
        assert_eq!(scratch.words[6], 0);
        assert_eq!(scratch.words[7], 0x10_0000 | PTE_PRESENT);
        assert_eq!(scratch.words[5], 0x20_0000 | PTE_PRESENT);
    }

    #[test]
    fn an_exhausted_aperture_is_an_error_and_not_a_wrap() {
        let (gtt, _table) = four_page_gtt();
        // Four pages of aperture, of which the first is reserved, so the
        // largest run is three pages and it lands directly above the reserved
        // page.
        let address = gtt.map_linear(0x1000, 3 * PAGE_SIZE as usize).unwrap();
        assert_eq!(address, RESERVED_LOW_APERTURE);
        // Nothing is left: the single free page is the reserved one.
        for pages in [1u64, 2, 3] {
            assert_eq!(
                gtt.map_linear(0x9000, pages as usize * PAGE_SIZE as usize),
                Err(GttError::ApertureExhausted {
                    pages,
                    aperture: FOUR_PAGES as u64 * PAGE_SIZE,
                }),
                "a {pages}-page run must not be answered out of the reserved page"
            );
        }
    }

    #[test]
    fn the_first_page_of_the_aperture_is_never_handed_out() {
        // Reference section 5.6 disables a plane with PLANE_SURF = 0 and
        // section 11 phase 4.3 reads PLANE_SURFLIVE = 0 as "the address was
        // rejected", so a surface at aperture zero would be indistinguishable
        // from an unarmed plane in the one register that proves it armed.  A
        // one-page aperture therefore has no room for anything.
        let (gtt, _table) = four_page_gtt_of(1);
        assert_eq!(
            gtt.map_linear(0x1000, PAGE_SIZE as usize),
            Err(GttError::ApertureExhausted {
                pages: 1,
                aperture: PAGE_SIZE,
            })
        );
        // Two pages: the second one is handed out.
        let (gtt, _table) = four_page_gtt_of(2);
        assert_eq!(
            gtt.map_linear(0x1000, PAGE_SIZE as usize).unwrap(),
            PAGE_SIZE
        );
    }

    #[test]
    fn a_write_that_does_not_stick_is_refused_and_named() {
        let (gtt, table) = four_page_gtt();
        table.drop_writes();
        let error = gtt.map_linear(0x1000, PAGE_SIZE as usize).unwrap_err();
        match error {
            GttError::ReadBackMismatch { index, wrote, read } => {
                // The run is taken from the top, so the entry is the last one.
                assert_eq!(index, FOUR_PAGES - 1);
                assert_eq!(wrote, 0x1000 | PTE_PRESENT);
                assert_eq!(read, 0, "the dropped write leaves the entry as it was");
            }
            other => panic!("expected a read-back mismatch, got {other:?}"),
        }
    }

    #[test]
    fn the_physical_range_is_validated_before_anything_is_written() {
        let (gtt, table) = four_page_gtt();
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
        for index in 0..FOUR_PAGES {
            assert_eq!(table.raw(index), 0);
        }
        // The aperture is still whole for the next caller: three pages is the
        // most a four-page table can hold, because the first is reserved.
        assert_eq!(
            gtt.map_linear(0x1000, 3 * PAGE_SIZE as usize).unwrap(),
            RESERVED_LOW_APERTURE
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
        let (gtt, _table) = four_page_gtt();
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
        let (gtt, _table) = four_page_gtt();
        assert_eq!(
            gtt.entry(FOUR_PAGES as u64 * PAGE_SIZE),
            Err(GttError::AddressOutsideAperture {
                address: FOUR_PAGES as u64 * PAGE_SIZE,
                aperture: FOUR_PAGES as u64 * PAGE_SIZE,
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
    fn the_aperture_follows_the_table_it_was_given() {
        let (gtt, _table) = four_page_gtt();
        assert_eq!(gtt.entries(), FOUR_PAGES);
        assert_eq!(gtt.aperture(), FOUR_PAGES as u64 * PAGE_SIZE);
        // A full-size table covers 4 GiB, which is what PLANE_SURF can name,
        // and a larger one is capped rather than believed.
        let full = Gtt::over(alloc::boxed::Box::new(MockPageTable::new(
            GGTT_ARRAY_BYTES / PTE_BYTES,
        )))
        .unwrap();
        assert_eq!(full.aperture(), MAX_APERTURE);
        let larger = Gtt::over(alloc::boxed::Box::new(MockPageTable::new(
            GGTT_ARRAY_BYTES / PTE_BYTES * 2,
        )))
        .unwrap();
        assert_eq!(larger.aperture(), MAX_APERTURE);
    }
}
