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
//! ## The aperture is read, not inferred
//!
//! **The BAR does not say how big the aperture is, and neither does the array
//! this module maps.**  `[I915]` asserts that BAR 0 is exactly 16 MiB on this
//! generation -- `GEM_WARN_ON(pci_resource_len(pdev, GEN4_GTTMMADR_BAR) !=
//! gen6_gttmmadr_size(i915))`, `gt/intel_ggtt.c:1158` -- and takes the aperture
//! from the **`GGMS` field of the device's own configuration header**: PCI
//! configuration space offset `0x50` (`offset::GMCH_CTL`), bits `[7:6]`
//! (`BDW_GMCH_GGMS_SHIFT`/`BDW_GMCH_GGMS_MASK`,
//! `include/drm/intel/i915_drm.h:54-55`, read at `gt/intel_ggtt.c:1228-1232`),
//! which `gen8_get_total_gtt_size()` (`:1107-1121`) turns into a power-of-two
//! number of MiB of page table, and `ggtt->vm.total = (size /
//! sizeof(gen8_pte_t)) * I915_GTT_PAGE_SIZE` (`:1238`) turns into bytes of
//! graphics address space.
//! The values the field names on this generation are 2, 4 and 8 MiB of page
//! table, which -- one 8-byte entry per 4 KiB page -- are a **1, 2 or 4 GiB
//! aperture**.
//!
//! That matters because the array window is not the page table.  This module
//! maps the whole second half of the BAR ([`GGTT_ARRAY_BYTES`], 8 MiB), but the
//! table the hardware walks is only the `GGMS`-sized prefix of it; the rest of
//! the window is BAR space that is not a page table, and an entry written there
//! is a write to something else.  So the aperture this module hands addresses
//! out of is [`ApertureSize`]'s answer and never the window's, and both
//! [`Gtt::entry`] and [`Gtt::map_linear`] bound themselves by it.
//!
//! The field is read where the rest of the device's configuration header is
//! read -- the probe, through [`super::pci::ConfigSpace`] -- and carried in as
//! [`ApertureSize`]; [`ApertureSize::read`] is that read.  Three answers are
//! possible and each is named rather than guessed at:
//!
//! * **a size this kernel models**: the aperture is that size;
//! * **a size this kernel has no model for** -- the field is zero, which the
//!   vendor function maps to a zero-byte page table and a zero-byte aperture --
//!   no address is handed out at all, and [`GttError::ApertureSizeUnmodelled`]
//!   names the value that was read;
//! * **no answer**, because configuration space could not be read: the aperture
//!   is the one the mapped window covers, which is how this kernel behaved
//!   before it read the field, and [`Gtt::describe`] prints the absence rather
//!   than a number that was never observed.
//!
//! [`MAX_APERTURE`] still caps the result at 4 GiB, which is the largest
//! address `PLANE_SURF` can carry (reference §5.4) and also what `[I915]`
//! clamps a larger table to (`gt/intel_ggtt.c:1471-1478`).
//!
//! ## Where an allocation may go
//!
//! An allocation is placed **downwards from just below the reserved top page**,
//! and every entry of a candidate block is read before any of it is written.
//! The second half of that is the load-bearing one: the firmware programmed this
//! machine's current scanout into the same table, and overwriting those entries
//! corrupts the only console the machine has.  `[I915]` treats it as a real
//! hazard in the same situation -- `display/intel_plane_initial.c:217-224`
//! refuses to relocate the firmware's framebuffer onto its own PTEs, "that
//! would corrupt the original PTEs which are still being used for scanout" --
//! so writing over a present entry is refused here rather than trusted to luck.
//! The top-down direction is defence in depth, not the protection: the usual
//! place for a firmware framebuffer is low in the aperture (the PRM's own
//! worked example maps it at `0x200000`, `[PRM]` DG1 Vol 12 §"GTT mapping"),
//! but `[I915]` records that at least one GOP places it high
//! (`intel_plane_initial.c:207-209`, "MTL GOP likes to place the framebuffer
//! high up in ggtt"), so neither direction is safe on its own.
//!
//! Two pages are reserved by name rather than by luck.  The **first page** of
//! the aperture is never handed out (see [`RESERVED_LOW_APERTURE`]), and neither
//! is the **last** one (see [`RESERVED_HIGH_APERTURE`]): `[I915]` leaves the top
//! page of the GGTT bound to its scratch page because the hardware prefetches
//! past the end of an object, and this kernel reserves it without writing it,
//! because the firmware's own entries may still be in that page.
//!
//! Every run is placed on a [`SCANOUT_ALIGNMENT`] boundary with
//! [`SCANOUT_PADDING_ENTRIES`] entries of [`zero_page_physical`] bound after it,
//! which is the VT-d workaround `[I915]` applies to a scanout buffer
//! (`display/intel_fb_pin.c:127-133`) applied unconditionally, because this
//! kernel cannot tell whether VT-d is active.  The padding entries must be free
//! before they are written: a present entry is never overwritten, padding or
//! not.
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
//! * **It does not unmap.**  An aperture range that is released must first be
//!   cleared out of the display engine's plane register, which belongs to the
//!   module that programs the plane, and the pages behind it must not go back
//!   to the allocator while entries naming them are present.  A partial
//!   teardown would be worse than none, so [`super::scanout`] keeps every
//!   surface it has offered alive instead -- including the ones a later offer
//!   replaced -- and nothing calls for an unmap.
//! * **It does not choose the physical memory.**  [`super::fb`] does that, and
//!   says why.

