# Early Screen — Design and Coverage

**Branch:** `feat/early-screen` · **Worktree:** `/home/ava/Worktrees/TheKernel/early-screen`
**Base:** `feat/baremetal-boot` (`9efcbfd1`)
**Problem:** the target machine is an Acer mini PC with an Intel i3-N305 and **no serial
port**. The kernel can render its log into the firmware framebuffer, but the framebuffer
console is installed from the device filesystem, which comes up inside `main()` — after
memory management, platform init, the scheduler, driver probing and secondary CPU
bring-up. Every one of those steps can stop the machine, and until this change all of them
were invisible: the panic handler writes only to the diagnostic UART at `0x2f8`, which
that machine does not have.

This document states what now reports, which page-table mapping the screen writes through
at each point, what the panic path relies on, and what is still invisible.

---

## 1. What draws, and where it lives

`kernel/src/pseudofs/dev/early_screen.rs` is a bounded, allocation-free renderer over the
firmware aperture. It reuses the existing pieces rather than adding a second copy of any
of them:

| Reused | Source |
|---|---|
| 8×16 glyphs, `glyph(byte) -> Option<&'static [u8; 16]>` | `pseudofs::dev::console_font` |
| canonical `0x00RRGGBB` → surface bytes, `PixelLayout::encode_into` | `pseudofs::dev::scanout` |
| aperture description and its validation | `axhal::boot::framebuffer()`, `pseudofs::dev::bootfb::pixel_layout` |
| the log tail | `axruntime::klog::snapshot_into` / `try_snapshot_into` |

The core is `paint(&TextView, &mut [u8], &Content, previous) -> Painted`: a pure function of
a geometry description and a byte slice. Every layout rule it enforces — the stride comes
from the surface *pitch*, the pixel width from the surface's *real* bit depth, a long line
wraps, the status row is clipped, a byte with no glyph is blank — is therefore exercised by
host tests over a `Vec<u8>` and not only on the live aperture
(`kernel/src/pseudofs/dev/early_screen/tests.rs`).

Bounds: at most 512 columns and 128 rows are addressed; the reachable text area is
`(width / 8) × (height / 16)` within those ceilings. The log tail is the newest ≤ 4096 bytes
of the ring, wrapped to the screen width; the newest lines that fit are the ones shown.

## 2. Which mapping is live when

`multiboot.S` installs one bootstrap page table before entering long mode, and the runtime
replaces it exactly once. Both facts are load-bearing, so they are stated rather than
implied:

```
.Ltmp_pml4                        (crates/ax/thekernel-axplat-x86-pc/src/multiboot.S:153)
  0x0000_0000_0000_0000 .. 0x0000_007f_ffff_ffff   1 GiB pages, PRESENT|WRITABLE
  0xffff_8000_0000_0000 .. 0xffff_807f_ffff_ffff   1 GiB pages, PRESENT|WRITABLE
```

The high half is the linear map at `PHYS_VIRT_OFFSET = 0xffff_8000_0000_0000`
(`config/x86_64/q35-uefi.toml:15`), which is the same base `multiboot.S` uses, so
`axhal::mem::phys_to_virt(p)` is a valid, writable address for every `p < 512 GiB` from the
first instruction of `rust_main`. These huge-page entries name no cache attribute, so the
mapping is **write-back** and a frame written through it is flushed with `clflush` before
the painter returns.

| Point in `rust_main` | Page table live | Aperture reachable | Painter |
|---|---|---|---|
| firmware handoff → `axhal::init_early` returns | `.Ltmp_pml4` | yes, via `phys_to_virt` | not claimed — nothing is drawn, and the aperture description does not exist yet |
| `axhal::init_early` returns → `boot_freeze()` | `.Ltmp_pml4` | yes | painting; each frame flushed |
| `boot_freeze()` → `paging_rebind()` | replaced by the kernel page table inside `axmm::init_memory_management` | **not guaranteed** | frozen: not one byte is written |
| `paging_rebind()` → the framebuffer console takes over | kernel page table | yes, via `axmm::iomap(aperture, len)` | painting; the mapping is DEVICE/uncached, so no flush |

