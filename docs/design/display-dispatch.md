# Display dispatch — who owns the screen, and on what grounds

**Worktree:** `/home/ava/Worktrees/TheKernel/display-dispatch`
**Branch:** `feat/display-dispatch`, based on `feat/baremetal-boot` (`5e09089e`)
**Scope:** the architecture a display driver plugs into. Not the Intel GPU probe
(`kernel/src/drm/intel/**`), not mode timing tables (`kernel/src/drm/modes/**`), and not the
firmware-framebuffer console itself (`docs/design/fbcon.md`).

Method: every factual claim cites `file:line` and quotes the code. `[V]` means verified by
reading the cited source in this worktree. `[X]` means verified in an external source (Cargo
registry / specification) whose full path is given. `[I]` marks an inference.

---

## 1. Current state, as found

### 1.1 Nothing in this workspace enables the `dyn` display path

`tk-axdriver` has a `dyn` feature whose entire purpose is to replace the static
driver type aliases with trait objects:

`[V]` `crates/ax/tk-axdriver/src/structs/dyn.rs:11-13`

```rust
/// The unified type of the graphics display devices.
#[cfg(feature = "display")]
pub type AxDisplayDevice = Box<dyn DisplayDriverOps>;
```

`[V]` Nothing enables it. A workspace-wide search for a feature reference returns no hit:

```
$ grep -rn "axdriver/dyn\|axdriver?/dyn" --include=Cargo.toml .
(no output)
```

`[V]` The feature's own dependency list names a block probe and no display probe —
`crates/ax/tk-axdriver/Cargo.toml:50-59`:

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

`[V]` and the only runtime probe function that exists under it enumerates block devices
only — `crates/ax/tk-axdriver/src/dyn_drivers/mod.rs:27-41`:

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

`[V]` `rdrive::get_list::<` has exactly one call site in the tree — the `rd_block::Block`
line above. There is no `rd_display`, and `AxDeviceEnum::from_display` in `dyn` form
(`structs/dyn.rs:34-38`) is never called. A workspace dependency
`rdif-display = { version = "0.2.0" }` is declared at `Cargo.toml:127` with no
`Cargo.lock` entry and no use site.

**Verdict: the `dyn` display path has never run.** It is not a disabled feature that would
work if switched on; there is no probe, no dependency edge and no call site that could hand
it a display device. Enabling `dyn` today would delete the one display driver the product
has, not add a second one.

### 1.2 What the product build actually compiles

`[V]` `Cargo.toml:146` `default = ["x86-product"]`, `[V]` `Cargo.toml:156`
`x86-product = ["qemu", "smp", "hwp-uclamp", "pmu", "perf-sampling"]`, `[V]`
`Cargo.toml:165-169` `qemu = [ … "axfeat/display" … ]`, `[V]`
`crates/ax/tk-axfeat/Cargo.toml:22-28`:

```toml
display = [
    "alloc",
    "paging",
    "axdriver/virtio-gpu",
    "dep:axdisplay",
    "axruntime/display",
]
```

So the product image contains **exactly one** display driver type. `[V]`
`crates/ax/tk-axdriver/src/macros.rs:21-27` turns it into the single alias:

```rust
macro_rules! register_display_driver {
    ($driver_type:ty, $device_type:ty) => {
        /// The unified type of the NIC devices.
        #[cfg(not(feature = "dyn"))]
        pub type AxDisplayDevice = $device_type;
    };
}
```

`[V]` invoked once, at `crates/ax/tk-axdriver/src/drivers.rs:52-56` with
`<virtio::VirtIoGpu as VirtIoDevMeta>::Driver`. `[V]` The build script enforces the same
singularity from the other side: `crates/ax/tk-axdriver/build.rs:3`
`const DISPLAY_DEV_FEATURES: &[&str] = &["virtio-gpu"];` and `:36-58`, which emits one
`display_dev` value or `dummy`.

A second `register_display_driver!` invocation would therefore be a duplicate type alias in
one module — a compile error, not a second resident driver.

### 1.3 Every display driver that exists