use core::sync::atomic::{Ordering, compiler_fence};

use spin::Mutex;

use super::{pci, regs};

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

/// The highest aperture address this module will hand out, exclusive of one
/// page at the top.
///
/// `[I915]` leaves the last page of the GGTT bound to its scratch page rather
/// than allocatable: "However, leave one page at the end still bound to the
/// scratch page.  There are a number of places where the hardware apparently
/// prefetches past the end of the object, and we've seen multiple hangs with
/// the GPU head pointer stuck in a batchbuffer bound at the last page of the
/// aperture.  One page should be enough to keep any prefetching inside of the
/// aperture" (`gt/intel_ggtt.c:815-826`), and `init_ggtt` clears exactly that
/// page to the scratch page at the end of its setup (`:906-907`).  The page
/// colouring that enforces the same gap between the reserved node above the GTT
/// and any object inside it is installed only where the platform has no LLC and
/// no PPGTT (`i915_ggtt_color_adjust`, `:36-53`, installed at `:65-67`), so on
/// this part the record of the guard is the reserve itself; this kernel makes
/// the reserve its own, by name, rather than relying on the direction of the
/// search.
///
/// The same region is where `[I915]` puts the GuC's firmware reserve -- the top
/// `GUC_TOP_RESERVE_SIZE` bytes, `SZ_4G - GUC_GGTT_TOP` (`:768-799`) -- and
/// where at least one GOP puts its framebuffer
/// (`display/intel_plane_initial.c:207-209`), which is the second reason not to
/// write anything here: the reserve is a bound in the allocator, not an entry
/// this module writes.  Reserving rather than clearing is the difference
/// between this kernel and `[I915]`: the vendor driver reaches this page before
/// it has bound anything, while this kernel runs after firmware that may have
/// left a live entry in it, and overwriting a present entry is the one thing
/// that costs the only console this machine has.
pub(crate) const RESERVED_HIGH_APERTURE: u64 = PAGE_SIZE;

/// The alignment a scanout run's graphics address is placed at, in bytes.
///
/// `[I915]` raises the alignment of a scanout buffer to 256 KiB under the VT-d
/// workaround: `if (intel_scanout_needs_vtd_wa(dev_priv) && alignment < 256 *
/// 1024) alignment = 256 * 1024;` (`display/intel_fb_pin.c:132-133`), where the
/// workaround is `DISPLAY_VER(i915) >= 6 && i915_vtd_active(i915)`
/// (`display/intel_display.c:8385-8388`).  This kernel applies the alignment
/// unconditionally, for the reason [`SCANOUT_PADDING_ENTRIES`] gives.
pub(crate) const SCANOUT_ALIGNMENT: u64 = 0x0004_0000;

