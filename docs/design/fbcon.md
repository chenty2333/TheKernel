# Firmware-Framebuffer Console (`fbcon`) — Implementation Design

**Target worktree:** `/home/ava/Worktrees/TheKernel/baremetal-boot`
**Branch / HEAD:** `feat/baremetal-boot` / `a4f60675`
**Goal:** display kernel and terminal text on the physical screen of a real Intel N305
mini-PC that has no serial port, using the linear framebuffer that GRUB/UEFI GOP already
set up.

**Method / evidence rules.** Every factual claim below cites `file:line` and quotes the
line(s). Claims are tagged:

| Tag | Meaning |
|---|---|
| `[V]` | Verified by reading the cited source in this worktree. |
| `[X]` | Verified in an **external** source that is not in this worktree (Cargo registry dependency or the Multiboot2 specification). Path is given in full. |
| `[I]` | Inference from verified code. Labelled explicitly. |
| `NOT FOUND` | Exhaustive search performed; result is negative. The search terms are listed. |

This analysis was read-only: the only artefact it produced is this document. No tracked
file in the worktree was modified, and no build, test or QEMU command was run.

---

## 0.1 Empirical corrections — three claims this document could not settle

The analysis above left three questions open because they are not answerable from source
alone. All three have since been settled by building and booting the kernel, and two of
them **contradict** the reasoning recorded in §1.2, §6.2 and change-set item 10. The
original text is left in place; the corrections below supersede it.

The instrument was a temporary MB2 tag inventory added to
`crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs` and reported on the COM2 diagnostic
channel, which the QEMU runner captures to `workdir/kernel.log`.

**Correction 1 — GRUB *does* emit the type-8 tag without `gfxmode`/`gfxpayload`.**

Observed on the unmodified `config/x86_64/grub.cfg` (`terminal_output serial`, no video
mode set), `-machine q35` + OVMF, profile `headless`:

```
MB2 tag inventory: protocol=Multiboot2 count=13 truncated=0
MB2 tag type=8 size=38
```

`size=38` is unambiguous under the §1.3 layout: 8-byte header + 24-byte fixed portion +
6 colour-layout bytes, i.e. `framebuffer_type = 1` (RGB). So §1.2's "NOT FOUND — whether
GRUB emits a type-8 tag without explicit `gfxmode`/`gfxpayload`" resolves to **yes**, and
change-set item 10's stated rationale ("so GRUB initialises video and emits the tag") is
**wrong**. Item 10 may still be worth doing for a different and narrower reason: see
Correction 2, where the *address* rather than the tag's existence is what depends on the
display device. On the evidence available, add `gfxmode`/`gfxpayload=keep` only if a real
machine is observed to report an unusable address.

**Correction 2 — the framebuffer *address* depends on the display device, and a
virtio-gpu with no display backend yields an unusable one.**

Two boots differing only in the graphics device:

| Topology | Reported framebuffer |
|---|---|
| `-display none -device virtio-gpu-pci,max_outputs=1,xres=800,yres=600` (existing `headless`) | `addr=0x0 800x600 bpp=32 pitch=3200 rgb_bits=8/8/8` |
| `-display none -device bochs-display` (new `firmware-fb`) | `addr=0x80000000 1280x800 bpp=32 pitch=5120 rgb_bits=8/8/8` |

A virtio-gpu with no display backend still reports a well-formed mode through GOP but
never a framebuffer base. §6.2 asked whether the bundled OVMF provides a GOP for
`virtio-gpu-pci` and recorded the check as inconclusive; the answer is that it does, and
that the GOP is not the problem — the missing base is. This vindicates the
`bochs-display` recommendation in §6.4, for a sharper reason than the document gave.

**Correction 3 — an all-zero address must be rejected, and the parser as specified did
not reject it.**

The first boot above was accepted by the parser as a usable 800x600 surface homed at
physical address 0. Writing console output there would overwrite the real-mode interrupt
vector table. Two checks were added beyond the §1.3 spec reading:

- `address == 0` is rejected as `FramebufferRejection::UnusableAddress`. This is the
  exact signature of a bootloader that described a mode but never programmed a display,
  and calling it out separately matters because its fix (bootloader configuration)
  differs from every other rejection's.
- A surface overlapping a usable-RAM region is rejected as
  `FramebufferRejection::OverlapsUsableMemory`. The check has to run after the whole tag
  block is read, because the memory-map tag is not required to precede the framebuffer
  tag. Note the rule is *overlap*, not proximity: `bochs-display`'s surface begins exactly
  at `0x8000_0000`, which is where usable RAM ends in the reference guest, and rejecting it
  would have cost the display on a correct boot.

For the reference profile this surface needs no `iomap`: `0x8000_0000` already lies inside
the product `mmio-ranges` entry `[0x8000_0000, 0x2000_0000]` (§2.2). A real N305 will
place it elsewhere, so the aperture mapping in change-set item 4 remains required.

---

## 0.2 Empirical results — the change set built, booted and photographed

This section supersedes the *predictions* in the change set below for items 1–9. It records
what was built and what a boot actually showed. The instrument is the new `firmware-fb`
profile (`-display none -device bochs-display`, no virtio-gpu at all), a 1280x800 bochs
surface, and a QMP `screendump` taken while the guest runs.

**Result 1 — the kernel's own log reaches a real screen, on a machine with no display
driver and no serial port.** Booting `firmware-fb`, the screendump contains the boot log
rendered as glyphs in the console's grey on black, beginning:

```
<6>[0.198049 cpu=None tid=None INFO target=axnet_ng ...] mac: 52-54-00-12-34-56
...
<6>[0.301987 cpu=Some(0) tid=Some(2) INFO target=thekernel_kernel::pseudofs::dev ...]
    Firmware framebuffer scanout: 1280x800 pitch 5120 at 0x80000000
<6>[0.321577 ...] Mounted devfs at FsPath([47, 100, 101, 118])
```

This is the stage-1 acceptance condition. It is the *only* output the kernel has on that
machine: `boot_info` reports no virtio-gpu (`no DRM-capable VirtIO GPU found`), so before
this work the same boot produced no observable output of any kind.

**Result 2 — a firmware splash is left on screen unless the console clears it.** The first
screendump showed OVMF's TianoCore logo and its `BdsDxe:` line still in the aperture. The
console only repaints once something writes to a VT, and nothing did before userspace
started. `present_while_text_active` already clears the frame, so this resolved itself as
soon as the log mirror existed; the observable point is that "console exists" and "screen
is ours" are different states, and only the second is what a person sees.

**Result 3 — the console wrote four bytes per pixel regardless of the surface's depth.**
`FbconFrame::write_pixel` computed `x * 4` and wrote a whole `u32`; `current_var_screen_info`
reported a fixed 32-bit BGRA. Both were correct for the only backend that existed (a
virtio-gpu `B8G8R8A8_UNORM` dumb buffer) and both are wrong for a firmware aperture whose
GOP mode is 16 or 24 bits deep: a 16-bit surface is walked at twice its stride, which
paints a garbled screen rather than an error. Fixed by making `PixelLayout` the single
place that knows how to turn a canonical `0x00RRGGBB` colour into a surface's pixel, with
`encode_into` returning the byte count so no caller can write the wrong width. The first
draft of the fix had this bug in **three** places (the two production backends and the
test double); the test double failing is what exposed it.

**Result 4 — the log mirror works, and its failure mode is a log feedback loop.** An early
attempt to instrument the mirror with an `info!` line inside its read loop produced
~160 KB of log every 10 ms, because the mirror reads the ring it was writing into: the
record it emitted was read back on the next pass. The bound and the pacing in
`mirror_new_log_bytes` are the fix, and
`the_console_write_path_does_not_log` is the regression guard — it installs a logger that
forwards to klog, asserts a canary record is visible, and then asserts the whole console
write path adds nothing. **A test without the canary would have passed by being deaf**;
this was checked by injecting a logging call and confirming the test fails.

**Result 5 — the built-in font was the other half of "can this be read".** The 5x7 table
folded every byte through `to_ascii_uppercase`, so a boot log rendered as
`THEKERNEL_TEST_BEGIN 2 ROOTFS TIMEOUT_SECONDS 60` and lost `f`/`|`/punctuation to a
placeholder. It is replaced by a full 95-glyph 8x16 table generated from Liberation Mono
(OFL-1.1) by `tools/gen-console-font.py`; the generator is deterministic and its output is
committed, so no build depends on the script or on that font being installed. The
generated table is data only — the interface and the invariant tests live in hand-written
files beside it, so regenerating cannot delete them.

**Result 6 — QMP screenshot markers match a whole console line, not a prefix.**
`screenshot_after_marker` is compared against the exact ANSI-stripped line. A marker of
`# THEKERNEL_TEST_BEGIN 1 mounts` never fires, because the guest prints
`# THEKERNEL_TEST_BEGIN 1 mounts timeout_seconds=60`. This cost two failed runs; the
acceptance test in items 13–15 must use full lines.

**What this does not establish.** Every result above is from QEMU with a `bochs-display`.
Nothing here has run on the N305: the firmware aperture address, the GOP mode and the
absence of a serial port are all still assumptions about that machine, and the N305 boots
from a USB image that has not been written yet. No claim in this section is a hardware
result.

---

## 0.3 Empirical results — the acceptance oracle and suite (items 13–16)

Same instrument as §0.2: the `firmware-fb` profile, no virtio-gpu, a 1280×800 bochs
surface, and a QMP `screendump` gated on a console line. `feat/fbcon-verify`, based on
`feat/baremetal-boot` at `5e09089e`.

**Result 7 — the screen can be read back, and it says what the kernel logged.** Decoding a
screendump cell by cell with the kernel's own generated font
(`kernel/src/pseudofs/dev/console_font/glyphs.rs`) recovered the boot log:

```
31|<6>[0.379222 cpu=Some(2) tid=Some(2) INFO target=thekernel_kernel::pseudofs module=thekernel_kernel::pseudofs] Mounted tmpfs at FsPath([47, 100, 101, 118, 47, 1
36|<6>[0.387974 cpu=Some(2) tid=Some(2) INFO target=thekernel_kernel::pseudofs module=thekernel_kernel::pseudofs] Mounted proc at FsPath([47, 112, 114, 111, 99])
37|<6>[0.396903 cpu=Some(2) tid=Some(2) INFO target=thekernel_kernel::pseudofs module=thekernel_kernel::pseudofs] Mounted sysfs at FsPath([47, 115, 121, 115])
47|<6>[0.442961 cpu=Some(0) tid=Some(32) INFO target=thekernel_kernel::task::user module=thekernel_kernel::task::user] Enter user space: ip=0x401dc0, sp=0x7ffeffff
```

This is the acceptance condition of §6.4 in literal form: the kernel's own log, on the
firmware framebuffer, of a machine with no display driver. The decode is evidence tooling,
not the oracle — see result 10 for why the committed oracle does not do it.

**Result 8 — a screendump gated on a serial marker is a frame behind, so "present" must be
asserted, not assumed.** At the first KTAP marker the screen held kernel-log-mirror records
only; not one character of the guest's own output was on it, even though `Console::write`
(`kernel/src/pseudofs/dev/tty/ntty.rs:91-100`) mirrors userspace output into the same cells.
The cause is not a routing gap but the deferred repaint: `fbcon::write` only records cells
and `trailing_present` repaints on its own schedule, while the serial marker is written a
few instructions earlier. A gate that screenshots on the marker and accepts whatever is
there photographs the *previous* repaint, and never tests the path the marker came from.

The lag is not marginal. With the marker on `# THEKERNEL_TEST_BEGIN 1 mounts`, the first
frame that actually showed the KTAP lines arrived while the guest was already running its
sixth case (`memory-pressure`), on a 1280×800 surface under TCG. The fix is to assert the
lines rather than hope: `QmpConsoleLine` expectations are matched cell for cell, a missing
line is a retryable frame mismatch, and the runner's existing screendump loop therefore
waits for the console to present the text before the run is allowed to stop. That frame,
decoded with the kernel's own font, reads with **0 unmatched cells**:

```
02|KTAP version 1
04|# THEKERNEL_TEST_BEGIN 1 mounts timeout_seconds=60
05|<6>[0.834545 cpu=Some(2) tid=Some(36) INFO target=thekernel_kernel::task::user ...] Enter user space: ip=0x4633b6, sp=0x7ffef3c9
07|<6>[0.872622 cpu=Some(2) tid=Some(36) INFO target=thekernel_kernel::task::ops ...] Task(36, init) exit with code: 0
09|# THEKERNEL_TEST_END 1 mounts result=0
10|ok 1 - mounts
42|ok 5 - procfs
48|# memory-pressure: THEKERNEL_MM_PRESSURE_BEGIN
```

Userspace output and the kernel's own log, interleaved in the order they happened, on a
screen belonging to a machine with no display driver. That is the acceptance condition with
nothing assumed.

**Result 9 — the oracle fails on a real bad image.** Verdicts from the committed oracle
(`tools/qemu_runner/process.py`) on the real screendump of result 8 and on copies of it.
Each copy keeps the PPM header, so every one of them is a well-formed image of the right
size:

| Image | Verdict |
|---|---|
| the real screendump | accepted: 160×50 cells, 3613 inked cells, 55864 ink pixels, 0 foreign, 0 outside the grid, 0 on a cell border |
| blanked to background | rejected: `does not show the expected console text: 'guest userspace: KTAP banner', 'guest userspace: the gated marker line', 'kernel log mirror: task entry' (… 0 inked cells, 0 ink pixels …)` |
| shifted one pixel right | rejected: same missing text, `(… 1058 on a cell border)` |
| repainted by a `u32`-per-pixel writer on a 16-bit surface | rejected: same missing text, `(… 77199 foreign pixels)` |

The last row is §0.2 result 3's bug reproduced on a real frame, and it is the class a
blank/not-blank check accepts: that frame is busy and colourful. The third row is the one
the colours cannot see, and it is why the oracle carries the font's border invariant. That
the blank frame is rejected for missing *text* rather than for missing ink is the point of
result 8: the assertion is what must be on the screen, not merely that something is.

**Result 10 — what the oracle deliberately cannot see.** It asserts structure and declared
lines, not a rendering. A frame in which every glyph were replaced by a different glyph of
the same size and colour would pass, and so would a font whose bitmaps changed under a
still-passing cell-border invariant. Catching that means comparing the whole screen against
a rendered expectation, which is a different test with a different failure mode (any font
change becomes a test failure). The declared-line expectations close the practical part of
that gap: the lines the suite requires are the ones whose absence would mean the acceptance
condition is not met.

**Result 11 — the run directory has a hard length budget.** The QMP monitor is a unix
socket inside the run directory and `sun_path` is 108 bytes, so a deep `--workdir` fails as
`QEMU process I/O failed: QMP control failed: AF_UNIX path too long` part-way through a
boot — which the runner reports as a missing completion marker, several layers from the
cause. `fbcon_suite_cmd` now measures the prospective socket path and refuses a workdir
that leaves no room, naming the byte count and the offending directory.

**Result 12 — the daily tier's stage cannot be run by `verify --tier daily` on this host.**
`verify` asserts the development image first (`dev-env/check-image.sh`) and fails with
`missing required command: bison` (and `flex`); CI runs the tier inside
`scripts/dev-shell.sh`. The tier was therefore reproduced stage by stage with the real
`tools.verification.plan` and `verify.execute`, substituting only that image assertion; the
substitution is printed in the transcript.

---

## 0. Executive summary — the five findings that change the design

1. **An fbcon already exists, but it is hard-wired to DRM.** `kernel/src/pseudofs/dev/tty/fbcon.rs`
   is a complete, allocation-bounded, per-VT character-cell console with a built-in 5×7
   glyph set, and `/dev/fb0` is a complete fbdev ABI implementation. Both obtain their
   pixels exclusively from a **DRM dumb GEM object** on a virtio-gpu 2D resource
   (`kernel/src/pseudofs/dev/fb.rs:1024-1026`, `kernel/src/drm/fbdev.rs:33-60`). The work
   is therefore **not** "write an fbcon"; it is "introduce a second scanout provider and
   make `DisplayCore`/`/dev/fb0`/fbcon accept it".

