# bcm283x-hvs

Low-level `#![no_std]` driver for the Broadcom **VideoCore IV HVS** display
controller and the **VC4 HDMI** transmitter (Raspberry Pi 0/1/2/3 and Zero
2W — the BCM2835/2836/2837 dies).

## Scope

- **HVS compositor**: channel 0 bring-up, single full-screen linear
  framebuffer plane, display-list installation (immediate or at the next
  frame boundary).
- **HDMI output**: fixed CEA-861 1080p60 mode — timing registers, RGB CSC
  passthrough, digital PHY reset, scheduler/packet engine. The pixel clock
  is requested from the GPU firmware through the mailbox property channel
  (`CLK_PIXEL` = 148.5 MHz, `CLK_PIXEL_BVB` = 150 MHz).
- **Framebuffer interface**: [`rdif_display::Interface`] implementation that
  hands the caller a `FrameBuffer` over caller-supplied DMA memory.

## Usage

```rust
let display = HvsDisplay::new(
    HvsResources {
        hvs_base: 0x3f40_0000,
        hdmi_base: 0x3f90_2000,
        fb_bus_addr: fb_bus_addr,   // contiguous DMA memory
        fb_vaddr: fb_vaddr,
        fb_size: fb_size,
        dlist_bus_addr: dlist_bus_addr, // 32-byte aligned
        dlist_vaddr: dlist_vaddr,
    },
    HvsDisplayConfig::xrgb8888(1920, 1080),
)?;

// rdif-display consumers:
let info = display.info();
let mut fb = display.framebuffer()?;
fb.as_mut_slice()[..].fill(0xff); // white screen
display.flush()?;
```

## Notes

- The framebuffer and display list must be contiguous DMA memory visible to
  the GPU (physical bus addresses). Cache maintenance for CPU writes is the
  caller's responsibility (clean before `flush`).
- The Zero 2W outputs 1080p60 over its mini-HDMI connector; the composite
  output (VEC) is not covered by this crate yet.
- License: hardware register facts and the fixed-mode sequences; written
  from the Broadcom-published register specifications, not from the GPL
  Linux driver.