`axmm::init_memory_management` builds the kernel address space from
`axhal::mem::memory_regions()` and then writes CR3. The aperture is *not* one of those
regions unless the firmware happens to place it inside a configured `mmio-ranges` entry —
on the reference QEMU profile it does (`0x8000_0000` lies in `[0x8000_0000, 0x2000_0000]`,
`config/x86_64/q35-uefi.toml:27`), and on the N305 it will not. The painter therefore
refuses the whole window instead of relying on the coincidence: `boot_freeze()` clears the
`MAPPED` flag, and `paging_rebind()` sets it again only after `axmm::iomap` has mapped the
same `phys_to_virt` address under the new page table. A panic inside that window paints
nothing rather than faulting through a mapping the CPU no longer has.

`iomap` returns `phys_to_virt(addr)`, which is the address the boot map already gave the
aperture, so only validity changes across the switch and no reader has to be re-pointed.
The claim also refuses the screen outright when `address + len > 512 GiB`, when the
bootloader reported no framebuffer, when its `bpp`/channel layout is one the console
encoder would reject, or when its extent does not fit the surface — a failed claim leaves a
machine behaving exactly as it did before this change.

## 3. The init points that now report

Each call names the step that is **about to** run, so a screen left showing a name says
where the machine is stuck, not merely where it last was. One call per step, on the boot
CPU, never per log record and never from an interrupt.

| Frame on screen | Painted immediately before |
|---|---|
| `runtime entry` | memory-region discovery and `init_allocator`'s preamble |
| `heap allocator` | `init_allocator()` |
| `memory management` | `axmm::init_memory_management()` (with the freeze/rebind pair around it) |
| `kernel page table live` | painted by `paging_rebind()` itself, once the aperture is mapped again |
| `platform devices` | `axhal::init_later()` — APIC, timer calibration, CET/HWP/PMU |
| `scheduler` | `axtask::init_scheduler()` |
| `driver init` | `axdriver::init_drivers()` |
| `filesystems` | `axfs_ng::init_filesystems()` |
| `secondary CPU bring-up` | `mp::start_secondary_cpus()` — the MADT/APIC paths named in the task |
| `interrupt init` | `init_interrupt()` |
| `kernel main` | `unsafe { main() }`, i.e. the device filesystem and the framebuffer console |

Each frame draws a status bar, then the newest log lines that fit under it, so a hang shows
both the step in progress and the last thing the kernel logged.

## 4. The panic path

`axruntime::lang_items::panic` calls `crate::invoke_panic_screen_hook(info, backtrace)`
*before* the diagnostic UART write and before `axhal::power::system_off()`. The screen the
kernel implements shows, top to bottom:

* `*** PANIC ***` on a red bar;
* the panic message, wrapped, up to 8 rows;
* the backtrace, wrapped, up to 6 rows;
* the log tail, filling whatever rows remain — at least 4 are always reserved for it.

The hook is a second, separately named hook and not the `register_panic_crash_hook` slot:
that one exists so crash-kexec runs *before* any logging, while this one is itself a consumer
of the log. It is bound at link time (`crate_interface`, the same mechanism `axfs` uses for
task I/O accounting) rather than through a runtime-registered function pointer, because
every step it exists to report and every panic it exists to make legible happen before
kernel code gets control: a registration slot could only be filled from `main()`, which is
the last milestone.

### Guarantees, and what happens when each fails

| Guarantee | Mechanism | If it does not hold |
|---|---|---|
| No allocation | buffers are stack arrays (≤ 4096 B tail, ≤ 2048 B message, ≤ 2048 B backtrace); nothing is boxed; in the shipped product `axbacktrace`'s `dwarf` feature is off, so the backtrace it is handed is not a `Vec` | the frame is drawn without the affected part |
| No blocking lock | the log tail is read through `klog::try_snapshot_into`, which returns `None` instead of waiting; the only other shared state is a set of atomics | the frame is drawn with no log tail |
| No unbounded loop | every buffer, row count, column count and wrap is capped by a constant | — |
| No scheduler assumption | only atomics, raw memory and `core::fmt` are used | — |
| Never re-enters itself | a `PAINTING` compare-and-swap makes a second painter return immediately | the loser's frame is dropped; the frame already in progress stays |
| Never writes through a dead mapping | `MAPPED` is cleared by `boot_freeze()` and set only by a successful claim or `iomap` | the frame is skipped and the previous one stays on screen |
| Never runs off the surface | every pixel offset is checked against `width`, `height` and the mapped length before it is written | that pixel, row or frame is skipped |
| The rest of the panic path is unchanged | screen painting returns `()` and is called before, not instead of, the UART write and `system_off()` | a panic the screen cannot paint still reaches the UART and powers off exactly as before |
| One backtrace capture | the handler captures once and passes it to both the screen and the UART line | — |

