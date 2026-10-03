# Alder Lake-N: minimal BCS path and read-only GT preparation

2026-10-04. **No command submission or GT state writes in this round.
未在硬件上验证.** QEMU cannot emulate the N305 Intel GT.

## What the read-only probe does

Read the Gen12 GT/render/VDBOX0/VEBOX0 forcewake ACK registers. Linux 7.2.3
`gt/intel_gt_regs.h` defines their offsets, and `intel_uncore.c`
`__gen12_fw_ranges` distinguishes always-on ACKs from GT-gated engine/fuse
registers. If firmware already acknowledges GT wake, read raw media-disable
fuses and BCS0 ring control, then recheck the ACK. Drop those observations if
wake disappeared. An absent value means unavailable, **not absent hardware**.
No request bit, forcewake acquisition, power state, reset, ring register,
GGTT/PPGTT entry or engine command is written. These unowned samples are not
stable engine-availability or readiness proof, even if both ACK samples match.
Host fake and a bounded memory window check read order and unchanged bytes.

## Minimal future implementation

1. Keep display KMS and GT initialization independent. First finish safe native
   scanout/rollback. Identify ADL-N stepping, GT topology/fuses and applicable
   workarounds from the actual GPU, not from the capture card's EDID.
2. Add an owned, refcounted forcewake and GT runtime-power contract with bounded
   ACK transitions and a failure-safe reset path. Apply engine/uncore
   workarounds from i915 for the identified stepping. A read-only ACK sample
   cannot be promoted into that ownership contract.
3. Audit DMA/IOMMU domain, stolen/system memory and GGTT resource allocation.
   Pin source, destination, ring, status page and batch BOs; reserve GGTT ranges
   without overwriting firmware scanout. Create a BCS logical-ring context,
   hardware status page and per-context PPGTT, including scratch mappings and
   address-width validation. Retain owners across timeout/reset.
4. Prefer a small **execlists** bring-up rather than starting with GuC
   submission. Follow `gt/intel_engine_cs.c`, `gt/intel_lrc.c`,
   `gt/intel_execlists_submission.c`, `gt/intel_ring.c`,
   `gt/intel_context.c`, `gt/intel_ggtt.c` and `gt/gen8_ppgtt.c`. Program only an
   admitted BCS engine, with context/ring/HWSP alignment, ordered publication,
   cache/TLB invalidation and completion breadcrumbs. Start with one context,
   one in-flight request and a bounded CPU waiter; add interrupts later.
5. Submit a validated linear **copy**, not arbitrary userspace batches. Encode
   the generation-appropriate `XY_FAST_COPY_BLT` (and a fence/breadcrumb), with
   exact source/destination addresses, pitch, formats, dimensions, bounds and
   overlap policy. Consult `gt/intel_gpu_commands.h`, i915 blitter selftests
   and the external PRM command/register volumes. Invalidate/synchronize CPU
   caches on return. Compare every destination byte and guard region first.
6. Only then add color fill/damage copies and opaque compositor operations.
   BCS is **not general alpha blending or shader composition**. Weston with
   translucent surfaces, transforms or filters needs an RCS/compute rendering
   path (or explicit CPU fallback), not a renamed blitter copy. The first BO
   copy is not evidence that Mesa/Weston GPU rendering works.

## Firmware dependencies

Execlists on the Gen12 BCS route is not inherently a GuC-submit path; the
minimal path should not assert GuC/HuC as mandatory merely because i915 supports
them. Confirm the stepping's supported execution path and reset workarounds
before enabling it. Linux 7.2.3 defaults ADL-P to GuC submission plus HuC
authentication (`uc_expand_default_options`); selecting execlists here is a
new, separately validated minimal path, not reproduction of that default.
The i915 ADL-P device table advertises 48-bit PPGTT and a 39-bit DMA mask;
validate actual SKU/fuses and reject addresses outside the admitted domain.
**GuC submission**, if selected later, requires the matching
ADL-P/N GuC binary, bootstrap/authentication, ADS/CT transport, engine/context
registration and policy. **HuC** is media firmware authentication/processing,
not a prerequisite for ordinary BCS memory copy. DMC manages display power and
is independent of GT batch execution. Sources: i915 `gt/uc/intel_guc_fw.c`,
`intel_guc_submission.c`, `intel_huc.c`, `intel_uc_fw.c` and device tables.
Firmware licensing, versions and failure behavior must be checked before
loading anything; this change neither ships nor loads GT firmware.

## TheKernel DRM/GEM connection

`kernel/src/drm/gem.rs` already has `GemBacking` and per-file GEM handles; it
is not an Intel execution context/address-space scheduler. Add an Intel GEM
backing that owns pinned pages and GPU mappings plus explicit DMA retirement,
then a bounded copy request taking authenticated source/destination handles
and checked regions. Do not expose unrestricted command buffers initially.
Completion must reference owned BOs through close/cancel/reset, separate
execution completion from display present, and integrate reservation fences
before sharing BOs or scanout. Define cache coherency and modifier/format
constraints; the first path can require linear XRGB8888. Existing dumb buffer
allocation/mmap stays the CPU fallback. No new execution ioctl is implemented
in this round.

## Hardware gates for the next round

Before enabling any GT writes, record the baseline console/display state and
complete B3 rollback acceptance. Then check forcewake acquisition/release,
engine reset and a no-op breadcrumb with watchdog-safe deadlines. For copy,
use two disposable BOs with pseudorandom/edge-case content and redzones; compare
full destination and untouched guards after GPU completion. Test misaligned
ranges, pitches, overflows, overlap, closed handles, process exit and forced
engine timeout/reset. Keep the screen visible throughout. Test CPU-copy versus
GPU-copy correctness before measuring speed; this round makes no performance
claim and does not enable any of those write tests.

Measured host validation: four GT tests pass, including asleep/no-gated-read,
awake raw observations, invalidation on lost ACK and byte-identical mock window
before/after probing. Full host, KVM guest and both lint profiles pass; TCG
firmware-fb glyph gate passes. These tests do not validate physical GT registers.