| Provider | Where | Model |
|---|---|---|
| virtio-gpu | `crates/ax/tk-axdriver-virtio/src/gpu.rs` (`impl DisplayDriverOps` at `:70`) | `axdriver` static driver, registered through `VirtIoDevMeta` at `crates/ax/tk-axdriver/src/virtio.rs:153` |
| dummy display | `crates/ax/tk-axdriver/src/dummy.rs:73-103` | Compiled only when `display` is on and no listed device is selected (`build.rs:36-58`). `info()` and `fb()` are `unreachable!()`; it exists to satisfy type resolution. |
| firmware aperture | `kernel/src/pseudofs/dev/bootfb.rs` | **Not an `axdriver` driver at all.** A `ScanoutSurface` built directly from `axhal::boot::framebuffer()`. |
| Intel display engine (`8086:46d0`) | `kernel/src/drm/intel/**`, in flight | Not present in this tree. |

There is no other display driver in the repository — no `bochs`, no `simple-framebuffer`
driver, no DRM driver other than virtio.

### 1.4 How the kernel reaches a scanout today

One function decides, and it is a hand-written `if`:

`[V]` `kernel/src/pseudofs/dev/mod.rs:547-584`

```rust
/// The scanout `/dev/fb0` should be published over, if this machine has one.
///
/// A DRM device wins when one exists: it is a real driver with a real
/// connector behind it.  The firmware aperture is the fallback which keeps a
/// machine with no display device at all able to show a console, and on a
/// machine with no serial port that console is the only way anything can be
/// reported at all.
fn primary_scanout() -> Option<Arc<dyn scanout::ScanoutSurface>> {
    if let Some(device) = crate::drm::primary_device() {
        match crate::drm::drm_scanout(device) {
            Ok(scanout) => return Some(scanout),
            // Fall through rather than give up.  A DRM setup which cannot
            // produce a scanout must not also cost the machine the aperture it
            // could still draw into.
            Err(error) => warn!("Failed to prepare the DRM scanout for fbdev: {error}"),
        }
    }
    let framebuffer = axhal::boot::framebuffer()?;
    let surface = match bootfb::BootFb::new(&framebuffer) { … };
    …
    info!(
        "Firmware framebuffer scanout: {}x{} pitch {} at {:#x}",
        framebuffer.width, framebuffer.height, framebuffer.pitch, framebuffer.address
    );
    Some(surface)
}
```

The chain behind the two branches:

* DRM branch. `[V]` `kernel/src/entry.rs:133` calls `crate::drm::init_virtio_gpu()` before
  pseudofs is mounted; `[V]` `kernel/src/drm/virtio.rs:2436-2452` takes the display device
  out of `axdisplay` (`axdisplay::take_drm_display()`) and publishes it with
  `register_primary_device`. `[V]` `kernel/src/drm/fbdev.rs:395-400` `drm_scanout()`
  constructs `DrmFbdev::new` — a dumb GEM buffer at `bpp: 32`, DRM master, and one
  `commit_mode` — and erases it to `Arc<dyn ScanoutSurface>`.
* Firmware branch. `[V]` `kernel/src/pseudofs/dev/bootfb.rs:100-121` validates the
  bootloader's description into a `PixelLayout`, `iomap`s the aperture device-uncached and
  presents it as a `ScanoutSurface`.

Both feed the same consumer: `[V]` `kernel/src/pseudofs/dev/fb.rs:947-976`
`FrameBuffer::try_new(scanout)` builds the `DisplayCore` (`:621-651`) that owns the fbdev
ABI and the damage tracker, and installs the fbcon's weak handle (`FBCON_DISPLAY`, `:646-650`).
`/dev/fb0` and the in-kernel console therefore share one surface, chosen once, in
`primary_scanout()`.

### 1.5 What is wrong with this picture

1. **The selection is a hard-coded `if` over a closed set.** A third provider — the Intel
   display engine, or any future driver — can only be added by editing
   `kernel/src/pseudofs/dev/mod.rs`. A driver in another directory cannot offer itself.
2. **The priority has no stated reason and no per-candidate record.** The doc comment states
   the order; the boot log does not. A candidate that loses silently is indistinguishable
   from a candidate that was never asked. On the target machine the screen is the only
   output channel, so "why did the firmware aperture win" has to be answerable *from the
   screen*.