## 5. What remains invisible

Named precisely, because "a panic is now visible" would be an overstatement:

1. **Before `axhal::init_early` returns.** The platform crate's `rust_entry`, the Multiboot
   handoff, memory-map and CPU-topology discovery. Nothing can be painted there: that is
   where the framebuffer *description* is produced, and no earlier code can know the
   aperture's address or geometry. This window closes a few milliseconds into `rust_main`.
2. **Inside `axmm::init_memory_management`.** The aperture is not reliably mapped while the
   kernel page table is being built and installed (see §2). A panic there cannot update the
   screen; what stays on the display is the previous frame — `memory management` on the
   status bar with the log tail up to that point. The step is still identified; the panic
   text is not shown.
3. **A panic on another CPU while a frame is being painted.** The panicking CPU drops its
   frame instead of waiting for the painter, deliberately. The display keeps the frame that
   was in progress, which is at least as new as anything the dropping CPU could have drawn.
4. **Anything after the framebuffer console installs.** The early screen paints only at the
   milestones above, all of which precede `main()`, and from the panic handler. Steady-state
   output continues to come from `fbcon`, whose behaviour is unchanged.

## 6. Verification

Seventeen host tests in `kernel/src/pseudofs/dev/early_screen/tests.rs` run under
`test --suite host`, over a plain `Vec<u8>`: stride from pitch, 32/24/16-bit pixel depth,
wrapping at the column limit, status-row clipping, newest-log-line selection, clearing of
rows a shorter frame no longer uses, the first frame's full repaint of the aperture,
out-of-range glyph bytes, the panic frame's message/backtrace/tail budget, and
bounded-text truncation on a UTF-8 boundary.

The live path was photographed on the `firmware-fb` profile (1280x800, `bochs-display`,
no virtio-gpu), with the PPM decoded back to text through the console font:

* **A normal boot, screendumped at ~75 s** shows `fbcon`'s own console carrying the
  boot-smoke test output — the framebuffer console takes the screen over and behaves as it
  did before this change.
* **A temporary `panic!` placed just after the `platform devices` milestone**, i.e. after
  the aperture is re-mapped under the kernel page table and long before the device
  filesystem exists, produces:

  ```
  *** PANIC ***
  panicked at crates/ax/thekernel-axruntime/src/lib.rs:536:5:
  early-screen panic probe: no UART at 0x2f8 on this machine
  <backtrace disabled>
  <6>[0.042357 cpu=None tid=None INFO target=axruntime] Logging is enabled.
  ...                                       (the newest log lines, newest last)
  <6>[0.133405 cpu=None tid=None INFO target=axmm] Initialize virtual memory management...
  ```

  That is the whole panic path on the display, on a machine whose panic handler has no UART
  to write to.  `<backtrace disabled>` is the product build stating that `axbacktrace`'s
  `dwarf` feature is off, so the backtrace capture allocates nothing at all.
* **A temporary stall at the `driver init` milestone** (spin with interrupts still masked,
  so the machine stops before the scheduler ticks, the secondary CPUs, userspace or the
  console exist) leaves `THEKERNEL  driver init` on the status bar with the log tail ending
  at `use EEVDF scheduler.` — the early screen is on the display with `fbcon` provably not
  yet installed, and a hang is as legible as a panic.

  The same run also demonstrates why the ring, and not the UART, is the right thing to
  mirror.  Its diagnostic stream stops at `THEKERNEL_CPU_ENABLED cpu=0`, because the
  diagnostic UART is drained by a deferred worker which this stall never lets run — while
  the screen shows two further records that are still in the ring.  On a machine with no
  UART at all there is no drain competing with the screen in the first place.
