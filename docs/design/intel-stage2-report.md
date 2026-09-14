# Stage 2: the Intel display engine

This is the review entry point for the branch `dev`, which carries the Intel
display work on top of `main`.  Read this first; it says what was built, what
was measured, what is still unverified, and where every claim is written down.

`main` is untouched.  `dev` is 206 commits ahead of it, of which the Intel work
is one merge away from the platform work that preceded it.

## 1. What stage 2 was, and what it now does

The goal was the Intel display engine of the target machine -- an Acer
蜂鸟mini with an i3-N305 (Alder Lake-N, Gen12 Xe-LP, device `8086:46d0`) whose
only console is the screen, because it has no serial port.  The reference is
`docs/design/intel-display-registers.md` §11, the bring-up order, and stage 2 is
its phases 3 through 6 plus the connector work in phase 2 that they depend on.

At boot the kernel now:

1. probes the device, maps its register window (phase 0, already present);
2. powers it up -- DC states, combo PHY, `PW_1`, CDCLK, DBUF slices, the
   platform workarounds (phase 1, already present);
3. powers the DDC pin pair, enables hotplug, reads it once, reads and validates
   the EDID over GMBUS, and lets the mode layer plan from it (phase 2);
4. allocates a framebuffer in memory the display engine can read -- the GGTT
   page table, a contiguous allocation, a stride and an aperture read from the
   device (phase 3.2);
5. paints the test pattern into it, before a single register is written
   (phase 6.5);
6. computes everything, then writes it: the timings, the DDB, the watermarks and
   the plane's shadow registers; the PLL, the DDI-to-PLL mapping, the board's
   voltage-swing values, the transcoder; and then arms the plane (phases 3.3-5);
7. proves it: `PIPEDSL` advancing, `PLANE_SURFLIVE` reading back the address
   written, `DDI_BUF_CTL.IS_IDLE` clear, `PIPESTAT` without a FIFO underrun
   (phase 6.1-6.4);
8. offers the surface to the console, which takes it **only** if phase 6 proved
   the pipe is scanning it out, and otherwise keeps the firmware's framebuffer
   and logs the reason (the console gate);
9. starts a watch that notices a monitor plugged in or unplugged afterwards,
   logs the transition and re-reads the sink -- without writing a register,
   because this kernel cannot receive the display engine's interrupt (see §4).

Where each piece lives:

| Phase | Module | Design record |
|---|---|---|
| 1 power, PHY, CDCLK, DBUF | `drm/intel/power.rs`, `phy.rs`, `clk.rs` | `intel-power.md` |
| 2.1 AUX/DDC well | `drm/intel/connect.rs` | `intel-connector.md` |
| 2.2/2.3 hotplug, EDID, GMBUS | `drm/intel/hpd.rs`, `gmbus.rs`, `sink.rs` | `intel-gmbus.md`, `intel-connector.md` |
| 2 after boot (the watch) | `drm/intel/hpd.rs`, `mod.rs` | `intel-hotplug.md` |
| 3.1 mode choice | `drm/intel/modeset.rs` | `intel-modeset.md` |
| 3.2 GGTT and framebuffer | `drm/intel/gtt.rs`, `fb.rs` | `intel-scanout.md` |
| 3.3, 5.1 PLL | `drm/intel/pll.rs` | `intel-pll.md` |
| 3.4 timings | `drm/intel/timing.rs` | `intel-pipe.md` |
| 4 DDB, watermarks, plane | `drm/intel/pipe.rs` | `intel-pipe.md` |
| 5.2-5.7 DDI and transcoder | `drm/intel/output.rs` | `intel-output.md` |
| 5.3 swing values, read back | `drm/intel/swing.rs` | `intel-swing.md` |
| 6 prove, pattern, the gate | `drm/intel/modeset.rs`, `pattern.rs`, `scanout.rs` | `intel-modeset.md`, `intel-scanout.md` |
| boot wiring | `drm/intel/mod.rs` (`modeset_at_boot`) | this document, §5 |

## 2. The evidence, and what each gate is worth

