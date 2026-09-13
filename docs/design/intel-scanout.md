# Intel scanout memory: the GGTT, a framebuffer, and the console's surface

**Status: none of this has run on the target machine.**  The target is an Acer
蜂鸟mini (SQM2270) with an i3-N305 (`8086:46d0`, Gen12 / Xe-LP) whose only
output is the screen, which today is still driven by the firmware's
framebuffer.  Everything below is a host test, a reading of
`docs/design/intel-display-registers.md` (the *reference document*), or a
reading of the source the reference cites.  No page table entry in this
workstream has been written to a real Intel GPU, no framebuffer has been
allocated from real memory for one, and no pixel has been scanned out of
anything this code produced.

This document covers one workstream: **reference §11 phase 3.2**, "allocate the
framebuffer in memory the display engine can read, which on an integrated GPU
means memory mapped through the GGTT".  Phases 4 and 5 (the pipe, the plane,
the DDB, the watermarks, the DDI) are other workstreams; §11 phase 6 is the
evidence they produce, and §5.1 below is where this module consumes it.

Provenance markers follow the reference document's §0.1: `[I915]` is the Linux
v6.12 `drm/i915` tree at commit `adc218676eef25575469234709c2d87185ca223a`,
`[PRM]` is Intel's DG1 (Xe-LP) display PRM, `[INF]` is inference from sourced
facts, and `[GAP]` is something no source in reach states.  Facts are cited by
`file:line`; no GPL source is copied, only read for facts.

## 1. What was implemented, and where

| file | what it owns |
|---|---|
| `kernel/src/drm/intel/gtt.rs` | the GGTT page table: the entry layout, **the aperture the device reports** (`ApertureSize`, read through `pci::ConfigSpace`), the reserved page at each end, `map_linear` (a run of entries for a contiguous physical range, aligned and padded), the read-back check, and the allocator |
| `kernel/src/drm/intel/fb.rs` | `Plan` (geometry, stride, size and every refusal), `Surface::allocate`, and the kernel's writable view of the memory |
| `kernel/src/drm/intel/scanout.rs` | `ScanoutSurface` over `fb::Surface`, and the `screen::Candidate` that offers it to the console |
| `kernel/src/drm/intel/pci.rs` | the configuration-space side: `offset::GMCH_CTL` and the `GGMS` field's shift and mask, next to every other header field the probe reads |

The call sequence a coordinator wires is:

```rust
// once, after the probe found a device and knows its BAR 0 and its function:
let aperture = intel::gtt::ApertureSize::read(&config, bdf);
let gtt = intel::gtt::Gtt::map(bar0_physical, bar0_len, aperture)?;
// per mode, after the mode layer chose one:
let surface = Arc::new(intel::fb::Surface::allocate(&gtt, width, height, fb::Format::Xrgb8888)?);
// after the modeset has programmed PLANE_SURF = surface.ggtt_address() and read
// the phase 6 evidence back:
intel::scanout::register(surface, scanout::Verdict::Scanning { surflive });
```

`Gtt::map` logs `Gtt::describe()` -- the observed field, the aperture it names and
the window that was mapped -- at the moment it decides, and the same string is
what a debug file should carry (it is `pub(crate)` and one line long).

**None of it is called from the boot path.**  Nothing in this kernel allocates a
framebuffer, writes a page table entry, or registers a console candidate at
boot, so the stage-1 claim that the kernel leaves the firmware's display alone
is still true.  The call site is the coordinator's, and §5.1 says exactly when
it becomes safe to call the last line.

## 2. The GGTT

### 2.1 What it is, and where the page table is

A display engine does not read a physical address: it reads a *graphics*
address, and the global graphics translation table turns one into the other.
One 8-byte entry per 4 KiB page; the plane's `PLANE_SURF` holds the address the
engine walks from, and the engine reads a linear surface by walking consecutive
entries.

The table itself is memory, not registers — `[I915]` says so in as many words:
"Global gtt pte registers are special registers which actually forward writes
to a chunk of system memory" (`display/intel_fb_pin.c:131-137`).  There are two
ways to reach it, and this module takes the first:

| route | address | source |
|---|---|---|
| the BAR 0 window | `BAR0 + 0x0080_0000`, 8 MiB | reference §1.1 and §3.2; `[I915]` `gt/intel_ggtt.c:1134-1147` (`gttadr_offset = gttmmadr_size / 2`, and the BAR is 16 MiB from Gen8) |
| the physical base | `GSMBASE` (`0x108100`, bits `[63:20]`) | reference §1.1; `[I915]` `gt/intel_ggtt.c:1160-1163` uses it only where the BAR window is not reachable (`i915_direct_stolen_access()`) |

`regs::PROBE_WINDOW` maps the first 2 MiB of the same BAR — the register window,
the `[I915]` under-map reference §3.2 documents — and deliberately stops short
of the page table, so `gtt::MappedArray` maps the second half itself, exactly as
`ggtt_probe_common()` does (`[I915]` `gt/intel_ggtt.c:1149-1185`).

**A BAR too short to hold the array is refused, not mapped.**  Reference §3.2
warns that a from-scratch driver must check the observed BAR length rather than
assume one, and mapping past the end of a BAR would map an unrelated physical
range and write page table entries into it.  `GttError::BarTooSmall` is that
refusal, and it is the load-bearing safety property of the module.

The mapping is uncached.  That is what `[I915]` wants here: from Gen11 on it
uses a plain (uncached) mapping rather than a write-combining one, because a
write-combining mapping of this range *drops* writes larger than 64 bits
(`gt/intel_ggtt.c:197-208` and the comment there).  Entries are written one at a
time, 64 bits at a time, which is the form that comment says is safe.

### 2.2 The entry, bit by bit

**The reference document does not state the GGTT page table entry layout
anywhere.**  §1.1 locates the array and `GSMBASE`, `regs/mod.rs` declares
`GSMBASE` as the page table base, and §5.4 gives `PLANE_SURF`'s fields — but no
section says which bits of an *entry* mean what.  This is the one research task
the brief left to the module, and it was answered from `[I915]`:

| bit(s) | meaning | source |
|---|---|---|
| 0 | present | `GEN8_PAGE_PRESENT`, `gt/intel_gtt.h:152` |
| 1 | local memory on Gen12 (`GEN12_GGTT_PTE_LM`); read/write on older parts (`GEN8_PAGE_RW`) | `gt/intel_gtt.h:97`, `:153` |
| 12..=45 | physical page address | `GEN12_GGTT_PTE_ADDR_MASK = GENMASK_ULL(45, 12)`, `gt/intel_gtt.h:100` |
| 2..=11, 46..=63 | clear for a page of system memory | inferred from the encoder below |

The encoder that decides them for this part is three lines long:

```c
/* [I915] gt/intel_ggtt.c:277-286 */
u64 gen8_ggtt_pte_encode(dma_addr_t addr, unsigned int pat_index, u32 flags)
{
        gen8_pte_t pte = addr | GEN8_PAGE_PRESENT;
        if (flags & PTE_LM)
                pte |= GEN12_GGTT_PTE_LM;
        return pte;
}
```

Three consequences, each of which is a way to get a black screen with no
diagnostic:

* **Bit 1 must stay clear.**  On this generation bit 1 is `GEN12_GGTT_PTE_LM`,
  set only for the discrete parts' local memory — not the read/write bit that
  the older `gen8_pte_encode` set, which is what a reader who has seen Gen9 code
  expects.  A PTE marking a page of system memory as local memory is precisely
  the class of mistake reference §11 phase 3.2 warns about.  `Pte::is_local_memory`
  reports the bit rather than masking it, so a wrong entry is visible in a log.
* **No cache-policy bits are set.**  The Gen12 GGTT encoder ignores the PAT
  index it is handed.  Earlier generations did encode a cache policy
  (`PPAT_UNCACHED`/`PPAT_CACHED`, `gt/intel_gtt.h:135-138`), which is why those
  bits are expected by anyone who has read that code.  The practical
  consequence: **this driver cannot ask for uncached GPU reads through the
  entry**, so the CPU-side mapping (§4.3) is what carries that decision.
* **`addr` is not masked by the encoder.**  It relies on the caller passing an
  address inside `GEN12_GGTT_PTE_ADDR_MASK`; `Pte::encode` checks it and refuses
  a wider address rather than truncating it to a different page.

### 2.3 The aperture, and the size the device reports

**The BAR does not say how big the aperture is, and neither does the array this
module maps.**  `[I915]` asserts that BAR 0 is exactly 16 MiB on this generation
-- `GEM_WARN_ON(pci_resource_len(pdev, GEN4_GTTMMADR_BAR) !=
gen6_gttmmadr_size(i915))`, `gt/intel_ggtt.c:1158` -- and takes the aperture from
the `GGMS` field of the device's own configuration header:

| what | where | source |
|---|---|---|
| the field | PCI configuration space offset `0x50`, bits `[7:6]` | `SNB_GMCH_CTRL 0x50`, `include/drm/intel/i915_drm.h:49`; `BDW_GMCH_GGMS_SHIFT 6` / `BDW_GMCH_GGMS_MASK 0x3`, `:54-55` |
| the read | `pci_read_config_word(pdev, SNB_GMCH_CTRL, &snb_gmch_ctl)` | `gt/intel_ggtt.c:1228` (in `gen8_gmch_probe`, which `ggtt_probe_hw` selects for `GRAPHICS_VER(i915) >= 8`, `:1459`) |
| the encoding | shift, mask, `1 << field` MiB of page table, zero for zero | `gen8_get_total_gtt_size()`, `gt/intel_ggtt.c:1107-1121` |
| the arithmetic | `ggtt->vm.total = (size / sizeof(gen8_pte_t)) * I915_GTT_PAGE_SIZE` | `gt/intel_ggtt.c:1238` |

One 8-byte entry per 4 KiB page, so the three values the two-bit field can name
are **2, 4 and 8 MiB of page table, and a 1, 2 or 4 GiB aperture**.

The difference matters because the array window is not the page table.  This
module maps the whole second half of the BAR (8 MiB), but the table the hardware
walks is only the `GGMS`-sized prefix of it: with a 2 MiB field, everything from
offset 2 MiB to 8 MiB of the window is BAR space that is not a page table, and an
entry written there is a write into something else.  So `Gtt::aperture()` is
`ApertureSize`'s answer, `Gtt::entries()` is that aperture in pages rather than
the window's length, and `index_of` -- the bound behind both `Gtt::entry` and the
allocator's writes -- refuses anything at or above it with
`GttError::AddressOutsideAperture { address, aperture }`, which now names the
bound it actually used.

`[I915]`'s own comment "if the size of the GGTT is 4G" (`gt/intel_ggtt.c:769-778`)
and its clamp of a larger table to `1ULL << 32` (`:1471-1478`) are still the
reason `MAX_APERTURE` is 4 GiB, which is also all `PLANE_SURF[31:12]` can name
(reference §5.4; `[I915]` `display/skl_universal_plane_regs.h:160`).

Three answers are possible and each is named rather than guessed at:

| what the read gave | what happens | where it says so |
|---|---|---|
| 1, 2 or 3 in the field | the aperture is 1, 2 or 4 GiB | `ApertureSize::Observed`, printed by `Gtt::describe` |
| 0 in the field | **no address is handed out**: the vendor function maps zero to a zero-byte page table, which is not a size to allocate out of, and guessing one is a write outside the real table | `ApertureSize::Unmodelled`, refused by `map_linear` as `GttError::ApertureSizeUnmodelled { raw }` |
| no answer (configuration space unreachable) | the aperture is the one the mapped window covers -- the behaviour this kernel had before it read the field -- and the report says the value is **absent** rather than printing a number nobody measured | `ApertureSize::Unreadable` |

`Gtt::over` is the fourth case and the honest one for a host test: no read was
attempted (`ApertureSize::NotObserved`).

### 2.4 Where an allocation may go: the reserved pages, the padding, and the entries that are never overwritten

The firmware programmed this machine's current scanout into the same table, and
the machine's only console is what that scanout shows.  Overwriting those
entries would take the console away, which is why `map_linear` reads every
candidate entry before it writes any of them and restarts below any that is
present:

* the search runs **downwards from below the reserved top page**, so the low
  region a firmware framebuffer usually occupies is touched last (`[PRM]` DG1
  Vol 12's own worked example maps a surface at `0x200000`);
* a candidate block that meets a present entry **restarts below it**, so the
  pages of one surface stay consecutive in the aperture — the display engine
  walks them linearly from `PLANE_SURF`;
* a block that cannot fit above the current floor is `ApertureExhausted`, a
  named error, never a wrap.

Two pages are reserved by name, and this is the difference between this kernel
and the vendor driver: `[I915]` runs before it has bound anything and *writes*
its scratch entry into the top page (`init_ggtt`'s "And finally clear the
reserved guard page", `gt/intel_ggtt.c:906-907`), while this kernel runs after
firmware that may have left a live entry there, so it reserves the page without
writing it.

**The top page.**  `[I915]` leaves one page at the end of the GGTT out of the
allocator because the hardware prefetches past the end of an object: "However,
leave one page at the end still bound to the scratch page.  There are a number
of places where the hardware apparently prefetches past the end of the object,
and we've seen multiple hangs with the GPU head pointer stuck in a batchbuffer
bound at the last page of the aperture.  One page should be enough to keep any
prefetching inside of the aperture" (`gt/intel_ggtt.c:815-826`, and the page is
the one `:906-907` clears).  The page-colouring half of the same guard
(`i915_ggtt_color_adjust`, "insert a guard page to prevent prefetches crossing
over the GTT boundary", `:36-53`) is installed only where the platform has no
LLC and no PPGTT (`:66-67`), so on this part the guard is the reserve itself.
The same bytes are where `[I915]` puts the GuC's firmware reserve, the top
`SZ_4G - GUC_GGTT_TOP` (`:768-799`; `GUC_GGTT_TOP` is `0xFEE00000`,
`gt/uc/intel_guc.h:401`, so 18 MiB on a 4 GiB GGTT), and where at least one GOP
puts its framebuffer (`display/intel_plane_initial.c:207-209`).  This kernel
reserves one page -- the record of the finding -- and does **not** reserve the
18 MiB: nothing here loads the GuC, so the GuC's reserve is not this kernel's to
keep, and the present-entry rule is what protects a GOP framebuffer that landed
inside it.

**The padding after a run.**  Under VT-d `[I915]` requires a scanout buffer to
be 256 KiB-aligned with 64 PTEs of valid entries after it:

```c
/* [I915] display/intel_fb_pin.c:127-133 */
/* Note that the w/a also requires 64 PTE of padding following the
 * bo. We currently fill all unused PTE with the shadow page and so
 * we should always have valid PTE following the scanout preventing
 * the VT-d warning.
 */
if (intel_scanout_needs_vtd_wa(dev_priv) && alignment < 256 * 1024)
        alignment = 256 * 1024;
```

and the workaround is `DISPLAY_VER(i915) >= 6 && i915_vtd_active(i915)`
(`display/intel_display.c:8385-8388`).  The vendor driver gets the padding for
free because it fills *every* unused entry with its scratch page; this kernel's
table is the firmware's and only the entries a run owns may be written, so the
padding is bound explicitly: `map_linear` searches for a block of the run's
pages plus `SCANOUT_PADDING_ENTRIES` (64) entries on a `SCANOUT_ALIGNMENT`
(256 KiB) boundary, writes the run into the first pages of it and the **zero
page** into the rest, and reads the whole block back.

Applying it unconditionally, rather than under `intel_scanout_needs_vtd_wa`, is
a deliberate `[INF]`: nothing in this kernel parses `DMAR`, so it cannot tell
whether VT-d is active, and a valid entry naming a page of zeros is harmless
when it is not.  Zero is the safe content because every byte the engine could
read there is black and no bit pattern in it names anything.  The padding
entries must be *free* before they are written: they are part of the block the
search proves absent, so "a present entry is never overwritten" is the same rule
and not a second one.

`[I915]` also binds scratch entries *before* an object (`:482-496`, its VMA
`guard` pages).  This kernel does not: the cited VT-d requirement is about the
PTEs *following* the scanout, and every run is 256 KiB-aligned, so the space
below a run is free aperture rather than an entry this code fills.

The top-down direction is defence in depth, not the protection.  `[I915]`
records that the usual assumption is false somewhere: "MTL GOP likes to place
the framebuffer high up in ggtt" (`display/intel_plane_initial.c:207-209`).  The
protection is the read-before-write, and the vendor driver guards the same
hazard explicitly: it refuses to relocate the firmware's framebuffer onto its
own entries, "that would corrupt the original PTEs which are still being used
for scanout" (`display/intel_plane_initial.c:217-224`).

A zeroed table has no present entries, so on a machine whose firmware never
used a given address this costs one read per entry of a candidate block and
changes nothing.  It also
means a window that is *not* the page table — the class of bug that "returns
zeroes rather than faulting" — fails loudly instead of quietly: a table full of
non-zero garbage exhausts the aperture and reports it, and a table that reads
zero everywhere still cannot be told apart from an empty one, which is why the
`BarTooSmall` refusal in §2.1 is the check that matters.

Every entry is read back after it is written, and a mismatch is
`GttError::ReadBackMismatch { index, wrote, read }`.  A dropped write is
otherwise a black screen with no diagnostic: reference §11 phase 4.3 says a
surface address the hardware rejected shows up as `PLANE_SURFLIVE` reading zero
and nothing else says why.  `[I915]` relies on the same check — "The WC issue is
easily caught by the readback check when writing GTT PTE entries"
(`gt/intel_ggtt.c:200-208`) — and an uncached load of an address just stored is
ordered against that store on this architecture, so the read-back is the
strongest check available rather than an approximation.

### 2.5 Aperture address zero is never handed out

`RESERVED_LOW_APERTURE` is one page.  This is not an off-by-one: reference §5.6
disables a plane by writing `PLANE_SURF = 0`, and §11 phase 4.3 reads
`PLANE_SURFLIVE = 0` as *the surface address was rejected*.  A framebuffer at
aperture address zero would therefore be indistinguishable, in the one register
that proves the plane armed, from a plane that never armed — and §11 phase 6.2
makes that register the proof.  A one-page aperture consequently has no room for
anything, and says so.

## 3. The translate-cache invalidate this kernel does not write

This section is deliberately placed next to the plane bring-up rather than
buried: it is the one thing in this workstream that a person on the N305 may
have to act on, and the symptom it causes is a black screen.

### 3.1 What the vendor driver does

`[I915]` calls `ggtt->invalidate(ggtt)` after **every** page table entry update,
on every path: `gen8_ggtt_insert_page` (`gt/intel_ggtt.c:437-451`), the bulk
`gen8_ggtt_insert_entries` (`:497-501`), the bind-context variants (`:453-470`,
`:543`).  On Gen12 that resolves to `guc_ggtt_invalidate()`, which writes

```c
/* [I915] gt/intel_ggtt.c:237-252 */
intel_uncore_write_fw(gt->uncore,
                      GEN12_GUC_TLB_INV_CR,
                      GEN12_GUC_TLB_INV_CR_INVALIDATE);
```

and the register is `_MMIO(0xcee8)`, bit 0 (`gt/uc/intel_guc_reg.h:83-84`,
upstream v6.12; the repository's cached `[I915]` subset does not carry that
header, so it was fetched from
`raw.githubusercontent.com/torvalds/linux/v6.12/drivers/gpu/drm/i915/gt/uc/intel_guc_reg.h`
and saved at `/home/ava/.cache/thekernel-targets/intel-gtt/ref-extra/guc_regs.h`).

### 3.2 Why this kernel cannot write it

`0xcee8` is below `0x40000`, and the reference's §2.1 model puts
`0xb400`–`0xcfff` in the **`FORCEWAKE_GT`** domain (`[I915]`
`intel_uncore.c:1371`, `GEN_FW_RANGE(0xb400, 0xcfff, FORCEWAKE_GT)`, inside
`__gen12_fw_ranges`).  Registers in the GT range need a forcewake handshake
before they can be written, and this kernel has none: `power.rs` brings up the
*display* power wells, and `regs::Register` refuses at **compile time** any
offset outside `FORCEWAKE_FREE_BANDS`, which does not include `0xcee8`.  Adding
the invalidate would therefore mean implementing forcewake (a request/ack
handshake on `FORCEWAKE_MT`, with its own timeouts and its own failure modes)
for one write.  That is a real piece of work, not a one-line addition, and it is
recorded here as a `[GAP]` rather than half-done.

### 3.3 Is it needed before the first use of a fresh entry?

**No source I have states this either way.**  What the sources do say, and how
each bears on the question:

* `[I915]` does not distinguish first use from an update.  Every insert path
  ends in `ggtt->invalidate(ggtt)`, including the very first binding of an
  object into the GGTT, so the vendor driver's answer is "always".
* The only ordering statement in the copy is about the *end* of the update, not
  about first use: "We want to flush the TLBs only after we're certain all the
  PTE updates have finished" (`gt/intel_ggtt.c:498-500`, repeated at `:638`).
* The register name and the surrounding code say the invalidate is for the
  **GuC and the engines' translation caches** (`guc_ggtt_invalidate`,
  `guc_ggtt_ct_invalidate`, and the `GEN8_GTCR`/`GEN12_GUC_TLB_INV_CR` writes
  that precede them).  Nothing in the copy describes how the *display engine*
  caches a translation, and nothing describes a display-side invalidate.
* `[I915]` never leaves a non-present hole inside a binding: unused entries
  beyond a buffer are filled with the scratch page's entry
  (`gt/intel_ggtt.c:485-496`).  Whether a non-present entry inside a scanout
  range would fault or read zeros is `[GAP]`; this module never creates one,
  because it maps whole pages for the whole run.

One property of this driver makes the question smaller than it looks: **nothing
in this kernel unmaps**.  Every address `Gtt::map_linear` hands out is an
address the display engine has never translated on this boot, so the only way a
stale translation could exist is if the firmware or another agent had used that
same aperture address earlier in the same boot — and the read-before-write rule
in §2.4 makes that page skip rather than reuse.  The two reserved pages are the
same argument in the other direction: the top page is never written, so a
translation the firmware left there is untouched, and the zero page's entries
are written only into entries that read as absent.  A stale *positive* entry is
therefore the case that cannot arise; a stale *negative* entry (the engine
having cached "nothing there" for an address it never saw) is the one that
would, and no source says whether the display engine even has such a cache.

### 3.4 What it would look like if the engine did cache a stale entry

* `PLANE_SURF` is written and `PLANE_SURFLIVE` reads back **the value that was
  written** — the plane armed as far as the register interface is concerned
  (§11 phase 4.3, 6.2).
* `PIPEDSL` still changes (§11 phase 6.1) and the DDI is not idle (§11 phase
  6.3): the pipe is running.
* The screen shows the *previous* buffer for that address, or a black or
  garbage surface if the entry the engine used names nowhere.
* There is no error bit anywhere.  The reference's own framing applies: "a
  black screen with correct sync means the plane or DDI is wrong" (§11 phase
  3.4), and a GGTT address the hardware cannot translate is listed as exactly
  that (`§11 phase 4.3`: "the surface address was rejected (alignment or a GGTT
  entry that is not valid)").

