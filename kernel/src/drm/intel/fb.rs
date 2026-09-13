//! A framebuffer the display engine can read.
//!
//! Reference §11 phase 3.2 is this module's step: "allocate the framebuffer in
//! memory the display engine can read -- which on an integrated GPU means
//! memory mapped through the **GGTT**.  Write the GGTT PTE for it.  Use a
//! linear, contiguous allocation; make the surface stride a multiple of 64
//! bytes (256 is safest) and the base address 4 KiB-aligned."
//!
//! [`Surface::allocate`] does exactly that, in that order, and hands back both
//! addresses a caller needs: the physical address the kernel writes through and
//! the graphics address a plane's surface register is given.
//!
//! ## Which memory, and why
//!
//! Two kinds of memory are available to this kernel, and the choice is stated
//! rather than implied.
//!
//! * **The kernel's own physical page allocator.**  Chosen.
//! * **The graphics aperture the firmware set aside** (stolen memory, described
//!   by `GGC`/`DSMBASE`).  Not chosen.
//!
//! The aperture is not a separate pool of memory that a scanout buffer must
//! live in; it is a *view* onto system memory through the page table.  On an
//! integrated part the vendor driver scans out of ordinary system memory
//! objects: `intel_fb_pin_to_ggtt()` pins the framebuffer object's pages into
//! the GGTT and migrates them to local memory only on the discrete parts
//! (`display/intel_fb_pin.c:105-174`; the `HAS_LMEM()` branch is the only
//! migration, and the comment above `pinctl` at `:140-149` restricts the
//! *mappable* window to GMCH platforms, which ADL-N is not).  Nothing in the
//! reference or in `[I915]` requires a scanout buffer to be stolen memory, and
//! nothing in this kernel manages the stolen region: taking pages out of it
//! without a stolen-memory manager risks overlapping whatever the firmware left
//! there, including the framebuffer the machine is scanning out at the moment
//! this runs, which is the only console it has.
//!
//! The cost of that choice is stated too: the allocation has to be
//! *physically* contiguous, because the surface is presented to the console as
//! one linear buffer, and a fragmented allocator can refuse it.  The refusal is
//! a named error and not a panic -- a machine whose display cannot come up must
//! still boot and say so -- and the console selection in
//! [`crate::drm::screen`] then falls through to the firmware's aperture, which
//! is why a failure here costs the Intel surface rather than the console.
//!
//! ## Why the CPU view is device-uncached
//!
//! The kernel writes the surface through a device-uncached mapping, not through
//! the cacheable direct map.  Two reasons, in order of weight:
//!
//! * **It is the arrangement this machine has already been observed to
//!   support.**  Stage 1 of this project draws the kernel's log into the
//!   firmware's aperture, and [`crate::pseudofs::dev::bootfb`] maps that
//!   aperture with `axmm::iomap` -- device-uncached -- and those writes reach
//!   the screen on the target machine.  So "CPU stores through an uncached
//!   mapping are visible to the display engine" is not an assumption here; it
//!   is a property of the machine this driver is for.
//! * **It does not depend on cache coherency this kernel cannot test.**  A
//!   cacheable CPU view would rely on the display engine snooping the CPU's
//!   cache.  `[I915]`'s device information does say this generation has an LLC
//!   (`.has_llc = 1` in `GEN7_FEATURES`, `i915_pci.c:316`, inherited through
//!   `GEN8_FEATURES` `:413-420`, `GEN9_FEATURES` `:477-481`, `GEN11_FEATURES`
//!   `:606-611` and `GEN12_FEATURES` `:634-640`), and the vendor driver sets a
//!   scanout object's cache coherency to `I915_CACHE_WT` where the platform has
//!   write-through and `I915_CACHE_NONE` otherwise
//!   (`display/intel_plane_initial.c:184-190`; `HAS_WT` is `HAS_EDRAM`,
//!   `i915_drv.h:659`, which ADL-N does not have).  Uncached writes do not
//!   need any of that to be true.
//!
//! The cost is one memory transaction per access rather than one per cache
//! line, on a console that is written a glyph at a time.  That is the same
//! trade `bootfb` already makes.
//!
//! ## The userspace mapping is cacheable, and nothing reconciles the two views
//!
//! `/dev/fb0` maps the surface as a physical range ([`super::scanout`]'s
//! `DeviceMmap::Physical`), so a userspace writer's view of these pages is
//! cacheable (write-back) while the kernel's view is device-uncached.  **The
//! fbdev ABI's publication points do not reconcile them.**  In this backend
//! `fsync` reaches [`super::scanout`]'s `present`, which accepts and does
//! nothing (there is no submission for a linear aperture), and
//! `FBIOPAN_DISPLAY` reaches `pan`, which refuses, so no flush, fence or
//! register write happens between a userspace store and the display engine's
//! read of the same bytes.
//!
//! The position is therefore the one this module's own argument was trying to
//! avoid, and it is stated rather than implied: **a userspace writer's pixels
//! reach the display engine only if the engine's GGTT reads snoop the CPU's
//! cache.**  `[I915]`'s device information does say this part has an LLC
//! (`.has_llc = 1` in `GEN7_FEATURES`, `i915_pci.c:316`, inherited by
//! `GEN12_FEATURES` at `:634-640`), which is the mechanism that would make it
//! work, but the *kernel's* view is uncached precisely so that it does not
//! depend on that, and the vendor driver's own scanout objects are set to
//! `I915_CACHE_NONE`/`WT` rather than left cacheable
//! (`display/intel_plane_initial.c:184-190`).  Nothing here has measured
//! whether a cacheable userspace store is visible to this display engine; an
//! early eviction may or may not have happened by the time the engine reads.
//! It is on the hardware checklist in `docs/design/intel-scanout.md` §8, and a
//! flush is *not* added for it: a flush this kernel cannot test is a claim,
//! not a fix.
//!
//! ## Stride
//!
//! The stride is rounded up to [`STRIDE_ALIGNMENT`] (256 bytes), which is
//! §11 phase 3.2's "256 is safest", and it is also a multiple of
//! [`STRIDE_UNIT`] (64 bytes) because that is the unit `PLANE_STRIDE` counts a
//! linear surface in: `skl_plane_stride_mult()` returns 64 for a linear buffer
//! and `skl_plane_stride()` divides the byte stride by it
//! (`display/skl_universal_plane.c:671-697`; the read-back multiplies,
//! `:2782-2785`).  Reference §5.4's table says the register is "stride in
//! bytes", which contradicts its own §11 phase 3.2 requirement that the stride
//! be a multiple of 64.
//!
//! **The conversion to register units happens where the register is written**
//! -- `pipe::PlaneProgram`'s `stride_field`, which refuses a stride that is not
//! a multiple of 64 and one that does not fit the twelve-bit field
//! (`super::pipe`).  [`Plan::stride_units_for_log`] is a *log* number and
//! nothing else: it is named so that it cannot be mistaken for the register
//! value, and the test
//! `the_plane_register_gets_the_surfaces_stride_in_sixty_four_byte_units`
//! builds one of these surfaces into a `pipe::PlaneSurface` and checks that the
//! two agree.  `docs/design/intel-scanout.md` records the correction.