**`python3 tools/thekernel.py verify --tier daily` passes on this branch**, in
full: `dependency-layers`, `graphics-config-seatd`, `graphics-config-desktop`,
`host`, `build`, `lint`, `guest-tcg` and `firmware-fbcon`.  The last of those is
the serial-less acceptance path -- the kernel booted on a profile with no
virtio-gpu and no serial port, its screen read back and checked -- and it
reports `readable on screen` for the banner, the gated marker and the kernel log
mirror.

**`drm::intel` host tests: 388, none failing**, and `lint --platform n305` exits
0 with no findings in the Intel modules.  The target lint matters here more than
usual: it is the only thing that compiles the `#[cfg(target_os = "none")]` halves
-- the BAR mapping, the device-uncached framebuffer view, the GTT aperture read
and the hotplug watch's task body -- which no host test can reach.

What the tests are worth, stated plainly:

* They are **host tests over a mock register file, a mock bus and a host page
  arena**, plus two QEMU boots of the platform path.  They establish sequencing,
  arithmetic, refusals, and the write order of every sequence; they cannot
  establish that a Gen12 display engine behaves as the sources say.
* **QEMU cannot emulate this display engine at all.**  No emulated machine
  presents `8086:46d0`, so no QEMU boot executes any line of phases 1-6.  A QEMU
  pass says the kernel still boots and the platform path still works; it says
  nothing about the Intel register programming.
* **Nothing in this work has run on the target machine.  Zero boots.**  Every
  register value, every sequence and every inference about hardware behaviour in
  the design records is derived from the reference document and from Linux i915
  (read as facts, cited file:line, never copied), and is labelled as such.

The document that turns those claims into facts is
`docs/design/n305-display-acceptance.md`: what to build, what to flash, what the
panel should show at each phase, which log line confirms it, how to falsify it,
and what to collect when it fails.  It also records that the target still needs
a USB HDMI capture dongle before any of it can be automated.

## 3. What is deliberately not done, and why

Each of these is a named gap with a written argument, not an oversight.  The
design record named is where the reasoning and the next step live.

1. **A hotplug does not re-set the mode.**  The watch detects, logs and re-reads
   the sink; it does not program the pipe.  That would have to answer who owns
   the console's surface while the pipe is being reprogrammed, whether the
   framebuffer is reused or reallocated, and what happens when the monitor that
   is unplugged is the one the console is on (`intel-hotplug.md` §4).
2. **The display engine's interrupt is not used** -- MSI/MSI-X is not
   implemented in this kernel, there is no LAPIC vector allocator distinct from
   the IOAPIC pin space, and the Intel path has no configuration-space write.
   The probe now reads and reports PCI `Interrupt Pin`/`Interrupt Line`, which
   is the fact that settles whether a legacy INTx route exists at all before
   anyone builds the MSI path (`intel-hotplug.md` §4).
3. **The buffer-translation (voltage-swing) values are the board's.**  §8.5 of
   the reference marks them a gap and forbids guessing them, so they are read
   back out of the PHY the firmware programmed (`swing.rs`).  A port the
   firmware never brought up is refused by name, before the first write, rather
   than filled in from a table this kernel does not have.  Transcribing i915's
   board-tuned tables remains a decision for the owner, not a silent fallback
   (`intel-swing.md` §6).
4. **The GGTT translate-cache invalidate is not written.**  The register
   (`GEN12_GUC_TLB_INV_CR`) is in the forcewake-GT domain, which this kernel
   cannot reach, and nothing here ever unmaps a page, so every address handed
   out is one the display engine has never translated.  The gap, its symptom and
   the experiment that would settle it are in `intel-scanout.md` §3.
5. **The Intel device does not expose the DRM/KMS ioctl surface.**  Stage 2
   lights the panel at boot and hands the console over; userspace mode setting
   on this device -- dumb buffers, page flips, atomic commits -- would need the
   GGTT allocator behind GEM, buffer ownership and eviction rules, and a story
   for the one framebuffer the kernel is already scanning.  None of that is
   here, and the DRM device node remains the virtio one.
6. **No scaling, no colour management, no audio, no DC states, no DMC, no
   Type-C/DKL ports, no HDMI scrambling** -- §11's own preamble excludes the
   first four, §8.8 defers the DKL path, and a mode at or above 340 MHz is
   refused by name rather than driven without the scrambling the sink needs.