/// How many entries after a scanout run are bound to the zero page.
///
/// The same workaround requires "64 PTE of padding following the bo", and
/// `[I915]` gets that padding for free because it fills every unused entry with
/// its scratch page: "We currently fill all unused PTE with the shadow page and
/// so we should always have valid PTE following the scanout preventing the VT-d
/// warning" (`display/intel_fb_pin.c:127-131`).  This kernel's table is not
/// `[I915]`'s -- it is the firmware's, and only the pages a run owns may be
/// written -- so the padding is bound explicitly, and only where the allocator
/// proved those entries free.
///
/// Applying it always, rather than under `intel_scanout_needs_vtd_wa`, is
/// deliberate: nothing in this kernel can tell whether VT-d is active (no
/// `DMAR` table is parsed), and a valid entry naming a zeroed page is harmless
/// when it is not.  64 entries is 256 KiB, which is also
/// [`SCANOUT_ALIGNMENT`]; the two numbers are separate facts of the same
/// workaround and the assertion below is what keeps them in step.
pub(crate) const SCANOUT_PADDING_ENTRIES: u64 = 64;

// One page per entry, so the documented 64-entry padding and the documented
// 256 KiB alignment are the same span.
const _: () = assert!(SCANOUT_PADDING_ENTRIES * PAGE_SIZE == SCANOUT_ALIGNMENT);

/// The page bound into the entries after a scanout run.
///
/// It is a page of zeros in the kernel image, mapped by the direct map at
/// `vaddr - PHYS_VIRT_OFFSET` (the kernel is linked at
/// `kernel-base-vaddr = phys + PHYS_VIRT_OFFSET`), and nothing writes it: a
/// read of it through the GGTT is a read of a valid page whose contents the
/// display engine cannot mistake for a picture, which is what the VT-d
/// workaround wants.  Zero is the safe content for the same reason it is the
/// safe *initial* content of a surface: every byte the engine reads is black
/// and no bit pattern in it names anything.
///
/// `[I915]`'s equivalent is `vm->scratch[0]`, the one page `setup_scratch_page()`
/// allocates and the per-table encode of it (`gt/intel_ggtt.c:1178-1192`, the
/// function itself at `gt/intel_gtt.c:360-392`), and every clear binds that
/// encoding rather than a zero (`gen8_ggtt_clear_range`, `:548-566`).
#[repr(align(4096))]
struct ZeroPage([u8; PAGE_SIZE as usize]);

/// The zero page itself.  It is never written, and it is deliberately not
/// reachable by name from anywhere but [`zero_page_physical`].
static ZERO_PAGE: ZeroPage = ZeroPage([0; PAGE_SIZE as usize]);

/// The physical address of [`ZERO_PAGE`], which is what the padding entries
/// name.
///
/// The kernel image is linked at its physical address plus
/// `PHYS_VIRT_OFFSET` (`config/x86_64/n305.toml`, `kernel-base-paddr` against
/// `kernel-base-vaddr`), so the direct map's translation is the right one and
/// the same one [`super::fb`] uses for the pages it allocates.  A host test
/// has no kernel image to translate -- the dummy platform's
/// `virt_to_phys` only accepts addresses inside its page arena, and a `static`
/// is not in it -- so under a host test the address is a fixed, page-aligned
/// value that the mock page table stores and compares and nothing dereferences.
#[cfg(target_os = "none")]
fn zero_page_physical() -> u64 {
    let address = ZERO_PAGE.0.as_ptr() as usize;
    axhal::mem::virt_to_phys(axhal::mem::VirtAddr::from_usize(address)).as_usize() as u64
}

#[cfg(not(target_os = "none"))]
fn zero_page_physical() -> u64 {
    0x9000
}

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

/// What the device's configuration header said about the size of the GGTT.
///
/// This is the one fact the aperture cannot be derived from anything else: the
/// BAR is always 16 MiB (`gt/intel_ggtt.c:1158`) and the array window is always
/// its second half, while the table the hardware implements is the `GGMS` field
/// of the header (`offset::GMCH_CTL`, bits `[7:6]`,
/// `gt/intel_ggtt.c:1107-1121` and `:1228-1232`).  Carrying the *answer* rather
/// than the field means the arithmetic happens once, in [`Self::decode`], where
/// a host test can drive all four values of a two-bit field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApertureSize {
    /// The field named a size this kernel models.
    Observed {
        /// The configuration-space word as it was read back, for the report.
        raw: u16,
        /// How many bytes of graphics address space those entries cover.
        aperture: u64,
    },
    /// The field named a size this kernel has no model for.
    ///
    /// The field is zero, which `gen8_get_total_gtt_size()` maps to a zero-byte
    /// page table and so to a zero-byte aperture.  A zero-entry table is not a
    /// state this module can allocate out of, and guessing a size instead is
    /// the failure this variant exists to refuse.
    Unmodelled { raw: u16 },
    /// Configuration space did not answer, so the field was not observed.
    Unreadable,
    /// No configuration-space read was attempted.
    ///
    /// The state a host test's page table and any window that was handed in
    /// without a probe are in.
    NotObserved,
}