use alloc::string::String;

use axalloc::GlobalPage;
use axgpu::PixelLayout;

use super::gtt::{Gtt, GttError, PAGE_SIZE};

/// The largest surface this kernel will allocate.
///
/// A refusal bound, not a request: it is set above the largest plane the
/// display engine is documented to accept (5120x3200, `[PRM]` DG1 Vol 12
/// "Maximum Size") so that no mode the mode layer can select is refused by
/// arithmetic, while a nonsense request is still refused before the allocator
/// is asked for a hundred megabytes of contiguous memory.
pub(crate) const MAX_SURFACE_BYTES: usize = 128 * 1024 * 1024;

/// The alignment a linear surface's stride is rounded up to.
pub(crate) const STRIDE_ALIGNMENT: u32 = 256;

/// The unit `PLANE_STRIDE` counts a linear surface's stride in.
pub(crate) const STRIDE_UNIT: u32 = 64;

/// The widest stride `PLANE_STRIDE` can express: its field is `[11:0]` units.
pub(crate) const MAX_STRIDE: u32 = 4095 * STRIDE_UNIT;

/// How a surface stores one pixel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Format {
    /// 32 bits per pixel, red in bits 23:16, green 15:8, blue 7:0, and eight
    /// bits the display engine reads as padding.
    ///
    /// This is the memory layout DRM calls `XRGB8888`, which on a
    /// little-endian machine is the byte order B, G, R, X.  It is what
    /// `PLANE_CTL` selects as `PLANE_CTL_FORMAT_XRGB_8888` with
    /// `PLANE_CTL_ORDER_RGBX` clear (reference §5.5), and it is the same
    /// [`PixelLayout::B8G8R8A8`] the virtio scanout and the console already
    /// use, so the kernel has one pixel format rather than two.
    Xrgb8888,
}

impl Format {
    /// The channel layout this format is written in.
    pub(crate) const fn layout(self) -> PixelLayout {
        match self {
            Self::Xrgb8888 => PixelLayout::B8G8R8A8,
        }
    }

    /// The name a boot log can carry.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Xrgb8888 => "XRGB8888",
        }
    }
}

