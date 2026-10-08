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
mutexes, GPIO bit-banging and diagnostics. `kernel/src/drm/intel/gmbus_full.rs`
implements that boundary over typed GMBUS0-5 and GPIO B/C/D/J/K/L/M registers, the
GMBUS power-domain manager, and the existing polling timer. The existing
`gmbus.rs` EDID path now invokes the translated indexed transaction for each
block and retains EDID validation, diagnostic classification, and retry policy;
it is not a second active hardware transfer engine. GPIO bit-bang fallback,
reset, force-bit policy, and source retry behavior come from the translated
state machine. The upstream MIT grant is preserved in the module header and
crate `LICENSE-MIT`.

The adapter currently binds the N305 display-13 pin map and the DDI A/B/C plus
Type-C 1-4 GPIO register instances. The current EDID scan admits the DDI A/B/C
pins; Type-C GPIO declarations make the source fallback addressable but do not
enable TC probe policy. Wait-queue wakeups, GMBUS interrupts, and a delayed
power put are unavailable in this kernel path; the adapter uses bounded
polling and balanced synchronous power release. Non-N305 platform pin-map
integration remains outside this connection.