impl ApertureSize {
    /// Read the GGTT size field out of `bdf`'s configuration header.
    ///
    /// The read is one 16-bit configuration-space access at
    /// [`pci::offset::GMCH_CTL`], and it is the same read `[I915]` makes for the
    /// same purpose (`gt/intel_ggtt.c:1228`).  `None` from the bus is
    /// [`Self::Unreadable`] rather than a guessed size: a probe that cannot
    /// read configuration space has no observation to carry.
    pub(crate) fn read<C: pci::ConfigSpace + ?Sized>(config: &C, bdf: pci::Bdf) -> Self {
        match config.read_u16(bdf, pci::offset::GMCH_CTL) {
            Some(word) => Self::decode(word),
            None => Self::Unreadable,
        }
    }

    /// Decode the 16-bit `GMCH_CTL` word.
    ///
    /// The vendors' arithmetic, in the order `[I915]` does it:
    /// `gen8_get_total_gtt_size()` (`gt/intel_ggtt.c:1107-1121`) shifts the
    /// field down, masks it to `BDW_GMCH_GGMS_MASK`, treats it as a power of two
    /// count of MiB, and returns zero for a field of zero; `ggtt->vm.total =
    /// (size / sizeof(gen8_pte_t)) * I915_GTT_PAGE_SIZE` (`:1238`) then turns
    /// those MiB of 8-byte entries into bytes of aperture.  So the three named
    /// sizes are 2, 4 and 8 MiB of page table, and 1, 2 and 4 GiB of aperture.
    ///
    /// Zero is [`Self::Unmodelled`], not zero bytes of aperture: this module
    /// will not hand out a single address for it, because the field is the only
    /// statement of where the real table ends and a guess there is a write
    /// outside it.
    pub(crate) fn decode(word: u16) -> Self {
        let field = (word >> pci::GMCH_GGMS_SHIFT) & pci::GMCH_GGMS_MASK;
        let page_table = match field {
            // 2, 4 and 8 MiB.  The mask is two bits, so these are all of the
            // non-zero values it can produce.
            1 => 1u64 << 21,
            2 => 1u64 << 22,
            3 => 1u64 << 23,
            // Zero is the vendor function's zero-byte table, and anything a
            // wider mask would allow names a table larger than the BAR window
            // this kernel maps.  Neither is a size to allocate out of.
            _ => return Self::Unmodelled { raw: word },
        };
        Self::Observed {
            raw: word,
            aperture: (page_table / PTE_BYTES as u64) * PAGE_SIZE,
        }
    }

    /// The aperture to use, given how many bytes the mapped window covers.
    ///
    /// An observed size smaller than the window is the whole point of reading
    /// the field, and one larger than the window -- which the two-bit encoding
    /// cannot produce -- is clamped to the window rather than believed, because
    /// the window is the only memory this module has mapped.
    fn aperture_within(self, window: u64) -> u64 {
        match self {
            Self::Observed { aperture, .. } => {
                if aperture < window {
                    aperture
                } else {
                    window
                }
            }
            Self::Unmodelled { .. } => 0,
            Self::Unreadable | Self::NotObserved => window,
        }
    }