7. **`PLANE_WM_LINES`'s ceiling is 31, the reference's figure, where i915
   allows 255 on this display version.**  Writing 31 is legal under both
   readings and is what the generous watermark uses; the disagreement is
   recorded rather than resolved (`intel-pipe.md`).
8. **VT-d is not parsed.**  The scanout buffer is padded with the 64 zeroed page
   table entries and the alignment i915 requires under VT-d, unconditionally,
   because this kernel cannot tell whether VT-d is active.  Whether that is
   needed or sufficient on this machine is unverified (`intel-scanout.md` §8).

## 4. Where this work departs from the reference document

The reference is the specification, and it has been found wrong in specific,
cited places.  Each correction is recorded in the module that hit it, and the
reference document itself was amended where the defect was in its own text.

| Reference | What it says | What the sources say | Where |
|---|---|---|---|
| §5.4 `PLANE_STRIDE` | stride in bytes | 64-byte units; a stride that is not a multiple cannot be expressed | `intel-pipe.md` |
| §7.3 watermarks | level 0 `BLOCKS(4096)` | the field is `[11:0]`, so 4095 with enable | `intel-pipe.md` |
| §7.3, §11 4.2 | eight watermark levels | six: levels 6 and 7 are the SAGV watermark registers, and zeroing them disables watermarks the pipe may be using | `intel-pipe.md` |
| §5.5 `PLANE_COLOR_CTL` | not mentioned | `PLANE_COLOR_PLANE_GAMMA_DISABLE` is set unconditionally by i915; a zero value enables gamma | `intel-pipe.md` |
| §11 4.3 | arm the plane in phase 4 | `PLANE_SURF` only latches at a vblank, and a disabled transcoder has none; i915 enables the crtc first and arms afterwards | `intel-modeset.md` |
| §11 5.6, §8.6 step 12 | `TRANSCONF = ENABLE \| STATE_ENABLE` | bit 30 is the hardware's "pipe is on" *status*; i915 writes bit 31 alone and polls bit 30 | `intel-output.md`, and the reference now carries the correction |
| §6.3, §11 5.4 | `TRANS_CLK_SEL` keyed by port | keyed by PHY from display version 13; `TRANS_DDI_FUNC_CTL` stays keyed by port | `intel-output.md`, reference amended |
| §6.3 DPLL table | config offsets for DPLL0 only | DPLL1's pair is `0x16428C`/`0x164290`, in the header the document already cites; without it combo PHY B has no address at all | `intel-output.md`, reference amended |
| §8.5 | swing values `[GAP]` | unchanged -- read from the firmware's PHY instead of guessed | `intel-swing.md` |

Three more findings came from workstreams rather than the reference:
`Wa_22012358565` (the ARB-slot workaround) is missing from §5.5 entirely; the
ADL-N PLL divider search is `icl_calc_wrpll`'s, not the Skylake one the earlier
code used, and the two disagree on 114 of 985 sampled symbol rates
(`intel-pll.md`); and the modes layer had an EDID 1.4 aspect-ratio bug, fixed
earlier in the branch.

## 5. The boot wiring, in one place

`drm::init_virtio_gpu` runs, in order: the probe, `bring_up_at_boot` (phase 1
then the connector step), `modeset_at_boot`, then the hotplug watch starts.
`/dev/fb0` asks for a surface only later, when the pseudofs is built, so the
console candidate registered by the modeset is in place before anyone consults
it.

`modeset_at_boot` takes the first connector a monitor answered on whose power
came up, reads the GGTT size out of configuration space, maps the page table,
allocates the framebuffer for whichever of the two modes `choose_mode` can
return is larger, reads the swing values back, calls `set_mode`, and hands
`scanout::register` the verdict.  Every refusal -- no monitor, no mapped window,
no power, not the GTTMMADR aperture, no documented size, no page table, no
memory, no swing values, a mode that cannot be programmed -- logs its own named
reason and leaves the firmware's framebuffer on the screen.  The same text goes
to `/sys/kernel/debug/dri/0/intel_gpu`, which is where a run is read back after
the boot log has scrolled.

## 6. Branches