2. **Multiboot2 tag type 8 (framebuffer) is silently discarded.** The parser's known-tag
   set is `{0=end, 3=module, 6=mmap, 14=ACPI old, 15=ACPI new}`
   (`crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs:24-28`) and everything else falls
   into the catch-all `_ => {}` (`:462`).

3. **The boot identity map does cover a high GOP address, but the runtime page table
   replaces it.** The boot PML4 maps 512 GiB at `0` and 512 GiB at `0xffff_8000_0000_0000`
   with 1 GiB huge pages (`crates/ax/thekernel-axplat-x86-pc/src/multiboot.S:153-175`).
   After `axmm::init_memory_management()` (`crates/ax/thekernel-axruntime/src/lib.rs:424`)
   the live kernel page table is a *new*, sparse table that directly maps only
   `axhal::mem::memory_regions()` (`crates/ax/thekernel-axmm/src/lib.rs:53-68`) — i.e. RAM
   plus the compile-time `mmio-ranges` list. The product q35 profile lists
   `[0x8000_0000, 0x2000_0000]` (`config/x86_64/q35-uefi.toml:27`) but the bare
   `axconfig.toml` fallback does **not** (`crates/ax/thekernel-axplat-x86-pc/axconfig.toml:45-51`).
   A framebuffer above 4 GiB is mapped by **neither**.

4. **`DisplayDriverOps` has no mode-setting verb, and the static device model permits
   exactly one display driver type per kernel image.** The full trait is 12 methods
   (`crates/ax/thekernel-axdriver-display/src/lib.rs:383-429`); none programs a display
   controller. `register_display_driver!` only emits a type alias
   (`crates/ax/thekernel-axdriver/src/macros.rs:21-27`) and the `dyn` feature — the only
   model that could alias to `Box<dyn DisplayDriverOps>` — is **not enabled by the product
   build**, and its probe function enumerates block devices only
   (`crates/ax/thekernel-axdriver/src/dyn_drivers/mod.rs:27-41`).

5. **Kernel `printk` output never reaches fbcon today.** `println!`/`info!` land in a ring
   buffer and are drained to the **diagnostic UART at 0x2f8**, not to the console UART at
   0x3f8 and not to any framebuffer. fbcon is driven only from the TTY write path
   (`kernel/src/pseudofs/dev/tty/ntty.rs:91-100`, `kernel/src/pseudofs/dev/tty/vt.rs:1074-1085`).
   A new console sink must be introduced.

---

## 1. Multiboot2 framebuffer tag

### 1.1 Where the Multiboot2 information block is parsed

The exact function is **`parse_multiboot2_info`**, and the exact tag dispatch is its
`match tag_type` at `crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs:418`.

`[V]` `crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs:360-364`

```rust
/// Parse a complete Multiboot2 information block into an owned `BootInfo`.
///
/// This function is intentionally independent of the physical-memory access
/// helper so host tests can exercise every boundary and malformed-tag case.
fn parse_multiboot2_info(bytes: &[u8], info_paddr: usize) -> Result<BootInfo, ParseError> {
```

`[V]` `crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs:404-418`

```rust
        let tag_type = read_u32(bytes, cursor).ok_or(ParseError::TagHeaderTruncated)?;
        let tag_size = read_u32(bytes, cursor + 4).ok_or(ParseError::TagHeaderTruncated)? as usize;
        if tag_size < 8 {
            return Err(ParseError::TagTooSmall);
        }
        if tag_size > remaining {
            return Err(ParseError::TagOutOfBounds);
        }
        let aligned_size = align_tag_size(tag_size).ok_or(ParseError::TagAlignment)?;
        if aligned_size > remaining {
            return Err(ParseError::TagAlignment);
        }
        let tag = &bytes[cursor..cursor + tag_size];

        match tag_type {
```

The caller is the early-handoff finaliser: `[V]`
`crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs:143-162`

```rust
pub(crate) fn finish_handoff() {
    let EarlyBootRecord { magic, info_paddr } = crate::boot::early_record();
    let protocol = match magic {
        MULTIBOOT_BOOTLOADER_MAGIC => BootProtocol::Multiboot1,
        MULTIBOOT2_BOOTLOADER_MAGIC => BootProtocol::Multiboot2,
        _ => panic!("unsupported x86 boot magic {magic:#x}"),
    };

    let owner = match protocol {
        BootProtocol::Multiboot1 => BootInfo::empty(protocol, info_paddr),
        BootProtocol::Multiboot2 => {
            let bytes = unsafe { multiboot2_info_bytes(info_paddr) }.unwrap_or_else(|| {
                panic!("invalid Multiboot2 information pointer {info_paddr:#x}")
            });
            parse_multiboot2_info(bytes, info_paddr)
                .unwrap_or_else(|error| panic!("invalid Multiboot2 information: {error:?}"))
        }
    };
    BOOT_INFO.init_once(owner);
}
```

`finish_handoff` is invoked from `InitIf::init_early` — `[V]`
`crates/ax/thekernel-axplat-x86-pc/src/init.rs:11-21`:

```rust
    fn init_early(_cpu_id: usize, _mbi: usize) {
        axcpu::init::init_trap();
        crate::console::init();
        crate::time::init_early();
        // The platform entry runs before axruntime clears `.bss`; finalize the
        // initialized-data handoff after the early diagnostics are available,
        // then copy/use the memory map.  CPU topology discovery is intentionally
        // performed before the runtime replaces the temporary boot page table:
        // MADT itself can reside in Multiboot's ACPI-reclaimable memory and is
        // therefore not part of the usable-memory direct map.
        crate::boot_info::finish_handoff();
        crate::mem::init();
        crate::cpu::init_topology();
    }
```

### 1.2 Is tag type 8 handled today? What is the known-tag set?

**No. Tag 8 is not handled.** It is not even named.

`[V]` Known-tag constants — `crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs:24-28`

```rust
const MB2_TAG_END: u32 = 0;
const MB2_TAG_MMAP: u32 = 6;
const MB2_TAG_MODULE: u32 = 3;
const MB2_TAG_ACPI_OLD: u32 = 14;
const MB2_TAG_ACPI_NEW: u32 = 15;
```

`[V]` The full dispatch, including the catch-all — `crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs:418-463`

```rust
        match tag_type {
            MB2_TAG_END => {
                if tag_size != 8 || read_u32(tag, 0) != Some(0) || cursor + 8 != total_size {
                    return Err(ParseError::EndTagMalformed);
                }
                saw_end = true;
                break;
            }
            MB2_TAG_MMAP => {
                if saw_mmap {
                    return Err(ParseError::DuplicateMemoryMap);
                }
                saw_mmap = true;
                owner.memory_region_count = parse_memory_map(tag, &mut owner.memory_regions)?;
            }
            MB2_TAG_MODULE => {
                if owner.module_count == MAX_MODULES {
                    return Err(ParseError::ModuleCapacity);
                }
                let module = parse_module(tag)?;
                if owner
                    .modules()
                    .iter()
                    .flatten()
                    .any(|existing| module.start < existing.end && existing.start < module.end)
                {
                    return Err(ParseError::ModuleRangeOverlap);
                }
                owner.modules[owner.module_count] = Some(module);
                owner.module_count += 1;
            }
            MB2_TAG_ACPI_OLD | MB2_TAG_ACPI_NEW => {
                // Firmware occasionally leaves a stale/partial ACPI tag next
                // to a valid one.  Keep validating each candidate, but let a
                // valid new tag win over a valid old tag without allowing a
                // malformed candidate to discard the other valid copy.
                if let Ok(parsed) = parse_acpi_tag(&tag[8..], tag_type) {
                    if tag_type == MB2_TAG_ACPI_NEW {
                        acpi_new = Some(parsed);
                    } else {
                        acpi_old = Some(parsed);
                    }
                }
            }
            _ => {}
        }
```

**Known-tag set: `{0, 3, 6, 14, 15}`. Tag `8` falls into `_ => {}` at line 462 and is
ignored without comment.**

The kernel also never *asks* for a framebuffer. The Multiboot1 header flags are
`0x0001_0002` — page-align plus memory-info only, no video bit:

`[V]` `crates/ax/thekernel-axplat-x86-pc/src/boot.rs:15-19`

```rust
/// Flags set in the ’flags’ member of the Multiboot1 header.
///
/// (bits 1, 16: memory information, address fields in header)
#[cfg(all(not(test), target_os = "none"))]
const MULTIBOOT_HEADER_FLAGS: usize = 0x0001_0002;
```

and the Multiboot2 header contains only header/address/entry/end tags:

`[V]` `crates/ax/thekernel-axplat-x86-pc/src/boot.rs:39-42`

```rust
// Header (16 bytes), address tag (24 bytes), entry tag (16 bytes including
// alignment padding), and end tag (8 bytes).
#[cfg(all(not(test), target_os = "none"))]
const MULTIBOOT2_HEADER_LENGTH: u32 = 16 + 24 + 16 + 8;
```

`[V]` matching assembly — `crates/ax/thekernel-axplat-x86-pc/src/multiboot.S:24-53` emits
exactly four tags (magic/arch/length/checksum header, type 2 address tag, type 3 entry
tag, type 0 end tag). No optional tag of type 8 is requested.

GRUB nevertheless already advertises video support to the loader:

`[V]` `scripts/build-x86-uefi-esp.sh:191-196`

```bash
# EFI cannot provide the legacy EGA text console that Multiboot defaults to.
# Keep GRUB itself on serial, but make the firmware framebuffer available to
# the Multiboot loader so it can hand the kernel a usable graphics console.
grub_modules='part_gpt fat search search_fs_file serial terminal all_video'
if [[ "$mode" == multiboot || "$mode" == multiboot-drive ]]; then
    grub_modules="$grub_modules multiboot2"
```

`[V]` `config/x86_64/grub.cfg:7` — `insmod all_video`.

However `[V]` `config/x86_64/grub.cfg:4-6` keeps GRUB on serial and never selects a
graphics mode:

```
serial --unit=0 --speed=115200 --word=8 --parity=no --stop=1
terminal_input serial
terminal_output serial
```

`[I]` Whether GRUB emits a type-8 tag without an explicit `gfxmode`/`gfxpayload` is a GRUB
behaviour question; see §7.1 assumption A3. The design must not depend on it: the change
set adds explicit `set gfxmode=` / `set gfxpayload=keep` and a `gfxterm`-free but
video-initialised path.

### 1.3 Exact byte layout of Multiboot2 tag type 8

This is **specification-derived** (`[X]`), with the byte-for-byte reference taken from the
`multiboot2` crate source that is present in the local Cargo registry
(`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/multiboot2-0.24.1/src/framebuffer.rs`).
That crate is *not* a dependency of this repository — `[V]` `Cargo.lock` has no
`multiboot2` entry (`grep -n "multiboot2" Cargo.lock` → no output), and the only
Multiboot crate in the lock is `multiboot 0.8.0` (`Cargo.lock:2140-2146`), which is the
Multiboot**1** parser used at `crates/ax/thekernel-axplat-x86-pc/src/mem.rs:36-52`.

Base tag (`[X]` `multiboot2-0.24.1/src/framebuffer.rs:63-96`, `:226-235`):

```rust
pub struct FramebufferTag {
    header: TagHeader,          // u32 type, u32 size
    address: u64,               // framebuffer physical address
    pitch: u32,                 // pitch in bytes
    width: u32,                 // pixels
    height: u32,                // pixels
    bpp: u8,                    // bits per pixel
    framebuffer_type: FramebufferTypeId,
    _padding: u16,
    buffer: [u8],               // type-dependent trailing data
}
```

```rust
    const BASE_SIZE: usize = mem::size_of::<TagHeader>()   // 8
        + mem::size_of::<u64>()                            // 8
        + 3 * mem::size_of::<u32>()                        // 12
        + 2 * mem::size_of::<u8>()                         // 2
        + mem::size_of::<u16>();                           // 2  => 32
```

| Offset | Size | Field | Notes |
|---:|---:|---|---|
| 0 | 4 | `type` = **8** | `[X]` `multiboot2-0.24.1/src/tag_type.rs:88-89` `/// Tag \`8\`: Framebuffer.` `Framebuffer,`; `:177` `8 => Self::Framebuffer,` |
| 4 | 4 | `size` | total tag size; **≥ 32** (base) and 8-byte aligned |
| 8 | 8 | `framebuffer_addr` | physical address. `[X]` `framebuffer.rs:66-71`: *"This field is 64-bit wide but bootloader should set it under 4GiB if possible for compatibility with payloads which aren't aware of PAE or amd64."* |
| 16 | 4 | `framebuffer_pitch` | bytes per scan line |
| 20 | 4 | `framebuffer_width` | pixels (characters when `type`=2) |
| 24 | 4 | `framebuffer_height` | pixels (characters when `type`=2) |
| 28 | 1 | `framebuffer_bpp` | bits per pixel (16 when `type`=2) |
| 29 | 1 | `framebuffer_type` | see below |
| 30 | 2 | `reserved` | must be ignored on read |
| 32 | var | type-dependent payload | present only when `type` = 0 or 1 |

**`framebuffer_type` values** (`[X]` `framebuffer.rs:276-294`):

```rust
pub enum FramebufferTypeId {
    Indexed = 0,
    RGB = 1,
    Text = 2,
    // spec says: there may be more variants in the future
}
```

| Value | Name | Meaning | Trailing payload |
|---:|---|---|---|
| 0 | Indexed | Palette-indexed pixels. | `u16 palette_num_colors` then `palette_num_colors` × 3 bytes RGB (`[X]` `framebuffer.rs:177-194`) |
| 1 | RGB | Direct colour. | 6 bytes: red position, red mask size, green position, green mask size, blue position, blue mask size — positions are the LSB bit index (`[X]` `framebuffer.rs:196-217`, comment at `:197` *"These refer to the bit positions of the LSB of each field"*) |
| 2 | EGA text | Character cells, not pixels. `[X]` `framebuffer.rs:325-331`: *"In this case the framebuffer width and height are expressed in characters and not in pixels. The bpp is equal 16 (16 bits per character) and pitch is expressed in bytes per text line."* | none |

**Design consequence:** only `framebuffer_type == 1` (RGB) is usable as a linear
framebuffer for a scanout. Values 0 and 2 must be parsed and then **declined** (not
panicked on) so that a BIOS/CSM or pure-text boot still produces a working kernel.

### 1.4 Where the parsed framebuffer info must be stored

**Existing handoff structures** (there are exactly two, and they are layered):

| # | Structure | Location | Role |
|---|---|---|---|
| 1 | `EarlyBootRecord { magic, info_paddr }` | `crates/ax/thekernel-axplat-x86-pc/src/boot.rs:50-55`, static at `:59-64` | Raw boot arguments surviving the `.bss` clear. `[V]` `:57-58` *"rust_entry runs before axruntime clears .bss. Keep this record in the initialized data segment so the raw boot arguments survive that clear."* |
| 2 | `BootInfo` (private) owned by `static BOOT_INFO: LazyInit<BootInfo>` | `crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs:88-98`, static at `:134`, accessor `get()` at `:136-140` | The sole owner of post-handoff platform data. `[V]` `:88` `/// The sole owner of boot protocol data after early handoff.` |

`[V]` `crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs:88-98`

```rust
/// The sole owner of boot protocol data after early handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BootInfo {
    protocol: BootProtocol,
    info_paddr: usize,
    rsdp: Option<AcpiRsdp>,
    memory_regions: [RawRange; MAX_REGIONS],
    memory_region_count: usize,
    modules: [Option<ModuleInfo>; MAX_MODULES],
    module_count: usize,
}
```

`AcpiRsdp` is the precedent to copy: it is a **fixed-size owned copy** copied out of the
bootloader-owned bytes while they are still borrowed. `[V]`
`crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs:36-45`

```rust
/// The subset of the ACPI RSDP needed to locate the root table.
///
/// The full RSDP is validated while the bootloader-owned bytes are still
/// borrowed.  Only this fixed-size copy is retained, so no platform code ever
/// holds a reference into the Multiboot information block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AcpiRsdp {
    bytes: [u8; 36],
    length: usize,
}
```

