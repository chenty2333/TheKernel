# N305 display acceptance: the hardware procedure for the Intel bring-up

**Target machine:** Acer 蜂鸟 mini (SQM2270), Intel Core i3-N305 (Alder Lake-N), display device
`8086:46d0`, 16 GB RAM, 512 GB NVMe, no operating system installed, **no serial port**. The screen
is the only output device the machine has.

**What this document is for.** Everything the Intel display work implements is verified against a
host mock register file and against QEMU, and QEMU cannot present an `8086:46d0`-class device: a
QEMU boot exercises the configuration-space walk, the rejection path and the "no Intel display
device present" verdict, "and never a real 8080-family display device"
(`docs/design/intel-gpu-probe.md`). No Intel display register has been written to real silicon.
This is the procedure that replaces that with measurement: what to build, what to do with the
machine, what should appear on the panel, which log lines confirm it, and — for every phase — what
a wrong result looks like and which later check it would explain.

**Status of the procedure itself: it has not been executed.** Nothing in this repository records a
boot of this kernel on the target machine. `config/x86_64/n305.toml` says in its header that the
profile "has never been booted on the machine it describes", and every Intel display design record
carries the same status. The first run is also the first test of this document.

**Companion documents.**

| Document | What it owns |
|---|---|
| `docs/design/n305-bringup.md` | the boot acceptance path: build the stick, boot it, prove the log reaches the screen, turn frames into gate evidence, and the separate hardware-facts capture image |
| `docs/design/intel-display-registers.md` | the register reference; **§11 is the bring-up order this document's acceptance table is organised around**, §11.1 the GMBUS failure modes, §12 the reads that settle each guess |
| `docs/design/intel-power.md`, `intel-gmbus.md`, `intel-pipe.md`, `intel-output.md`, `intel-scanout.md` | each workstream's own record of what is implemented, what is sourced and what is not |
| `docs/design/intel-modeset.md` | the specification of the phase 3–6 driver and of the test pattern (workstream `feat/intel-verify`, not on the branch this document is written on; see §3.4) |

---

## 1. Before starting

### 1.1 The machine

* **One monitor, attached before power-on.** Which output is wired, and which of the chip's DDIs
  that output is, is one of the things this procedure measures: §11 phase 2.3 identifies the
  physical port by the DDC pin that answers, and `docs/design/intel-display-registers.md` §12.2
  makes `SHOTPLUG_CTL_DDI` "the authority on which ports are wired on this board". Attaching the
  monitor later changes the answer mid-run.
* **Secure Boot off.** The boot chain is unsigned. A Secure Boot refusal looks exactly like nothing
  happening, which is the worst failure mode on a screen-only machine
  (`docs/design/n305-bringup.md` §1.2).
* **Firmware in UEFI mode, not legacy/CSM.** The ESP is a GPT disk carrying
  `EFI/BOOT/BOOTX64.EFI`; a CSM boot of it is not a supported path.
* **The screen is the only console, and nothing can be typed on it.** The machine has no serial
  port: `config/x86_64/n305.toml` states it, and the kernel's console probes `0x3f8` for a 16550 and
  writes to no port when none answers (`crates/ax/thekernel-axplat-x86-pc/src/console.rs`). Console
  input has exactly one source — that same UART — so the VT line discipline behind `/bin/sh -i` is
  fed by `axhal::console::read_bytes` and by nothing else
  (`kernel/src/pseudofs/dev/tty/ntty.rs`). A USB keyboard does not change this even if it
  enumerates: its events reach `/dev/input/event*`, not the console. **Every step below is "power
  on and look". The power button is the only input the person at the machine has.**
* **A USB stick of at least 1 GiB**, and at least as large as the image built in §2.1. The ESP
  builder rounds its default size up to a 64 MiB boundary with a 128 MiB floor
  (`scripts/build-x86-uefi-esp.sh`).
* **The NVMe is not part of this procedure.** It is not written to, by the kernel or by the person:
  `lsblk -o NAME,SIZE,TRAN,MODEL` is in §2.2 for the reason it is in
  `docs/design/n305-bringup.md` §1.1 — writing the image to the wrong device destroys the disk.

### 1.2 The development host

