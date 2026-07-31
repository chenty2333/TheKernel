# bcm283x-v3d

Low-level `#![no_std]` driver for the Broadcom **VideoCore IV V3D** 3D engine
(Raspberry Pi 0/1/2/3 and Zero 2W — the BCM2835/2836/2837 dies).

## Scope

- **Bring-up**: V3D clock via the firmware mailbox, `V3D_IDENT0` identity
  check, binner overflow memory configuration, completion interrupts
  (`FLDONE` / `FRDONE`).
- **Submission**: bin control list (thread 0) and render control list
  (thread 1) primitives, L2/slice cache flushing, seqno completion tracking.
  Writing `V3D_CTNEA` starts the job, mirroring the upstream Linux `vc4`
  driver's job flow without the DRM layer.
- **Command lists** ([`cl`]): builders for the clear-only job shape (tile
  binning config, per-tile sub-list branches, tile buffer stores) and for a
  halt-only self-test.

The GPU-side memory protection (GMP) is left disabled, exactly like the
current Linux driver, which validates command lists in software instead. A
kernel that submits untrusted command streams must add its own validation.

## Usage

```rust
let mut core = V3dCore::new(V3dResources {
    base: 0x3fc0_0000,
    binner_bus_addr: bin_bo_bus_addr,
    binner_size: 16 * 1024 * 1024,
    binner_overflow_addr: bin_bo_bus_addr, // first binner slot
    irq: Some(42),
});
core.init();

// Clear-only frame: build the BCL and RCL into DMA memory, then submit.
let bcl_len = cl::build_clear_bcl(&mut bcl[..], tile_state_addr, tile_alloc_addr,
                                  tile_alloc_size, tiles_x, tiles_y);
let rcl_len = cl::build_bin_rcl(&mut rcl[..], tile_alloc_addr, tiles_x, tiles_y, true);
core.submit_frame(Some((bcl_addr, bcl_addr + bcl_len as u32)),
                  (rcl_addr, rcl_addr + rcl_len as u32));
core.wait_for_seqno(core.seqno());
```

Wire `V3dCore::handle_irq()` to the V3D IRQ (42 on BCM283x) for async
completion.

## Notes

- The binner memory must be a contiguous DMA allocation whose top 4 address
  bits are constant across its extent (binner 28-bit addressing), like the
  Linux driver's 16 MB `bin_bo`.
- License: hardware register facts and command formats; the implementation
  is written from the Broadcom-published specs and register layout, not from
  the GPL Linux driver.