/// What a surface of a given size costs, before any memory is asked for.
///
/// Every refusal in this module happens here or in the page table, which is why
/// the arithmetic is a value of its own: it can be checked without a display
/// device, without an allocation and without a page table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Plan {
    width: u32,
    height: u32,
    /// Bytes between the starts of two scan lines, a multiple of
    /// [`STRIDE_ALIGNMENT`].
    stride: u32,
    /// Bytes of memory, rounded up to whole pages.
    size: usize,
    format: Format,
}

impl Plan {
    /// Work out the geometry of a `width` x `height` surface.
    pub(crate) fn of(width: u32, height: u32, format: Format) -> Result<Self, FbError> {
        if width == 0 || height == 0 {
            return Err(FbError::EmptyExtent { width, height });
        }
        let layout = format.layout();
        if !layout.is_drawable() {
            return Err(FbError::FormatNotDrawable { format });
        }
        let minimum = u64::from(width) * u64::from(layout.bytes_per_pixel());
        let stride = minimum.div_ceil(u64::from(STRIDE_ALIGNMENT)) * u64::from(STRIDE_ALIGNMENT);
        if stride > u64::from(MAX_STRIDE) {
            return Err(FbError::StrideTooWide {
                stride,
                limit: MAX_STRIDE,
            });
        }
        let bytes = stride * u64::from(height);
        let size = bytes.div_ceil(PAGE_SIZE) * PAGE_SIZE;
        if size > MAX_SURFACE_BYTES as u64 {
            return Err(FbError::TooLarge {
                bytes: size,
                limit: MAX_SURFACE_BYTES,
            });
        }
        Ok(Self {
            width,
            height,
            stride: stride as u32,
            size: size as usize,
            format,
        })
    }

    /// Visible width in pixels.
    pub(crate) const fn width(self) -> u32 {
        self.width
    }

    /// Visible height in pixels.
    pub(crate) const fn height(self) -> u32 {
        self.height
    }

    /// Bytes between the starts of two scan lines.
    pub(crate) const fn stride(self) -> u32 {
        self.stride
    }

    /// The stride in 64-byte units, for a log line only.
    ///
    /// This is **not** the value written to `PLANE_STRIDE`: the register's own
    /// conversion lives where the register is written (`super::pipe`, which is
    /// the module that refuses a stride that is not a multiple of 64 and one
    /// that does not fit the field).  This one can refuse nothing and nothing
    /// acts on it, which is why it is named for the log rather than for the
    /// register.
    pub(crate) const fn stride_units_for_log(self) -> u32 {
        self.stride / STRIDE_UNIT
    }

    /// Bytes of memory the surface occupies.
    pub(crate) const fn size(self) -> usize {
        self.size
    }

    /// How the surface stores one pixel.
    pub(crate) const fn format(self) -> Format {
        self.format
    }
}

/// Why a surface could not be allocated.
///
/// Every variant is a refusal and none of them is a panic: the caller is the
/// console's scanout selection, and on a machine whose only output is the
/// screen, a driver that panics while preparing a framebuffer takes the
/// diagnostic with it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FbError {
    /// A zero width or height.
    EmptyExtent { width: u32, height: u32 },
    /// The format's channel layout is not one the console can write.
    FormatNotDrawable { format: Format },
    /// The stride does not fit `PLANE_STRIDE`.
    StrideTooWide { stride: u64, limit: u32 },
    /// The surface is larger than this kernel will allocate.
    TooLarge { bytes: u64, limit: usize },
    /// The physical page allocator could not supply a contiguous run.
    OutOfMemory { bytes: usize, pages: usize },
    /// A physical address no pointer can name on this machine.
    Unaddressable { physical: u64 },
    /// A byte range that does not lie inside the surface.
    OffsetOutsideSurface { offset: usize, len: usize },
    /// The allocation could not be mapped for the CPU to write into.
    Unmappable { physical: u64, size: usize },
    /// The page table refused the run.
    Gtt(GttError),
}

impl From<GttError> for FbError {
    fn from(error: GttError) -> Self {
        Self::Gtt(error)
    }
}