3. **`DisplayInfo` cannot describe a scanout.** `[V]`
   `crates/ax/tk-axdriver-display/src/lib.rs:26-32` is
   `{width, height, fb_base_vaddr, fb_size}`: no pitch, no pixel format. Anything built on
   it has to fabricate a layout, which is exactly what the fbcon did twice before the
   `PixelLayout` consolidation.
4. **There are two `ColorChannel` types and one `PixelLayout` type with no shared home.**
   `[V]` `axhal::boot::ColorChannel` (`crates/ax/tk-axhal/src/lib.rs:66-71`) and
   `[V]` `crate::pseudofs::dev::scanout::ColorChannel`
   (`kernel/src/pseudofs/dev/scanout.rs:20-25`) are structurally identical, and
   `kernel/src/pseudofs/dev/bootfb.rs:50-84` converts between them field by field.

Sections 2 and 3 record what was done about each.

---
## 2. Decision

### 2.1 The `dyn` display path is dead weight and stays dead

Correction to the brief's framing, on the evidence in §1: the `dyn` path is not a layer
"nobody uses today" that could be switched on later. It has no enabling manifest, no probe
for display devices, no dependency edge that could link one, and its only effect on a
product build would be to replace the single static display type with a trait object that
nothing constructs. Enabling `dyn` would **remove** the one display driver the product has.

It is therefore not resurrected, and the `dyn_drivers` module is not extended. The three
things the brief asked of a registry are provided where display drivers actually live.

### 2.2 What takes its place: a ranked scanout provider registry

`kernel/src/drm/screen.rs` is now the kernel's single selection point, and it is a registry
rather than a chain of `if`s:

* **A candidate is data, not code.** `Candidate::new(name, rank, reason, acquire)` describes
  a provider: a name for the log, a rank from the table in §3.1, the provider's own
  statement of why it is entitled to the screen, and a `fn` that tries to take it. A driver
  in any directory can build one and call `crate::drm::screen::register`.
* **The decision is a function over candidates and nothing else.** `decide(&[Candidate],
  &mut dyn FnMut(&Considered)) -> Selection` touches no hardware of its own — the candidates
  do — so all four cases the brief names (no candidate, a candidate that errors, two
  candidates that both work, a candidate that succeeds with unusable geometry) are host unit
  tests (§3.3). The observer is how each verdict reaches the log the moment it is decided,
  which §3.2 explains.
* **Every candidate is accounted for.** `Selection` records one `Verdict` per candidate in
  consultation order, and `report` prints each one at `info!` as it is decided (§3.2). A
  candidate that was never asked because an earlier one won is recorded as `Outranked`, not
  omitted: "lost to a better provider" and "never ran" are different facts and only one of
  them is a configuration problem.
* **Failure falls through.** A candidate that is absent, that fails, or that hands back a
  surface which cannot be drawn into does not end the search; the next candidate is
  consulted. The kernel's own two providers are the built-in candidates at ranks 100 and
  200, so today's behaviour is preserved exactly while a third provider can now outrank
  either of them.

The registry is in `kernel/src/drm/` because that is the directory display drivers are
written in, not because the decision is a DRM one — one of the two built-in candidates has
no DRM device behind it at all, and the module doc says so.

## 3. The new structure

### 3.1 Priority, and why

| Rank | Tier | Reason |
|---:|---|---|
| 0 | `rank::DRIVER` | It owns the display controller and programmed the mode it reports. It can present at the panel's own mode, and it is the surface a graphics client on the same device presents through. |
| 100 | `rank::DRM` | The fbdev emulation of the registered DRM primary device (virtio-gpu today). A real driver with a real connector, but its console pixels live in a dumb GEM buffer and reach the screen only through an atomic commit — strictly more that can fail than a surface the controller scans directly. |
| 200 | `rank::FIRMWARE` | The linear aperture the firmware programmed before the kernel started. Last because it is the only candidate that cannot present at a mode of the kernel's choosing, and it exists only because nothing above it did. |