### 3.5 What to do about it

1. **Do nothing, and treat the first bring-up as the experiment.**  This is the
   current state, and the argument above is that a fresh address is the case
   least likely to need an invalidate.  If pixels appear, the question is
   answered for this path on this machine — write the answer down here.
2. **If the plane arms but the screen stays dark**, the invalidate is one of
   three candidates; the other two are the stride unit (§6.1) and the DDB /
   watermark step (§11 phase 4.1–4.2, which is WS-2's).  Try a *different*
   aperture address before implementing forcewake: an address the engine has
   never seen is what the current code already gives, and a second, different
   address distinguishes "the engine cached something" from "the plane never
   read anything".
3. **If forcewake has to be implemented anyway**, the write is one line and the
   handshake is the work: request the GT domain, poll its ack, write
   `GEN12_GUC_TLB_INV_CR = 1`, release.  It belongs in `power.rs` next to the
   display wells, not here, and it would need its own host tests against
   `MockRegisters` for the ack timeout and the rollback.

## 4. The framebuffer

### 4.1 Which memory, and the question that decides it

The brief asks explicitly whether the display engine can read ordinary system
RAM through the GGTT, or whether the framebuffer has to be inside the graphics
aperture (BAR 2 / `GMADR`).  **It can, and this is sourced, not inferred.**

* `intel_fb_pin_to_ggtt()` pins the framebuffer *object's pages* into the GGTT
  (`display/intel_fb_pin.c:105-174`, upstream v6.12, saved at
  `ref-extra/fb_pin.c`).  On an integrated part those pages are ordinary system
  memory: the only migration in that function is to local memory, and it is
  guarded by `HAS_LMEM()`, which ADL-N is not.
* The aperture is the *CPU's window* onto the same address space, not a region
  the buffer must live in: `[I915]` says "GMADR is the PCI mmio aperture into
  the global GTT" (`gt/intel_ggtt.c:1490`), and the pin above is against the
  GGTT address space with no aperture requirement at all on this class of part.
* The one platform restriction in that function is explicit about its scope:
  "Valleyview is definitely limited to scanning out the first 512MiB.  Lets
  presume this behaviour was inherited from the g4x display engine and that all
  earlier gen are similarly limited … Cherryview appears quite happy to scanout
  from anywhere within its global aperture", followed by

  ```c
  /* [I915] display/intel_fb_pin.c:147-156 */
  pinctl = 0;
  if (HAS_GMCH(dev_priv))
          pinctl |= PIN_MAPPABLE;
  ```

  — the mappable-window restriction is applied only on GMCH platforms.  ADL-N is
  not one (reference §3.1: a single PCI function, no separate display device and
  no GMCH decode).

So `fb::Surface::allocate` takes the framebuffer from **the kernel's own
physical page allocator**, not from the firmware's stolen memory.  Stolen memory
is not chosen because nothing in this kernel manages it: the region is described
by `GGC`/`DSMBASE`, and taking pages out of it without a manager risks
overlapping whatever the firmware left there — including the framebuffer the
machine is scanning out at the moment this runs, which is the only console it
has.

What this does *not* prove is that ADL-N silicon behaves as the vendor driver
expects it to; that is §8's first hardware check, and §5.1's evidence gate is
what keeps a wrong answer from costing the console.

### 4.2 Contiguity, alignment and stride

The run is **physically contiguous**, because the surface is presented to the
console as one linear buffer.  Contiguity is a property of *this* presentation,
not of the hardware: the GGTT is a page table, so a scatter-gather surface would
work equally well for the display engine and would survive a fragmented
allocator.  That is noted as the obvious future improvement, and it is not done
here because the brief asks for the linear case and one thing at a time.

* base **4 KiB aligned** — the least `PLANE_SURF[31:12]` can name, and what the
  page table entry addresses;
* stride rounded up to **256 bytes**, which is reference §11 phase 3.2's "256 is
  safest", and therefore also a multiple of 64;
* allocation rounded up to **whole pages**, so the last scan line is never
  shared with anything else;
* a refusal above **128 MiB** (`MAX_SURFACE_BYTES`), which is above the largest
  plane the PRM documents (5120×3200, `[PRM]` DG1 Vol 12 "Maximum Size") and
  exists so a nonsense request is refused before the allocator is asked for a
  hundred megabytes of contiguous memory.

1920×1080 at 32 bits per pixel is the mode reference §11 phase 3.1 prefers:
8 294 400 bytes of pixels, which is already a whole number of 256-byte strides
(7680 bytes) and of 4 KiB pages (2025), so 2025 pages are allocated and none are
wasted.  (The workstream brief gives 8 291 520 bytes for the same surface; that
is 2880 bytes short of `1920 * 1080 * 4` and is not the size of anything.  The
test `a_1080p_surface_lays_out_at_the_documented_size` now asserts the real
arithmetic, including that the padded size equals the pixel size.)

### 4.3 The kernel's view of the memory, and cache coherency

The kernel writes the surface through a **device-uncached** mapping
(`axmm::iomap`), not through the cacheable direct map.  Two reasons, in order of
weight:

1. **The machine has already been observed to support it.**  Stage 1 draws the
   kernel's log into the firmware's aperture, and `pseudofs::dev::bootfb` maps
   that aperture with `axmm::iomap` — device-uncached — and those writes reach
   the screen on the target machine.  "CPU stores through an uncached mapping
   are visible to the display engine" is therefore a measured property of this
   machine, not an assumption about it.
2. **It does not depend on coherency this workstream cannot test.**  A cacheable
   view would rely on the display engine snooping the CPU's cache.  `[I915]`'s
   device information does say this part has an LLC (`.has_llc = 1` in
   `GEN7_FEATURES`, `i915_pci.c:316`, inherited through `GEN8_FEATURES` `:413`,
   `GEN9_FEATURES` `:477`, `GEN11_FEATURES` `:606` and `GEN12_FEATURES` `:634`),
   and the vendor driver sets a scanout object's cache coherency to
   `I915_CACHE_WT` where the platform has write-through and `I915_CACHE_NONE`
   otherwise (`display/intel_plane_initial.c:189-191`; `HAS_WT` is `HAS_EDRAM`,
   `i915_drv.h:659`, which ADL-N does not have).  Uncached writes need none of
   that to be true.

The cost is one memory transaction per access instead of one per cache line, on
a console written a glyph at a time; that is the same trade `bootfb` already
makes, so the Intel surface is not slower than the console it replaces.

One consequence is written down rather than hidden, and **the earlier version of
this section was wrong about it**.  `/dev/fb0` maps the surface as a physical
range (`DeviceMmap::Physical`), so a userspace writer's mapping of those pages is
cacheable while the kernel's is not -- and the fbdev ABI's publication points do
**not** reconcile the two views on this backend:

| publication point | what it reaches here |
|---|---|
| `fsync` | `ScanoutSurface::present`, which accepts and writes nothing: a linear aperture has no submission |
| `FBIOPAN_DISPLAY` | `ScanoutSurface::pan`, which returns `Unsupported` |

So no cache is flushed, no fence is waited on and no register is written between
a userspace store and the display engine's read of the same bytes.  The true
position is therefore the one §4.3's own argument about the kernel's view was
trying to avoid: **a userspace writer's pixels reach the display engine only if
the engine's GGTT reads snoop the CPU's cache.**  `[I915]`'s device information
says this part has an LLC (`.has_llc = 1` in `GEN7_FEATURES`, `i915_pci.c:316`,
inherited through `GEN8_FEATURES` `:413-420`, `GEN9_FEATURES` `:477-481`,
`GEN11_FEATURES` `:606-611` and `GEN12_FEATURES` `:634-640`), which is the
mechanism that would make it work, but the vendor driver does not rely on it for
its own scanout objects -- it sets them to `I915_CACHE_NONE`, or `WT` where the
platform has write-through (`display/intel_plane_initial.c:184-190`; `HAS_WT` is
`HAS_EDRAM`, `i915_drv.h:659`, which ADL-N does not have).

A flush is **not** added for this.  Nothing in this kernel can test one: a
`clflush`/`wbinvd` on the user's pages would have to be driven from the fbdev
`fsync` path and its effect on the display engine is exactly what is unverified
in the first place, so writing it would be a claim rather than a fix.  It is
§8's sixth item, and it is a thing to *measure* on the machine: a raw writer
followed by a read of the same pixels through the console is the experiment.

### 4.4 When the allocation fails

Every failure is a named error with a sentence a boot log can carry, and none of
them panics:

| error | when |
|---|---|
| `FbError::EmptyExtent` | zero width or height |
| `FbError::FormatNotDrawable` | the format's channel layout is not writable |
| `FbError::StrideTooWide` | the stride does not fit `PLANE_STRIDE` (§6.1) |
| `FbError::TooLarge` | above `MAX_SURFACE_BYTES` |
| `FbError::OutOfMemory` | the page allocator could not supply a contiguous run |
| `FbError::Unaddressable` / `Unmappable` | the allocation cannot be mapped for the CPU |
| `FbError::OffsetOutsideSurface` | a byte range outside the surface |
| `FbError::Gtt(..)` | the page table refused the run (§2.4, §2.5) |

The page table's own refusals are named in `gtt::GttError::describe`, and three
of them are new with §2.3 and §2.4: `ApertureSizeUnmodelled` (the device reported
a GGTT size this kernel has no model for, so nothing is allocated),
`AddressOutsideAperture` (an address at or above the **observed** aperture, even
when the mapped window covers it) and `ApertureExhausted` (no block of the run
plus its padding fits on a 256 KiB boundary between the two reserved pages).

The failure path a coordinator sees is the one `screen::decide` already
implements: the candidate returns `Unavailable::Failed(reason)`, the search
moves on, and the **firmware aperture keeps the console**.  That is the whole
reason the failure is a value rather than a panic — a machine whose display
cannot come up must still boot and say so on whatever console it had.

One detail is stated rather than fixed: if the page table refuses the run, the
already-mapped pages are **not** returned to the allocator.  The device-uncached
mapping of them belongs to the kernel address space and outlives the allocation,
and a live mapping of memory that has been handed to someone else is worse than
a few leaked pages on a path that runs at most once per boot.

## 5. What the console gets

### 5.1 The candidate, and the evidence it is gated on

`scanout::register(surface, verdict)` publishes the surface and registers a
`screen::Candidate` named `intel-display` at `screen::rank::DRIVER`.  It enters
`drm::screen` through the interface that exists — no decision logic is changed.

The target machine has no serial port, so the screen is the only output and the
log that would explain a bad modeset is printed on it.  A candidate that won the
screen for a display that is dark would take that log with it — strictly worse
than never setting a mode.  `register` therefore takes the evidence as data:

```rust
pub(crate) enum Verdict {
    /// §11 phase 6 held; `surflive` is what PLANE_SURFLIVE read back.
    Scanning { surflive: u64 },
    /// It did not; `reason` is what was observed.
    NotScanning { reason: String },
}
```

`Scanning` with a `surflive` that is not `surface.ggtt_address()` is refused by
this module: a plane that armed on some *other* address is a display that shows
something, just not this framebuffer.  When the verdict refuses, the candidate
is still registered and answers `Unavailable::Failed(reason)`, which is what
puts the reason in the boot log while the firmware is still driving the screen.

The call sequence, in order:

1. `gtt = Gtt::map(bar0_physical, bar0_len, ApertureSize::read(&config, bdf))` —
   once the probe knows BAR 0, its observed length, and the function it read the
   `GGMS` field from.  This is the one signature that changed for §2.3, and it
   is the only change: `Surface::allocate` and `scanout::register` are as they
   were.
2. `surface = Surface::allocate(&gtt, width, height, Format::Xrgb8888)` — after
   the mode is chosen; `Arc::new` it for the candidate.
3. Program the pipe and the plane with `PLANE_SURF = surface.ggtt_address()`
   (WS-2/WS-3).
4. Read the §11 phase 6 evidence back (WS-3's reads, WS-4's verdicts).
5. `scanout::register(surface, verdict)`.

Until step 5 the console stays on the firmware aperture, which is what
`screen::decide` does with a candidate that has not been offered yet.

### 5.2 The bad cases, one by one

| what happened | what the log says | what the screen does |
|---|---|---|
| the mode was programmed but the pipe is not scanning (`PIPEDSL` unchanged, §11 phase 6.1) | `scanout: candidate 'intel-display' (rank 0) has nothing to offer: the modeset did not prove the display engine is scanning this surface out: PIPEDSL did not change between two reads …`.  The firmware framebuffer keeps the console, and that is the sentence that says so | unchanged: the firmware's own scanout |
| the plane is armed (`PLANE_CTL.ENABLE` set) but `PLANE_SURFLIVE` disagrees (§11 phase 6.2) | `… PLANE_SURFLIVE reads 0x… but this surface is at 0x…, so the plane is scanning a different buffer …`, then the same "the firmware framebuffer keeps the console" sentence | the firmware's buffer, or the buffer the plane actually armed on — either way not this surface, and the log says which address was armed |
| `DDI_BUF_CTL.IS_IDLE` never clears (§11 phase 6.3) | the same shape, carrying the caller's observation of the idle bit | unchanged; the reference's §11 phase 5.7 note applies (the DDI has no clock — check the PLL and the DDI→PLL mapping) |
| the modeset succeeded and all three held | `scanout: candidate 'intel-display' (rank 0) selected: WxH pitch N, because the display engine reads this framebuffer through the GGTT …` | the kernel's console, drawn into this surface |

In every bad case the console pixel source does not change.  The failure is
visible in the log, on a screen that is still being driven by something that
works, which is the only way a machine without a serial port can report it.

### 5.3 What each surface method does

| method | behaviour, and why |
|---|---|
| `width` / `height` / `pitch` / `pixel_layout` | the plan's numbers; `pitch` is in **bytes** because that is what the trait says.  `Surface::stride_units_for_log()` is a log-only number and is named for that: the register's own conversion happens where the register is written (§6.1) |
| `write_pixel` | encodes the canonical `0x00RRGGBB` in the surface's own layout and writes only its bytes; an out-of-range offset is ignored, because the console clips its own output and a console write must not fault the kernel |
| `virtual_height` / `yoffset` | one screen tall and zero: this is a single scanout buffer, and pretending to a second page would be a lie a pan would then act on |
| `len` | the allocation's size, which is what `screen::usable` checks against `pitch * virtual_height` |
| `read_bytes` / `write_bytes` | range-checked copies into the device-uncached mapping; out of range is `InvalidInput` |
| `mmap` | `DeviceMmap::Physical` over the same physical range; see §4.3, which now states the real position -- the user's mapping is cacheable, the kernel's is not, and nothing in this backend reconciles them |
| `present` | accepts, does nothing: a linear aperture has no publication step for a write through the kernel's own uncached mapping, and the damage tracker and fbdev ABI need one meaning on every backend.  It is **not** a publication point for a cacheable userspace mapping (§4.3) |
| `pan` | **refuses** (`Unsupported`): the visible window is the plane's, not this surface's, and there is no second page.  A caller that believed a pan happened would display the wrong page |
| `set_blank` | accepts, does nothing: blanking is a pipe/plane write this surface does not own.  The same choice, for the same reason, that `bootfb` documents |
| `restore_text` | nothing to do: the console's pixels *are* the surface's contents |
| `set_master` | accepts: there is no exclusive ownership to yield, and whoever writes last is what the display shows |

## 6. Where this deviates from the reference document

### 6.1 `PLANE_STRIDE` is in 64-byte units, not bytes

Reference §5.4's table gives `PLANE_STRIDE` (`0x70188`) as "`[11:0]` stride in
bytes".  That is wrong for a linear surface, and the error is the kind that
produces a sheared or unstartable image rather than a diagnostic.  `[I915]`:

* `display/skl_universal_plane.c:671-684` — "The stride is either expressed as
  a multiple of 64 bytes chunks for linear buffers or in number of tiles for
  tiled buffers", and the multiplier it returns for a linear buffer is 64;
* `:686-697` — `skl_plane_stride()` returns `scanout_stride / 64` for the
  register;
* `:2782-2785` — the read-back is `value * stride_mult`, i.e. bytes again;
* `display/skl_universal_plane_regs.h:109` — `PLANE_STRIDE__MASK` is
  `REG_GENMASK(11, 0)`, so the largest expressible stride is 4095 × 64 =
  262 080 bytes.

Reference §11 phase 3.2's own requirement — "make the surface stride a multiple
of 64 bytes (256 is safest)" — is a consequence of that encoding, and the two
sections of the reference therefore contradict each other.  This document and the
code follow `[I915]`: `Plan::stride()` is bytes for the console, `Plan::of`
refuses a stride above 262 080, and the padding to 256 bytes guarantees
divisibility by 64.

**There is one conversion, and it is where the register is written.**
`pipe::PlaneProgram`'s `stride_field` is what turns a byte stride into
`PLANE_STRIDE`'s units, and it is the one that refuses a stride that is not a
multiple of 64 and one that does not fit the twelve-bit field.  The number
`fb.rs` exposes is `Plan::stride_units_for_log()` / `Surface::stride_units_for_log()`
— named for the log because that is all it is, and it can refuse nothing because
nothing acts on it.  The handoff is pinned by a test rather than by this
paragraph: `the_plane_register_gets_the_surfaces_stride_in_sixty_four_byte_units`
in `fb.rs` allocates a real 1920×1080 surface, builds it into the
`pipe::PlaneSurface` the pipe module takes, and asserts that the value in the
program's own `PLANE_STRIDE` write is `stride_bytes / 64` (120) — which no test
or production code did before.

### 6.2 Nothing else

The other apparent disagreements are the reference's own: §1.1's "register
window inside BAR 0: first 8 MiB" versus `[I915]`'s 2 MiB map is explained by
§3.2 itself (the 2 MiB is a deliberate under-map), and §3.2's warning about
checking the observed BAR length is implemented as `BarTooSmall` rather than
assumed away.

## 7. Everything the reference document does not state

This is the list a person needs before running this on the N305, and it is a
deliverable rather than an apology.  Each row says what was done about it.

| # | the gap | where the fact came from instead | what this code does |
|---|---|---|---|
| 1 | the GGTT page table entry bit layout | `[I915]` `gt/intel_gtt.h:97,100,152-153`; `gt/intel_ggtt.c:277-286` | implemented and cited in `gtt.rs`; `Pte::is_local_memory` keeps the one dangerous bit visible |
| 2 | the aperture size | the device's own configuration header: `GGMS` at `0x50` bits `[7:6]`, `gt/intel_ggtt.c:1107-1121`, `:1228`, `:1238`; `PLANE_SURF[31:12]` and `[I915]`'s "GGTT size = 4G" comment and clamp for the 4 GiB cap | **read**, not inferred: 1, 2 or 4 GiB, bounded on every entry access and every allocation; a zero field refuses allocation by name; an unreadable field keeps the window-derived aperture and says the value is absent (§2.3) |
| 3 | whether a scanout buffer may live in ordinary system memory | `[I915]` `display/intel_fb_pin.c:105-174` (pins object pages; local-memory migration is discrete-only) | allocated from the kernel's page allocator (§4.1) |
| 4 | whether a scanout buffer must be inside the mappable aperture (BAR 2) | `[I915]` `display/intel_fb_pin.c:147-156` (`PIN_MAPPABLE` only when `HAS_GMCH`) | no mappable-window restriction; the allocator uses the whole aperture |
| 5 | whether the display engine snoops the CPU cache for scanout reads | `[I915]` `.has_llc = 1` for this generation (`i915_pci.c:316`, `:634-640`) and `I915_CACHE_NONE`/`WT` for a scanout object; stage 1 on this machine for the *kernel's* writes | the kernel's view is device-uncached, so the answer does not matter for it -- but a **userspace** `/dev/fb0` writer gets a cacheable mapping and its pixels do depend on the answer, because no publication point on this backend flushes them (§4.3) |
| 6 | whether the GTT translate cache must be invalidated before a fresh entry is used | **not stated either way**; `[I915]` invalidates after every update including the first binding; the register is in the GuTG forcewake domain | not invalidated; §3 records what it would take, the fresh-address argument, and the symptom |
| 7 | the unit of `PLANE_STRIDE` | `[I915]` `display/skl_universal_plane.c:671-697,2782-2785` | 64-byte units; both numbers exposed (§6.1) |
| 8 | the widest stride the hardware accepts | `[I915]` `display/skl_universal_plane_regs.h:109` (12-bit field) | `Plan::of` refuses above 262 080 bytes |
| 9 | whether a non-present entry inside a scanout range is legal | `[I915]` fills unused entries with the scratch page (`gt/intel_ggtt.c:482-496`, `:548-566`); nothing states what a hole does | the run is fully populated and so are the 64 entries after it, so no hole is created anywhere a scanout addresses (§2.4) |
| 10 | whether aperture address zero is usable for scanout | reference §5.6 and §11 phase 4.3 use zero as "disabled"/"rejected" | never handed out (§2.5) |
| 11 | whether VT-d on the target requires the 64-PTE padding and 256 KiB alignment `[I915]` applies (`intel_scanout_needs_vtd_wa`, `display/intel_display.c:8385-8388`; `display/intel_fb_pin.c:127-133`) | `[I915]` applies it when `DISPLAY_VER >= 6 && i915_vtd_active(i915)` | **implemented unconditionally**, because this kernel cannot tell whether VT-d is active and a zero page is harmless when it is not (§2.4); whether it is *needed* here is unverified (§8) |
| 12 | the observed BAR 0 length and BAR 2 size on this machine | §11 phase 0.4 says to log them | `Gtt::map` refuses a short BAR; the sizes are the coordinator's to log.  The `GGMS` field itself is logged by `Gtt::map` and printed by `Gtt::describe` (§2.3) |
| 13 | whether a scatter-gather surface would scan out | the GGTT is a page table, so it should; no source states it for this part | not implemented; the linear case was the brief's |
| 14 | the physical size of a 1920×1080 surface | arithmetic; the brief's figure is wrong (§4.2) | asserted in a test: 8 294 400 bytes, 2025 pages |
| 15 | where the firmware's own scanout sits in the aperture | `[I915]` records that it may be low or high (`display/intel_plane_initial.c:207-209`, `:217-224`) | the allocator never overwrites a present entry; a coordinator can log `PLANE_SURFLIVE` at boot for the record |
| 16 | whether the top page of the GGTT may be used by anything of this kernel's | `[I915]` reserves it for prefetch (`gt/intel_ggtt.c:815-826`, `:906-907`), puts the GuC's reserve in the same bytes (`:768-799`) and records a GOP framebuffer there (`display/intel_plane_initial.c:207-209`) | reserved by name and never handed out; **not** written, because a firmware entry may still be live there, and the 18 MiB GuC reserve is not claimed by a kernel that does not use the GuC (§2.4) |

## 8. What is not verified, and how to verify it on the machine

Nothing in this workstream has run on hardware.  Specifically, **unverified**:

1. that BAR 0 on this machine is 16 MiB and the page table is where §1.1 says;
2. that the entry encoding is right for this part (it is the vendor driver's
   encoder, but nothing here has written one to silicon);
3. that the display engine reads ordinary system memory through the GGTT here
   (§4.1 is a fact about the vendor driver's design, not about this machine);
4. that device-uncached CPU writes are visible through the GGTT to the display
   engine (they are visible through the *firmware's* entries today, which is
   suggestive and not the same test);
5. that a fresh entry needs no translate-cache invalidate (§3.3);
6. that a **userspace** `/dev/fb0` writer's cacheable mapping is visible to the
   display engine at all: no publication point on this backend flushes it, so
   the pixels depend on the engine snooping the LLC (§4.3).  This is the one
   item on this list whose failure mode is a *stale* picture rather than a dark
   one, and it is read with a raw writer (write a known pattern through the
   mapping, then read the same pixels back through the console);
7. that the `GGMS` field on this machine names the aperture this kernel models.
   If the field reads zero, `map_linear` refuses every allocation by name and
   the firmware console keeps the screen -- a boot that logs
   `ApertureSizeUnmodelled` and no Intel surface is this check failing, not a
   regression;
8. that the 256 KiB alignment and the 64 entries of zero page after the run are
   *needed* here (VT-d may be off, in which case they cost aperture and change
   nothing) or *sufficient* (if VT-d is on and the padding is the wrong length,
   the symptom is the `DMAR` fault `[I915]` cites, which this kernel cannot
   read without an IOMMU driver);
9. that the firmware has left the top page of the aperture absent or live.  The
   kernel does not write it either way, so this reading changes nothing about
   the code; it is recorded by reading the entry before any allocation;
10. the host tests themselves prove nothing about device memory: they exercise
   the volatile access path over ordinary memory and the allocator over a mock
   page table, in a 32 MiB host page arena.  What they do prove is listed in §9.

The readings that would settle the most, in the order they become possible:

| reading | what it settles |
|---|---|
| the `GGMS` word at PCI config `0x50` (the kernel logs it at `Gtt::map`) | gap 2: whether the aperture is 1, 2 or 4 GiB, and whether this kernel will hand out a single address on this machine |
| PCI config space BAR 0 and BAR 2 lengths (§11 phase 0.4) | gap 12, and whether `Gtt::map` will refuse on this machine |
| `GSMBASE` (`0x108100`, bits `[63:20]`) | where the firmware's page table really is, and a cross-check on the BAR window |
| `PLANE_SURF` / `PLANE_SURFLIVE` for the live plane, **before** this driver writes anything | where the firmware put its surface: if that address is above the mappable end, gaps 3 and 4 are answered on this machine for good |
| the PTE read-back result after `map_linear` | whether writes to the array stick at all; a `ReadBackMismatch` here is a diagnostic this module produces instead of a black screen |
| §11 phase 6 after the modeset | whether a fresh entry needed the invalidate (gap 6): pixels mean it did not |

## 9. Test inventory

`drm::intel` host tests at this branch's tip: **385 run, 381 pass**, with the
invocation in the workstream brief (`--tests`, the percpu linker script, the
linker wrapper, `env -u` for the product build flags, `--test-threads=1`), in
0.5 s of test time after the build.  The whole kernel test binary, unfiltered,
is **2340 run, 2336 pass**.

The four failures are the same four `modeset` tests in both runs, and they are
not this workstream's: at the base commit `18b4bd18`, `modeset.rs` still refers
to the `pipe.rs` API that `14e49ffe` replaced (`SurfaceCheck::NotArmed`,
`Vec<ScanlineSample>` where `line_rate` wants `&[u32]`), so the **library does
not compile** and the test binary cannot be built at all until the modeset fix
lands.  The numbers above come from a run with a local, uncommitted three-line
patch that only makes that base tree compile
(`/home/ava/.cache/thekernel-targets/intel-scanout-fix/local-modeset-compile-hack.diff`);
nothing in this workstream's commits touches `modeset.rs`, and with that patch
the four tests fail on the verdict they were already failing on before these
changes.  **This is the one number in this document that is not reproducible
from the branch alone**, and it is recorded rather than smoothed over.

The product configuration compiles and lints as well:
`python3 tools/thekernel.py lint --platform n305` (the n305 profile, clippy with
`clippy::correctness` and `clippy::suspicious` denied) exits 0, and that is what
compiles the `#[cfg(target_os = "none")]` half of these modules -- the BAR
mapping, the GGMS read, the zero page's physical address and the device-uncached
CPU view, none of which a host test can reach.

| module | tests | what they pin down |
|---|---:|---|
| `fb` | 13 | the 1920×1080 layout (stride 7680 = 120 units, 8 294 400 bytes, 2025 pages), stride padding to 256, the stride and size refusals, an empty extent, every error describing itself, allocation alignment (page **and** 256 KiB)/presence/blackness, **the handoff into the pipe module: a real surface's `stride_bytes / 64` is the value in the program's own `PLANE_STRIDE` write**, **the 64 padding entries after a surface naming the zero page and not the surface**, `len >= pitch * virtual_height` for both an exact and a padded geometry, byte-range refusals including a wrapping offset, an allocation the allocator cannot satisfy, and a page table that refuses the run |
| `gtt` | 28 | the PTE round trip, the bits the vendor encoder leaves clear (including local memory being reported rather than masked), address refusals (zero, unaligned, too wide), a run written present and in order, page rounding of a partial length, non-overlapping successive mappings, **a present entry never being overwritten**, a run restarting below an occupied page rather than straddling it, exhaustion as an error rather than a wrap, a dropped write caught by the read-back, validation before any write, every error describing itself, a short window refused, an address outside the table refused, a BAR too short to hold the array (2 MiB, the length i915 under-maps to), a zero-length run, and — new with §2.3 and §2.4 — **all four values of the `GGMS` field decoded**, **the field read through `ConfigSpace` (including a bus that does not answer)**, **the observed aperture bounding `entry`, `plan` and the allocator below an 8 MiB window**, **an unmodelled field refusing allocation by name with nothing written**, **an unobserved field keeping the window aperture**, **the padding bound into the entries after a run and refusing to overwrite a live one**, **the top page reserved and not written**, and **the report carrying both numbers and the observation** |
| `scanout` | 12 | the console geometry check selecting the candidate, a pixel written and read back in the layout the engine reads, `pitch / 64` as the register value, `present` accepted and `pan` refused, the no-op methods, out-of-range console writes refused, the physical mmap range, `register` publishing what the candidate hands over, **a replaced offer being retired rather than freed** (a `Weak` reference proves the pages are still owned), **a `NotScanning` verdict being refused with the reason in the log while the search continues**, and a `SURFLIVE` mismatch refused with both addresses named |

What these tests are worth is bounded and stated: they drive the module's own
logic over ordinary memory and a mock page table.  They cannot show that a real
aperture behaves the same way, that the entry bits mean what the vendor driver
says they mean, or that any of it reaches a screen.

## 10. Sources

* The reference document, `docs/design/intel-display-registers.md`, and the
  `[I915]` tree it cites, cached at
  `/home/ava/.cache/thekernel-targets/intel-display-ref/ref/i915` (v6.12 subset:
  `gt/intel_ggtt.c`, `gt/intel_gtt.h`, `intel_uncore.c`, `i915_reg.h`,
  `i915_pci.c`, `i915_drv.h`, and the `display/` files named above).
* Three files the cached subset does not carry were fetched from
  `raw.githubusercontent.com/torvalds/linux/v6.12/` on 2026-09-13 and saved
  under `/home/ava/.cache/thekernel-targets/intel-gtt/ref-extra/`:
  `drivers/gpu/drm/i915/display/intel_fb_pin.c`,
  `drivers/gpu/drm/i915/gt/uc/intel_guc_reg.h`,
  `drivers/gpu/drm/i915/gt/intel_gt_regs.h`, and
  `drivers/gpu/drm/i915/gt/intel_ggtt.c` (md5 in that directory).
* `[PRM]`: Intel DG1 (Xe-LP, Gen12) display PRM, as cached by the reference
  document at `/home/ava/.cache/thekernel-targets/intel-display-ref/ref/prm`.