impl FbError {
    /// A sentence a boot log can carry.
    pub(crate) fn describe(&self) -> String {
        use alloc::format;

        match self {
            Self::EmptyExtent { width, height } => format!(
                "a {width}x{height} surface was asked for, and a surface with no pixels has no \
                 scan lines to lay out"
            ),
            Self::FormatNotDrawable { format } => format!(
                "{} describes a pixel the console cannot write, so it would paint a colour that \
                 does not match what was asked for",
                format.name()
            ),
            Self::StrideTooWide { stride, limit } => format!(
                "the stride {stride} bytes does not fit PLANE_STRIDE, whose 12-bit field counts \
                 64-byte units and so stops at {limit} bytes (reference section 5.4 gives the \
                 field, and [I915] display/skl_universal_plane.c:671-697 gives the unit)"
            ),
            Self::TooLarge { bytes, limit } => format!(
                "the surface is {bytes} bytes, above the {limit} bytes this kernel will allocate \
                 in one contiguous run"
            ),
            Self::OutOfMemory { bytes, pages } => format!(
                "the page allocator could not supply {pages} physically contiguous pages ({bytes} \
                 bytes) for the framebuffer"
            ),
            Self::Unaddressable { physical } => {
                format!("physical address {physical:#x} cannot be named by a pointer here")
            }
            Self::OffsetOutsideSurface { offset, len } => format!(
                "a {len}-byte access at offset {offset} does not lie inside the surface, and a \
                 console write must not be able to reach past it"
            ),
            Self::Unmappable { physical, size } => format!(
                "the {size}-byte framebuffer at {physical:#x} could not be mapped for the CPU to \
                 draw into"
            ),
            Self::Gtt(error) => error.describe(),
        }
    }
}

/// A surface of memory the display engine can read.
///
/// The value owns its memory: dropping it returns the pages to the allocator.
/// **Nothing in this kernel drops a surface once its page table entries exist**,
/// and that is deliberate rather than incidental.  Two things forbid freeing
/// one: the GGTT entries naming its pages stay present for the rest of the
/// boot, because [`Gtt`] has no unmap path (see its module comment), and the
/// display engine may still be reading the address those entries name.  Pages
/// handed back to the allocator under a present entry would be scanned out as
/// whatever the next owner puts there, and pages handed back under a live
/// *mapping* would be worse still -- see [`Surface::allocate`]'s failure path.
/// [`super::scanout`] keeps every surface it has offered, including the ones a
/// later offer replaced.
pub(crate) struct Surface {
    /// The allocation that owns the memory.  Held whether or not anything
    /// reads it, because dropping it is what returns the pages.
    memory: GlobalPage,
    /// The kernel's writable view of the first byte, device-uncached.
    cpu: usize,
    /// The physical base the page table entries name.
    physical: u64,
    /// The graphics address a plane's surface register is given.
    ggtt: u64,
    plan: Plan,
}

/// The kernel's view of `size` bytes of physical memory, mapped so that a write
/// reaches the display engine without depending on cache coherency.
#[cfg(target_os = "none")]
fn cpu_view(physical: u64, size: usize) -> Result<usize, FbError> {
    let address = usize::try_from(physical).map_err(|_| FbError::Unaddressable { physical })?;
    // Device-uncached, and this is also what makes the *direct map* of these
    // pages device-uncached: `axmm::iomap` maps at the direct-map address with
    // device flags, so there is one mapping and not two views with different
    // memory types.
    let mapped = axmm::iomap(axhal::mem::PhysAddr::from_usize(address), size)
        .map_err(|_| FbError::Unmappable { physical, size })?;
    Ok(mapped.as_usize())
}

/// In a host test there is no kernel address space to map into.  The dummy
/// platform's direct map is the identity over its page arena, so the pages are
/// addressed where they lie; the geometry, the page table and every refusal are
/// the production code, and the mapping is not.
#[cfg(not(target_os = "none"))]
fn cpu_view(physical: u64, _size: usize) -> Result<usize, FbError> {
    let address = usize::try_from(physical).map_err(|_| FbError::Unaddressable { physical })?;
    Ok(axhal::mem::phys_to_virt(axhal::mem::PhysAddr::from_usize(address)).as_usize())
}

