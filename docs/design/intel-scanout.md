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
| `kernel/src/drm/intel/gtt.rs` | the GGTT page table: the entry layout, the aperture, `map_linear` (a run of entries for a contiguous physical range), the read-back check, and the allocator |
| `kernel/src/drm/intel/fb.rs` | `Plan` (geometry, stride, size and every refusal), `Surface::allocate`, and the kernel's writable view of the memory |
| `kernel/src/drm/intel/scanout.rs` | `ScanoutSurface` over `fb::Surface`, and the `screen::Candidate` that offers it to the console |

The call sequence a coordinator wires is:

```rust
// once, after the probe found a device and knows its BAR 0:
let gtt = intel::gtt::Gtt::map(bar0_physical, bar0_len)?;
// per mode, after the mode layer chose one:
let surface = Arc::new(intel::fb::Surface::allocate(&gtt, width, height, fb::Format::Xrgb8888)?);
// after the modeset has programmed PLANE_SURF = surface.ggtt_address() and read
// the phase 6 evidence back:
intel::scanout::register(surface, scanout::Verdict::Scanning { surflive });
```

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

### 2.3 The aperture, and the size that is inferred

The array is 8 MiB of 8-byte entries: 1 048 576 entries, each naming a 4 KiB
page, which is exactly **4 GiB of aperture**.  That number is `[INF]`, not a
statement in the reference, and it rests on three independent supports:

1. the reference's own BAR split (the array is the last 8 MiB of a 16 MiB BAR);
2. `PLANE_SURF` carrying the graphics address in bits `[31:12]`
   (reference §5.4; `[I915]` `display/skl_universal_plane_regs.h:160`), which
   can name exactly 4 GiB and no more;
3. `[I915]`'s own comment "if the size of the GGTT is 4G"
   (`gt/intel_ggtt.c:769-778`) and its clamp of any larger table to
   `1ULL << 32` (`:1471-1478`).

The module does not hard-code it: `Gtt::over` derives the aperture from the
number of entries it was given and caps it at `MAX_APERTURE` (4 GiB), so the
arithmetic follows the mapping rather than a belief about the mapping.

### 2.4 Where an allocation may go: present entries are never overwritten

The firmware programmed this machine's current scanout into the same table, and
the machine's only console is what that scanout shows.  Overwriting those
entries would take the console away, which is why `map_linear` reads every
candidate entry before it writes one and steps over any that is present:

* the search runs **downwards from the top of the aperture**, so the low region
  a firmware framebuffer usually occupies is touched last (`[PRM]` DG1 Vol 12's
  own worked example maps a surface at `0x200000`);
* a candidate run that meets a present entry **restarts below it**, so the pages
  of one surface stay consecutive in the aperture — the display engine walks
  them linearly from `PLANE_SURF`;
* a run that cannot fit above the current floor is `ApertureExhausted`, a named
  error, never a wrap.

The top-down direction is defence in depth, not the protection.  `[I915]`
records that the usual assumption is false somewhere: "MTL GOP likes to place
the framebuffer high up in ggtt" (`display/intel_plane_initial.c:206-210`).  The
protection is the read-before-write, and the vendor driver guards the same
hazard explicitly: it refuses to relocate the firmware's framebuffer onto its
own entries, "that would corrupt the original PTEs which are still being used
for scanout" (`display/intel_plane_initial.c:218-221`).

A zeroed table has no present entries, so on a machine whose firmware never
used a given address this costs one read per page and changes nothing.  It also
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
in §2.4 makes that page skip rather than reuse.  A stale *positive* entry is
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

One consequence is written down rather than hidden.  `/dev/fb0` maps the
surface as a physical range (`DeviceMmap::Physical`), and the mapping a
userspace writer gets through the fbdev path is cacheable while the kernel's
view is not.  That is exactly the arrangement the firmware aperture already has
today, and the fbdev ABI's explicit publication points (`fsync`,
`FBIOPAN_DISPLAY`) are where the two views are reconciled.  It is a caveat, not
a measured problem, and it is on §8's list of things to watch on hardware.

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