**The design adds a third field, `framebuffer: Option<BootFramebuffer>`, to `BootInfo`**,
with a `#[derive(Clone, Copy)]` POD struct holding exactly the validated tag fields, and a
`pub(crate) fn framebuffer(&self) -> Option<&BootFramebuffer>` accessor alongside
`rsdp()` (`:121-123`), `memory_regions()` (`:125-127`) and `modules()` (`:129-131`).

**The cross-layer handoff (platform → display driver) does not exist yet and must be
created.** The platform crate is `thekernel-axplat-x86-pc`; the consumer would be a driver
in `thekernel-axdriver*`. The established pattern for that direction is the
`thekernel-axhal` façade, and the boot-module accessor is the exact precedent:

`[V]` `crates/ax/thekernel-axhal/src/lib.rs:57-65`

```rust
pub mod dtb;
/// Immutable boot-module metadata supplied by the selected platform.
pub mod boot {
    /// Returns the physical range of the Multiboot module explicitly marked
    /// `rootfs`, if one was supplied by the bootloader.
    #[cfg(all(target_os = "none", feature = "defplat"))]
    pub fn rootfs_module() -> Option<(usize, usize)> {
        axplat_x86_pc::boot_modules()
```

which is `[V]` consumed at `crates/ax/thekernel-axdriver/src/lib.rs:519`
(`let Some((start, end)) = axhal::boot::rootfs_module() else {`). That is the only existing
"bootloader handed us a physical region, a driver consumes it" path in the tree, and the
framebuffer must reuse it.

### 1.5 What happens today on parse failure, and how to add a tag without weakening it

Today, **any** parse failure is fatal and unconditional:

`[V]` `crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs:154-159`

```rust
            let bytes = unsafe { multiboot2_info_bytes(info_paddr) }.unwrap_or_else(|| {
                panic!("invalid Multiboot2 information pointer {info_paddr:#x}")
            });
            parse_multiboot2_info(bytes, info_paddr)
                .unwrap_or_else(|error| panic!("invalid Multiboot2 information: {error:?}"))
```

The parser enforces, in order: 8-byte alignment of `info_paddr` and of every tag cursor
(`:365-367`, `:396-399`), `16 ≤ total_size ≤ 16 MiB` (`:19`, `:371-377`), `total_size`
8-aligned (`:378-380`), reserved header word zero (`:385-387`), `tag_size >= 8` (`:406-408`),
`tag_size <= remaining` (`:409-411`), padded tag size within bounds (`:412-415`), the end
tag present, 8 bytes long, alone and last (`:419-425`, `:468-470`), an mmap tag present
with at least one usable region (`:471-473`), and every module range page-aligned,
non-overlapping and inside usable RAM (`:474-479`). Its host-test coverage is extensive
(`:554-798`).

**Rules for adding tag 8 (must be followed literally):**

1. Add `const MB2_TAG_FRAMEBUFFER: u32 = 8;` to the constant block at `:24-28`. Do **not**
   renumber or reuse the existing constants; `:24-28` is the whole known-tag set.
2. Add **one** arm `MB2_TAG_FRAMEBUFFER => { ... }` immediately before `_ => {}` at `:462`.
   The existing catch-all must stay, because unknown tags currently succeed and tests
   depend on it — e.g. `valid_info()` at `:536-543` pushes tag 15 and tag 0 only, while
   `parser_copies_mmap_and_new_rsdp` (`:554-566`) asserts success.
3. The new arm must **not** return `Err` for a merely unusable framebuffer (type 0 or 2,
   zero dimensions, `bpp` not in {15,16,24,32}, unaligned or overlapping pitch). It must
   store `None`. Rationale: a bootloader that offers an EGA-text or indexed framebuffer is
   a supported configuration today (the kernel boots to serial), and turning it into a
   `panic!` at `:158` would be a regression. Only *structurally impossible* tags
   (`tag_size < 32` when `type == 8`) should be rejected, and even that can be folded into
   the existing `ParseError::TagTooSmall`-style handling by returning a dedicated
   `ParseError::FramebufferTagTruncated` variant added to the enum at `:184-214`.
4. A duplicate type-8 tag must be handled deterministically. The mmap arm's precedent is
   `ParseError::DuplicateMemoryMap` (`:196`, `:427-430`); for the framebuffer the design
   chooses **first wins, later duplicates ignored**, because the ACPI arm already
   demonstrates tolerance for firmware emitting redundant tags (`:449-461`).
5. Validation to apply before storing: `size >= 32`; `width > 0 && height > 0`;
   `pitch >= width * bytes_per_pixel`; `bpp ∈ {15,16,24,32}`; `framebuffer_type ∈ {0,1,2}`;
   and for `type == 1`, `size >= 38` with the six colour bytes read from offset 32.
   Any failure → store `None`, do not error.
6. Add host tests mirroring the existing style at `:554-798`, using the `push_tag` helper
   at `:494-502`, for: valid RGB tag; truncated tag; indexed tag; EGA-text tag; duplicate
   tags; and "valid mmap + valid RGB tag together" (proving the arm does not disturb
   `memory_regions()`).

---

## 2. Boot memory map and framebuffer addressability

### 2.1 Boot page tables

They are built statically in assembly and loaded before long mode is entered.

`[V]` `crates/ax/thekernel-axplat-x86-pc/src/multiboot.S:65-72`

```asm
    # set PAE, PGE bit in CR4
    mov     eax, {cr4}
    mov     cr4, eax

    # load the temporary page table
    lea     eax, [.Ltmp_pml4 - {offset}]
    mov     cr3, eax
```

`[V]` PML4 — `crates/ax/thekernel-axplat-x86-pc/src/multiboot.S:153-160`

```asm
.balign 4096
.Ltmp_pml4:
    # 0x0000_0000 ~ 0x7f_ffff_ffff (512 GiB)
    .quad .Ltmp_pdpt_low - {offset} + 0x3   # PRESENT | WRITABLE | paddr(tmp_pdpt)
    .zero 8 * 255
    # 0xffff_8000_0000_0000 ~ 0xffff_807f_ffff_ffff (512 GiB)
    .quad .Ltmp_pdpt_high - {offset} + 0x3  # PRESENT | WRITABLE | paddr(tmp_pdpt)
    .zero 8 * 255
```

`[V]` the two PDPTs, each 512 × 1 GiB huge pages — `crates/ax/thekernel-axplat-x86-pc/src/multiboot.S:162-175`

```asm
# FIXME: may not work on macOS using hvf as the CPU does not support 1GB page (pdpe1gb)
.Ltmp_pdpt_low:
.set i, 0
.rept 512
    .quad 0x40000000 * i | 0x83  # PRESENT | WRITABLE | HUGE_PAGE | paddr(0x4000_0000 * i)
    .set i, i + 1
.endr

.Ltmp_pdpt_high:
.set i, 0
.rept 512
    .quad 0x40000000 * i | 0x83  # PRESENT | WRITABLE | HUGE_PAGE | paddr(0x4000_0000 * i)
    .set i, i + 1
.endr
```

**Extent:** identity `0 .. 512 GiB` and `PHYS_VIRT_OFFSET .. PHYS_VIRT_OFFSET + 512 GiB`,
all present, writable, 4 KiB-page… actually 1 GiB-page, **cacheable WB** (no PCD/PWT bits).
`PHYS_VIRT_OFFSET` is `0xffff_8000_0000_0000` (`[V]`
`crates/ax/thekernel-axplat-x86-pc/axconfig.toml:25`, and the product profile
`config/x86_64/q35-uefi.toml:15`).

### 2.2 Would a typical UEFI GOP framebuffer address be covered? — precise answer

**During early boot: yes. After `axmm::init_memory_management()`: only if the address is
inside RAM or inside the compile-time `mmio-ranges` list.**

**Early boot (yes).** The boot PDPTs map the entire first 512 GiB, so any GOP BAR up to
`0x7f_ffff_ffff` is reachable both identity-mapped and via `phys_to_virt`. A typical Intel
N305 GOP framebuffer BAR (whether at `0x8000_0000`, `0xC000_0000`, or in a 64-bit window
below 512 GiB) is covered by `[V]` `multiboot.S:163-168`.

**After the switch (no, unless listed).** The runtime replaces CR3 with a brand-new,
sparse hierarchy:

`[V]` `crates/ax/thekernel-axmm/src/lib.rs:88-105`

```rust
pub fn init_memory_management() {
    info!("Initialize virtual memory management...");

    let kernel_aspace = new_kernel_aspace().expect("failed to initialize kernel address space");
    debug!("kernel address space init OK: {kernel_aspace:#x?}");
    KERNEL_ASPACE.init_once(SpinNoIrq::new(kernel_aspace));
    unsafe {
        // KERNEL_ASPACE is static and owns this hierarchy until shutdown.
        // Bind task creation before the scheduler can publish any workers.
        axhal::asm::bind_kernel_task_page_table_root(kernel_page_table_root());
        axhal::asm::write_kernel_page_table(kernel_page_table_root());
        axhal::asm::flush_tlb(None);
    }
}
```

(It is called at `[V]` `crates/ax/thekernel-axruntime/src/lib.rs:424` —
`axmm::init_memory_management();` — and the CR3 write is `[V]`
`crates/ax/thekernel-axcpu/src/x86_64/asm.rs:1410-1412`
`pub unsafe fn write_kernel_page_table(root_paddr: PhysAddr) {` /
`    unsafe { write_user_page_table(root_paddr) }`.)

What the new table maps is exactly the region list:

`[V]` `crates/ax/thekernel-axmm/src/lib.rs:51-68`

```rust
/// Creates a new address space for kernel itself.
pub fn new_kernel_aspace() -> AxResult<AddrSpace> {
    let (base, size) = axhal::mem::kernel_aspace();
    let mut aspace = AddrSpace::new_empty(base, size)?;
    for r in axhal::mem::memory_regions() {
        // mapped range should contain the whole region if it is not aligned.
        let start = r.paddr.align_down_4k();
        let end = (r.paddr + r.size).align_up_4k();
        aspace.map_linear(
            phys_to_virt(start),
            start,
            end - start,
            reg_flag_to_map_flag(r.flags),
        )?;
    }
    Ok(aspace)
}
```

and that list includes MMIO ranges as **device** regions:

`[V]` `crates/ax/thekernel-axhal/src/mem.rs:56-59`

```rust
    // Push MMIO & reserved regions
    for &(start, size) in mmio_ranges() {
        push(PhysMemRegion::new_mmio(start, size, "mmio"));
    }
```

with `[X]` `axplat-0.3.1-pre.6/src/mem.rs:45-48`

```rust
/// The default flags for a MMIO region (readable, writable, device, and reserved).
pub const DEFAULT_MMIO_FLAGS: MemRegionFlags = MemRegionFlags::READ
    .union(...)
    .union(MemRegionFlags::DEVICE)
```

Evidence: `[V]` the product q35 profile *does* include the 32-bit PCI BAR window and a
64-bit window —

`config/x86_64/q35-uefi.toml:26-41`

```toml
mmio-ranges = [
    [0x8000_0000, 0x2000_0000],       # Fallback 32-bit PCI BAR allocation
    [0xe000_0000, 0x1000_0000],       # QEMU 10.2.2/OVMF Q35 PCIe ECAM (buses 0-255)
    [0xfec0_0000, 0x0000_1000],       # I/O APIC
    [0xfed0_0000, 0x0000_1000],       # HPET
    [0xfee0_0000, 0x0000_1000],       # Local APIC
    ["0x0000_00c0_0000_0000", 0x0100_0000], # Fallback 64-bit PCI BAR allocation
] # [(uint, uint)]
...
pci-ranges = [
    [0, 0],
    [0x8000_0000, 0x2000_0000],
    ["0x0000_00c0_0000_0000", 0x0100_0000],
] # [(uint, uint)]
```

and the profile's own comment states the direct-map extent:

`[V]` `config/x86_64/q35-uefi.toml:19-20`

```toml
# Four 4 KiB per-CPU secret mapping slots, immediately below the exclusive
# kernel-aspace end.  The direct map occupies only phys-memory-size at its base.
```

— but the **bare platform default** does not contain `0x8000_0000`:

`[V]` `crates/ax/thekernel-axplat-x86-pc/axconfig.toml:44-51`

```toml
# MMIO ranges with format (`base_paddr`, `size`).
mmio-ranges = [
    [0xb000_0000, 0x1000_0000], # PCI config space
    [0xfe00_0000, 0xc0_0000],   # PCI devices
    [0xfec0_0000, 0x1000],      # IO APIC
    [0xfed0_0000, 0x1000],      # HPET
    [0xfee0_0000, 0x1000],      # Local APIC
]                               # [(uint, uint)]
```

`[I]` **Conclusion for the N305 target:** a firmware framebuffer is *not* reliably in the
runtime direct map. The design must **explicitly map it** rather than relying on the
existing linear map, and must do so on every profile.

### 2.3 The runtime iomap facility

`[V]` Exact signature, flags and duplicate tolerance — `crates/ax/thekernel-axmm/src/lib.rs:108-129`

```rust
/// Maps a physical memory region to virtual address space for device access.
pub fn iomap(addr: PhysAddr, size: usize) -> AxResult<VirtAddr> {
    let virt = phys_to_virt(addr);

    let virt_aligned = virt.align_down_4k();
    let addr_aligned = addr.align_down_4k();
    let size_aligned = (addr + size).align_up_4k() - addr_aligned;

    let flags = MappingFlags::DEVICE | MappingFlags::READ | MappingFlags::WRITE;
    let mut tb = kernel_aspace().lock();
    match tb.map_linear(virt_aligned, addr_aligned, size_aligned, flags) {
        Err(AxError::AlreadyExists) => {}
        Err(e) => {
            return Err(e);
        }
        Ok(_) => {}
    }
    // flush TLB
    // FIXME: remove this
    tb.protect(virt_aligned, size_aligned, flags)?;
    Ok(virt)
}
```

| Property | Value | Evidence |
|---|---|---|
| Signature | `pub fn iomap(addr: PhysAddr, size: usize) -> AxResult<VirtAddr>` | `crates/ax/thekernel-axmm/src/lib.rs:109` |
| Returned VA | `phys_to_virt(addr)` = `addr + PHYS_VIRT_OFFSET` | `:110`; `crates/ax/thekernel-axplat-x86-pc/src/mem.rs:109-111` |
| Flags | `MappingFlags::DEVICE \| READ \| WRITE` | `:116` |
| Tolerates existing mapping | **Yes** — `Err(AxError::AlreadyExists) => {}` | `:119` |
| Always reprotects | `tb.protect(virt_aligned, size_aligned, flags)?` | `:127` |
| Alignment | both ends rounded to 4 KiB | `:112-114` |

The `axklib` façade used by the `dyn` driver path forwards to the same function:

`[V]` `crates/ax/thekernel-axruntime/src/klib.rs:23-28`

```rust
        /// This function forwards the request to `axmm::iomap` and returns the
        /// resulting virtual address wrapped in an `AxResult`.
        fn mem_iomap(addr: PhysAddr, size: usize) -> AxResult<VirtAddr> {
            // Convert from AxError (struct in axerrno 0.2) to AxErrorKind (enum used by axklib)
            axmm::iomap(addr, size)
        }
```

`[V]` consumed as `axklib::mem::iomap` at `crates/ax/thekernel-axdriver/src/dyn_drivers/mod.rs:18-25`.

`[I]` The `AlreadyExists` tolerance plus the unconditional `protect` means `iomap` is
**idempotent and safe to call on a range that is already linearly mapped as RAM** — but it
would also *downgrade* an existing WB RAM mapping to UC. The design must therefore never
`iomap` a range that overlaps usable RAM (the §1.5 validation should already reject such a
tag, and §2.4 adds an explicit `phys_ram_ranges()` overlap check).

### 2.4 How the framebuffer must be mapped; existing cache-attribute control