impl Surface {
    /// Allocate a `width` x `height` surface and map it into the aperture.
    ///
    /// `gtt` is a parameter rather than a global because the surface's whole
    /// purpose is to be reachable at a graphics address, and hiding where that
    /// address comes from would hide the one step §11 phase 3.2 is about.
    ///
    /// The order is deliberate.  The geometry is checked first, so a mode that
    /// cannot be laid out is refused before any memory is taken.  The memory is
    /// then allocated and cleared, so the first frame is black rather than
    /// whatever the previous owner of those pages left there.  Only then are
    /// the page table entries written, because an entry naming a page is a
    /// promise that the page holds a framebuffer.
    pub(crate) fn allocate(
        gtt: &Gtt,
        width: u32,
        height: u32,
        format: Format,
    ) -> Result<Self, FbError> {
        let plan = Plan::of(width, height, format)?;
        let page = PAGE_SIZE as usize;
        let pages = plan.size() / page;
        let memory =
            GlobalPage::alloc_contiguous(pages, page).map_err(|_| FbError::OutOfMemory {
                bytes: plan.size(),
                pages,
            })?;
        let physical = axhal::mem::virt_to_phys(memory.start_vaddr()).as_usize() as u64;
        let cpu = cpu_view(physical, plan.size())?;
        // SAFETY: `cpu` addresses `plan.size()` bytes of memory this function
        // just allocated and mapped for writing, and nothing else refers to it
        // yet.  The write goes through the device-uncached mapping, which is
        // the only mapping of these pages the kernel uses.
        unsafe { core::ptr::write_bytes(cpu as *mut u8, 0, plan.size()) };
        let ggtt = match gtt.map_linear(physical, plan.size()) {
            Ok(ggtt) => ggtt,
            Err(error) => {
                // The device-uncached mapping of these pages belongs to the
                // kernel address space and outlives the allocation, so the
                // pages are deliberately not returned to the allocator here: a
                // live mapping of memory that has been handed to someone else
                // is worse than a few leaked pages on a path that runs at most
                // once per boot.  The leak is the reason and not a side effect
                // -- the pages cannot be freed while that mapping names them,
                // and it cannot be unmapped from here.
                core::mem::forget(memory);
                return Err(FbError::Gtt(error));
            }
        };
        Ok(Self {
            memory,
            cpu,
            physical,
            ggtt,
            plan,
        })
    }

    /// The graphics address a plane's surface register is given.
    pub(crate) fn ggtt_address(&self) -> u64 {
        self.ggtt
    }

    /// The physical address the display engine's reads end up at, which is
    /// what a GGTT entry names.
    pub(crate) fn physical_address(&self) -> u64 {
        self.physical
    }

    /// Bytes between the starts of two scan lines.
    pub(crate) fn stride(&self) -> u32 {
        self.plan.stride()
    }

    /// The stride in 64-byte units, for a log line only; see
    /// [`Plan::stride_units_for_log`].
    pub(crate) fn stride_units_for_log(&self) -> u32 {
        self.plan.stride_units_for_log()
    }

    /// Visible width in pixels.
    pub(crate) fn width(&self) -> u32 {
        self.plan.width()
    }

    /// Visible height in pixels.
    pub(crate) fn height(&self) -> u32 {
        self.plan.height()
    }

    /// Bytes of memory the surface occupies.
    pub(crate) fn len(&self) -> usize {
        self.plan.size()
    }

    /// How the surface stores one pixel.
    pub(crate) fn layout(&self) -> PixelLayout {
        self.plan.format().layout()
    }

    /// The plan this surface was laid out with.
    pub(crate) fn plan(&self) -> Plan {
        self.plan
    }

    /// Copy `dst.len()` bytes out of the surface, starting `offset` bytes in.
    ///
    /// A range that does not lie inside the surface is refused rather than
    /// clamped: a caller that believes it read a pixel it did not is a caller
    /// that will draw the wrong thing.
    pub(crate) fn read_bytes(&self, offset: usize, dst: &mut [u8]) -> Result<(), FbError> {
        let range = self.range(offset, dst.len())?;
        // SAFETY: the range was just checked to lie inside the surface, and the
        // surface's memory stays mapped for as long as this value exists.  The
        // source is device memory and the destination is ordinary memory, so
        // they cannot overlap.
        unsafe {
            core::ptr::copy_nonoverlapping(
                (self.cpu + range.start) as *const u8,
                dst.as_mut_ptr(),
                dst.len(),
            )
        };
        Ok(())
    }

    /// Copy `src` into the surface, starting `offset` bytes in.
    pub(crate) fn write_bytes(&self, offset: usize, src: &[u8]) -> Result<(), FbError> {
        let range = self.range(offset, src.len())?;
        // SAFETY: as for `read_bytes`, with the roles of the two regions
        // exchanged.
        unsafe {
            core::ptr::copy_nonoverlapping(
                src.as_ptr(),
                (self.cpu + range.start) as *mut u8,
                src.len(),
            )
        };
        Ok(())
    }

    /// The byte range `offset..offset + len`, or an error if it leaves the
    /// surface.
    fn range(&self, offset: usize, len: usize) -> Result<core::ops::Range<usize>, FbError> {
        let end = offset
            .checked_add(len)
            .ok_or(FbError::OffsetOutsideSurface { offset, len })?;
        if end > self.plan.size() {
            return Err(FbError::OffsetOutsideSurface { offset, len });
        }
        Ok(offset..end)
    }
}

// The numbers a reader is most likely to check by hand are asserted in
// `a_1080p_surface_lays_out_at_the_documented_size`: 1920x1080 at 32 bits per
// pixel is 8 294 400 bytes of pixels, which is already a whole number of
// 256-byte strides (7680 bytes) and of 4 KiB pages (2025), so the surface laid
// out for it is exactly as large as its pixels are.  The brief that
// commissioned this module gives 8 291 520 for the same surface; that is 2880
// bytes short of `1920 * 1080 * 4` and is not the size of anything.