    /// A sentence for the boot log and the debug file.
    ///
    /// It names the field, its offset, its bit positions and the citation, so
    /// that a reader who has only the log can check the number against the
    /// vendor driver -- and it says *which* of the four states it is in, because
    /// "the aperture is 4 GiB" and "the aperture was never observed and 4 GiB is
    /// what the window covers" are different claims.
    pub(crate) fn describe(self) -> alloc::string::String {
        use alloc::format;

        match self {
            Self::Observed { raw, aperture } => format!(
                "observation: the GGTT size field reads {:#06x} (PCI configuration space {:#04x} \
                 bits {}:{}, the one [I915] gt/intel_ggtt.c:1228-1232 reads), which \
                 gen8_get_total_gtt_size (gt/intel_ggtt.c:1107-1121) maps to {} MiB of page table \
                 and so to the {aperture:#x}-byte aperture used here",
                raw,
                pci::offset::GMCH_CTL,
                pci::GMCH_GGMS_SHIFT + 1,
                pci::GMCH_GGMS_SHIFT,
                aperture / (1 << 29),
            ),
            Self::Unmodelled { raw } => format!(
                "observation: the GGTT size field reads {:#06x} (PCI configuration space {:#04x} \
                 bits {}:{}), which names no page table size this kernel models -- \
                 gen8_get_total_gtt_size (gt/intel_ggtt.c:1107-1121) maps zero to a zero-byte \
                 table -- so no graphics address is handed out and no page table entry is written",
                raw,
                pci::offset::GMCH_CTL,
                pci::GMCH_GGMS_SHIFT + 1,
                pci::GMCH_GGMS_SHIFT,
            ),
            Self::Unreadable => format!(
                "observation: the GGTT size field at PCI configuration space {:#04x} bits {}:{} \
                 was not read, because configuration space did not answer for this function; the \
                 field's value is therefore absent from this report, and the aperture is the one \
                 the mapped page table array covers ([I915] reads the field at \
                 gt/intel_ggtt.c:1228-1232)",
                pci::offset::GMCH_CTL,
                pci::GMCH_GGMS_SHIFT + 1,
                pci::GMCH_GGMS_SHIFT,
            ),
            Self::NotObserved => format!(
                "observation: no configuration-space read of the GGTT size field ({:#04x} bits \
                 {}:{}) was made, so the aperture is the one the mapped page table array covers; \
                 [I915] reads this field at gt/intel_ggtt.c:1228-1232",
                pci::offset::GMCH_CTL,
                pci::GMCH_GGMS_SHIFT + 1,
                pci::GMCH_GGMS_SHIFT,
            ),
        }
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
    ///
    /// The bound is [`Gtt::aperture`], which is the aperture **this kernel
    /// observed**, not the length of the mapped window: an address the window
    /// covers but the device's `GGMS` field does not is as far outside the real
    /// table as one past the end of the BAR.
    AddressOutsideAperture { address: u64, aperture: u64 },
    /// The device reported a GGTT size this kernel has no model for.
    ///
    /// The aperture is unknown rather than zero, and no address is handed out:
    /// the only alternative is guessing a size, and a guessed size writes page
    /// table entries outside the table the hardware walks.
    ApertureSizeUnmodelled { raw: u16 },
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
                 table covers, which is the aperture the device's GGTT size field names and not \
                 the length of the mapped window"
            ),
            Self::ApertureSizeUnmodelled { raw } => format!(
                "the GGTT size field read {raw:#06x} at PCI configuration space {:#04x} bits \
                 {}:{}, which names no page table size this kernel models, so no graphics address \
                 is handed out: [I915] gt/intel_ggtt.c:1107-1121 maps a zero field to a zero-byte \
                 page table, and guessing an aperture instead would write page table entries \
                 outside the table the hardware walks",
                pci::offset::GMCH_CTL,
                pci::GMCH_GGMS_SHIFT + 1,
                pci::GMCH_GGMS_SHIFT,
            ),
            Self::EmptyRun => String::from("a run of zero pages was requested"),
            Self::ApertureExhausted { pages, aperture } => format!(
                "no run of {pages} free pages is left in the {aperture:#x}-byte aperture: every \
                 candidate entry was already present, or the run and the \
                 {SCANOUT_PADDING_ENTRIES} entries of padding a scanout needs do not fit on a \
                 {SCANOUT_ALIGNMENT:#x} boundary above the reserved page at each end"
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
    /// How many entries of the array are inside the aperture: the entries that
    /// are really the page table.
    entries: usize,
    /// How many entries the mapped window holds, which is at least
    /// [`Self::entries`] and usually more.
    window_entries: usize,
    /// How many bytes of graphics address space may be handed out.
    aperture: u64,
    /// What the device's configuration header said about the size, kept for
    /// [`Self::describe`].
    size: ApertureSize,
    /// The entry bound into the padding after a run: a page of zeros.
    scratch: Pte,
    /// The next address to hand out, counted downwards from just below the
    /// reserved top page.
    next: Mutex<u64>,
}

impl Gtt {
    /// Take a page table that is already mapped, with no observation of the
    /// device's GGTT size field.
    ///
    /// This is [`Self::over_reported`] with [`ApertureSize::NotObserved`]: the
    /// aperture is the one the window covers, which is what a host test wants
    /// and what the probe falls back to when configuration space cannot be read.
    pub(crate) fn over(
        array: alloc::boxed::Box<dyn PageTable + Send + Sync>,
    ) -> Result<Self, GttError> {
        Self::over_reported(array, ApertureSize::NotObserved)
    }