Within one rank a registered candidate is consulted before a built-in one, and registration
order breaks any remaining tie (`sort_by_key` is stable), so the order is deterministic
rather than incidental. On the profiles that exist today the candidate set is exactly
`drm-primary`, `firmware-aperture`, and the order is the one the previous `if` hard-coded.

### 3.2 What a boot log says

One `info!` line per candidate, in consultation order, on the `scanout:` prefix. Captured
verbatim from QEMU, with the profile that produces each shape.

**A machine with a DRM device** (`test --suite guest`, `headless` graphics profile, kernel
log at `0.45 s`): the DRM device takes the screen and the firmware aperture is recorded as
never asked.

```
<6>[0.382711 cpu=Some(0) tid=Some(2) INFO target=tk_kernel::entry module=tk_kernel::entry] registered VirtIO GPU as DRM primary device
<6>[0.455935 cpu=Some(0) tid=Some(2) INFO target=tk_kernel::drm::screen module=tk_kernel::drm::screen] scanout: candidate 'drm-primary' (rank 100) selected: 800x600 pitch 3200, because a driver published this device and presents through it
<6>[0.456091 cpu=Some(0) tid=Some(2) INFO target=tk_kernel::drm::screen module=tk_kernel::drm::screen] scanout: candidate 'firmware-aperture' (rank 200) not consulted: 'drm-primary' already won
```