1. `gtt = Gtt::map(bar0_physical, bar0_len)` — once the probe knows BAR 0 and its
   observed length.
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
| `width` / `height` / `pitch` / `pixel_layout` | the plan's numbers; `pitch` is in **bytes** because that is what the trait says, and `Surface::stride_units()` is the separate value a plane register wants (§6.1) |
| `write_pixel` | encodes the canonical `0x00RRGGBB` in the surface's own layout and writes only its bytes; an out-of-range offset is ignored, because the console clips its own output and a console write must not fault the kernel |
| `virtual_height` / `yoffset` | one screen tall and zero: this is a single scanout buffer, and pretending to a second page would be a lie a pan would then act on |
| `len` | the allocation's size, which is what `screen::usable` checks against `pitch * virtual_height` |
| `read_bytes` / `write_bytes` | range-checked copies into the device-uncached mapping; out of range is `InvalidInput` |
| `mmap` | `DeviceMmap::Physical` over the same physical range (see §4.3 for the cacheability caveat) |
| `present` | accepts, does nothing: a linear aperture has no publication step, and the damage tracker and fbdev ABI need one meaning on every backend |
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
sections of the reference therefore contradict each other.  This document and
`fb.rs` follow the code: `Plan::stride()` is bytes for the console,
`Surface::stride_units()` is the register value, `Plan::of` refuses a stride above
262 080, and the padding to 256 bytes guarantees divisibility by 64.

WS-3 owns the `PLANE_STRIDE` write, and the coordinator has been told; this
section is the record.

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
| 2 | the aperture size (4 GiB) | `[INF]` from the BAR split, `PLANE_SURF[31:12]` and `[I915]`'s own "GGTT size = 4G" comment and clamp | derived from the mapped array's length, capped at 4 GiB |
| 3 | whether a scanout buffer may live in ordinary system memory | `[I915]` `display/intel_fb_pin.c:105-174` (pins object pages; local-memory migration is discrete-only) | allocated from the kernel's page allocator (§4.1) |
| 4 | whether a scanout buffer must be inside the mappable aperture (BAR 2) | `[I915]` `display/intel_fb_pin.c:147-156` (`PIN_MAPPABLE` only when `HAS_GMCH`) | no mappable-window restriction; the allocator uses the whole aperture |
| 5 | whether the display engine snoops the CPU cache for scanout reads | `[I915]` `.has_llc = 1` for this generation and `I915_CACHE_NONE`/`WT` for a scanout object; stage 1 on this machine | device-uncached CPU view, so the answer does not matter (§4.3) |
| 6 | whether the GTT translate cache must be invalidated before a fresh entry is used | **not stated either way**; `[I915]` invalidates after every update including the first binding; the register is in the GuTG forcewake domain | not invalidated; §3 records what it would take, the fresh-address argument, and the symptom |
| 7 | the unit of `PLANE_STRIDE` | `[I915]` `display/skl_universal_plane.c:671-697,2782-2785` | 64-byte units; both numbers exposed (§6.1) |
| 8 | the widest stride the hardware accepts | `[I915]` `display/skl_universal_plane_regs.h:109` (12-bit field) | `Plan::of` refuses above 262 080 bytes |
| 9 | whether a non-present entry inside a scanout range is legal | `[I915]` fills unused entries with the scratch page (`gt/intel_ggtt.c:485-496`); nothing states what a hole does | the run is fully populated; no holes are created |
| 10 | whether aperture address zero is usable for scanout | reference §5.6 and §11 phase 4.3 use zero as "disabled"/"rejected" | never handed out (§2.5) |
| 11 | whether VT-d on the target requires the 64-PTE padding `[I915]` applies (`intel_scanout_needs_vtd_wa`, `display/intel_display.c:8385-8388`) | `[I915]` applies it when `DISPLAY_VER >= 6 && i915_vtd_activei915` | not implemented; §8 lists it as a candidate cause of a black screen, and the target's VT-d state is unknown |
| 12 | the observed BAR 0 length and BAR 2 size on this machine | §11 phase 0.4 says to log them | `Gtt::map` refuses a short BAR; the sizes are the coordinator's to log |
| 13 | whether a scatter-gather surface would scan out | the GGTT is a page table, so it should; no source states it for this part | not implemented; the linear case was the brief's |
| 14 | the physical size of a 1920×1080 surface | arithmetic; the brief's figure is wrong (§4.2) | asserted in a test: 8 294 400 bytes, 2025 pages |
| 15 | where the firmware's own scanout sits in the aperture | `[I915]` records that it may be low or high (`display/intel_plane_initial.c:206-221`) | the allocator never overwrites a present entry; a coordinator can log `PLANE_SURFLIVE` at boot for the record |

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
6. that `/dev/fb0`'s cacheable userspace mapping and the kernel's uncached view
   do not visibly disagree under a raw-writing client (§4.3);
