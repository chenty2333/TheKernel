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

`thekernel-axdriver` has a `dyn` feature whose entire purpose is to replace the static
driver type aliases with trait objects:

`[V]` `crates/ax/thekernel-axdriver/src/structs/dyn.rs:11-13`

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
`crates/ax/thekernel-axdriver/Cargo.toml:50-59`:

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
only — `crates/ax/thekernel-axdriver/src/dyn_drivers/mod.rs:27-41`:

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
`crates/ax/thekernel-axfeat/Cargo.toml:22-28`:

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
`crates/ax/thekernel-axdriver/src/macros.rs:21-27` turns it into the single alias:

```rust
macro_rules! register_display_driver {
    ($driver_type:ty, $device_type:ty) => {
        /// The unified type of the NIC devices.
        #[cfg(not(feature = "dyn"))]
        pub type AxDisplayDevice = $device_type;
    };
}
```

`[V]` invoked once, at `crates/ax/thekernel-axdriver/src/drivers.rs:52-56` with
`<virtio::VirtIoGpu as VirtIoDevMeta>::Driver`. `[V]` The build script enforces the same
singularity from the other side: `crates/ax/thekernel-axdriver/build.rs:3`
`const DISPLAY_DEV_FEATURES: &[&str] = &["virtio-gpu"];` and `:36-58`, which emits one
`display_dev` value or `dummy`.

A second `register_display_driver!` invocation would therefore be a duplicate type alias in
one module — a compile error, not a second resident driver.

### 1.3 Every display driver that exists

| Provider | Where | Model |
|---|---|---|
| virtio-gpu | `crates/ax/thekernel-axdriver-virtio/src/gpu.rs` (`impl DisplayDriverOps` at `:70`) | `axdriver` static driver, registered through `VirtIoDevMeta` at `crates/ax/thekernel-axdriver/src/virtio.rs:153` |
| dummy display | `crates/ax/thekernel-axdriver/src/dummy.rs:73-103` | Compiled only when `display` is on and no listed device is selected (`build.rs:36-58`). `info()` and `fb()` are `unreachable!()`; it exists to satisfy type resolution. |
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
   `crates/ax/thekernel-axdriver-display/src/lib.rs:26-32` is
   `{width, height, fb_base_vaddr, fb_size}`: no pitch, no pixel format. Anything built on
   it has to fabricate a layout, which is exactly what the fbcon did twice before the
   `PixelLayout` consolidation.
4. **There are two `ColorChannel` types and one `PixelLayout` type with no shared home.**
   `[V]` `axhal::boot::ColorChannel` (`crates/ax/thekernel-axhal/src/lib.rs:66-71`) and
   `[V]` `crate::pseudofs::dev::scanout::ColorChannel`
   (`kernel/src/pseudofs/dev/scanout.rs:20-25`) are structurally identical, and
   `kernel/src/pseudofs/dev/bootfb.rs:50-84` converts between them field by field.

Sections 2 and 3 record what was done about each.

---