    /// Take a page table that is already mapped, bounded by what the device
    /// reported.
    ///
    /// The aperture follows from [`ApertureSize`] and never from a constant, so
    /// a shorter window in a test is a smaller aperture rather than a lie about
    /// the hardware, and a `GGMS` field that names 1 GiB bounds every entry
    /// access and every allocation to that 1 GiB even though the window mapped
    /// out of the BAR is 8 MiB long.
    pub(crate) fn over_reported(
        array: alloc::boxed::Box<dyn PageTable + Send + Sync>,
        size: ApertureSize,
    ) -> Result<Self, GttError> {
        let window_entries = array.entries();
        if window_entries == 0 {
            return Err(GttError::WindowTooSmall { bytes: 0 });
        }
        let window = (window_entries as u64)
            .saturating_mul(PAGE_SIZE)
            .min(MAX_APERTURE);
        let aperture = size.aperture_within(window);
        Ok(Self {
            array,
            entries: (aperture / PAGE_SIZE) as usize,
            window_entries,
            aperture,
            size,
            scratch: Pte::encode(zero_page_physical())?,
            next: Mutex::new(aperture.saturating_sub(RESERVED_HIGH_APERTURE)),
        })
    }

    /// Map a Gen12 device's page table array out of its BAR 0.
    ///
    /// `bar0_len` must be the length the device reports for BAR 0, and `size`
    /// must be what [`ApertureSize::read`] made of the same function's
    /// configuration header: this is the only constructor that touches
    /// hardware, and it needs both halves of what the device says about itself.
    #[cfg(target_os = "none")]
    pub(crate) fn map(
        bar0_physical: u64,
        bar0_len: u64,
        size: ApertureSize,
    ) -> Result<Self, GttError> {
        let gtt = Self::over_reported(
            alloc::boxed::Box::new(MappedArray::map_bar(bar0_physical, bar0_len)?),
            size,
        )?;
        // The aperture is the one number in this module that a wrong answer
        // turns into a write outside the page table, and the only one that
        // cannot be recovered from a later read, so it goes into the log at the
        // moment it is decided rather than into a report that may never be read.
        axlog::info!("intel-gtt: {}", gtt.describe());
        Ok(gtt)
    }

    /// Take a page table window over memory that is already mapped.
    ///
    /// The test-side twin of `MappedArray::map_bar`: a test stands ordinary
    /// memory in for the device aperture, and no production path has a reason
    /// to point the page table at memory this driver did not map.  The test
    /// gate keeps that true by construction rather than by inspection.
    ///
    /// The window must be a whole number of entries; a window that is not is an
    /// error rather than a silently shortened aperture.
    ///
    /// # Safety
    ///
    /// As [`MappedArray::over_mapped`].
    #[cfg(test)]
    pub(crate) unsafe fn from_mapped(base: usize, len: usize) -> Result<Self, GttError> {
        if len < PTE_BYTES || !len.is_multiple_of(PTE_BYTES) {
            return Err(GttError::WindowTooSmall { bytes: len });
        }
        let entries = len / PTE_BYTES;
        // SAFETY: forwarded from this function's contract.
        let array = unsafe { MappedArray::over_mapped(base, entries) };
        Self::over(alloc::boxed::Box::new(array))
    }

    /// How many entries of the array are really the page table.
    ///
    /// This is the aperture divided into pages, which is the window's length
    /// only when the device's GGTT size field names the whole window.
    pub(crate) fn entries(&self) -> usize {
        self.entries
    }