* The pinned toolchain: `scripts/setup-toolchain.sh` provisions `rustup`, `cargo`, `rustc` and
  `axconfig-gen`, which `tools/verification.py` checks before any tier runs.
* The ESP builder needs `grub2-mkstandalone` (or `grub-mkstandalone`), `parted`, `mkfs.fat`,
  `mcopy` and `mmd` on `PATH`; it names each one it cannot find
  (`scripts/build-x86-uefi-esp.sh`).
* A state directory on disk, not on tmpfs: the product tooling refuses a state root under `/tmp` or
  `/dev/shm` or on a `tmpfs` mount (`tools/product_state.py` `validate_storage`). The build below
  runs with `THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/n305-display-acceptance`, per
  `AGENTS.md`.
* For the later capture step only: `ffmpeg` and a `/dev/video*` capture device
  (`scripts/ci/n305-screen-capture.sh`).

### 1.3 What "acceptance" means here

Two records of the same boot have to agree, and either one alone is not evidence:

1. **The panel.** What a person sees, photographed. It is the only channel the machine has today,
   and it is what §3's tables describe phase by phase.
2. **The log's own record.** The kernel log ring is mirrored to the screen by the framebuffer
   console (`kernel/src/pseudofs/dev/tty/fbcon.rs`, `install_log_mirror`), and the same text is
   served from `/sys/kernel/debug/dri/0/intel_gpu` (`kernel/src/drm/intel/debugfs.rs`, which serves
   `kernel/src/drm/intel/mod.rs::report_text`: the probe, power and sink reports in one rendering).

The two must agree on the same numbers. The clearest case is the test pattern of §3.4: the marker's
position is a pure function of the frame counter, the log prints the frame counter and the marker's
rectangle, and the panel shows where the marker actually is. A disagreement between those two is
itself the finding.

---

## 2. Build, write the stick, boot

### 2.1 Build the shell-profile image for this machine

```sh
cd /home/ava/Worktrees/TheKernel/n305-display-acceptance
THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/n305-display-acceptance \
python3 tools/thekernel.py build --profile shell --platform n305
```

The build prints two paths on success, and they are the ones to use below:

```text
<state>/out/x86_64/n305/shell/mem1g/kernel-x86_64
<state>/out/x86_64/n305/shell/mem1g/kernel-x86_64.esp
```