#[cfg(test)]
mod tests {
    use super::{
        super::{
            gtt::{SCANOUT_ALIGNMENT, SCANOUT_PADDING_ENTRIES, mock::MockPageTable},
            pipe::{self, Pipe},
        },
        *,
    };
    use crate::drm::modes::CTA_VIC_TIMINGS;

    /// A page table with room for the surfaces these tests allocate *and* for
    /// the padding and 256 KiB alignment every run now needs: a 1920x1080
    /// surface is 2025 pages, its block is 2089 entries, and the search needs a
    /// 256 KiB boundary below the reserved top page for it.
    const ROOM_FOR_A_1080P_SURFACE: usize = 4096;

    /// A page table big enough for any surface a test allocates here.
    fn test_gtt(entries: usize) -> Gtt {
        Gtt::over(alloc::boxed::Box::new(MockPageTable::new(entries))).unwrap()
    }

    /// The mode §11 phase 3.1 names for a first light-up: CTA-861 VIC 16,
    /// 1920x1080@60.
    fn vic16() -> crate::drm::modes::Mode {
        CTA_VIC_TIMINGS
            .iter()
            .find(|entry| entry.vic == 16)
            .expect("VIC 16 is in the kernel's CTA table")
            .mode
    }

    #[test]
    fn a_1080p_surface_lays_out_at_the_documented_size() {
        let _guard = crate::test_support::scheduler_test_context();
        let plan = Plan::of(1920, 1080, Format::Xrgb8888).unwrap();
        assert_eq!(plan.stride(), 7680);
        assert_eq!(plan.stride_units_for_log(), 120);
        // 2025 whole pages, and the last of them is not shared with anything.
        assert_eq!(plan.size(), 8_294_400);
        assert_eq!(plan.size(), 1920 * 1080 * 4);
        assert_eq!(plan.size() / PAGE_SIZE as usize, 2025);
        assert_eq!(plan.size() % PAGE_SIZE as usize, 0);
        // The stride is a multiple of 256, which is what makes it a multiple of
        // the 64-byte unit the register counts in.
        assert_eq!(plan.stride() % STRIDE_ALIGNMENT, 0);
        assert_eq!(plan.stride() % STRIDE_UNIT, 0);
    }

    #[test]
    fn a_stride_that_needs_padding_is_padded_to_the_alignment() {
        let _guard = crate::test_support::scheduler_test_context();
        // 1000 pixels at 32 bits is 4000 bytes, which is not a multiple of 256.
        let plan = Plan::of(1000, 100, Format::Xrgb8888).unwrap();
        assert_eq!(plan.stride(), 4096);
        assert_eq!(plan.stride_units_for_log(), 64);
        assert_eq!(plan.size(), 409_600);
    }

