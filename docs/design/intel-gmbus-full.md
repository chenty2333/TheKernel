# Intel GMBUS transaction engine

`crates/ax/tk-intel-display/src/intel_gmbus_full.rs` translates 30/39
functions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_gmbus.c` (MIT,
Copyright © 2006 Dave Airlie and © 2006-2008, 2010 Intel). It includes the
platform pin maps, GPIO bit manipulation and workarounds, interrupt/poll wait
rules, indexed and chunked read/write cycles, burst reads, NAK/timeout recovery,
bit-bang fallback policy, power/reference and AKSV transaction paths. Nine
omitted functions are framework adapters only: container-of, GPIO callback
registration, I2C functionality/locking callbacks, and I2C adapter setup/get /
teardown.

`GmbusIo` is the boundary for MMIO, delay and wait queues, IRQ state, power,
mutexes, GPIO bit-banging and diagnostics. The existing kernel `gmbus.rs` still
owns the N305 EDID probe; no `GmbusIo` adapter calls this translated state
machine yet. The source module therefore compiles as a portable mechanism but
does not by itself replace or expand the live connector path. The upstream MIT
grant is preserved in the module header and crate `LICENSE-MIT`.