`kernel-x86_64.esp` is a complete GPT disk image, not a file to be copied onto a filesystem: it
contains `EFI/BOOT/BOOTX64.EFI` (standalone GRUB), `TheKernel.elf`, and the rootfs as a Multiboot2
module (`scripts/build-x86-uefi-esp.sh` with the tool's default `--rootfs-transport module`).

**Why `--profile shell`.** The acceptance question is "is this on the glass, and can a person still
be looking at it a minute later". `--profile shell` boots `/etc/thekernel/shell-init.sh`, which ends
in `/bin/sh -i`; on a machine with no console input that read never returns, so the log and the
`THEKERNEL_SHELL_READY` prompt stay on the screen indefinitely. `--profile system` runs the KTAP
suite and returns, and nothing on this machine sends the shutdown command that the QEMU runner
would. The reasoning is `docs/design/n305-bringup.md` §2.1 and it applies unchanged to the display
work: on the modeset's success path the console moves to the kernel's own framebuffer, and only a
profile that keeps running keeps drawing into it.

**Why `--platform n305`.** The platform profile selects `config/x86_64/n305.toml` and the
compile-time CPU admission limit — eight slots for the N305's eight E-cores, against the q35
profile's four (`tools/product_state.py` `MACHINE_PROFILES`, applied in `generate_config`). It does
**not** select a QEMU machine; the flag's own help text says so. `--smp` and `--memory` shape the
QEMU run and the generated configuration only; installed RAM on the target comes from the
Multiboot2 memory map and `plat.phys-memory-size` is inert (`config/x86_64/n305.toml`). Leaving both
at their defaults is correct.

Record what was built, because every later question needs it:

```sh
sha256sum <state>/out/x86_64/n305/shell/mem1g/kernel-x86_64.esp
```

### 2.2 Write the image to the stick

The ESP *is* the disk image, so there is no partition to format and no file to copy:

```sh
lsblk -o NAME,SIZE,TRAN,MODEL                      # the stick is the one with TRAN=usb
sudo dd if=<state>/out/x86_64/n305/shell/mem1g/kernel-x86_64.esp \
        of=/dev/sdX bs=4M conv=fsync status=progress
sync
```

`conv=fsync` is the part that matters: without it `dd` returns while the stick still holds data in
its own cache (`docs/design/n305-bringup.md` §1.1).

### 2.3 Power on

Insert the stick, power the machine on, and select the USB entry if the firmware does not prefer
it. Nothing is typed on the machine at any point.

### 2.4 What the screen shows, in order

| # | Screen | Where it comes from |
|---|---|---|
| 1 | the firmware's own logo or splash | firmware |
| 2 | GRUB's output, brief or absent: `timeout=0` and one entry means the menu does not linger, but GRUB's terminal is on the console as well as the serial port, so anything GRUB has to say — an unreadable kernel, a missing module — is said on the screen | `config/x86_64/grub.cfg`, whose comment records that the screen carries GRUB's output with `console` in `terminal_output` and is untouched without it |
| 3 | `THEKERNEL  <step>` in the top row, with the newest log lines beneath it, the step advancing through `runtime entry`, `heap allocator`, `memory management`, `platform devices`, `scheduler`, `driver init`, `filesystems`, `secondary CPU bring-up`, `interrupt init`, `kernel main` | the early screen: `crates/ax/thekernel-axruntime/src/lib.rs` calls `early_screen_milestone` at each step, `kernel/src/pseudofs/dev/early_screen.rs` paints it. It exists because the framebuffer console is installed from the device filesystem, which is far too late to report a failure before it (`docs/design/early-screen.md`) |
| 4 | the retained kernel log, then `THEKERNEL_SHELL_READY` and a `# ` prompt, and then **no further change** | the framebuffer console mirrors the kernel log ring to the screen once the device filesystem publishes fbdev (`kernel/src/pseudofs/dev/tty/fbcon.rs`), replaying the ring from its start, so the probe's lines — printed before there was a console — arrive on the panel rather than being lost |

The screen holds 64 rows (`MAX_ROWS` in `kernel/src/pseudofs/dev/tty/fbcon.rs`) and the log ring
holds 64 KiB, so the log is bounded from both ends. On a boot where the display work runs, the
phase 0–2 report is roughly forty lines and fits; a later phase 3–6 run adds the pipe and output
programs and can push the earliest probe lines off the top. That is what
`/sys/kernel/debug/dri/0/intel_gpu` is for (§4).

### 2.5 Telling a boot from a handoff that produced nothing

| What the screen shows | What it means | What to do |
|---|---|---|
| nothing at all, not even the firmware logo | no boot from USB: wrong boot entry, CSM mode, or Secure Boot refusing the stick | firmware setup: UEFI mode, Secure Boot off, boot the USB entry |
| firmware logo, then GRUB's text and nothing after it | the firmware ran `BOOTX64.EFI` and GRUB stopped: a missing `/TheKernel.elf`, an unreadable module, a failed `search` | read GRUB's message on the screen; the ESP contents are listed in §2.1 |
| firmware logo, then nothing, with no GRUB text either | Secure Boot is the first suspect; clear the platform keys | `docs/design/n305-bringup.md` §2.4 |
| `THEKERNEL  <step>` present and frozen at one step | the kernel started and stopped there. A `*** PANIC ***` banner and a backtrace replace the status row when the panic handler runs | photograph the whole screen; that frame is the evidence |
| milestones advance, then the screen stops before `THEKERNEL_SHELL_READY` | the kernel stopped during or after filesystem initialization | keep the frames; a panic display separates the two cases |
| a log appears whose first lines include `boot framebuffer: accepted addr=… WxH bpp=… pitch=…` | the bootloader handed over a framebuffer and the console is drawing into it | normal; continue to §3 |
| `boot framebuffer: declined: …` instead | the kernel has no surface to draw the console on, so the log will not appear anywhere | this is a bootloader video-configuration problem, not a kernel one; see the row for it in `docs/design/n305-bringup.md` §2.4 |
| log, then `THEKERNEL_SHELL_READY` and no further change | the boot reached the shell and stayed there | acceptance of the boot path; go to §3 |

`MB2 framebuffer: …` is not in this table on purpose: it is written by
`crates/ax/thekernel-axplat-x86-pc/src/boot_info.rs` through the diagnostic channel at `0x2f8`,
which this machine does not have. The same verdict is in the kernel log as
`boot framebuffer: …` (`crates/ax/thekernel-axruntime/src/lib.rs`), and that is the form the panel
can show.

---

## 3. Per-phase acceptance

The log records carry a severity, a timestamp, a CPU and a target, so what appears on the panel
looks like `[0.91 INFO target=thekernel_kernel::drm::intel::probe] intel-gpu: …`. The strings quoted
below are the message text: that is what to look for on the panel, and the same text is in
`/sys/kernel/debug/dri/0/intel_gpu` and in `dmesg`.

### 3.1 Which phases run at boot on this branch

The bring-up order is `docs/design/intel-display-registers.md` §11. What is wired into the boot
path at the time of writing:

| §11 phase | Code | Runs at boot? |
|---|---|---|
| 0 — enumerate, identify, map, read | `kernel/src/drm/intel/probe.rs`, `pci.rs`, `id.rs`, `regs.rs` | **yes** — `drm::init_virtio_gpu` calls `intel::probe_at_boot()` (`kernel/src/drm/mod.rs`) |
| 1 — power, clocks, combo PHY | `power.rs`, `clk.rs`, `phy.rs` | **yes** — `intel::bring_up_at_boot()` calls `power::bring_up` per mapped device |
| 2 — AUX/DDC well, hotplug, EDID | `sink.rs`, `gmbus.rs`, `hpd.rs` | **yes** — the same function calls `sink::probe_at_boot` after power |
| 3.1 — choose the mode | `drm::modes::plan_modeset` | **yes, as a plan** — the sink step hands the validated EDID to the mode layer, which logs its choice. Nothing consumes the plan yet |
| 3.2 — framebuffer and GGTT | `fb.rs`, `gtt.rs`, `scanout.rs` | no — implemented and tested, called from nowhere |
| 3.3–3.4 — PLL dividers, timing registers | `pll.rs`, `timing.rs`, `pipe.rs` | no |
| 4 — DDB, watermarks, plane | `pipe.rs` | no |
| 5 — PLL, DDI, transcoder | `output.rs`, `pll.rs`, `phy.rs` | no |
| 6 — prove it | `pipe.rs` (6.1, 6.2, 6.4), `output.rs` (6.3) | no |
| the driver that calls 3.2–6 in order | `modeset::set_mode` | **not written**; specified in `docs/design/intel-modeset.md` |

So a boot of this branch today accepts phases 0, 1 and 2, plus the mode *plan* of 3.1. §3.5 states
what the procedure becomes when the modeset driver lands, and what on the panel will identify it.

### 3.2 Phase 0 — the probe (§11 phase 0)

`intel::probe_at_boot` walks PCI configuration space through the platform's ECAM aperture, finds
Intel display functions, identifies them against the device table, maps the register aperture of a
device it has a model for, and reads the registers the model says are safe. It writes no register
and no configuration space (`docs/design/intel-gpu-probe.md`).

| | |
|---|---|
| **Panel** | Nothing changes *while it runs*: the framebuffer console is installed later, when the device filesystem publishes `/dev/fb0`, and it mirrors the log ring from its start (`kernel/src/pseudofs/dev/fb.rs` calls `fbcon::install()`; `kernel/src/entry.rs` runs the DRM initialization before `pseudofs::mount_all`). The report appears on the panel when the console does. |
| **Lines that confirm it** | `intel-gpu: Intel display probe`; `intel-gpu: configuration space: ECAM at 0x…, buses 0..=<N>`; `intel-gpu: bus walk: <N> PCI functions answered, <M> from Intel, <K> display-class`; `intel-gpu: intel function 0000:00:02.0 (…): display controller`; `intel-gpu:   identity: Alder Lake-N integrated graphics (Gen12 graphics, …, 0x46d0)`; `intel-gpu:   register window: mapped 2097152 bytes of … at 0x… to 0x…, uncached, read-only in effect`; one `intel-gpu:   <REGISTER> (0x<offset>) = 0x<value>…` line per modelled register; `intel-gpu: verdict: 0000:00:02.0 identified and read (<N> registers over a 2097152 byte window); nothing was written; no register was written and no display mode was set` |
| **How to falsify it** | `verdict: probe did not run: …` means the DRM initialisation path itself did not execute, and nothing later in this document applies. `no Intel display device present: … none of them a display controller` means the device is not on the bus the kernel scanned — check the ECAM line first, then the firmware's graphics setting. `register window: none (<reason>)` means the device was found and refused: the BAR is missing, decodes no address, or contradicts the modelled aperture size. `verdict: … reported but not touched: …` is the same refusal in the device's own line. A register line reading `not read: <reason>` is deliberate and named, not a silent omission. The other falsifier is a **cross-check** line ending in disagreement — `GGC VAMEN=… and class …` is a comparison of one value read over PCI with one read out of the aperture, and disagreement means the aperture being read does not belong to the function being described. |
| **What a wrong result explains** | Everything downstream. With no mapped window, `bring_up_at_boot` logs `intel-gpu: no device with a mapped register window, so the power and sink steps of the bring-up order did not run` and stops. A blank screen after that is a phase 0 result, and the phases below cannot be blamed. |

### 3.3 Phase 1 — power (§11 phase 1)

`power::bring_up` reads the fuses and straps first, then executes §11's order: DC states off, combo
PHY initialization with PHY A first, `PW_1`, CDCLK, raw clock, DBUF slices, and the platform
workarounds. The sequence is `docs/design/intel-power.md` §2.

| | |
|---|---|
| **Panel** | No change; the log is the observable. |
| **Lines that confirm it** | `intel-gpu: powering up 0000:00:02.0 (reference section 11 phase 1)`, then a block whose every line starts `intel-gpu: power: `. The load-bearing ones: `fuses: pipes present A+B+C+D (A 1 B 1 C 1 D 1); …`; `power well PW_1 (index 0, request 0x2, state 0x1): state came up, control 0x… -> 0x…; PG0 distributed (FUSE_STATUS 0x…); …; requesters: bios … driver … kvmr … debug …`; `combo PHY A: present (COMP_DW0 0x…), …, is the compensation source; initialised (<N> register writes)`; `combo PHY A: COMP_INIT after PW_1 is 1`; `CDCLK: reference 19.2 MHz (DSSM[31:29] 0b001), PLL enabled, ratio <n>, VCO <n> kHz, CD2X <divider>, CDCLK <n> kHz, decimal 0x…`; `raw clock: SFUSE_STRAP[8]=… states <n> kHz, PCH_RAWCLK_FREQ was …, already correct, left alone`; `DBUF: <n> of <n> slices up (…)` |
| **How to falsify it** | A single `intel-gpu: power: <reason>` warning line, repeated in the debug file, and **no sink step for that device**: §11 phase 1 is the gate everything else is behind, so the sequence stops there by design. The specific falsifiers to look for in the block: `state …` never `came up` (the well handshake; §11 phase 1.3 lists the well index, the fuse bit and another requester as the causes, in that order); `combo PHY A: COMP_INIT after PW_1 is 0` (the PHY is not powered, and §11 phase 1.2 says a `COMP_INIT` that does not stick means exactly that); `CDCLK: … PLL disabled` (no pixel clock exists, so no mode can be programmed — the phase 3.1 refusal would be true but useless); `raw clock: … the firmware and the strap DISAGREE about the crystal and the strap won` (GMBUS and hotplug de-glitching are then mis-timed, which shows up as *intermittent* EDID failures, not clean ones); `DBUF: 0 of <n> slices up` |
| **What a wrong result explains** | Phase 2's characteristic failure — GMBUS returning NAK on every address, always (§11 phase 2.1) — is a phase 1 result when the AUX/DDC well could not be requested, and `intel-gmbus` says so in those words rather than reporting it as a missing monitor. A phase 1 DBUF failure is the first candidate explanation for a latched `PIPESTAT` underrun at phase 6.4. |

### 3.4 Phase 2 — the sink (§11 phase 2)

The boot-time form of §11 steps 2.2 and 2.3: hotplug is enabled for every DDI and read once, then
every DDC pin this driver can select is asked for an EDID. **Step 2.1 — requesting the AUX/DDC
power well — is deliberately not here**: the sink step names a well that reads back off, with its
state bit, rather than writing another workstream's register
(`kernel/src/drm/intel/sink.rs`). The validated base block and extension go to the mode layer,
which logs one decision line.

| | |
|---|---|
| **Panel** | No change; the log is the observable. |
| **Lines that confirm it** | `intel-sink: looking for a monitor on 0000:00:02.0 (reference section 11 phase 2)`; then, for the pin that answers, `intel-gmbus: a monitor answered on pin 1 (dpa, DDI A): 128 bytes, checksum valid, <n> extension block(s) declared`; then the mode layer's `drm: display mode chosen: 1920x1080@60 (sink preferred timing, <n> considered, <m> excluded)`; then one hotplug line per DDI, `intel-hpd: DDI A: HPD enabled (SHOTPLUG_CTL_DDI 0x… -> 0x…, latched detect field: …), the live connect bit is set: a sink is connected; SDEISR 0x…, SOUTH_CHICKEN1 0x…` |
| **How to falsify it** | `intel-gmbus: no monitor answered on any DDC pin this kernel can select` is a **negative result about the wiring, not a failure**: the pin that answers *is* how the physical port is identified (§11 phase 2.3), so this line means the monitor is on a pin this driver does not select (a Type-C pin, whose DKL PHY §8.8 defers) or is not attached/awake. The per-pin lines name which of §11.1's failure modes occurred: `SATOER` (sink NAKed, or nothing at that address), `STALL_TIMEOUT` (a secondary held the clock), `ACTIVE` (the transaction never terminated), all bytes `0xff` (the bus floated, so no pull-ups or no device), a checksum that does not sum to zero modulo 256 (a partial or stale read), and the one that is a phase 1 finding: `every address NAKed … and AUX_A reads back as off`. On the hotplug side, `HPD NOT enabled after the write`, or a live-connect bit whose reading is stated both ways because the board's inversion is unresolved (§9.5), falsifies the assumption that the port is wired. For the mode: `drm: no advertised display mode is usable; programming the built-in fallback …` is a **warning**, and a driver that programmed that fallback would be turning a parse failure into a display failure. |
| **What a wrong result explains** | Phase 3.1's refusal. With no usable EDID there is no timing to program, so a blank panel later is not evidence about the pipe, the plane, the PLL or the DDI. A monitor found on DDI C or on a Type-C pin is refused by the output stage before it writes anything (`UnsupportedDdi`: C and D are not combo-PHY ports, and Type-C needs DKL), which is a port-selection result rather than a clock result. |

### 3.5 Phases 3 to 6 — the procedure once the modeset driver lands

**Status: not on this branch.** Phases 3.2 to 5.7 exist as tested modules (`fb.rs`, `gtt.rs`,
`timing.rs`, `pipe.rs`, `pll.rs`, `output.rs`, `scanout.rs`) but nothing calls them: there is no
`set_mode` driver, so no register of phases 3 to 5 is written at boot on this branch, no plane is
armed, and no pixel is scanned out by this kernel. The driver and the test pattern are specified in
`docs/design/intel-modeset.md` on the `feat/intel-verify` workstream; until that lands, this section
is the procedure to run afterwards, and the panel will keep showing the firmware's picture.

When it lands, the panel stops being neutral, and the acceptance of phases 3 to 6 is read off it.
The signature is §11 phase 6.5's: **eight vertical colour bars, left to right white, yellow, cyan,
green, magenta, red, blue, near-black**, with a marker drawn as the complement of the bar underneath
it, walking a grid of its own size, one cell per frame counter value. Every visible pixel of the
pattern is non-zero, including the darkest bar (`0x0010_1010`) and the marker over the white bar
(mid grey `0x0080_8080`), so a black screen can never be the pattern
(`kernel/src/drm/intel/pattern.rs`, workstream `feat/intel-verify`).

| §11 step | Panel | Log lines | Falsify |
|---|---|---|---|
| 3.1 mode choice | — | `drm: display mode chosen: …` (or the refusal: no EDID, nothing programmable) | A refusal leaves the firmware framebuffer alone; the panel then shows the firmware's picture and the run is over for that device |
| 3.2 framebuffer, GGTT, pattern fill | — | the pattern geometry line, `pattern: <W>x<H> xrgb8888, stride <n> B, frame <n>, marker <side>x<side> at (<x>, <y>)`, and the surface's GGTT address | A stride that is not a multiple of 64 bytes is refused before the plane is armed; a plane armed on an address the GGTT does not translate is §11 phase 6.2's failure |
| 3.3–3.4 PLL arithmetic, timing registers | the monitor may lose sync here | every timing register value, then the PLL's `ref`, `(P, Q, K)` and symbol rate | A rolling or off-centre image means a total is off by one; "no signal" or "out of range" means the timing or the pixel clock is outside what the sink accepts; a doubled or halved image is the 38.4/19.2 MHz reference or the DCO fraction workaround |
| 4.1–4.3 DDB, watermarks, plane | **from the write of `PLANE_SURF`, the bars** — the pipe no longer scans the firmware's aperture, so the log is no longer on the panel | `intel-pipe: pipe A programmed with <n> writes (reference section 11 phases 3.4 and 4)`, then one line per write | If the bars never appear, the plane did not arm or the pipe is not scanning: the pipe module's own `intel-pipe: pipe A phase 6.1/6.2/6.4: …` lines say which |
| 5.1–5.7 PLL, DDI, transcoder | dark until the DDI is up; then the bars again if the clock is right | `intel-output: DDI A on combo PHY A, hdmi, 4 lane(s), pixel clock <n> kHz`, the divider block, then `intel-output: DDI A up: … DDI_BUF_CTL 0x… (IS_IDLE clear, read back 0x…), …` | A failure at any step is one named error line; the panel's state is in `docs/design/intel-modeset.md` §4.2 |
| 6.1–6.4 prove it | the bars, and the marker where the log says it is | `intel-pipe: pipe A phase 6.1: PIPEDSL … changed …` (with the sampled values and the derived line rate), `… phase 6.2: PLANE_SURFLIVE …`, `… phase 6.4: PIPESTAT …`, and the output stage's `IS_IDLE clear` line | Each check's failure carries its reading: samples that never changed, a `PLANE_SURFLIVE` that disagrees with the address written, a raw `DDI_BUF_CTL`, a raw `PIPESTAT` |

The one check that deserves its own reaction is §11 phase 5.7, `DDI_BUF_CTL.IS_IDLE`, because the
real-hardware field report is about exactly it — see §6.3.

### 3.6 The two outputs that must agree

The pattern is a measuring instrument only if the panel and the log can be compared, and they can:
the marker's position is a pure function of the frame counter, and the log prints both.

* The log line is `pattern: <W>x<H> xrgb8888, stride <n> B, frame <n>, marker <side>x<side> at (<x>,
  <y>)`. The rectangle is in surface coordinates, in pixels, with the origin at the top left.
* On the panel, the marker is a square of that side, at that position, drawn as the complement of
  the bar beneath it. At `frame 0` it is at the top-left corner by construction — the position is
  `(index % columns) * side, (index / columns) * side` with `index = frame % positions` — so it sits
  over the white bar and is mid grey.
* **Agreement is the acceptance**: the square on the panel is where the log says it is, and the bar
  boundaries in the photograph fall where integer division of the width by eight puts them.
* **Disagreement is a finding.** A marker that is not where the log says, or bars that are sheared
  or shifted, means the surface the engine is scanning is not the surface that was filled, or its
  stride is not the stride the pattern was written with. Both are visible by eye and both are what
  §11 phase 6.2's address comparison would report in numbers.
* **A marker that never moves is not proof of anything yet.** On the workstream as specified the
  frame counter is advanced by a bounded repaint that is not implemented
  (`docs/design/intel-modeset.md` §7.4), so a first run writes one frame and the marker stands
  still. "Is the pipe scanning" is then answered by §11 phase 6.1's `PIPEDSL` samples, not by the
  panel; the panel answers "is the framebuffer path wired up at all".
