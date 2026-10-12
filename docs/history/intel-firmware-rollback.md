# ADL-N boot modeset transaction and firmware recovery

`intel.modeset=1` now enters the native boot modeset through a guarded hardware
transaction. Default boot still returns before all display writes. **Native
modesetting and recovery are 未在硬件上验证.** QEMU has no Intel display engine.

## Admission, not a blanket permission to replay registers

Supported device: N305 `8086:46d0`. The initial firmware configuration must have
one stable primary plane on pipe A, a combo HDMI/DVI link on DDI A or B, no
active overlay/cursor/eDP transcoder, a usable firmware CDCLK, and a powered,
enabled, locked combo PLL selected by the actual firmware port-clock route.
DDI B can use PLL 0: PLL choice is not inferred from the port's number.
The active PHY must pass the existing calibration verification through a
**write-denying** register view. A PHY that would need recalibration is refused,
not reinitialized and later treated as if its hidden analog state were RAM.
Unused PHYs and the other PLL are not programmed. DP/Type-C, multiple active
pipes, overlays/cursors and cold display initialization remain unsupported and
are refused **before the first write**, even with the explicit parameter.

GMBUS must initially be idle with no firmware interrupt owner. Missing
before-images, an unknown GGTT size, allocation failure, unstable firmware
plane/link identity or a stalled scanline also refuse admission. A captured
inventory alone is not permission: `Snapshot::capture` stays fail-closed until
`rollback::Transaction::begin` validates this supported configuration.

## Before-image and ownership

Before any register write, capture the driver's complete relevant register
inventory: PLL divisors/enables/routes, combo PHY calibration and lane controls,
DDI, transcoder, pipe timings/misc/arbitration, primary-plane geometry/format/
surface/color, DDB/watermarks, driver power requests, DBUF, DC policy, CDCLK/raw
clock and GMBUS/HPD. Supplemental TX_DW5 shadows cover **every lane**, not only
lane 0: group stores broadcast, so replaying lane 0 would lose firmware lane
settings. Group reads/readbacks use the readable lane shadows.

Capture the entire measured GGTT array, including absent PTE bits, and its
allocation cursor. Register/journal/PTE tracking capacity exists before the
first mutation. The allocator never overwrites a present firmware PTE. Changes
are tracked once per register, including an attempted write that can have landed
before a failure is reported. All newly exposed framebuffer allocations remain
owned on failure; uncertain DMA retirement never frees or reuses them.

Only the supported footprint can be written. Other pipes/PHYs/PLL, display
interrupt registers, CDCLK and PHY recalibration registers are denied by the
transaction. CDCLK and calibrated PHY state are **preserved**, not gratuitously
reprogrammed. BIOS/KVMR/debug power requesters are read but never changed.
Read-only, W1C and transient command state are not configuration to replay.
HPD stores neutralize W1C pulse-latch bits. GMBUS recovery cancels/resets a
transaction and restores idle selectors/masks/index, never replays an old I2C
command. Power/PLL/pipe/DDI status bits are excluded from restoration writes.

## Forward and reverse dependency order

Forward boot stages:

1. Disable/arm the original primary plane; observe two real scanline wraps
   (frame boundaries), then disable pipe/transcoder, DDI and its clock route.
   Wait for pipe-off/DDI-idle/PLL-unlock, with bounded deadlines.
2. Reuse the verified active PHY and usable CDCLK; bring up display power,
   raw clock, DBUF and workarounds, then read **live** EDID through GMBUS.
   The connector must match the saved firmware combo port.
3. Select the advertised mode (1080p60 when present), allocate exact geometry,
   paint the native surface, map fresh GGTT entries, program timings and WM.
4. Program/lock the same combo PLL, route its clock, program PHY signal levels,
   configure transcoder routing, enable HDMI DDI **before** the CPU transcoder,
   then arm the primary plane. This corrects the old DDI/transcoder order to
   Linux ADL-P/N's encoder-enable -> transcoder-enable -> plane-update order.
5. Publish the native scanout only after the existing phase-6 proof succeeds.
   There is no after-boot hotplug programming that bypasses the transaction.

On any error, including an inconclusive final scanout proof:

1. Disable/arm the attempted plane and wait for frame boundaries when its pipe
   is running; stop pipe/link/clock/PLL with status readbacks.
2. Restore changed PTEs in descending index order; verify the **whole** GGTT
   array before reinstating the firmware surface.
3. Undo non-PHY data/timings/WM/PLL divisors in reverse tracked order while
   the pipe/PLL are off; reset any outstanding GMBUS transaction.
4. Restore original PLL power, enable and lock, then its original clock route.
   With the reference clock back, disable PHY training, restore coefficients,
   then restore every original lane's TX_DW5 (not a lane-0 broadcast substitute).
5. Restore firmware DDI/transcoder/pipe, then primary control and surface commit.
   Withdraw only added driver power requests; restore DC policy last.
6. Verify all touched writable fields, unchanged CDCLK, exact PTEs, original
   pitch/size/format/offset/route/surface and live SURFLIVE, and multiple fresh
   scanline advances. Equality alone and a saved framebuffer address are not
   accepted as successful recovery.

Every poll also has an iteration bound. A persistent hardware restore failure
reports `ROLLBACK_FAILED`, retains DMA, publishes no native console and makes
no further display attempt; it is **not** disguised as firmware recovery.
`ROLLBACK_MMIO_VERIFIED` means the MMIO/PTE/scanout-progress contract passed,
not that pixels on a physical monitor were compared. Physical verification must
also see the original 800x600 console scanning normally and updating text.

## Host validation and physical acceptance

Existing register models execute the real power-preserving and native 1080p60
program, then restore the original 800x600 state. The matrix exercises DDI A and
DDI B on PLL 0, every write failure prefix, both an unposted refusal and a store
that landed before failure, derived PLL/pipe/DDI status, distinct per-lane PHY
shadows, exact PTE retirement and restored layout. Negative tests cover dormant
PHY preservation, forbidden clock/calibration/foreign-state writes, GMBUS abort,
unsupported firmware topology, dropped PTE restoration and a stopped scanline
with otherwise equal registers. These are model/order/content tests, not
physical PLL, PHY, cache or monitor-output validation.

For a user-run recovery exercise, append both `intel.modeset=1` and
`intel.modeset.fail_write=N` (1..4096). The selected forward write is deliberately
refused once; restoration bypasses that failed wrapper. Without `intel.modeset=1`
this option does nothing. Start with N=1; use the normal successful boot's
reported forward write count to exercise later prefixes. A value past the last
write does **not** trigger a fault and is not a recovery test.

See `n305-next-session-b.md` for the exact boot/visible acceptance steps. Do not
enable NVMe writes or GT submission/forcewake for these tests. The display-only
GGTT path does not initialize/invalidate an executing GT; its fresh binding and
visible pattern/console still require physical verification. HDMI audio remains
separate future display-power/link/ELD work.

Reference behavior/register facts: Linux 7.2.3 `i915/display/intel_display.c`,
`intel_ddi.c`, `intel_dpll.c`, `intel_dpll_mgr.c`, `intel_combo_phy.c`,
`intel_combo_phy_regs.h`, `intel_cdclk.c`, `intel_display_power*.c`,
`skl_universal_plane*.c` and `skl_watermark.c`. Rust implementation is original.