7. the host tests themselves prove nothing about device memory: they exercise
   the volatile access path over ordinary memory and the allocator over a mock
   page table, in a 32 MiB host page arena.  What they do prove is listed in §9.

The five readings that would settle the most, in the order they become possible:

| reading | what it settles |
|---|---|
| PCI config space BAR 0 and BAR 2 lengths (§11 phase 0.4) | gap 12, and whether `Gtt::map` will refuse on this machine |
| `GSMBASE` (`0x108100`, bits `[63:20]`) | where the firmware's page table really is, and a cross-check on the BAR window |
| `PLANE_SURF` / `PLANE_SURFLIVE` for the live plane, **before** this driver writes anything | where the firmware put its surface: if that address is above the mappable end, gaps 3 and 4 are answered on this machine for good |
| the PTE read-back result after `map_linear` | whether writes to the array stick at all; a `ReadBackMismatch` here is a diagnostic this module produces instead of a black screen |
| §11 phase 6 after the modeset | whether a fresh entry needed the invalidate (gap 6): pixels mean it did not |

## 9. Test inventory

`drm::intel` host tests: **208 pass** (168 before this workstream; 40 added by
it), run with the invocation in the workstream brief (`--tests`, the percpu
linker script, the linker wrapper, `env -u` for the product build flags), in
0.04 s of test time after the build.

| module | tests | what they pin down |
|---|---:|---|
| `fb` | 11 | the 1920×1080 layout (stride 7680 = 120 units, 8 294 400 bytes, 2025 pages), stride padding to 256, the stride and size refusals, an empty extent, every error describing itself, allocation alignment/presence/blackness, `len >= pitch * virtual_height` for both an exact and a padded geometry, byte-range refusals including a wrapping offset, an allocation the allocator cannot satisfy, and a page table that refuses the run |
| `gtt` | 18 | the PTE round trip, the bits the vendor encoder leaves clear (including local memory being reported rather than masked), address refusals (zero, unaligned, too wide), a run written present and in order, page rounding of a partial length, non-overlapping successive mappings, **a present entry never being overwritten**, a run restarting below an occupied page rather than straddling it, exhaustion as an error rather than a wrap, a dropped write caught by the read-back, validation before any write, every error describing itself, a short window refused, an address outside the table refused, the aperture following the table, and the reserved first page, a BAR too short to hold the array (2 MiB, the length i915 under-maps to), and a zero-length run |
| `scanout` | 11 | the console geometry check selecting the candidate, a pixel written and read back in the layout the engine reads, `pitch / 64` as the register value, `present` accepted and `pan` refused, the no-op methods, out-of-range console writes refused, the physical mmap range, `register` publishing what the candidate hands over, **a `NotScanning` verdict leaving the firmware console alone with the reason in the log**, and a `SURFLIVE` mismatch refused with both addresses named |

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