**A machine with no display device at all** (`--graphics-profile firmware-fb`, a
`bochs-display` and no virtio-gpu — the N305's topology): the DRM candidate loses on the
stated ground that no device registered, the aperture wins, and the losing verdict is on the
log *before* the winning one, which is the only order in which a serial-less machine can
read it.

```
<6>[0.578926 cpu=Some(0) tid=Some(2) INFO target=tk_kernel::entry module=tk_kernel::entry] no DRM-capable VirtIO GPU found
<6>[0.582261 cpu=Some(0) tid=Some(2) INFO target=tk_kernel::drm::screen module=tk_kernel::drm::screen] scanout: candidate 'drm-primary' (rank 100) has nothing to offer: no DRM primary device is registered
<6>[0.583333 cpu=Some(0) tid=Some(2) INFO target=tk_kernel::drm::screen module=tk_kernel::drm::screen] Firmware framebuffer scanout: 1280x800 pitch 5120 at 0x80000000
<6>[0.583728 cpu=Some(0) tid=Some(2) INFO target=tk_kernel::drm::screen module=tk_kernel::drm::screen] scanout: candidate 'firmware-aperture' (rank 200) selected: 1280x800 pitch 5120, because the firmware programmed this display and nothing in the kernel did
```

The `Firmware framebuffer scanout: …` line is the one the previous code emitted, unchanged
and in the same place, so nothing that greps for it breaks.

**Every verdict is emitted as it is decided, not collected and printed at the end.** A
verdict that is only printed once the search has finished is a verdict that is lost exactly
when it matters: on a machine whose only output is the screen, the screen exists only after
some candidate has won, and if the fallback that a losing verdict enabled is what faults on
the way up, nothing is ever read. `decide` therefore takes a verdict observer and calls it
before the next candidate is asked; `a_verdict_is_reported_before_the_next_candidate_is_asked`
pins that ordering with a synthetic provider that refuses to be acquired until it has
happened.

**Gap: the bootloader's own rejection reason does not reach this log.** The platform parses
Multiboot2 tag 8 into seven distinct rejections (`FramebufferRejection::Truncated`,
`Indexed`, `Text`, `UnknownKind`, `Inconsistent`, `UnusableAddress`,
`OverlapsUsableMemory` — `crates/ax/tk-axplat-x86-pc/src/boot_info.rs:172-191`) and
reports them at `report_framebuffer` (`:385-405`), but only through `diagnostic_println!`,
which is COM2 (`crates/ax/tk-axplat-x86-pc/src/lib.rs:12-16` →
`console::emergency_diagnostic_print`). The N305 has no serial port, so on that machine the
distinction is invisible, and all the kernel can say from `axhal::boot::framebuffer() ==
None` is that there is no framebuffer. The enum and the accessor exist but are `pub(crate)`
(`boot_info.rs:172`, `:325-327`), and `axplat_x86_pc::boot_framebuffer()` returns only
`Option<FramebufferInfo>` (`crates/ax/tk-axplat-x86-pc/src/lib.rs:64-66`). Closing
this needs the platform crate, which this change does not own; §5.4 gives the two-line
shape.

### 3.3 Tests

`kernel/src/drm/screen.rs` carries host tests over synthetic candidates: a `SyntheticSurface`
that reports geometry a test chooses and a set of `fn` factories for the provider
behaviours. They cover, by name:

| Test | Case |
|---|---|
| `no_candidate_at_all_selects_nothing_and_says_so` | no candidate |
| `a_candidate_that_has_nothing_to_offer_falls_through_to_the_next` | absent provider |
| `a_candidate_that_fails_falls_through_to_the_next` | provider errors |
| `every_candidate_is_accounted_for_in_the_order_it_was_consulted` | verdict ordering |
| `two_working_candidates_leave_the_later_one_unconsulted` | two working candidates |
| `a_surface_that_cannot_be_drawn_into_falls_through` | unusable geometry, fall-through |
| `every_geometry_a_console_cannot_draw_into_is_rejected` | empty extent, undrawable layout, short pitch, short allocation, rows below the visible ones |
| `the_last_candidate_still_wins_when_everyone_above_it_failed` | fall-through to the last |
| `a_verdict_is_reported_before_the_next_candidate_is_asked` | logging order (§3.2) |

## 4. What a future Intel display driver must implement

Two routes, and a driver may take either. Both end with the same guarantee: the surface is
used only if it is consulted first *and* passes the console's own geometry check.

**Route A — offer your own scanout (`rank::DRIVER`).** Implement
`ScanoutSurface` (`kernel/src/pseudofs/dev/scanout.rs`) over the aperture you programmed:

| Method | What it must be |
|---|---|
| `width`, `height`, `pitch`, `pixel_layout` | the mode you programmed, the real stride, and how you pack a pixel (build it from `axgpu::PixelLayout`) |
| `write_pixel`, `read_bytes`, `write_bytes` | write/read through your mapping, clipped to the surface |
| `virtual_height`, `yoffset`, `pan` | `height`, `0`, `Err(Unsupported)` unless you can really pan |
| `len` | bytes addressable from the surface's first byte |
| `mmap` | `DeviceMmap::Physical(range)` for a linear aperture, `SharedPages` if the memory is owned elsewhere |
| `present` | `Ok(())` if a write is already visible, otherwise publish and return `Err` on failure |
| `restore_text`, `set_master`, `set_blank` | accept if you have nothing to do; the console holds master while text is active |

then register it, once, before `/dev/fb0` is published:

```rust
crate::drm::screen::register(Candidate::new(
    "intel-display",
    crate::drm::screen::rank::DRIVER,
    "it owns the display engine and programmed this mode",
    acquire, // fn() -> Result<Arc<dyn ScanoutSurface>, Unavailable>
));
```

`Unavailable::Absent` is for "this machine has no such hardware"; `Unavailable::Failed` is
for "it is there and I could not prepare a surface". The log distinguishes them because the
first is a machine or configuration question and the second is a driver bug.

**Route B — be the DRM device.** Implement `DisplayAdapter`
(`kernel/src/drm/device.rs`) and publish it with `register_primary_device`. The built-in
`drm-primary` candidate then offers your fbdev emulation at rank 100 with no registration
call of your own.

**What the console checks before it will use your surface** (`screen::usable`): non-zero
extent; a drawable `PixelLayout` (every channel present and inside the pixel, depth a whole
number of bytes); `pitch >= width * bytes_per_pixel`; and
`len() >= pitch * virtual_height()`. A surface that fails any of these is rejected with the
reason in the log and the next candidate is consulted.

**Modes.** A driver programs the mode it wants and reports it; the console never changes a
mode. The mode *description* type belongs to the modes workstream
(`kernel/src/drm/modes/**`) and reaches a driver through the DRM layer, not through the
scanout registry. See §5.1.

## 5. What I did not do

### 5.1 No mode-setting verb was added to the display driver traits

The brief allowed this to be declined with an argument. Three facts decide it:

1. **The mode description type is not mine.** A timing description (pixel clock, blank and
   sync geometry, polarities, interlace) is being built in `kernel/src/drm/modes/**`, and
   `tk-axdriver-display` is a mechanism-layer crate that cannot name a kernel type.
   A verb on `DisplayDriverOps` would therefore have to invent a second mode description, or
   reach for `axgpu::Mode`/`drm::kms::Mode` — a third and fourth way to say "mode" in one
   kernel. The repository has spent the last weeks removing exactly that duplication.
2. **The trait is not on the path.** `DisplayDriverOps` is not consulted by the console,
   `/dev/fb0` or the scanout registry; those use `ScanoutSurface`. A defaulted
   `Err(Unsupported)` verb there would have no caller and no defined effect on an
   already-published `/dev/fb0` node, whose geometry is cached by `DisplayCore`.
3. **Modes already have a direction.** `DisplayAdapter::preferred_mode()` states the mode a
   driver programmed, and every commit carries it to the adapter in `Scanout.mode`. A driver
   that can change modes can already express what it is in; what it cannot do is be
   *commanded* from the console, which is deliberate — the console never changes a mode.

Where the verb goes when there is a caller for it — recorded here exactly, so that the next
person adds a line instead of re-deriving the layering. On `DisplayAdapter` in
`kernel/src/drm/device.rs`:

```rust
/// Program `mode` on the display controller.
///
/// Defaulted: an adapter that cannot change modes — every adapter in the tree
/// today — keeps compiling and states so by returning `Unsupported`.
fn set_mode(&mut self, mode: crate::drm::modes::Mode) -> Result<(), DrmError> {
    let _ = mode;
    Err(DrmError::Unsupported)
}
```

It is a one-line addition once two things exist: `kernel/src/drm/modes/**` (which owns the
timing description this references by path), and a KMS path that changes mode on a live
CRTC. Adding it before either would be a verb with no implementation site and no caller.
Note also that `DisplayAdapter`'s methods take `&self`; the first implementation will have
to decide whether a mode change needs interior mutability or a `&mut self` sibling, which is
another reason not to guess at the signature ahead of its implementation.

### 5.2 The choice is made once, and a loser cannot take over later

`console_scanout()` runs when devfs is built, and its surface is handed to
`fb::FrameBuffer::try_new`. There is no re-selection, no hotplug path into it, and a driver
that registers after that point is simply never consulted. A real handover protocol (a
display that appears later, or a driver that only becomes ready after init) is a separate
piece of work and is not in this change.

### 5.3 `FrameBuffer`'s bytes still have no accessor

`DisplayDriverOps::fb()` returns a `FrameBuffer` whose `_raw` is private with no accessor,
so the value remains unusable by any caller. It is *not* on the console path (that is
`ScanoutSurface`), the new `DisplayInfo` fields describe a surface rather than expose it,
and inventing an accessor with no caller would be API for its own sake. Recorded here as the
next thing that interface needs.

### 5.4 The bootloader's rejection reason is not visible on a serial-less machine

See the gap analysed in §3.2. Within this change's ownership the kernel's line is as precise
as the information it is given allows, and the fix belongs to the platform crate:

```rust
// crates/ax/tk-axplat-x86-pc/src/lib.rs
/// Why the bootloader's framebuffer was declined, if it offered one.
pub fn boot_framebuffer_rejection() -> Option<&'static str> { /* map the enum */ }
```

plus a two-line relay in `axhal::boot` (`framebuffer_rejection()` next to `framebuffer()`)
and one word in `screen::firmware_aperture`'s `Unavailable::Absent` message. That is a
change to a crate another workstream owns, so it is recorded rather than made.

### 5.5 Nothing here has met Intel hardware

No display driver other than virtio-gpu and the firmware aperture has ever registered
itself. Every claim about the registry is from host tests and a QEMU boot; the registry's
first real client will be the Intel driver, and its `Unavailable::Failed` path is the one
most likely to be exercised there for the first time.