    #[test]
    fn a_surface_larger_than_a_plane_can_name_is_refused() {
        let _guard = crate::test_support::scheduler_test_context();
        // PLANE_STRIDE's 12-bit field counts 64-byte units, so the widest
        // expressible stride is 4095 * 64 = 262 080 bytes.
        assert_eq!(MAX_STRIDE, 262_080);
        let error = Plan::of(70_000, 1, Format::Xrgb8888).unwrap_err();
        match error {
            FbError::StrideTooWide { stride, limit } => {
                assert_eq!(stride, 280_064);
                assert_eq!(limit, MAX_STRIDE);
            }
            other => panic!("expected a stride refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_surface_larger_than_the_allocator_will_take_is_refused() {
        let _guard = crate::test_support::scheduler_test_context();
        // 4096x9000 at 32 bits is about 147 MiB, above the refusal bound.
        assert_eq!(
            Plan::of(4096, 9000, Format::Xrgb8888),
            Err(FbError::TooLarge {
                bytes: 4096 * 4 * 9000,
                limit: MAX_SURFACE_BYTES,
            })
        );
    }

    #[test]
    fn an_empty_extent_is_refused() {
        let _guard = crate::test_support::scheduler_test_context();
        assert_eq!(
            Plan::of(0, 1080, Format::Xrgb8888),
            Err(FbError::EmptyExtent {
                width: 0,
                height: 1080
            })
        );
        assert_eq!(
            Plan::of(1920, 0, Format::Xrgb8888),
            Err(FbError::EmptyExtent {
                width: 1920,
                height: 0
            })
        );
    }

    #[test]
    fn every_refusal_names_itself() {
        let _guard = crate::test_support::scheduler_test_context();
        let cases = [
            FbError::EmptyExtent {
                width: 0,
                height: 0,
            },
            FbError::FormatNotDrawable {
                format: Format::Xrgb8888,
            },
            FbError::StrideTooWide {
                stride: 1 << 20,
                limit: MAX_STRIDE,
            },
            FbError::TooLarge {
                bytes: 1 << 40,
                limit: MAX_SURFACE_BYTES,
            },
            FbError::OutOfMemory {
                bytes: 1 << 30,
                pages: 1 << 18,
            },
            FbError::Unaddressable { physical: 1 << 50 },
            FbError::OffsetOutsideSurface {
                offset: 4096,
                len: 8,
            },
            FbError::Unmappable {
                physical: 1 << 20,
                size: 4096,
            },
            FbError::Gtt(GttError::EmptyRun),
        ];
        for error in cases {
            assert!(
                !error.describe().is_empty(),
                "{error:?} must describe itself"
            );
        }
    }

    #[test]
    fn an_allocated_surface_is_aligned_present_and_black() {
        let _guard = crate::test_support::scheduler_test_context();
        let gtt = test_gtt(ROOM_FOR_A_1080P_SURFACE);
        let surface = Surface::allocate(&gtt, 64, 64, Format::Xrgb8888).unwrap();
        // 64 pixels at 32 bits is already a multiple of 256.
        assert_eq!(surface.stride(), 256);
        assert_eq!(surface.stride_units_for_log(), 4);
        assert_eq!(surface.len(), 256 * 64);
        // The base is page aligned, which is what PLANE_SURF[31:12] requires
        // and what the page table entry can name at all, and it is on the
        // 256 KiB boundary and followed by the padding entries the VT-d
        // workaround wants (see `gtt::SCANOUT_ALIGNMENT`).
        assert_eq!(surface.physical_address() % PAGE_SIZE, 0);
        assert_eq!(surface.ggtt_address() % PAGE_SIZE, 0);
        assert_eq!(surface.ggtt_address() % SCANOUT_ALIGNMENT, 0);
        // Every page of the run has a present entry naming the right page.
        let pages = surface.len() as u64 / PAGE_SIZE;
        for page in 0..pages {
            let entry = gtt
                .entry(surface.ggtt_address() + page * PAGE_SIZE)
                .unwrap();
            assert!(entry.is_present());
            assert!(!entry.is_local_memory());
            assert!(entry.describes(surface.physical_address() + page * PAGE_SIZE));
        }
        // The console can write into it, and it starts black.
        surface.write_bytes(0, &[0xaa; 16]).unwrap();
        let mut read = [0u8; 16];
        surface.read_bytes(0, &mut read).unwrap();
        assert_eq!(read, [0xaa; 16]);
        let mut tail = [0xffu8; 8];
        surface.read_bytes(surface.len() - 8, &mut tail).unwrap();
        assert_eq!(tail, [0u8; 8], "the surface starts black");
    }

    #[test]
    fn the_surface_addresses_every_scan_line_it_claims() {
        // The console's own check (drm::screen::usable) is that the surface
        // addresses pitch * virtual_height bytes.  That has to hold whatever
        // the geometry, because the allocation is rounded up to whole pages and
        // the padding is what makes it hold.
        let gtt = test_gtt(ROOM_FOR_A_1080P_SURFACE);
        // 100 pixels at 32 bits is 400 bytes, padded to a 512-byte stride, and
        // 100 scan lines of that is 51 200 bytes: not a whole number of pages.
        let padded = Surface::allocate(&gtt, 100, 100, Format::Xrgb8888).unwrap();
        assert_eq!(padded.stride(), 512);
        assert_eq!(padded.plan().size(), 53_248);
        assert!(padded.plan().size() as u64 > u64::from(padded.stride()) * 100);
        assert!(padded.len() as u64 >= u64::from(padded.stride()) * u64::from(padded.height()));
        // 100 by 64 closes exactly: 512 * 64 is eight whole pages.
        let exact = Surface::allocate(&gtt, 100, 64, Format::Xrgb8888).unwrap();
        assert_eq!(exact.stride(), 512);
        assert_eq!(exact.plan().size(), 32_768);
        assert_eq!(
            u64::from(exact.stride()) * u64::from(exact.height()),
            exact.plan().size() as u64
        );
    }

    #[test]
    fn a_byte_range_outside_the_surface_is_refused() {
        let _guard = crate::test_support::scheduler_test_context();
        let gtt = test_gtt(ROOM_FOR_A_1080P_SURFACE);
        let surface = Surface::allocate(&gtt, 64, 64, Format::Xrgb8888).unwrap();
        let len = surface.len();
        let mut dst = [0u8; 8];
        assert!(surface.read_bytes(len - 8, &mut dst).is_ok());
        assert!(surface.read_bytes(len - 4, &mut dst).is_err());
        assert!(surface.read_bytes(len, &mut []).is_ok());
        assert!(surface.write_bytes(len, &[0]).is_err());
        // A huge offset must not wrap into the surface.
        assert!(surface.write_bytes(usize::MAX - 1, &[0; 4]).is_err());
    }

    #[test]
    fn an_allocation_the_allocator_cannot_satisfy_is_a_named_error() {
        let _guard = crate::test_support::scheduler_test_context();
        // A request the plan accepts and the host page arena cannot: 64 MiB of
        // contiguous memory against a 32 MiB arena.  On the target the same
        // path is what a fragmented boot would produce, and it has to be an
        // error rather than a panic or a short allocation.
        let gtt = test_gtt(1 << 18);
        let error = Surface::allocate(&gtt, 4096, 4096, Format::Xrgb8888)
            .err()
            .expect("64 MiB of contiguous memory cannot come out of the host arena");
        match error {
            FbError::OutOfMemory { bytes, pages } => {
                assert_eq!(bytes, 4096 * 4 * 4096);
                assert_eq!(pages, bytes / PAGE_SIZE as usize);
            }
            other => panic!("expected an allocation failure, got {other:?}"),
        }
    }

    #[test]
    fn the_plane_register_gets_the_surfaces_stride_in_sixty_four_byte_units() {
        // The handoff the two modules never had a test for: a real surface from
        // this module, built into the `PlaneSurface` the pipe module programs,
        // and the value it writes to `PLANE_STRIDE` -- taken from the program's
        // own write list, so it is the number a register receives and not a
        // second computation of it.
        let _guard = crate::test_support::scheduler_test_context();
        let gtt = test_gtt(ROOM_FOR_A_1080P_SURFACE);
        let surface = Surface::allocate(&gtt, 1920, 1080, Format::Xrgb8888).unwrap();
        assert_eq!(surface.stride(), 7680);
        let program = pipe::compute(
            Pipe::A,
            &vic16(),
            pipe::PlaneSurface {
                ggtt_address: surface.ggtt_address(),
                stride_bytes: surface.stride(),
            },
        )
        .unwrap();
        // `PLANE_STRIDE` counts 64-byte units for a linear surface
        // (display/skl_universal_plane.c:671-697), so the register value is the
        // byte stride divided by 64 and nothing else.
        assert_eq!(program.plane.stride, surface.stride() / STRIDE_UNIT);
        assert_eq!(program.plane.stride, 120);
        assert_eq!(program.plane.stride_bytes, surface.stride());
        let write = program
            .writes(0, 0)
            .into_iter()
            .find(|write| write.register == Pipe::A.plane_stride())
            .expect("the pipe programs PLANE_STRIDE");
        assert_eq!(write.value, surface.stride() / STRIDE_UNIT);
        // The log-only number is the same number here, and it is a log-only
        // number: see `Plan::stride_units_for_log`.
        assert_eq!(write.value, surface.stride_units_for_log());
    }

    #[test]
    fn the_padding_after_a_surface_is_bound_to_the_zero_page() {
        // The entries after the surface are present and name a page of zeros,
        // not the surface: that is what the VT-d workaround asks for, and it
        // must not be the surface's own memory, which the engine would read
        // past the end of the picture.
        let _guard = crate::test_support::scheduler_test_context();
        let gtt = test_gtt(ROOM_FOR_A_1080P_SURFACE);
        let surface = Surface::allocate(&gtt, 1920, 1080, Format::Xrgb8888).unwrap();
        let scratch = gtt.scratch_entry();
        let pages = surface.len() as u64 / PAGE_SIZE;
        for entry in pages..pages + SCANOUT_PADDING_ENTRIES {
            let pte = gtt
                .entry(surface.ggtt_address() + entry * PAGE_SIZE)
                .unwrap();
            assert_eq!(pte, scratch, "padding entry {entry}");
            assert!(!pte.describes(surface.physical_address()));
        }
    }

    #[test]
    fn a_page_table_that_refuses_the_run_fails_the_allocation() {
        let _guard = crate::test_support::scheduler_test_context();
        // One entry: a two-page surface cannot be mapped into it.
        let gtt = test_gtt(1);
        let error = Surface::allocate(&gtt, 64, 64, Format::Xrgb8888)
            .err()
            .expect("a one-page aperture cannot hold a four-page surface");
        match error {
            FbError::Gtt(GttError::ApertureExhausted { pages, aperture }) => {
                assert_eq!(pages, 256 * 64 / PAGE_SIZE);
                assert_eq!(aperture, PAGE_SIZE);
            }
            other => panic!("expected the page table to refuse, got {other:?}"),
        }
    }
}