    /// How many entries the mapped window holds.
    pub(crate) fn window_entries(&self) -> usize {
        self.window_entries
    }

    /// How many bytes of graphics address space may be handed out.
    pub(crate) fn aperture(&self) -> u64 {
        self.aperture
    }

    /// What the device's configuration header said about the size.
    pub(crate) fn size(&self) -> ApertureSize {
        self.size
    }

    /// The entry the [`SCANOUT_PADDING_ENTRIES`] entries after a run are bound
    /// to: the zero page.
    ///
    /// A caller that wants to check the padding after its own run compares
    /// against this rather than encoding the page itself.
    pub(crate) fn scratch_entry(&self) -> Pte {
        self.scratch
    }

    /// What this page table is, for the boot log and the debug file.
    ///
    /// The numbers a reader has to check are all on the line and each is
    /// labelled, because two of them are easy to confuse: the **aperture** is
    /// the graphics address space this module will hand addresses out of, and
    /// the **window** is the array mapped out of the BAR -- `GGTT_ARRAY_BYTES`
    /// of memory whose entries cover four times as much address space.  A
    /// reader who sees an aperture of 1 GiB next to an 8 MiB array can tell that
    /// the device reported a 2 MiB page table, which is the fact that decides
    /// how much of the window may ever be written; a reader who sees the field
    /// reported as absent knows the aperture was never observed and is the
    /// window's.
    pub(crate) fn describe(&self) -> alloc::string::String {
        use alloc::format;

        format!(
            "GGTT: aperture {aperture:#x} ({entries} entries of {PAGE_SIZE} bytes, the page table \
             the device reports); the mapped window is {array:#x} bytes of array ({window} \
             entries, covering {space:#x} bytes of graphics address space); the first \
             {RESERVED_LOW_APERTURE:#x} and the top {RESERVED_HIGH_APERTURE:#x} of the aperture \
             are reserved, and a run is placed on a {SCANOUT_ALIGNMENT:#x} boundary with \
             {SCANOUT_PADDING_ENTRIES} entries of zero page after it.  {observation}",
            aperture = self.aperture,
            entries = self.entries,
            array = self.window_entries as u64 * PTE_BYTES as u64,
            window = self.window_entries,
            space = self.window_entries as u64 * PAGE_SIZE,
            observation = self.size.describe(),
        )
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
    /// the display engine walks the aperture linearly from that address.  It is
    /// also aligned to [`SCANOUT_ALIGNMENT`], and the
    /// [`SCANOUT_PADDING_ENTRIES`] entries after it are bound to the zero page,
    /// for the VT-d reason those constants give.
    ///
    /// Three properties are deliberate:
    ///
    /// * **A present entry is never overwritten.**  Each entry of a candidate
    ///   block is read before any of it is written, and a block that contains a
    ///   present entry is refused and the search restarts below it.  The
    ///   firmware's live scanout is in this table, and on a machine whose screen
    ///   is its only output, corrupting it costs the only diagnostic the machine
    ///   has.  `[I915]` guards the same thing explicitly in
    ///   `intel_plane_initial.c:217-224`.  The cost is one extra read per
    ///   candidate entry; the alternative is a heuristic about where firmware
    ///   puts things, and `[I915]` records that the heuristic is false on at
    ///   least one platform.  The padding is part of the block, so "the padding
    ///   entries must be free" is the same rule and not a second one.
    /// * **Every entry is read back**, the padding included.  A write to device
    ///   memory can be dropped, and §11 phase 4.3 states what that looks like:
    ///   `PLANE_SURFLIVE` reads zero and nothing else says why.  Failing here,
    ///   with the entry index and both values, is the difference between a
    ///   named error and a black screen.
    /// * **An unmodelled GGTT size allocates nothing.**  The aperture is unknown
    ///   in that case, and the entries a run would be written into might not be
    ///   page table at all; see [`ApertureSize`].
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
        if let ApertureSize::Unmodelled { raw } = self.size {
            return Err(GttError::ApertureSizeUnmodelled { raw });
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
        // hold is bounded by the block rather than by the aperture -- at boot,
        // on one CPU, for a surface of a few thousand pages -- and it is the
        // only way to be sure two callers cannot interleave a reservation with
        // the write that makes it real.
        let mut next = self.next.lock();
        let start = self.reserve_run(&mut next, pages)?;
        for page in 0..pages {
            let index = self.index_of(start + page * PAGE_SIZE)?;
            let pte = Pte::encode(physical + page * PAGE_SIZE)?;
            self.array.write(index, pte.raw());
        }
        for entry in 0..SCANOUT_PADDING_ENTRIES {
            let index = self.index_of(start + (pages + entry) * PAGE_SIZE)?;
            self.array.write(index, self.scratch.raw());
        }
        for entry in 0..pages + SCANOUT_PADDING_ENTRIES {
            let index = self.index_of(start + entry * PAGE_SIZE)?;
            let wrote = if entry < pages {
                Pte::encode(physical + entry * PAGE_SIZE)?.raw()
            } else {
                self.scratch.raw()
            };
            let read = self.array.read(index);
            if read != wrote {
                return Err(GttError::ReadBackMismatch { index, wrote, read });
            }
        }
        Ok(start)
    }

    /// Find a block of `pages` entries plus the padding that follows them, all
    /// absent and starting on a [`SCANOUT_ALIGNMENT`] boundary, counting
    /// downwards from the cursor, and move the cursor below the block.
    ///
    /// The block is what is searched for rather than the run, because the
    /// padding after a run is part of what the allocation occupies: an
    /// allocation whose padding would land on a present entry has to move, and
    /// the alignment is a property of where the run starts.
    fn reserve_run(&self, next: &mut u64, pages: u64) -> Result<u64, GttError> {
        let block = (pages + SCANOUT_PADDING_ENTRIES) * PAGE_SIZE;
        // The top page of the aperture is not allocatable: see
        // [`RESERVED_HIGH_APERTURE`].  The cursor already starts below it, and
        // the `min` is what keeps that true if a caller ever hands in a cursor
        // that does not.
        let ceiling = self.aperture.saturating_sub(RESERVED_HIGH_APERTURE);
        let mut end = (*next).min(ceiling);
        loop {
            // A block ending at `end` would start at `end - block`, rounded
            // down to the alignment, and nothing below [`RESERVED_LOW_APERTURE`]
            // is handed out.  When `end` is below that there is no room left in
            // the aperture, however many free entries below it happen to be.
            if end < RESERVED_LOW_APERTURE + block {
                return Err(GttError::ApertureExhausted {
                    pages,
                    aperture: self.aperture,
                });
            }
            let start = ((end - block) / SCANOUT_ALIGNMENT) * SCANOUT_ALIGNMENT;
            if start < RESERVED_LOW_APERTURE {
                return Err(GttError::ApertureExhausted {
                    pages,
                    aperture: self.aperture,
                });
            }
            // `start` is inside the aperture by the check above, and the block
            // ends at or below `end`, which is at or below the ceiling, so every
            // index the search reads is inside the array.
            let first = (start / PAGE_SIZE) as usize;
            let count = (block / PAGE_SIZE) as usize;
            match (first..first + count).find(|&index| self.array.read(index) & PTE_PRESENT != 0) {
                None => {
                    debug_assert!(start + block <= end);
                    *next = start;
                    return Ok(start);
                }
                // Occupied: the block restarts below this entry, which is what
                // keeps the pages of one surface consecutive in the aperture.
                // `end` strictly decreases and is bounded below, so the search
                // terminates.
                Some(occupied) => end = occupied as u64 * PAGE_SIZE,
            }
        }
    }

    /// The array index a graphics address names.
    ///
    /// The bound is the **aperture**, not the number of entries in the mapped
    /// window: an address the window covers but the device's GGTT size field
    /// does not is outside the table the hardware walks, and writing there is a
    /// write into whatever else that BAR space is.
    fn index_of(&self, ggtt_address: u64) -> Result<usize, GttError> {
        if !ggtt_address.is_multiple_of(PAGE_SIZE) {
            return Err(GttError::AddressNotAligned {
                address: ggtt_address,
            });
        }
        if ggtt_address >= self.aperture {
            return Err(GttError::AddressOutsideAperture {
                address: ggtt_address,
                aperture: self.aperture,
            });
        }
        let index = (ggtt_address / PAGE_SIZE) as usize;
        debug_assert!(index < self.window_entries);
        Ok(index)
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
mod tests;