**Current attribute model.** `MappingFlags` is a generic bitflags type
(`[V]` `crates/ax/thekernel-page-table-entry/src/lib.rs:12-39`):

```rust
        /// The memory is device memory.
        const DEVICE        = 1 << 4;
        /// The memory is uncached.
        const UNCACHED      = 1 << 5;
```

On x86_64 **both** collapse to the same PTE encoding:

`[V]` `crates/ax/thekernel-page-table-entry/src/arch/x86_64.rs:95-97`

```rust
        if f.contains(MappingFlags::DEVICE) || f.contains(MappingFlags::UNCACHED) {
            ret |= Self::NO_CACHE | Self::WRITE_THROUGH;
        }
```

i.e. `PCD=1, PWT=1` → the **UC** memory type. With the reset value of `IA32_PAT` this is
strong uncached.

**There is no write-combining support of any kind.** Searches performed (whole worktree,
`--include=*.rs`):

| Search | Result |
|---|---|
| `grep -rnw "PAT\|PAT_MSR\|IA32_PAT\|MTRR\|write_combining\|WriteCombining\|WC\b" --include=*.rs crates/ kernel/` | **NOT FOUND** (no output) |
| `grep -rn "0x277\|0x1a0\|MSR_PAT" --include=*.rs crates/ kernel/` | **NOT FOUND** (no output) |
| `grep -rn "MTRR\|mtrr" --include=*.rs .` (tree-wide) | **NOT FOUND** (no output) |

So: the kernel never programs `IA32_PAT`, never programs MTRRs, and has no
`MappingFlags` bit that can produce a WC mapping.

**Required design.** Two options, in order of preference:

*Option 1 (recommended, smallest, no new page-attribute semantics).* Map the framebuffer
with the existing `axmm::iomap` (`DEVICE|READ|WRITE` → UC). This is **correct** on every
platform, needs no PAT work, and is the only option whose correctness is provable from the
code that exists today. Its cost is throughput: `[I]` a full 1920×1080×4 repaint is ~8.3 MB
of UC stores, and the existing fbcon already coalesces repaints to at most one per 33 ms
(`kernel/src/pseudofs/dev/tty/fbcon.rs:116-120`) and draws glyph-by-glyph
(`kernel/src/pseudofs/dev/fb.rs:663-686`).

*Option 2 (write-combining, a follow-up).* Add a new `MappingFlags` bit
(e.g. `WRITE_COMBINE = 1 << 11`, free — the highest currently used is `SHADOW_STACK = 1 << 10`
at `crates/ax/thekernel-page-table-entry/src/lib.rs:38`), program `IA32_PAT` entry 1 to
`WC` during early per-CPU init, and encode the flag as `PWT=0, PCD=0` **plus** a PAT bit.
`[I]` This is a real, self-contained project: the x86_64 `X64PTE` type
(`crates/ax/thekernel-page-table-entry/src/arch/x86_64.rs:102-105`) has no PAT-bit accessor
today, and PAT must be set identically on every CPU before any WC mapping is used. The
design therefore gates Option 2 behind a feature and ships Option 1 first.

**Additional required guard (both options).** Before mapping, verify the tag's
`[addr, addr + pitch*height)` does not intersect `axhal::mem::phys_ram_ranges()`
(`[V]` `crates/ax/thekernel-axhal/src/mem.rs:4` re-exports `phys_ram_ranges`), because
`iomap`'s unconditional `protect` at `crates/ax/thekernel-axmm/src/lib.rs:127` would
silently make RAM uncached. The overlap primitives already exist: `ranges_difference` /
`check_sorted_ranges_overlap` are imported at `crates/ax/thekernel-axhal/src/mem.rs:7`.

---

## 3. Display driver architecture

### 3.1 The interface: exact name and every method

The trait is **`DisplayDriverOps`**, defined at
`crates/ax/thekernel-axdriver-display/src/lib.rs:383`, with supertrait
`BaseDriverOps` (`:11` re-exports it from `axdriver_base`).

`[V]` Full method list, quoted verbatim from
`crates/ax/thekernel-axdriver-display/src/lib.rs:378-429`:

```rust
/// Operations required by display drivers.
///
/// The DRM methods transfer sole scanout ownership from the legacy framebuffer
/// user to a driver that supports caller-owned pinned backing. Drivers that do
/// not implement this transport remain usable through the framebuffer API.
pub trait DisplayDriverOps: BaseDriverOps {
    fn pci_identity(&self) -> Option<DisplayPciIdentity> {
        None
    }
    fn set_pci_identity(&mut self, _identity: DisplayPciIdentity) {}

    fn info(&self) -> DisplayInfo;
    fn fb(&self) -> FrameBuffer<'_>;
    fn need_flush(&self) -> bool;
    fn flush(&mut self) -> DevResult;

    fn supports_drm_transport(&self) -> bool {
        false
    }

    /// A non-virgl display returns `None`; its 2D fallback remains usable.
    fn render_transport(&mut self) -> Option<&mut dyn GpuTransport> {
        None
    }

    /// Unified asynchronous DRM submission path.  The driver owns every DMA
    /// request and response until the corresponding completion is drained.
    fn drm_submit(
        &mut self,
        _queue: GpuQueue,
        _batch: GpuBatch,
        _fence_id: u64,
    ) -> DevResult<GpuSubmission> {
        Err(DevError::Unsupported)
    }
    fn drm_drain_completions(
        &mut self,
        _queue: GpuQueue,
        _out: &mut [GpuCompletion],
    ) -> DevResult<usize> {
        Ok(0)
    }
    fn drm_reset(&mut self, _queue: GpuQueue, _out: &mut [GpuCompletion]) -> usize {
        0
    }
    /// Acknowledge and consume one display-config notification.  The returned
    /// mode is sampled after acknowledgement, never reconstructed from stale
    /// framebuffer state.
    fn drm_display_config_changed(&mut self) -> DevResult<Option<DrmDisplayConfig>> {
        Ok(None)
    }
}
```

Inherited from `BaseDriverOps`, re-exported at
`crates/ax/thekernel-axdriver-display/src/lib.rs:11` and defined at
`crates/ax/thekernel-axdriver-base/src/lib.rs:75-84`:

```rust
pub trait BaseDriverOps: Send + Sync {
    fn device_name(&self) -> &str;
    fn device_type(&self) -> DeviceType;
    fn irq_num(&self) -> Option<usize> {
```

(all three used by `StaticBlockDevice` at
`crates/ax/thekernel-axdriver/src/structs/static.rs:34-61`).

| Method | Required? | Implementable by a linear-framebuffer driver? |
|---|---|---|
| `pci_identity` / `set_pci_identity` | defaulted | **No** — a firmware framebuffer has no owning PCI function in the kernel's model; leave `None` (default at `:384-387`). |
| `info() -> DisplayInfo` | **required** | **Yes** — from the Multiboot2 tag. |
| `fb() -> FrameBuffer<'_>` | **required** | **Yes, but currently useless** — see §3.5. |
| `need_flush() -> bool` | **required** | **Yes** — `false` (the framebuffer is the scanout). |
| `flush() -> DevResult` | **required** | **Yes** — `Ok(())` no-op, or a fence. |
| `supports_drm_transport()` | defaulted `false` | **No, and must stay `false`** — returning `true` is what hands the device to `virtio::init()` (`kernel/src/drm/virtio.rs:2438-2441`), which only works for virtio-gpu. |
| `render_transport()` | defaulted `None` | No. |
| `drm_submit` / `drm_drain_completions` / `drm_reset` / `drm_display_config_changed` | defaulted | No. |

### 3.2 CRITICAL: is there any mode-setting verb?

**No. There is no mode-setting verb anywhere in `DisplayDriverOps`, and none in
`DisplayInfo`.**

`[V]` The complete method list quoted in §3.1 contains no `set_mode`, `modeset`,
`set_resolution`, `mode`, `preferred_mode`, or equivalent. The only mode-shaped data is
the plain value struct:

`[V]` `crates/ax/thekernel-axdriver-display/src/lib.rs:26-32`

```rust
#[derive(Debug, Clone, Copy)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    pub fb_base_vaddr: usize,
    pub fb_size: usize,
}
```

— read-only fields, no pitch, no pixel format, no setter.

`[I]` Consequences for the design:

* A firmware-framebuffer driver needs **no** mode-setting verb, because the mode is chosen
  by the firmware and reported in the Multiboot2 tag. It *implements* `info()` from the
  tag. This is the one place where the missing verb is harmless.
* Any future requirement to change resolution (e.g. a VT switch to a bigger mode, or a
  userspace `FBIOPUT_VSCREENINFO` with new timings) **cannot** be expressed against this
  trait. Adding it means either (a) a new `fn set_mode(&mut self, info: DisplayInfo) -> DevResult`
  on `DisplayDriverOps` with a default `Err(DevError::Unsupported)` — cheap, but every
  existing implementor (virtio-gpu at `crates/ax/thekernel-axdriver-virtio/src/gpu.rs:70`,
  the dummy at `crates/ax/thekernel-axdriver/src/dummy.rs:88`) must still compile, which a
  defaulted method guarantees — or (b) leaving mode setting entirely to the DRM layer,
  which is what happens today. The design chooses **(b)**: the fbcon path never changes
  the mode.

### 3.3 How a driver is registered and discovered

**Build-time macros.**

`[V]` `crates/ax/thekernel-axdriver/src/macros.rs:21-27`

```rust
macro_rules! register_display_driver {
    ($driver_type:ty, $device_type:ty) => {
        /// The unified type of the NIC devices.
        #[cfg(not(feature = "dyn"))]
        pub type AxDisplayDevice = $device_type;
    };
}
```

`[V]` the dispatch macro's display arm — `crates/ax/thekernel-axdriver/src/macros.rs:73-77`

```rust
        #[cfg(display_dev = "virtio-gpu")]
        {
            type $drv_type = <virtio::VirtIoGpu as VirtIoDevMeta>::Driver;
            $code
        }
```

`[V]` the `display_dev` cfg is generated by the build script —
`crates/ax/thekernel-axdriver/build.rs:3` (`const DISPLAY_DEV_FEATURES: &[&str] = &["virtio-gpu"];`)
and `:39-58`, which emits `display_dev="virtio-gpu"` when the feature is on and
`display_dev="dummy"` when `display` is on with no listed device.

`[V]` the actual registration site — `crates/ax/thekernel-axdriver/src/drivers.rs:52-56`

```rust
#[cfg(display_dev = "virtio-gpu")]
register_display_driver!(
    <virtio::VirtIoGpu as VirtIoDevMeta>::Driver,
    <virtio::VirtIoGpu as VirtIoDevMeta>::Device
);
```

**Runtime discovery path (static model, which is what the product builds).**

1. `AllDevices::probe()` iterates the compile-time driver list —
   `[V]` `crates/ax/thekernel-axdriver/src/lib.rs:495-512`

```rust
        #[cfg(not(feature = "dyn"))]
        {
            #[cfg(feature = "rootfs-module")]
            self.probe_rootfs_module();

            for_each_drivers!(type Driver, {
                if let Some(dev) = Driver::probe_global() {
                    info!(
                        "registered a new {:?} device: {:?}",
                        dev.device_type(),
                        dev.device_name(),
                    );
                    self.add_device(dev);
                }
            });

            self.probe_bus_devices();
        }
```

2. `probe_bus_devices` lives on `AllDevices` in the bus module —
   `[V]` `crates/ax/thekernel-axdriver/src/bus/pci.rs:389-390`
   (`pub(crate) fn probe_bus_devices(&mut self) {` / `let mut root = pci_root();`), which
   walks every reachable PCI function and calls each driver's `probe_pci` —
   `[V]` `crates/ax/thekernel-axdriver/src/bus/pci.rs:430-431`
   (`for_each_drivers!(type Driver, {` / `match Driver::probe_pci(root, bdf, dev_info) {`).
   (The MMIO variant is `crates/ax/thekernel-axdriver/src/bus/mmio.rs:5-26`.)
3. The `DriverProbe` trait with its three defaulted hooks is
   `[V]` `crates/ax/thekernel-axdriver/src/drivers.rs:20-38`.
4. Devices are accumulated in `AxDeviceContainer<AxDisplayDevice>` —
   `[V]` `crates/ax/thekernel-axdriver/src/lib.rs:466-468`
   (`/// All graphics device drivers.` / `#[cfg(feature = "display")]` / `pub display: AxDeviceContainer<AxDisplayDevice>,`)
   — a `SmallVec<[D; 1]>` (`crates/ax/thekernel-axdriver/src/structs/mod.rs:73`).
5. `thekernel-axdisplay` consumes it and keeps **exactly one** device —
   `[V]` `crates/ax/thekernel-axdisplay/src/lib.rs:14` and `:16-24`:

```rust
static MAIN_DISPLAY: LazyInit<Mutex<Option<AxDisplayDevice>>> = LazyInit::new();

pub fn init_display(mut display_devs: AxDeviceContainer<AxDisplayDevice>) {
    info!("Initialize display subsystem...");
    if let Some(dev) = display_devs.take_one() {
        info!("  use display device 0: {:?}", dev.device_name());
        MAIN_DISPLAY.init_once(Mutex::new(Some(dev)));
    } else {
        warn!("  No display device found!");
    }
}
```

6. Called from the runtime — `[V]` `crates/ax/thekernel-axruntime/src/lib.rs:454-455`
   (`#[cfg(feature = "display")]` / `axdisplay::init_display(all_devices.display);`).

### 3.4 CRITICAL: can the registry hold more than one display driver?

**No, not today — in either device model.**

**Static model (what the product builds).** `[V]` root `Cargo.toml:156`
(`x86-product = ["qemu", "smp", "hwp-uclamp", "pmu", "perf-sampling"]`) and `:166-169`
(`qemu = [` … `"axfeat/display",` …); `[V]` `crates/ax/thekernel-axfeat/Cargo.toml:22-28`
(`display = [` … `"axdriver/virtio-gpu",` …). The `dyn` feature appears **nowhere** in the
product feature graph — searching the root `Cargo.toml` and
`crates/ax/thekernel-axfeat/Cargo.toml` for `dyn` yields no hits.

Therefore `register_display_driver!` (`macros.rs:21-27`) expands to
`pub type AxDisplayDevice = <virtio::VirtIoGpu as VirtIoDevMeta>::Device;` — **one concrete
type for the whole kernel image**. A second invocation would define the same alias twice in
the same module → duplicate-definition compile error. The build script enforces the same
singularity from the other side: it emits **one** `display_dev` value, or `dummy`
(`crates/ax/thekernel-axdriver/build.rs:39-58`).

**What happens to the display type alias when `dyn` is enabled.** The alias becomes a trait
object:

`[V]` `crates/ax/thekernel-axdriver/src/structs/dyn.rs:11-13`

```rust
/// The unified type of the graphics display devices.
#[cfg(feature = "display")]
pub type AxDisplayDevice = Box<dyn DisplayDriverOps>;
```

and a boxing constructor exists:

`[V]` `crates/ax/thekernel-axdriver/src/structs/dyn.rs:34-38`

```rust
    /// Constructs a display device.
    #[cfg(feature = "display")]
    pub fn from_display(dev: impl DisplayDriverOps + 'static) -> Self {
        Self::Display(Box::new(dev))
    }
```

**But a runtime-dispatch path for display does not exist and would have to be written.**
The `dyn` probe function enumerates **block devices only**:

`[V]` `crates/ax/thekernel-axdriver/src/dyn_drivers/mod.rs:27-41`

```rust
pub fn probe_all_devices() -> Vec<super::AxDeviceEnum> {
    rdrive::probe_all(true).unwrap();
    #[allow(unused_mut)]
    let mut devices = Vec::new();
    #[cfg(feature = "block")]
    {
        let ls = rdrive::get_list::<rd_block::Block>();
        for dev in ls {
            devices.push(super::AxDeviceEnum::from_block(
                crate::dyn_drivers::blk::Block::from(dev),
            ));
        }
    }
    devices
}
```

There is no `rd_display`, no `get_list::<...>()` for a display type, and no display entry
in the `dyn` feature's dependency list:

`[V]` `crates/ax/thekernel-axdriver/Cargo.toml:50-59`

```toml
dyn = [
    "dep:axerrno",
    "dep:axhal",
    "dep:axklib",
    "dep:memory_addr",
    "dep:dma-api",
    "dep:rd-block",
    "dep:rdrive",
    "dep:spin",
]
```

(`dep:rd-block` present, no display equivalent.) A repository-wide search for
`rd_display`, `rd_gpu`, `get_list::<` returns only
`crates/ax/thekernel-axdriver/src/dyn_drivers/mod.rs:33` — `let ls = rdrive::get_list::<rd_block::Block>();`.
`[V]` A workspace dependency `rdif-display = { version = "0.2.0" }` is declared at
`Cargo.toml:127` but is **unused**: it has no `Cargo.lock` entry and no `use` site
(`grep -rn "rdif-display\|rdif_display"` → only `Cargo.toml:127`).

**And even in `dyn` mode only one display device survives**, because
`axdisplay::init_display` calls `take_one()` on a single `Option` slot
(`crates/ax/thekernel-axdisplay/src/lib.rs:18-20`, quoted in §3.3).

**Design decision for the framebuffer.** Do **not** attempt to enable `dyn`. Instead,
copy the pattern the block subsystem already uses for exactly this problem — a static
**enum** wrapping multiple concrete device types:

`[V]` `crates/ax/thekernel-axdriver/src/structs/static.rs:9-17`

```rust
#[cfg(feature = "block")]
pub enum StaticBlockDevice {
    /// A driver discovered through the normal global/MMIO/PCI probes.
    Existing(RegisteredStaticBlockDevice),
    #[cfg(feature = "usb-xhci")]
    Usb(crate::usb::UsbBlock),
    /// The immutable root filesystem module supplied by the bootloader.
    BootModule(axdriver_block::boot_module::BootModuleBlockDevice),
}
```

The display equivalent is a `StaticDisplayDevice { Virtio(..), BootFb(..) }` enum with a
delegating `DisplayDriverOps` impl, plus a rename of the macro-generated alias to
`RegisteredStaticDisplayDevice` (mirroring `register_block_driver!` at
`crates/ax/thekernel-axdriver/src/macros.rs:13-19`, which already emits
`RegisteredStaticBlockDevice` rather than `AxBlockDevice`). This keeps the static model,
adds no runtime dispatch, and is the smallest change that satisfies "more than one display
driver in one kernel image".

### 3.5 `fb()`, `need_flush()`, `flush()` — meaning and current state

`[V]` `FrameBuffer` — `crates/ax/thekernel-axdriver-display/src/lib.rs:65-67` and `:366-376`

```rust
pub struct FrameBuffer<'a> {
    _raw: &'a mut [u8],
}
```

```rust
impl<'a> FrameBuffer<'a> {
    pub unsafe fn from_raw_parts_mut(ptr: *mut u8, len: usize) -> Self {
        Self {
            _raw: core::slice::from_raw_parts_mut(ptr, len),
        }
    }

    pub fn from_slice(slice: &'a mut [u8]) -> Self {
        Self { _raw: slice }
    }
}
```

| Item | Meaning | Verified state |
|---|---|---|
| `fb()` | Returns a borrowed view of the driver's own linear framebuffer, `info().fb_size` bytes starting at `info().fb_base_vaddr`. | **`_raw` is private with no accessor, no `Deref`, no `AsRef`.** `grep -rn "_raw" crates/ax/thekernel-axdriver-display/src/lib.rs` returns only lines 66, 369, 374. The return value is therefore **unusable by any caller today** — a real gap the change set must close (add `as_slice`/`as_mut_slice`, or `Deref`). |
| `need_flush()` | Whether the driver's own framebuffer is a shadow that must be pushed to the device. | `[V]` virtio-gpu returns `true` (`crates/ax/thekernel-axdriver-virtio/src/gpu.rs:89-91`); the dummy returns `false` (`crates/ax/thekernel-axdriver/src/dummy.rs:95-97`). A firmware framebuffer returns **`false`**: the memory *is* the scanout. |
| `flush()` | Push the shadow to the device. | `[V]` virtio-gpu returns `Err(DevError::Unsupported)` (`crates/ax/thekernel-axdriver-virtio/src/gpu.rs:93-95`); the dummy likewise (`dummy.rs:98-100`). A firmware framebuffer returns `Ok(())`. |

`[V]` Note the DRM path deliberately zeroes the legacy framebuffer so nothing can double-own
the scanout — `crates/ax/thekernel-axdriver-virtio/src/gpu.rs:44-49`:

```rust
        let info = DisplayInfo {
            width,
            height,
            // DRM takes this device before devfs creates fb0. Do not create a
            // compatibility resource here: it would be a second scanout owner.
            fb_base_vaddr: 0,
            fb_size: 0,
        };
```

`[V]` and the legacy consumers are dead code with **zero** callers:
`crates/ax/thekernel-axdisplay/src/lib.rs:26` (`has_display`), `:44` (`framebuffer_info`),
`:52` (`framebuffer_flush`). A repository-wide grep for these three names returns only
their definitions and the `take_drm_display` call site at `kernel/src/drm/virtio.rs:2439`.
`[I]` `framebuffer_info()` would additionally `panic!` after the DRM handoff, because
`take_drm_display` empties the slot — `[V]` `crates/ax/thekernel-axdisplay/src/lib.rs:44-50`:

```rust
pub fn framebuffer_info() -> DisplayInfo {
    MAIN_DISPLAY
        .lock()
        .as_ref()
        .expect("display unavailable")
        .info()
}
```

**Therefore the "legacy framebuffer API" is not a viable plug-in point for the new
console.** The design must attach the boot framebuffer at the `DisplayCore`/fbdev layer
(§5), not through `axdisplay::framebuffer_info()`.

---

## 4. Kernel console routing

### 4.1 The printk implementation, and how output reaches COM1

There are **two distinct byte sinks** and it is essential not to conflate them.

| Sink | Port | Code | Reached by |
|---|---|---|---|
| Console UART | **COM1, `0x3f8`** | `crates/ax/thekernel-axplat-x86-pc/src/console.rs:7` `static COM1: SpinNoIrq<SerialPort> = unsafe { SpinNoIrq::new(SerialPort::new(0x3f8)) };` | `ConsoleIf::write_bytes` (`:30-34`) → `putchar` (`:11-13`); i.e. the **userspace TTY path only** |
| Diagnostic UART | **COM2, `0x2f8`** | `crates/ax/thekernel-axplat-x86-pc/src/console.rs:68-69` `#[cfg(target_os = "none")]` / `const DIAGNOSTIC_BASE: u16 = 0x2f8;` | `try_write_diagnostic_bytes` (`:179-195`) and `emergency_diagnostic_print` (`:200-219`); i.e. **all kernel logging** |

`[V]` `crates/ax/thekernel-axplat-x86-pc/src/console.rs:20-34`

```rust
pub fn init() {
    COM1.lock().init();
    init_diagnostic();
}

struct ConsoleIfImpl;

#[cfg_attr(target_os = "none", impl_plat_interface)]
impl ConsoleIf for ConsoleIfImpl {
    /// Writes given bytes to the console.
    fn write_bytes(bytes: &[u8]) {
        for c in bytes {
            putchar(*c);
        }
    }
```

The COM1 path is only reachable through the `axplat::console::ConsoleIf` crate interface,
re-exported as `axhal::console::write_bytes`:

`[V]` `crates/ax/thekernel-axhal/src/lib.rs:472-475`

```rust
    #[cfg(feature = "irq")]
    pub use axplat::console::irq_num;
    pub use axplat::console::{read_bytes, write_bytes};
```

`[V]` and its only users are the TTY layer:
`kernel/src/pseudofs/dev/tty/ntty.rs:93` (`axhal::console::write_bytes(buf);`) and
`kernel/src/pseudofs/dev/tty/vt.rs:1078` (same).

### 4.2 Exact path from `println!`/`print!` to a serial port

```
ax_println!(..)  /  info!(..) error!(..) warn!(..) debug!(..)
        │
        │  ax_println!  →  axlog::__print_impl → axlog::print_fmt → axlog::Logger::write_str
        │                 → crate_interface call LogIf::console_write_str
        │  info!(..) etc. → log crate → registered logger
        ▼
axruntime::klog::Logger::log                       crates/ax/thekernel-axruntime/src/klog.rs:342-369
        │  format!("<{prio}>[{secs}.{usec} cpu=.. tid=.. LEVEL target=.. module=..] {args}")  :354-366
        ▼
klog::append(text, level)                          :317-336   (bounded ring buffer, CAPACITY = 64 KiB, :9)
        ▼
DiagnosticDrain::drain_once()                      :444-446
        │  self.drain_with(axhal::console::try_write_diagnostic_bytes)   :445
        ▼
axplat_x86_pc::console::try_write_diagnostic_bytes  crates/ax/thekernel-axplat-x86-pc/src/console.rs:179-195
        ▼
write_diagnostic(...)  →  x86::io::outb(0x2f8, byte)   console.rs:152-162, :160
        ▼
COM2, base 0x2f8                                    console.rs:69
```

Step-by-step evidence:

* `[V]` `kernel/src/lib.rs:27-28` — `#[macro_use]` / `extern crate axlog;` brings the
  `ax_println!` macro into kernel scope (preceding `pub mod entry;` at `:32`).
* `[X]` `axlog-0.3.0-preview.2/src/lib.rs:61-72` defines `ax_print!`/`ax_println!` calling
  `$crate::__print_impl`; `:221-232` defines `print_fmt`/`__print_impl`; `:126-134`
  `Logger::write_str` performs `call_interface!(LogIf::console_write_str, s)`.
* `[V]` the `LogIf` implementation is `crates/ax/thekernel-axruntime/src/lib.rs:114-118`:

```rust
#[crate_interface::impl_interface]
impl axlog::LogIf for LogIfImpl {
    fn console_write_str(s: &str) {
        klog::record(s.as_bytes());
    }
```

* `[V]` `klog::record` re-enters the diagnostic funnel —
  `crates/ax/thekernel-axruntime/src/klog.rs:392-398`:

```rust
/// Legacy ax_print fragments are diagnostics, never terminal output.
pub fn record(bytes: &[u8]) {
    diagnostic(format_args!(
        "{}",
        core::str::from_utf8(bytes).unwrap_or("[invalid diagnostic UTF-8]")
    ));
}
```

* `[V]` the logger is installed by `klog::init`, not by `axlog::init` —
  `crates/ax/thekernel-axruntime/src/klog.rs:372-381`:

```rust
pub(crate) fn init(level: &str) {
    {
        let mut store = STORE.lock();
        store.supported = axhal::console::diagnostic_available();
        store.set_enabled(true);
    }
    let _ = set_filter(level);
    log::set_logger(&Logger).expect("kernel logger already installed");
    log::set_max_level(LevelFilter::Trace);
}
```

  `[V]` `axlog::init` and `axlog::set_max_level` are **never called** in this repository
  (`grep -rn "axlog::init\|axlog::set_max_level" --include=*.rs .` → no output).
* `[V]` the drain is driven by a dedicated deferred-work task —
  `kernel/src/deferred_work.rs:239-258`:

```rust
fn diagnostic_worker() {
    let mut drain = axruntime::klog::DiagnosticDrain::new();
    let mut retry_ms = 1;
    loop {
        if wait_with_bounded_retry(
            || wait_for_worker(&LOG_WORKER_WAKE, axruntime::klog::diagnostic_work_pending),
            axtask::yield_now,
        )
        .is_err()
        { ... }
        while axruntime::klog::diagnostic_work_pending() {
            let written = drain.drain_once();
```

`[I]` **Key consequence:** a no-serial N305 today shows **nothing at all** — not even the
boot banner — because every kernel message terminates at `outb(0x2f8, ..)`
(`console.rs:160`), and `store.supported` (`klog.rs:375`) is only true if COM2 answered
the scratch-register probe (`console.rs:82-93`).

### 4.3 Every place that must change for output to ALSO render on a framebuffer

`[I]` There is **no existing generic "console" abstraction** that a framebuffer backend can
plug into. The two candidate abstractions are both crate-interface singletons with exactly
one implementation slot: `axplat::console::ConsoleIf` (implemented once at
`crates/ax/thekernel-axplat-x86-pc/src/console.rs:27-59`) and `axlog::LogIf` (implemented
once at `crates/ax/thekernel-axruntime/src/lib.rs:114`). Adding a second output is not
"plugging in a backend"; it is adding a **second sink inside an existing consumer**.

Required change points, in dependency order:

| # | Location | Change | Why |
|---|---|---|---|
| 1 | `crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs` | Parse and store tag 8 (§1). | Nothing downstream can exist without the mode. |
| 2 | `crates/ax/thekernel-axplat-x86-pc/src/lib.rs` | Export a `framebuffer()` accessor (next to the existing `boot_modules` export at `:48`). | Cross-layer handoff. |
| 3 | `crates/ax/thekernel-axhal/src/lib.rs` (`boot` module) | Re-export it as `axhal::boot::framebuffer()`, mirroring `rootfs_module()` at `:60`. | Driver-layer access. |
| 4 | new `kernel/src/pseudofs/dev/fbcon` early sink **or** `crates/ax/thekernel-axruntime/src/klog.rs` | Add a second drain target for `DiagnosticDrain`. The clean insertion point is `drain_with` at `klog.rs:447`, which already takes `sink: impl FnMut(&[u8]) -> usize` — a generic sink parameter, so a **second consumer** (`DiagnosticDrain::drain_once` at `:444` is the only caller) can be added without touching the ring buffer. | Kernel `printk` must reach the screen. |
| 5 | `kernel/src/deferred_work.rs:239-268` | Drive the new sink alongside the UART drain. | The drain is task-driven; a sink with no pump emits nothing. |
| 6 | `kernel/src/pseudofs/dev/fb.rs:616-634` (`DisplayCore`) | Generalise the scanout field from `Arc<DrmFbdev>` to a scanout abstraction, so fbcon and `/dev/fb0` work without DRM. | Removes the DRM hard dependency (§5). |
| 7 | `crates/ax/thekernel-axdriver/src/structs/static.rs` + `macros.rs` + `drivers.rs` | Add the `StaticDisplayDevice` enum so virtio-gpu and bootfb coexist (§3.4). | Only if the bootfb is modelled as a driver; an alternative is to bypass `axdriver` entirely and construct the bootfb directly from `axhal::boot::framebuffer()` inside `kernel/src/drm/` or `kernel/src/pseudofs/dev/`. `[I]` The bypass is smaller and is what this design recommends, because the `DisplayDriverOps` trait contributes nothing the fbcon needs (no mode verb, unusable `fb()`). |
| 8 | `config/x86_64/grub.cfg` (and `grub-drive.cfg`) | Add explicit `set gfxmode=` / `set gfxpayload=keep` so GRUB initialises video and emits tag 8. | Otherwise the tag may be absent (§7.1 A3). |

Change points 1–5 and 8 are mandatory; 6 is mandatory for `/dev/fb0` to exist on a
serial-less machine (because `dev/mod.rs:639` gates `/dev/fb0` on a DRM primary device);
7 is optional.

### 4.4 Does a font/bitmap glyph renderer already exist?

**Yes.** A compact built-in 5×7 ASCII glyph table with a lookup function already exists:

`[V]` `kernel/src/pseudofs/dev/fb.rs:714-716`

```rust
// A compact built-in 5x7 ASCII subset, doubled vertically by `glyph`.  It
// covers normal kernel/login text without a font cache or any allocation.
fn glyph_row(byte: u8, row: usize) -> u8 {
```

`[V]` the table and lookup — `kernel/src/pseudofs/dev/fb.rs:717-769`

```rust
    const GLYPHS: [[u8; 7]; 36] = [
        [14, 17, 17, 31, 17, 17, 17],
        ...
    ];
    if row >= 7 {
        return 0;
    }
    let upper = byte.to_ascii_uppercase();
    let index = match upper {
        b'A'..=b'Z' => (upper - b'A') as usize,
        b'0'..=b'9' => 26 + (upper - b'0') as usize,
        _ => {
            return matches!(byte, b'-' | b'_' | b'.' | b':' | b'/' | b'[' | b']')
                .then_some(if row == 6 { 0x3e } else { 0 })
                .unwrap_or(0);
        }
    };
    GLYPHS[index][row] << 1
```