`dev` is the deliverable.  The Intel work was done on `feat/intel-stage2` and
merged here in one commit; the branches below are its workstreams, all merged
into that integration branch except where noted.  None of them is on `main`.

| Branch | What it carries |
|---|---|
| `feat/intel-gtt` | the GGTT page table, the framebuffer allocation, the console surface |
| `feat/intel-mode` | phase 5: PLL selection, DDI, transcoder |
| `feat/intel-pipe` | phases 3.4 and 4: timings, DDB, watermarks, the plane |
| `feat/intel-verify` | phases 3.1, 6: the mode choice, the proof, the pattern, the console gate |
| `feat/intel-connect` | phase 2.1 and the connector the modeset consumes |
| `feat/intel-swing` | §8.5's values, read back from the firmware's PHY |
| `feat/intel-dpll1` | DPLL1's config registers, so combo PHY B can be driven |
| `feat/intel-hpd` | the after-boot hotplug watch |
| `fix/intel-pll-adln` | the ADL-N PLL divider search |
| `fix/intel-swing-per-lane` | per-lane TX register writes, as i915 does them |
| `fix/intel-pipe-registers` | the confirmed defects above, plus the arm split |
| `fix/intel-transcoder-values` | `TRANSCONF`, `TRANS_CLK_SEL`, the DDI buffer's preserved field |
| `fix/intel-scanout-memory` | the observed aperture, the guard page, the padding, the retired offer |
| `fix/klog-loss` | the kernel log's lost records -- not Intel work, merged here because it was branched from this line |
| `feat/exit-status-race` | the registry walk that answered `ECHILD` for a live child -- likewise |
| `docs/n305-display-acceptance` | the hardware acceptance procedure |

Three branches were **absorbed rather than merged**, and should not be merged:
`feat/intel-power` and `feat/intel-gmbus-power` were 50-odd commits behind and
their content was ported file by file into the integration branch, and
`feat/intel-pll` contains only formatting and visibility commits -- its `pll.rs`
was already on the branch.

### What else `dev` carries

Stage 2 is not all of `dev`.  Two merges were made after it, both because the
work was finished and was otherwise sitting outside the branch a reviewer is
asked to read:

* `feat/nic-igc`, merged as `2284b239` -- the Intel i225/i226 driver for the
  acceptance (b) channel, six commits.  It was written 159 commits earlier, so
  the merge was clean textually and it took a build and the suites to believe
  it; `docs/design/nic-igc.md` is its own record, including the long list of
  what host tests and one QEMU boot cannot establish.  Note the shape: nothing
  in `verify --tier daily` compiles it, because the driver exists only in the
  `--net-igc` variant and the product variant's static NIC type is `virtio-net`.
* `feat/hw-bringup`'s last two commits, merged as `608f98cf` -- the capture
  image's measured unreliability on the UEFI path (three misses out of three,
  against a QEMU kernel-loader path that finds the payload every time) and the
  shell profile's byte-identical screenshots.  The merge is followed by
  `7d585ed6`, which corrects the one sentence in it that this tree had
  overtaken -- the run that showed "the marker and no kernel log" predates
  `fix/klog-loss` -- and by the same commit's correction of two rows of the
  acceptance table that pointed at workstreams which have since landed.

One branch is **deliberately not merged**: `feat/hw-facts` is marked `wip` by
its own commit message.  It is the N305 hardware-facts capture tooling
(`tools/hw_facts.py`, `scripts/hw-facts/capture-n305.sh`, and its tests),
written for review rather than for `dev`, and it stays on its branch until it
has been reviewed.

## 7. How to review this

Reading order, if you want the argument rather than the diff:

1. this document, then `docs/design/n305-display-acceptance.md` (what the
   machine will do and how to check it);
2. `intel-modeset.md` (the sequence, its ordering argument, and the bad-case
   table), then `intel-pipe.md` and `intel-output.md` (the two register
   sequences it drives);
3. `intel-scanout.md` (memory, the page table, the console gate), then
   `intel-connector.md`, `intel-swing.md` and `intel-hotplug.md`;
4. `intel-display-registers.md` §11 beside any of them, for what the sequence
   was specified to be.

Every design record ends with what is *not* verified.  Those sections are the
honest edge of this work and are worth reading before the code.