`[V]` the glyph blitter — `kernel/src/pseudofs/dev/fb.rs:663-686`:

```rust
    pub(crate) fn glyph(&self, x: usize, y: usize, byte: u8) {
        for dy in 0..16 {
            let bits = glyph_row(byte, dy / 2);
            for dx in 0..8 {
                ...
                let color = if bits & (0x80 >> dx) != 0 {
                    0x00d0_d0d0
                } else {
                    0
                };
```

`[V]` the character-cell model — `kernel/src/pseudofs/dev/tty/fbcon.rs:17-21`:

```rust
const VT_COUNT: usize = 63;
const MAX_COLS: usize = 160;
const MAX_ROWS: usize = 64;
const CELL_WIDTH: usize = 8;
const CELL_HEIGHT: usize = 16;
```

| Property | Value | Evidence |
|---|---|---|
| Glyph coverage | ASCII `A–Z`, `0–9`, plus `- _ . : / [ ]` | `fb.rs:759-767` |
| **Missing** | lowercase is folded to uppercase (`to_ascii_uppercase()` at `:758`), so **no true lowercase**; no punctuation beyond the seven listed; no box-drawing, no non-ASCII | `fb.rs:758-767` |
| Glyph cell | 8 px wide × 16 px tall (5×7 doubled vertically: `dy / 2` at `:665`) | `:663-666`, `fbcon.rs:20-21` |
| Colour | foreground `0x00d0d0d0`, background `0` | `:672-676` |
| Renderer limit | 160 cols × 64 rows, 63 VTs, no heap per glyph | `fbcon.rs:17-19`, `:23-28` |

`[I]` The design **reuses** this renderer rather than importing a PSF/VGA font. A full
8×16 VGA font (256 glyphs × 16 bytes = 4 KiB) is a strictly better follow-up because the
current table renders `println!("Hello")` as `HELLO`, which is acceptable for a boot
console but not for a login prompt. This is called out as a deliverable in the change set.

---

## 5. `/dev/fb0` and usersystem

### 5.1 How `/dev/fb0` is implemented today, and what backs it

`[V]` The node is created in `kernel/src/pseudofs/dev/mod.rs:637-650`:

```rust
    // DRM owns VirtIO-GPU scanout before devfs publication. `/dev/fb0` is an
    // emulation client of that primary device, never a competing raw display.
    if crate::drm::primary_device().is_some() {
        match fb::FrameBuffer::try_new() {
            Ok(framebuffer) => root.add(
                "fb0",
                Device::new_with_permissions(
                    fs.clone(),
                    NodeType::CharacterDevice,
                    fb::FB_DEVICE_ID,
                    NodePermission::from_bits_truncate(FB_NODE_MODE),
                    Arc::new(framebuffer),
                ),
            ),
            Err(error) => error!("Failed to initialize framebuffer device: {error}"),
        }
    }
```

Identity: `[V]` `kernel/src/pseudofs/dev/fb.rs:28`
(`pub(crate) const FB_DEVICE_ID: DeviceId = DeviceId::new(29, 0);` — Linux fb major 29,
minor 0) and `[V]` `kernel/src/pseudofs/dev/mod.rs:59`
(`const FB_NODE_MODE: u16 = 0o660;`).

**It is DRM-backed, unconditionally.** `[V]`
`kernel/src/pseudofs/dev/fb.rs:1023-1029`:

```rust
impl FrameBuffer {
    pub fn try_new() -> Result<Self, AxError> {
        let device = crate::drm::primary_device().ok_or(AxError::NoSuchDevice)?;
        let scanout = alloc::sync::Arc::try_new(DrmFbdev::new(device).map_err(AxError::from)?)
            .map_err(|_| AxError::NoMemory)?;
        let core =
            alloc::sync::Arc::try_new(DisplayCore::new(scanout)).map_err(|_| AxError::NoMemory)?;
```

`[V]` and `DisplayCore`'s own doc says so — `kernel/src/pseudofs/dev/fb.rs:614-617`:

```rust
/// The sole fbdev-side scanout authority. Its backing is a DRM dumb GEM
/// object; fbdev and fbcon do not retain a raw-display ownership path.
struct DisplayCore {
    scanout: alloc::sync::Arc<DrmFbdev>,
```

`[V]` `DrmFbdev` is a dumb GEM object plus a real DRM framebuffer on the kernel's private
primary-node file, and it takes DRM master — `kernel/src/drm/fbdev.rs:18-21` and `:33-47`:

```rust
/// The kernel's single fbdev scanout.  The GEM object remains owned by this
/// private DRM file for its entire lifetime, so fbcon and `/dev/fb0` always
/// draw into the same buffer that KMS presents.
pub struct DrmFbdev {
```

```rust
    pub fn new(device: Arc<DrmDevice>) -> DrmResult<Self> {
        let mode = device.preferred_mode();
        if mode.width == 0 || mode.height == 0 {
            return Err(DrmError::Invalid);
        }
        let file = device.open_fbdev_primary();
        file.become_master()?;
        let virtual_height = mode.height.checked_mul(2).ok_or(DrmError::Overflow)?;
        let dumb = file.create_dumb(DumbRequest {
            width: mode.width,
            height: virtual_height,
            bpp: 32,
        })?;
```

**Implication for a firmware framebuffer.** `[I]` On a serial-less N305 running a
virtio-gpu-less image, `crate::drm::primary_device()` is `None`
(`kernel/src/drm/virtio.rs:2438-2441` returns `Ok(false)` when
`axdisplay::take_drm_display()` yields `None`), so **`/dev/fb0` does not exist at all**, and
`fbcon::install()` — which is only called from `FrameBuffer::try_new()`
(`kernel/src/pseudofs/dev/fb.rs:1049-1050`) — is never called either. Making the firmware
framebuffer useful therefore requires decoupling `DisplayCore` from `DrmFbdev`. The natural
seam is a small `ScanoutSurface` trait with two implementations (`DrmFbdev` and
`BootFb`), exposing exactly what `DisplayCore` and `FbconFrame` use today:
`mode() -> Mode`, `pitch() -> u32`, `virtual_height() -> u32`, `pages() -> Arc<SharedPages>`
(see `kernel/src/drm/fbdev.rs:64-86`) and `present() -> DevResult` (`:91-93`).

Ioctls (all in `kernel/src/pseudofs/dev/fb.rs:1086-1170`):

| cmd | Line | Behaviour |
|---|---:|---|
| `FBIOGET_VSCREENINFO` (0x4600) | `:1089` | writes the fixed 32-bpp `VarScreenInfo` |
| `FBIOPUT_VSCREENINFO` (0x4601) | `:1100-1111` | **rejects any mode other than the current one** with `InvalidInput` |
| `FBIOGET_FSCREENINFO` (0x4602) | `:1114-1139` | `id: *b"DRM fbdev\0\0\0\0\0\0\0"` (`:1116`), `smem_start: 0` (`:1119`), `line_length: self.core.scanout.pitch()` (`:1127`), `visual: 2` (`:1123`) |
| `FBIOGETCMAP` / `FBIOPUTCMAP` (0x4604/0x4605) | `:1143-1144` | 16-entry software pseudo-palette only |
| `FBIOPAN_DISPLAY` (0x4606) | `:1147-1158` | source-offset flip via `self.core.scanout.pan(request.yoffset)`; `xoffset` must be 0 |
| `FBIOBLANK` (0x4611) | `:1161-1167` | DPMS on/off; arg > 4 → `EOPNOTSUPP` |
| anything else | `:1168` | `_ => Err(AxError::NotATty)` (ENOTTY) |

`[V]` `mmap` is `DeviceMmap::SharedPages` and only usable at offset 0 —
`kernel/src/pseudofs/dev/fb.rs:1176-1180`:

```rust
    fn mmap(&self) -> DeviceMmap {
        // mmap is deliberately passive: raw writers publish through fsync or
        // FBIOPAN_DISPLAY. The mapping retains the actual SG GEM pages.
        DeviceMmap::SharedPages(self.core.scanout.pages())
    }
```

`[V]` `FN sync` — publication is explicit, `kernel/src/pseudofs/dev/fb.rs:1182-1187`:

```rust
    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        // fsync is the explicit publication point for raw mmap writes.
        self.core.mark_full();
        self.core.commit_if_needed();
        Ok(())
    }
```

### 5.2 What a userspace program needs to use the firmware framebuffer

Two viable userspace strategies; both are already implemented against the DRM-backed
scanout and only need the backing abstraction from §5.1.

**Strategy A — fbdev (`/dev/fb0`).** Minimum operations, all present today:

1. `open("/dev/fb0", O_RDWR)` — `O_RDWR` is required for a writable `MAP_SHARED`.
2. `ioctl(FBIOGET_FSCREENINFO)` → `smem_len`, `line_length`.
3. `ioctl(FBIOGET_VSCREENINFO)` → `xres`, `yres`, `bits_per_pixel`, channel bitfields.
4. `mmap(NULL, smem_len, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0)`.
5. Draw; then either `write()` (published by the 60 Hz worker), `fsync()` (immediate full
   commit), or `FBIOPAN_DISPLAY` (page flip).

   **NOT FOUND for a firmware framebuffer:** nothing in the fbdev ABI needs to change for a
   linear framebuffer — the only DRM-specific parts are the *implementation*, not the ABI.
   Specifically `smem_start: 0` (`fb.rs:1119`) becomes the real physical base address, and
   `mmap` becomes a device-memory mapping instead of `SharedPages`. `[I]` This is the
   cheapest path and the one the design recommends for the console milestone.

**Strategy B — DRM/KMS (e.g. Weston with a pixman/software backend on the DRM backend).**
`[I]` Weston's DRM backend requires, at minimum: `DRM_IOCTL_MODE_GETRESOURCES`,
`GETCONNECTOR`, `GETENCODER`, `GETCRTC`, `SET_CLIENT_CAP(ATOMIC|UNIVERSAL_PLANES)`,
`MODE_CREATE_DUMB`, `MODE_MAP_DUMB`, `MODE_ADDFB2` (XRGB8888), `MODE_ATOMIC` or
`MODE_SETCRTC`, `MODE_PAGE_FLIP` + `WAIT_VBLANK`, and `GET_CAP`. `[V]` all of these are in
the dispatch table at `kernel/src/drm/ioctl.rs:134-304` — the table is unusually complete
(51 core commands, including syncobj, PRIME import/export, universal planes, properties and
`MODE_ATOMIC`). It is **not a gap in the ioctl layer**.

The gap is the **adapter**, not the ABI: `DisplayAdapter` requires `create_dumb` returning
caller-owned pinned backing and a real `present()` submission —

`[V]` `kernel/src/drm/device.rs:113-124`:

```rust
pub trait DisplayAdapter: Send + Sync {
    fn create_dumb(
        &self,
        request: DumbRequest,
        pitch: u32,
        size: u64,
    ) -> DrmResult<Arc<dyn GemBacking>>;
    fn present(&self, scanout: Scanout) -> DrmResult<Arc<Fence>>;
```

`[V]` with a companion `GemBacking` that must yield map-able fixed pages —
`kernel/src/drm/gem.rs:5-8`:

```rust
/// Driver-owned storage for a GEM object.  The DRM core never maps or copies it.
pub trait GemBacking: Send + Sync {
```

`[I]` A firmware-framebuffer DRM adapter is therefore possible (dumb buffer = a sub-rect of
the linear framebuffer, `present()` = no-op fence) but it is strictly more work than
Strategy A and adds no console capability. The design treats it as a follow-up, and
Strategy A as the milestone.

### 5.3 DRM device initialization, and card vs render node

`[V]` Init path — `kernel/src/entry.rs:133-137`:

```rust
    match crate::drm::init_virtio_gpu() {
        Ok(true) => info!("registered VirtIO GPU as DRM primary device"),
        Ok(false) => info!("no DRM-capable VirtIO GPU found"),
        Err(error) => error!("failed to initialize DRM VirtIO GPU: {error}"),
    }
```

`[V]` `kernel/src/drm/mod.rs:33-35` → `kernel/src/drm/virtio.rs:2438-2454`:

```rust
/// Claims a VirtIO GPU from axdisplay and publishes the single DRM device.
/// No compatible GPU simply leaves the legacy display path untouched.
pub fn init() -> DrmResult<bool> {
    let Some(display) = axdisplay::take_drm_display() else {
        return Ok(false);
    };
    ...
    let adapter: Arc<dyn DisplayAdapter> = Arc::new(adapter);
    super::register_primary_device(DrmDevice::with_render(adapter, render, 1, 2, 3, 4))?;
    Ok(true)
}
```

`[V]` the hand-off requires `supports_drm_transport()` —
`crates/ax/thekernel-axdisplay/src/lib.rs:30-42`:

```rust
/// Transfers the one display device to DRM only if it implements the pinned
/// backing transport. After success, framebuffer users observe no display.
pub fn take_drm_display() -> Option<AxDisplayDevice> {
    if !MAIN_DISPLAY.is_inited() {
        return None;
    }
    let mut display = MAIN_DISPLAY.lock();
    display
        .as_ref()
        .is_some_and(|device| device.supports_drm_transport())
        .then(|| display.take())
        .flatten()
}
```

**How the card node vs render node decision is made.** There are **no DRM driver feature
bits** (`DRIVER_RENDER`, `DRM_NODE_PRIMARY`, `DRM_NODE_RENDER`) anywhere —
`grep -rn "DRIVER_RENDER\|DRM_NODE\|drm_node"` → **NOT FOUND**. The decision is:

1. `/dev/dri/card0` is created whenever a primary device exists —
   `[V]` `kernel/src/pseudofs/dev/mod.rs:655-661`:

```rust
    if let Some(device) = crate::drm::primary_device() {
        let mut dri = DirMapping::new();
        dri.add("card0", dri::primary_node(fs.clone(), device.clone()));
        if let Some(render) = dri::render_node(fs.clone(), device) {
            dri.add("renderD128", render);
        }
```

2. `/dev/dri/renderD128` exists only if the device carries a render adapter —
   `[V]` `kernel/src/pseudofs/dev/dri.rs:220-225`:

```rust
/// `/dev/dri/renderD128` exists only for a GPU which negotiated legacy VIRGL.
pub(crate) fn render_node(
    fs: Arc<crate::pseudofs::SimpleFs>,
    device: Arc<DrmDevice>,
) -> Option<Arc<crate::pseudofs::Device>> {
    device.has_render().then(|| {
```

   `[V]` and `has_render` is simply `self.render.is_some()` — `kernel/src/drm/device.rs:509-511`.
   The render adapter is only attached when the virgl capset is present —
   `[V]` `kernel/src/drm/virtio.rs:2443-2451`:

```rust
    // Do not create a render node for a plain 2D virtio-gpu.  Querying the
    // first capset also proves the VIRGL transport completed a round trip.
    let render = candidate
        .capset_info(0)
        .ok()
        .and_then(|(id, ..)| (id == 1).then_some(candidate));
```

3. Per open-file-description, the node type is one `bool` —
   `[V]` `kernel/src/drm/file.rs:211-216` and `:254-256`:

```rust
    pub(crate) fn new(
        device: Arc<DrmDevice>,
        id: OpenId,
        render_node: bool,
```
```rust
    pub(crate) fn is_render_node(&self) -> bool {
        self.render_node
    }
```

   `[V]` which selects the dispatch table — `kernel/src/drm/file.rs:517-528` splits into
   `ioctl::render_dispatch` (allowlist) vs `ioctl::dispatch`.

`[I]` **A framebuffer-only driver must provide, for a DRM card node to come up**, in order:

| # | Requirement | Evidence |
|---|---|---|
| 1 | A display device that stays in `MAIN_DISPLAY` **or** a new path that does not go through `take_drm_display` — a bootfb returns `supports_drm_transport() == false` (`crates/ax/thekernel-axdriver-display/src/lib.rs:394-396`) and therefore can never be handed to DRM. | `crates/ax/thekernel-axdisplay/src/lib.rs:32-42` |
| 2 | An `Arc<dyn DisplayAdapter>` with `create_dumb` + `present`. | `kernel/src/drm/device.rs:113-124` |
| 3 | A `GemBacking` whose `shared_pages()` returns the framebuffer pages (needed by both `MAP_DUMB` and `/dev/fb0` mmap). | `kernel/src/drm/gem.rs:5-8`; used at `kernel/src/drm/fbdev.rs:48` and `kernel/src/pseudofs/dev/fb.rs:1179` |
| 4 | `register_primary_device`, which refuses a second device with `Busy`. | `kernel/src/drm/device.rs:156-163` |
| 5 | A non-zero `preferred_mode()`, else fbdev construction fails. | `kernel/src/drm/virtio.rs` adapter default is 1024×768@60 at `kernel/src/drm/device.rs:144-150`; enforced at `kernel/src/drm/fbdev.rs:34-37` |
| 6 | `create_dumb` accepting `bpp: 32` and a 64-byte-aligned pitch (virtio's rule). | `kernel/src/drm/virtio.rs:2294-2298`, `kernel/src/drm/device.rs:1663` |

`[I]` Requirements 2–5 are exactly the work that Strategy A in §5.2 avoids. **The design
recommends Strategy A (fbdev-only) for the milestone, and keeps the DRM adapter as an
optional follow-up**, because the stated goal is text on a physical screen, not a
compositor.

---

## 6. Testability in QEMU

### 6.1 How the kernel is booted under QEMU by the product tooling

| Property | Value | Evidence |
|---|---|---|
| Arg assembler | `build_qemu_command` — the single place | `[V]` `tools/qemu_runner/command.py:71` `def build_qemu_command(` |
| Binary | `qemu-system-x86_64` | `[V]` `tools/qemu_runner/command.py:117` `qemu_argv = [qemu_binary or "qemu-system-x86_64"]` |
| Machine | `q35,max-ram-below-4g=2G` | `[V]` `tools/qemu_runner/command.py:16` `Q35_MACHINE = "q35,max-ram-below-4g=2G"` |
| Firmware | OVMF/edk2 via two pflash drives | `[V]` `tools/qemu_runner/command.py:146-149` `f"if=pflash,format=raw,readonly=on,aio=threads,file={_escaped_path(ovmf_code)}"` / `f"if=pflash,format=raw,aio=threads,file={_escaped_path(ovmf_vars)}"` |
| Media | ESP as an IDE snapshot disk | `[V]` `tools/qemu_runner/command.py:150-151` `f"file={_escaped_path(esp.path)},if=ide,format=raw,snapshot=on,aio=threads"` |
| Memory / SMP | `-m 1G -smp 4` by default | `[V]` `tools/qemu_runner/command.py:152-155`; `tools/qemu_runner/runner.py:203` `memory: str = "1G"`; `tools/thekernel.py:1046-1047` |
| No legacy VGA | `-nodefaults` | `[V]` `tools/qemu_runner/command.py:159-161` `# q35's defaults include VGA and PS/2 input.  Define the` … `"-nodefaults",` |
| Serial | `-serial stdio` plus a second `-serial chardev:kernel-log` | `[V]` `:162-163` `"-serial",` / `"stdio",`; `:206-211` `f"file,id=kernel-log,path={_escaped_path(diagnostic_log_path)},append=on"` / `"-serial", "chardev:kernel-log"` |
| Display + GPU | `-display <profile>` + `-device <profile gpu>` | `[V]` `:164-167` `"-display",` / `GRAPHICS_PROFILES[graphics_profile].display,` / `"-device",` / `graphics_device(graphics_profile, graphics_width, graphics_height),` |
| Monitor | QMP unix socket only; **no `-monitor`** | `[V]` `:218` `command.extend(["-qmp", f"unix:{qmp_socket},server=on,wait=off"])` |
| Boot path | UEFI + GRUB standalone, **not** `-kernel` | `[V]` `:119` `if direct_kernel:` is a debug-only branch; `tools/qemu_runner/runner.py:466-471` requires an ESP; `config/x86_64/grub.cfg:11` `multiboot2 /TheKernel.elf` |
| Display profiles | default `headless` | `[V]` `tools/qemu_runner/profiles.py:26` `"headless": GraphicsProfileTopology("none", "virtio-gpu-pci", "software"),`; `tools/thekernel.py:1177` `default="headless"` |
| Device string | `<device>,max_outputs=1,xres=W,yres=H` | `[V]` `tools/qemu_runner/profiles.py:52` `return f"{topology.device},max_outputs=1,xres={width},yres={height}"` |
| Local QEMU | 10.2.2 | `[V]` `qemu-system-x86_64 --version` → `QEMU emulator version 10.2.2 (qemu-10.2.2-1.fc44)` |

**Does the default display device provide a GOP/linear framebuffer that GRUB can use?**

`[I]` The only display device in the default product invocation is `virtio-gpu-pci`
(`profiles.py:26`) and `-nodefaults` removes q35's built-in VGA (`command.py:159-161`).
`[V]` **NOT FOUND** in this worktree: any evidence that the bundled OVMF build instantiates
a GOP producer for `virtio-gpu-pci`. Searches run: `grep -rni "simplefb|simpledrm|efifb|gop\b|graphics output protocol|firmware framebuffer|bootloader framebuffer|vesafb|uvesafb"`
over the whole tree — only two unrelated hits
(`kernel/src/drm/file.rs:843` comment "linear framebuffer view", and
`scripts/build-x86-uefi-esp.sh:192`). A local-artifact check was **inconclusive**: the
installed `/usr/share/edk2/ovmf/OVMF_CODE.fd` (1 966 080 bytes) is a firmware volume with
compressed sections; `strings -a` and `strings -a -el` both return 0 hits for `VirtioGpu`,
`QemuVideo`, `Bochs`, `Ramfb` and even `Simple Network`, so the file's driver inventory
cannot be read this way.

`[I]` Consequence: the acceptance test must **not** assume the OVMF-virtio-gpu GOP. Use a
device whose GOP is not in question.

### 6.2 Can an existing QEMU invocation test a firmware framebuffer console without real hardware?

**Yes.** Everything needed is already wired, with one device substitution.

`[V]` QEMU 10.2.2 on this host offers, besides `virtio-gpu-pci`:

```
name "VGA", bus PCI
name "bochs-display", bus PCI
name "ramfb", bus System, desc "ram framebuffer standalone device"
name "virtio-vga", bus PCI
```
(source: `qemu-system-x86_64 -device help`)

**Recommended device option: `-device bochs-display`** (a.k.a. the `bochs-display` PCI
device). `[I]` Reasons:

* It has **no legacy VGA compatibility** and exposes a plain linear framebuffer BAR — which
  is precisely the shape of a modern UEFI GOP framebuffer, so it exercises the real code
  path (aperture address ≠ `0xE000_0000`, `framebuffer_type == 1`).
* It is a PCI device, so it slots into the existing `-device` argument position
  (`command.py:166-167`) with no runner restructuring; `graphics_device()` builds the
  string from `GRAPHICS_PROFILES[...].device` (`profiles.py:47-52`), so a
  `"firmware-fb"` profile is a one-line table entry.
* It is *not* used by the kernel's virtio-gpu driver, so the bootfb path is exercised
  **instead of** the DRM path rather than alongside it — which is the configuration a
  serial-less N305 actually has.

**Alternative: `-device VGA`.** `[I]` Swap-in is identical, but `VGA` also drags in legacy
VGA I/O and a Bochs VBE interface, and its BAR lands at the legacy `0xE000_0000`
aperture — a less faithful model of an N305 GOP BAR.

`[I]` Whichever is chosen, the GRUB configuration must be extended with an explicit video
mode request (`set gfxmode=…` / `set gfxpayload=keep` in `config/x86_64/grub.cfg`), because
the current config keeps GRUB entirely on serial (`config/x86_64/grub.cfg:4-6`) and the
presence of a type-8 tag is therefore GRUB-version-dependent (§7.1 A3).

### 6.3 How the existing screenshot facility works, and whether it can verify fbcon

`[V]` The screendump is a QMP `screendump` to a PPM path —
`tools/qemu_runner/process.py:458-464`:

```python
                        checkpoint.screenshot.unlink(missing_ok=True)
                        self._request(
                            client,
                            buffer,
                            "screendump",
                            {"filename": str(checkpoint.screenshot)},
```

with a retry loop that tolerates a stale frame —
`[V]` `tools/qemu_runner/process.py:456-457`:

```python
                    # Wayland frame callbacks can precede scanout. Keep the
                    # pixel oracle strict while waiting within this deadline.
                    while True:
```

`[V]` The oracle is `_validate_ppm` — `tools/qemu_runner/process.py:59-64`:

```python
def _validate_ppm(
    screenshot: Path,
    expected_size: tuple[int, int] | None,
    color_blocks: tuple[QmpColorBlock, ...],
) -> None:
    """Validate QEMU's P6 screendump before reporting graphics success."""
```

`[V]` It checks: file exists and is non-empty (`:66-71`), magic `P6` (`:88-89`
`if len(tokens) != 4 or tokens[0] != b"P6":`), non-zero geometry and `maxval == 255`
(`:94-95`), pixel payload length (`:103-104` `if len(pixels) != width * height * 3:`), exact
size (`:105-108`), and **exact solid-RGB rectangles** (`:109-116`):

```python
    for block in color_blocks:
        if block.x + block.width > width or block.y + block.height > height:
            raise ProcessError("QMP screenshot color block is outside the image")
        expected = bytes(block.rgb)
        for y in range(block.y, block.y + block.height):
            start = (y * width + block.x) * 3
            if pixels[start : start + block.width * 3] != expected * block.width:
                raise _ScreenshotColorMismatch("QMP screenshot color block did not match")
```

`[V]` The expected rectangles are declared data —
`tools/thekernel.py:757-758`:

```python
            screenshot_size=(800, 600),
            screenshot_color_blocks=(QmpColorBlock(300, 200, 200, 200, (255, 0, 0)),),
```

**Can it verify fbcon text output automatically? Yes, with a new oracle — the plumbing is
reusable, the assertion is not.** Evidence:

* `[V]` fbcon's output is **pixel-deterministic**: foreground is the fixed constant
  `0x00d0_d0d0` and background `0` (`kernel/src/pseudofs/dev/fb.rs:672-676`), the cell grid
  is a fixed 8×16 (`kernel/src/pseudofs/dev/tty/fbcon.rs:20-21`), and each repaint begins
  with a full clear to black (`fbcon.rs:244` `frame.clear(0x0000_0000);`). An exact
  glyph-bitmap match is therefore a sound oracle.
* `[V]` The framebuffer console is installed unconditionally whenever the DRM primary
  device exists (`kernel/src/pseudofs/dev/fb.rs:1049-1050`), and the kernel is built with
  the display feature in every product ELF (`Cargo.toml:166-169`, `crates/ax/thekernel-axfeat/Cargo.toml:22-28`).
* `[V]` **But fbcon is fed only by the TTY layer, never by printk** — see §4.2/§4.3. The
  guest userspace must write a known string to `/dev/console` or `/dev/tty1`
  (`kernel/src/pseudofs/dev/tty/ntty.rs:91-100`, `kernel/src/pseudofs/dev/tty/vt.rs:1074-1085`).
* `[V]` And a DRM-master client suspends text rendering —
  `kernel/src/pseudofs/dev/fb.rs:828-830`:

```rust
    if graphics {
        display.suspend_refresh();
        display.scanout.release_master();
```

  so the conformance test must run **without** the Weston/graphics rootfs.
* `[V]` **Missing assertion primitive:** `_validate_ppm` only compares uniform rectangles
  (`process.py:109-116`). There is no hash, no baseline diff, and no OCR anywhere —
  `grep -rn "md5\|sha1\|sha256\|imagediff" tools/qemu_runner/` → **NOT FOUND**.

### 6.4 Proposed concrete, automatable acceptance test

**Stage name:** `firmware-fbcon`. **Runtime target:** ≤ 240 s under TCG.

**1. Build.** Reuse the existing `build` stage artifacts (`tools/verification.py:36`) with
`--no-build`. No new rootfs: the test uses a tiny statically-linked initramfs/rootfs whose
`init` writes a fixed ASCII string to `/dev/console` and exits. `[I]` This is the only new
guest artifact and it can be produced by the existing `scripts/build-rootfs.sh`.

**2. Launch.** Add one graphics profile:

```python
"firmware-fb": GraphicsProfileTopology("none", "bochs-display", "software"),
```
in `tools/qemu_runner/profiles.py` next to `:26-29`, and boot with
`--graphics-profile firmware-fb`. The runner then emits
`-display none -device bochs-display` from `command.py:164-167`. **No `virtio-gpu-pci` is
present**, so `axdisplay::take_drm_display()` finds nothing, `drm::virtio::init()` returns
`Ok(false)` (`kernel/src/drm/virtio.rs:2438-2441`), and the only scanout is the firmware
framebuffer — exactly the N305 configuration.

**3. Marker.** The guest writes a line beginning with a fixed marker, e.g.
`THEKERNEL_FBCON_READY`, to `/dev/console`. The runner's existing marker gate
(`tools/qemu_runner/process.py:448-455`, `checkpoint.screenshot_after_marker`) then fires
the screendump only after the text has been accepted by fbcon.

**4. Oracle.** Add a `text_cells` field to `QmpColorBlock`'s sibling data model
(`tools/qemu_runner/model.py:72-80`) and extend `_validate_ppm`
(`tools/qemu_runner/process.py:59-116`) with a glyph check: for each expected
`(col, row, char)`, compare the 8×16 pixel rect at
`(col*8*3 + row*pitch*3 …)` against the glyph bitmap produced by the **same** table the
kernel uses. The table is `GLYPHS` at `kernel/src/pseudofs/dev/fb.rs:717-754`; because the
test is in Python, either re-encode the 36 glyphs as a fixture (36 × 7 bytes) or, better,
assert structural properties that cannot be faked by a blank screen:

* background of the whole cell area is exactly `(0, 0, 0)`;
* at least *N* pixels of exactly `(0xd0, 0xd0, 0xd0)` inside the expected cell rects;
* zero `(0xd0, 0xd0, 0xd0)` pixels outside the expected rects.

`[I]` The second and third conditions together prove "text was rendered, and only where
text was expected" without duplicating the font table, which keeps the test honest if the
font is later upgraded to a full 8×16 VGA set.

**5. CI hook.** Add one `Stage` beside `guest-tcg` in `tools/verification.py:31-39`:

```python
Stage("firmware-fbcon", "test",
      (*cli, "test", "--suite", "fbcon", "--smp", "4", "--memory", "512M",
       "--accel", "tcg", "--no-build", "--timeout", "240",
       "--graphics-profile", "firmware-fb",
       "--screenshot", str(state / "verify-fbcon/console.ppm")),
      300),
```

in the **daily** tier, not only `full`. `[V]` justification: the daily tier already boots
the product kernel under TCG with `--no-build` artifacts (`verification.py:36,38`) and the
`full` tier's graphics stage is gated behind a 3-hour `graphics-rootfs` build
(`verification.py:47`, timeout 10800) that this test does not need. `[V]` The daily tier's
workflow timeout is 150 minutes (`.github/workflows/ci.yml:36`), so a 300 s stage fits.

**6. Required plumbing.** `[V]` `--screenshot` is currently consumed only by the graphics
suite (`tools/thekernel.py:1270-1275`), and `test --suite guest` sets neither
`qmp_screenshot` nor `qmp_checkpoints`. The new suite must populate them the same way the
graphics smoke does (`tools/thekernel.py:467-477`).

`[I]` **Minimal first version (recommended as the initial milestone gate):** skip the font
oracle entirely and assert only (a) the PPM exists, (b) it is 800×600, and (c) the
`(0xd0,0xd0,0xd0)` pixel count inside the console area is above a floor and the rest of the
image is black. That single assertion already proves "the kernel wrote text to a firmware
framebuffer", which is the entire stated goal, and it needs ~40 lines of Python.

---

## 7. Risks and unknowns

### 7.1 Assumptions that cannot be verified from source code alone

| # | Assumption | Why it cannot be checked here | Mitigation |
|---|---|---|---|
| A1 | The N305 firmware GOP framebuffer BAR is below 512 GiB, so the **boot** page table reaches it. | The address is firmware-assigned per machine; no N305 hardware or firmware dump is in the tree. | Do not depend on the boot map; always `axmm::iomap` explicitly (§2.3). Log the address at boot. |
| A2 | `IA32_PAT` is at its architectural reset value on the N305, so `DEVICE`/`UNCACHED` PTE bits really mean UC. | `[V]` the kernel never reads or writes PAT (`grep -rnw "PAT\|IA32_PAT"` → NOT FOUND). Firmware may have programmed it. | Option 1 (§2.4) is correct under either PAT value for a *strong-uncached* intent only if entry 0 is honoured; log `IA32_PAT` early if this becomes a concern; prefer explicit PAT setup in the WC follow-up. |
| A3 | GRUB emits a Multiboot2 type-8 tag given `insmod all_video` and `terminal_output serial`. | GRUB's behaviour lives outside this repository. `[V]` `config/x86_64/grub.cfg` sets no `gfxmode`/`gfxpayload`; a repository-wide grep for `gfxpayload|gfxmode|gfxterm` → **NOT FOUND**. | Add explicit `set gfxmode=` / `set gfxpayload=keep` to both GRUB configs, and make the kernel **log a clear missing-framebuffer diagnostic** instead of failing silently. |
| A4 | OVMF/edk2 in CI exposes a GOP for the chosen QEMU display device. | `[V]` the installed `OVMF_CODE.fd` is a compressed firmware volume; `strings` cannot enumerate its drivers — **inconclusive, not negative**. | Pick `bochs-display` (whose GOP support is not in doubt) and have the acceptance test assert the PIT/type-8 tag is present, so a firmware regression fails loudly. |
| A5 | The N305's GOP framebuffer is `framebuffer_type == 1` (RGB) with a sane pitch. | Machine-specific. `[X]` the spec permits 0 (indexed) and 2 (EGA text). | §1.5 rule 5: parse all three, decline 0 and 2 without panicking. |
| A6 | fbcon repaint throughput is acceptable at 4K on UC memory. | No hardware to measure. `[V]` the 33 ms coalescing interval (`kernel/src/pseudofs/dev/tty/fbcon.rs:120`) and the per-pixel `write_volatile` loop (`kernel/src/pseudofs/dev/fb.rs:705-710`) are the only data points. | Ship Option 1 (UC) first; measure on hardware; add WC (§2.4 Option 2) only if needed. |
| A7 | The BOCHS-display BAR is assigned inside `mmio-ranges` or is safely `iomap`-able. | Address assignment is firmware-driven. | The explicit `iomap` handles any address that does not overlap RAM; §2.4 adds the RAM-overlap guard. |
| A8 | Extending `GLYPHS` to lowercase/ASCII is purely additive. | It is a `const [[u8;7];36]` indexed by a `match` (`kernel/src/pseudofs/dev/fb.rs:717-768`); changing the array size changes the index arithmetic. | Treat the font upgrade as its own change with the oracle from §6.4 updated in the same commit. |

### 7.2 Places where the design could be wrong on real hardware, and why

| # | Failure mode | Specific reason | Detection / mitigation |
|---|---|---|---|
| B1 | **Black screen despite a parsed tag.** | The runtime page table replaces the boot map (`crates/ax/thekernel-axmm/src/lib.rs:95`), and the fb address is outside `mmio-ranges` for any profile except `q35-uefi.toml:27`. `[I]` A driver that writes to `phys_to_virt(fb_addr)` without `iomap` will take a **page fault in the kernel** on first pixel. | Mandatory `axmm::iomap` before the first write; assert the returned VA equals `phys_to_virt(addr)` (it always does — `axmm/src/lib.rs:110` — so the real check is that the mapping call succeeded). |
| B2 | **Silent data corruption / UC downgrade of RAM.** | `axmm::iomap` unconditionally re-`protect`s the range (`crates/ax/thekernel-axmm/src/lib.rs:127`) and tolerates an existing mapping (`:119`). A bogus tag pointing into RAM would make RAM uncached — catastrophic but not immediately visible. | §2.4 RAM-overlap guard, plus the §1.5 validation. |
| B3 | **The framebuffer works but the kernel still shows nothing.** | `[V]` `println!`/`info!` terminate at COM2 (`crates/ax/thekernel-axplat-x86-pc/src/console.rs:160`), and fbcon is fed only from the TTY path (`kernel/src/pseudofs/dev/tty/ntty.rs:91-100`). `[I]` Implementing only the driver, without change points 4–5 of §4.3, yields a working `/dev/fb0` and a blank screen for all kernel messages. | Implement the klog sink; the acceptance test of §6.4 asserts on rendered text, so this cannot pass silently. |
| B4 | **The screen is correct for one VT and wrong for another.** | fbcon keeps 63 bounded cell screens (`kernel/src/pseudofs/dev/tty/fbcon.rs:17`, `:101-109`) and repaints only the active VT (`:239-265`). A VT switch on hardware that never triggers `present_while_text_active` leaves stale pixels. | Verify VT switching on hardware; the mode-change path is `kernel/src/pseudofs/dev/tty/vt.rs:379` (`self.with_text_active_locked(active, || super::fbcon::present_while_text_active(active));`). |
| B5 | **Text appears mirrored/offset.** | `[X]` the tag's pitch is in bytes (`multiboot2-0.24.1/src/framebuffer.rs:73-74`) and *may exceed* `width * bpp/8`. `[V]` the existing `FbconFrame` already multiplies by `self.pitch` (`kernel/src/pseudofs/dev/fb.rs:677-683`), but the fbdev `line_length` comes from the DRM dumb pitch (`kernel/src/pseudofs/dev/fb.rs:1127`), not from the tag. | Take `pitch` from the tag, never recompute it; expose it as `FixScreenInfo::line_length`. |
| B6 | **A resolution mismatch between GRUB's mode and the kernel's expectation.** | GRUB may have set a mode that the kernel then changes (if it ever gains a mode verb) or that the display renegotiates. `[V]` `DisplayDriverOps` has **no** mode verb (§3.2), so the kernel cannot follow a hotplug mode change on the GOP path; `[V]` only the virtio path has `drm_display_config_changed` (`crates/ax/thekernel-axdriver-display/src/lib.rs:426-428`). | Treat the mode as immutable for the fbcon milestone; document that a display hotplug on the GOP path is unsupported. |
| B7 | **`/dev/fb0` is absent.** | `[V]` `kernel/src/pseudofs/dev/mod.rs:639` gates the node on `crate::drm::primary_device().is_some()`, and a firmware framebuffer never registers a DRM primary device. | Mandatory §5.1 decoupling; the acceptance test must assert `/dev/fb0` exists. |
| B8 | **Lowercase text is unreadable.** | `[V]` `glyph_row` folds with `byte.to_ascii_uppercase()` (`kernel/src/pseudofs/dev/fb.rs:758`), so `init` renders as `INIT` and error text loses information. | Font upgrade deliverable (§4.4, change set item 9). |
| B9 | **Write-combining is assumed but absent.** | `[V]` `MappingFlags::DEVICE` and `UNCACHED` are the *same* encoding (`crates/ax/thekernel-page-table-entry/src/arch/x86_64.rs:95-97`); there is no WC anywhere in the kernel. A design that says "map it write-combining" would be unimplementable as written. | This document specifies UC for the milestone and PAT-based WC as an explicit, separate change (§2.4). |

---

## PROPOSED CHANGE SET

Dependency order. Sizes are rough line counts of new/changed code including tests.

| # | File | Add / Modify | One-line description | Size |
|---:|---|---|---|---:|
| 1 | `crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs` | Modify | Add `MB2_TAG_FRAMEBUFFER = 8`, a `BootFramebuffer` POD struct, a `framebuffer: Option<BootFramebuffer>` field on `BootInfo`, a validating parse arm before the `_ => {}` catch-all at `:462`, and ~8 host tests. | ~180 |
| 2 | `crates/ax/thekernel-axplat-x86-pc/src/lib.rs` | Modify | Export `framebuffer()` from the boot module beside the existing `boot_modules` accessor at `:48`. | ~10 |
| 3 | `crates/ax/thekernel-axhal/src/lib.rs` | Modify | Re-export the boot framebuffer descriptor as `axhal::boot::framebuffer()`, mirroring `rootfs_module()` at `:60`. | ~8 |
| 4 | `crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs` | Modify | Refuse a framebuffer surface which overlaps a usable-RAM region. **As built:** the guard sits in the boot parser as `FramebufferRejection::OverlapsUsableMemory` rather than in `axmm`, because the memory map is already there and the check must run after the whole tag block is read. No new `axmm` entry point was needed: `iomap` already tolerates an address its caller has validated. | ~30 |
| 5 | `kernel/src/pseudofs/dev/bootfb.rs` | **Add** | The firmware-framebuffer scanout provider: map the aperture, own the mode/pitch/base, and implement the `ScanoutSurface` operations that `DisplayCore`/`FbconFrame` use (`mode`, `pitch`, `virtual_height`, pixel write, `present`). | ~350 |
| 6 | `kernel/src/pseudofs/dev/fb.rs` | Modify | Introduce `trait ScanoutSurface` and change `DisplayCore::scanout` and `FbconFrame::scanout` from `Arc<DrmFbdev>` to `Arc<dyn ScanoutSurface>`; keep `DrmFbdev` as one impl. | ~200 |
| 7 | `kernel/src/pseudofs/dev/mod.rs` | Modify | Change the `/dev/fb0` gate at `:639` from "DRM primary device exists" to "DRM primary device **or** boot framebuffer exists", and construct the matching `ScanoutSurface`. | ~40 |
| 8 | `crates/ax/thekernel-axruntime/src/klog.rs` + `kernel/src/deferred_work.rs` + `kernel/src/pseudofs/dev/tty/fbcon.rs` | Modify | Mirror the kernel log to the active VT. **As built:** not a second `DiagnosticDrain` sink. The drain's record queue is 64 deep and is not even fed when `Store::supported` is false — which is exactly the serial-less machine — so a drain-side sink would have shown at most the first 64 records and then stalled. The mirror is instead a **cursor reader of the klog ring**, started by `fbcon::install`, which replays everything the ring retains and is independent of serial backpressure in both directions. The cost, stated in the module: the screen shows every retained byte rather than only the records the console threshold admits. | ~180 |
| 9 | `kernel/src/pseudofs/dev/console_font/` (new module) + `tools/gen-console-font.py` | **Add** | Replace the 36-glyph 5×7 uppercase-only table with a 95-glyph 8×16 table covering 0x20–0x7e. **As built:** generated from Liberation Mono (OFL-1.1) rather than copied from a VGA ROM font, because the obvious sources (Linux `font_8x16.c`, `kbd` consolefonts) are GPL-2.0 and this tree is Apache-2.0. Data lives in a generated `glyphs.rs`; the interface and invariant tests are hand-written beside it (§0.2 result 5). | ~90 + 12 KB data |
| 10 | `config/x86_64/grub.cfg`, `config/x86_64/grub-drive.cfg` | Modify | Add `set gfxmode=…` / `set gfxpayload=keep` so GRUB initialises video and emits the Multiboot2 type-8 tag. | ~6 each |
| 11 | `crates/ax/thekernel-axplat-x86-pc/src/console.rs` | Modify | Emit an explicit boot diagnostic when no framebuffer tag was found, so a serial-less machine is not silently dead. | ~20 |
| 12 | `tools/qemu_runner/profiles.py` | Modify | Add the `"firmware-fb"` profile (`-display none`, `-device bochs-display`). | ~4 |
| 13 | `tools/qemu_runner/process.py`, `tools/qemu_runner/model.py` | Modify | Extend `_validate_ppm` (`process.py:59-116`) with a glyph-pixel oracle and its data model. | ~90 |
| 14 | `tools/thekernel.py` | Modify | Add the `fbcon` suite that populates `qmp_screenshot`/`qmp_checkpoints` and reuses the marker gate (`thekernel.py:467-477`, `:1270-1275`). | ~70 |
| 15 | `tools/verification.py` | Modify | Add the `firmware-fbcon` stage to the **daily** tier beside `guest-tcg` at `:38`. | ~6 |
| 16 | `tests/` (new fbcon test module) | **Add** | Host-side unit tests for the new oracle and a static assertion that the guest writes the expected marker. | ~150 |
| 17 | `docs/design/fbcon.md` | **Add** | This document. | — |

**Ordering rationale.** 1→2→3 is a strict chain (parse, then platform export, then HAL
façade). 4 must precede 5 because the mapper is what makes the aperture safe. 6 must
precede 7 because the `ScanoutSurface` abstraction is what lets `/dev/fb0` accept a
non-DRM provider. 8 is independent of 5–7 and can land in parallel, but the acceptance test
(12–16) requires both 5–7 and 8 to observe rendered text. 9 is independent and cosmetic.
10 must land before any hardware trial. 11 is a safety net for 10 failing.

**State at the time of writing** — items 1, 2, 3, 4, 5, 6, 7, 8, 9, 12 and 17 are
implemented and committed on `feat/baremetal-boot`; items 13–16 are implemented and
committed on `feat/fbcon-verify`, which is based on that branch. Every implemented item has
host tests, and items 5–9 were additionally confirmed by booting the `firmware-fb` profile
and reading a QMP screendump (§0.2). Items 13–16 are confirmed the same way, with the
results recorded in §0.3. Nothing is verified on the N305.

Four items were **deliberately not done as specified**, for reasons the build settled:

- **Item 10 (`gfxmode`/`gfxpayload=keep`) is dropped.** §0.1 correction 1 shows GRUB emits
  the type-8 tag without either directive, so the stated rationale is void. Pinning a mode
  would also make the guest's aperture a product decision rather than the firmware's, and
  there is no evidence yet that the firmware's choice is unusable. It stays a fallback for
  a real machine that reports an address the kernel cannot map.
- **Item 11 is subsumed.** The thing that makes a serial-less machine diagnosable is not a
  diagnostic *about* the missing tag; it is the log reaching the screen at all. The `info!`
  in `primary_scanout` for each of the two paths, plus the explicit
  `No scanout surface available; /dev/fb0 is not published`, covers the same ground.
- **Item 13's glyph-bitmap comparison is not what was built.** The oracle compares the
  framebuffer against the console's *cell grid*: ink and background are the only colours
  present, no ink falls outside the declared cells, no ink lands on a cell border, the ink
  floors are met, and each declared console line is matched cell for cell. Glyph bitmaps
  are used for the line expectations, but they are read from the kernel's generated table
  (`tools/qemu_runner/console_font.py`) rather than re-encoded in the test, so a font change
  moves both sides together. §6.4's "minimal first version" — assert only that the image is
  800×600 and non-black — was not used; it cannot see the bug this oracle exists for.
- **Item 14's guest-written marker is not what was built.** §6.4 step 3 planned a tiny
  rootfs whose `init` writes a fixed string. The kernel-log mirror makes that unnecessary:
  the screen shows the kernel's own log with no guest cooperation, so the suite boots the
  ordinary system image and gates on the KTAP lines the guest already prints. Item 16's
  "static assertion that the guest writes the expected marker" survives as a host test that
  matches `FBCON_MARKER` against the printf in `tests/guest/system-init.c`, which is what
  catches a marker drift before a boot can burn a run on it.

**Explicitly out of scope (documented as follow-ups):** write-combining via `IA32_PAT`
(§2.4 Option 2), a DRM `DisplayAdapter` for the firmware framebuffer so
`/dev/dri/card0` comes up on a serial-less machine (§5.3), a `set_mode` verb on
`DisplayDriverOps` (§3.2), and multi-display support in the `dyn` device model (§3.4).
