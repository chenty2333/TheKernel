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
| `docs/design/intel-power.md`, `intel-gpu-probe.md`, `intel-gmbus.md`, `intel-connector.md`, `intel-modeset.md`, `intel-pipe.md`, `intel-output.md`, `intel-swing.md`, `intel-scanout.md` | each workstream's own record of what is implemented, what is sourced and what is not |
| `docs/design/intel-hotplug.md` | the after-boot watch, and §4's argument for why nothing re-modesets when a monitor arrives later — the limitation §6.5 of this document states |

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
  writes to no port when none answers (`crates/ax/tk-axplat-x86-pc/src/console.rs`). Console
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
* The BusyBox rootfs the ESP carries is built locally on the first build: `scripts/build-rootfs.sh`
  needs `curl`, `debugfs`, `make`, `mke2fs`, `realpath`, `tar`, `touch` and `truncate` on `PATH`, and
  downloads BusyBox 1.36.1 into the state directory's `source-cache` unless it is already there.
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

The two must agree on the same numbers. The clearest case is the test pattern of §3.9: the marker's
position is a pure function of the frame counter, the log prints the frame counter and the marker's
rectangle, and the panel shows where the marker actually is. A disagreement between those two is
itself the finding.

---

## 2. Build, write the stick, boot

### 2.1 Build the shell-profile image for this machine

```sh
cd <the checkout of the branch being accepted>
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
| 3 | `THEKERNEL  <step>` in the top row, with the newest log lines beneath it, the step advancing through `runtime entry`, `heap allocator`, `memory management`, `platform devices`, `scheduler`, `driver init`, `filesystems`, `secondary CPU bring-up`, `interrupt init`, `kernel main` | the early screen: `crates/ax/tk-axruntime/src/lib.rs` calls `early_screen_milestone` at each step, `kernel/src/pseudofs/dev/early_screen.rs` paints it. It exists because the framebuffer console is installed from the device filesystem, which is far too late to report a failure before it (`docs/design/early-screen.md`) |
| 4 | the retained kernel log, then `THEKERNEL_SHELL_READY` and a `# ` prompt, and then **no further change** | the framebuffer console mirrors the kernel log ring to the screen once the device filesystem publishes fbdev (`kernel/src/pseudofs/dev/tty/fbcon.rs`), replaying the ring from its start, so the probe's lines — printed before there was a console — arrive on the panel rather than being lost |

The screen holds `MAX_ROWS` = 64 rows at most — fewer on a smaller mode, since the console's
geometry is the panel's height divided by the 16-pixel cell height and clamped to that ceiling
(`dimensions()` in `kernel/src/pseudofs/dev/tty/fbcon.rs`) — and the log ring holds 64 KiB. Both are
bounded, and the mirror shows the **newest** content, so by the time the shell prompt is on the
screen the earliest lines of a boot may have scrolled off the top: the display report is logged
before the console exists, and everything after it is newer. Photograph the screen as the log
appears, not only at the end, and read `/sys/kernel/debug/dri/0/intel_gpu` (§4) when a channel to
the machine exists. `/proc/sys/kernel/log_stats` says whether the ring overlapped anything.

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
`crates/ax/tk-axplat-x86-pc/src/boot_info.rs` through the diagnostic channel at `0x2f8`,
which this machine does not have. The same verdict is in the kernel log as
`boot framebuffer: …` (`crates/ax/tk-axruntime/src/lib.rs`), and that is the form the panel
can show.

---

## 3. Per-phase acceptance

Every log record is one line shaped
`<6>[<secs>.<micros> cpu=<n> tid=<n> INFO target=<module path>] <message>`
(`crates/ax/tk-axruntime/src/klog.rs`), so a display line on the panel looks like:

```text
<6>[0.910000 cpu=0 tid=1 INFO target=tk_kernel::drm::intel::probe] intel-gpu: Intel display probe
```

The strings quoted below are the message text, which is what to look for on the panel: everything is
in `dmesg`, and the probe, power, connector and hotplug parts are also rendered into
`/sys/kernel/debug/dri/0/intel_gpu` (§4.2 — the modeset's own lines are not, so the phase 3–6 report
has to be read from the log or off the panel).

### 3.1 What runs at boot, in order

The bring-up order is `docs/design/intel-display-registers.md` §11. One call chain runs it, from
`drm::init_virtio_gpu()` (`kernel/src/drm/mod.rs`, `kernel/src/entry.rs`), before the device
filesystem is mounted:

| Step | §11 phase | What it does | Code |
|---|---|---|---|
| 1 | 0 | walk PCI configuration space, identify the display function, map its register window, read the safe registers, print the report | `intel::probe_at_boot` → `probe::run` |
| 2 | 1 | fuses and straps, DC states off, combo PHY init, `PW_1`, the PHY re-read, CDCLK, raw clock, DBUF, the platform workarounds | `intel::bring_up_at_boot` → `power::bring_up` |
| 3 | 2.1 | request the AUX/DDC power well behind every candidate pin | `connect::resolve_at_boot` → `connect::enable_wells` |
| 4 | 2.2–2.3 | enable hotplug and read every DDI once; ask every DDC pin for an EDID; validate it; hand it to the mode layer; assemble the connector | `connect::resolve_at_boot` → `sink::probe_one` → `gmbus` / `hpd` / `drm::modes::plan_modeset` |
| 5 | 3.2 | map the GTT array, allocate the surface for `max(mode layer's choice, 1920x1080)`, paint §11 phase 6.5's pattern into it | `intel::modeset_at_boot` → `gtt::Gtt::map`, `fb::Surface::allocate`, `modeset::set_mode` → `paint_pattern` |
| 6 | 3.1, 3.3–3.4, 4, 5 | read the swing values back out of the firmware's PHY; choose the mode against the live CDCLK; compute every register value; write the pipe's shadow registers; program the output; **arm the plane**; prove it took | `modeset::set_mode` → `swing::read_firmware_swing`, `pipe::compute`, `pipe::program`, `output::program`, `pipe::arm`, `prove_it` |
| 7 | 6 | offer the surface to the console with the verdict — the handover is a decision, not a side effect | `scanout::register(surface, verdict)` |
| 8 | 2.2 | start the after-boot hotplug watch: one `SDEISR` read every 250 ms, and a re-probe of the sink on a transition | `intel::start_hotplug_watch` |
| 9 | — | when devfs publishes `/dev/fb0`, the console consults its candidates in rank order and takes the first usable surface: `intel-display` (rank 0) if phase 6 proved a scanout, otherwise `firmware-aperture` (rank 200) | `drm::screen::console_scanout` |

So a boot of this branch runs phases 0 to 6. Steps 5 to 7 are the newest part of the wiring, and
three greps say whether the tree an image was built from contains all of it — each is a call the
tables below assume exists, and each is invisible in a log that otherwise looks healthy:

```sh
grep -n modeset_at_boot kernel/src/drm/intel/mod.rs          # the boot wiring: no mode is set without it
grep -n "pipe::arm" kernel/src/drm/intel/modeset.rs          # the arm call, between the output and prove_it
grep -n "MODESET.lock() = Some" kernel/src/drm/intel/mod.rs  # the modeset section of the debug file
```

If the first prints nothing, the log stops after the connector and the panel keeps the firmware's
picture. If the second does, the plane is never armed: `pipe::program` writes only shadow registers,
so `PLANE_SURF` is never written, phase 6.2 reports `WrongAddress` (the firmware's own surface is
still armed) and the console stays on the firmware aperture. If the third does, the phase 3–6 report
exists only in the boot log, which is the one thing on this machine that scrolls.

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
| **What a wrong result explains** | Everything downstream. With no mapped window, `bring_up_at_boot` logs `intel-gpu: no device with a mapped register window, so the power and connector steps of the bring-up order did not run` and stops. A blank screen after that is a phase 0 result, and the phases below cannot be blamed. |

### 3.3 Phase 1 — power (§11 phase 1)

`power::bring_up` reads the fuses and straps first, then executes §11's order: DC states off, combo
PHY initialization with PHY A first, `PW_1`, CDCLK, raw clock, DBUF slices, and the platform
workarounds. The sequence is `docs/design/intel-power.md` §2.

| | |
|---|---|
| **Panel** | No change; the log is the observable. |
| **Lines that confirm it** | `intel-gpu: powering up 0000:00:02.0 (reference section 11 phase 1)`, then a block whose every line starts `intel-gpu: power: `. The load-bearing ones: `fuses: pipes present A+B+C+D (A 1 B 1 C 1 D 1); …`; `power well PW_1 (index 0, request 0x2, state 0x1): state came up, control 0x… -> 0x…; PG0 distributed (FUSE_STATUS 0x…); …; requesters: bios … driver … kvmr … debug …`; `combo PHY A: present (COMP_DW0 0x…), …, is the compensation source; initialised (<N> register writes)`; `combo PHY A: COMP_INIT after PW_1 is 1`; `CDCLK: reference 19.2 MHz (DSSM[31:29] 0b001), PLL enabled, ratio <n>, VCO <n> kHz, CD2X <divider>, CDCLK <n> kHz, decimal 0x…`; `raw clock: SFUSE_STRAP[8]=… states <n> kHz, PCH_RAWCLK_FREQ was …, already correct, left alone`; `DBUF: <n> of <n> slices up (…)` |
| **How to falsify it** | A single `intel-gpu: power: <description> (<Debug of the error>)` warning line, repeated in the debug file, and **no sink step for that device**: §11 phase 1 is the gate everything else is behind, so the sequence stops there by design. A failure *after* `PW_1` came up is reported as `<step> failed after PW_1 came up: <cause>`, followed by whether this call's well request was withdrawn — the rollback exists so the display is not left powered but unprogrammed (`power::unwind`). The specific falsifiers to look for in the block: `state …` never `came up` (the well handshake; §11 phase 1.3 lists the well index, the fuse bit and another requester as the causes, in that order); `combo PHY A: COMP_INIT after PW_1 is 0` (the PHY is not powered, and §11 phase 1.2 says a `COMP_INIT` that does not stick means exactly that); `no DBUF slice came up` (§11 phase 1.5: the symptom is not a failure to start but every frame underrunning, which is far harder to attribute later); `CDCLK: … PLL disabled` (no pixel clock exists, so no mode can be programmed — the phase 3.1 refusal would be true but useless); `raw clock: … the firmware and the strap DISAGREE about the crystal and the strap won` (GMBUS and hotplug de-glitching are then mis-timed, which shows up as *intermittent* EDID failures, not clean ones) |
| **What a wrong result explains** | Phase 2's characteristic failure — GMBUS returning NAK on every address, always (§11 phase 2.1) — is a phase 1 result when the AUX/DDC well could not be requested, and `intel-gmbus` says so in those words rather than reporting it as a missing monitor. A phase 1 DBUF failure is the first candidate explanation for a latched `PIPESTAT` underrun at phase 6.4. |

### 3.4 Phase 2 — the connector (§11 phase 2)

Phase 2 is one composed step, `connect::resolve_at_boot`, and it runs in the reference's order for
the reference's reason: **the AUX/DDC power wells before the bus** (§11 phase 2.1 — a channel behind
a shut gate NAKs every address and looks exactly like an empty port), **then one pass over the bus**
(§11 phases 2.2 and 2.3: hotplug enabled and read once per DDI, then an EDID from every selectable
pin), **then the mode layer**, whose plan is carried in the connector so that the modeset takes the
bytes the plan was actually made from. What comes out is one `Connector`: the pin a monitor answered
on, the DDI that pin carries, the validated blocks, the plan, the live hotplug state, and what each
well handshake observed.

| | |
|---|---|
| **Panel** | No change; the log is the observable. |
| **Lines that confirm it** | `intel-connect: powering the DDC pin pair on 0000:00:02.0 and asking what is attached (reference section 11 phase 2)`; then one line per well, `intel-connect: power well AUX_A (index 0, request 0x2, state 0x1): state came up, control 0x… -> 0x…; requesters: bios … driver … kvmr … debug …`; then the bus, `intel-gmbus: a monitor answered on pin 1 (dpa, DDI A): 128 bytes, checksum valid, <n> extension block(s) declared`; then one hotplug line per DDI, `intel-hpd: DDI A: HPD enabled (SHOTPLUG_CTL_DDI 0x… -> 0x…, latched detect field: …), the live connect bit is set: a sink is connected; SDEISR 0x…, SOUTH_CHICKEN1 0x…`; then the connector itself, `intel-connect: display 0000:00:02.0: pin 1 (dpa, DDI A) carries DDI A, 128 bytes, checksum valid, 1 extension block declared; mode layer chose 1920x1080@60 (SinkPreferred)` — the mode layer's own `drm: display mode chosen: …` line is in the same block, because it logs where it is called |
| **How to falsify it** | The step ends with **no connector**, and the warning names which of five things happened, because the next move differs for each: `no monitor answered on any DDC pin this kernel can select: <per-pin answers>` (the wiring, not a failure — the pin that answers *is* how the physical port is identified, so this means the monitor is on a pin this driver does not select, or is not attached/awake); `a sink answered on <pin> but the block it returned did not validate: …` (a display whose bytes cannot be trusted — re-read, lower the GMBUS rate, check the cable); `every address NAKed … and AUX_A reads back as off` (`ConnectError::WellDown`, the §11 phase 2.1 finding, with the register words §11 phase 1.3 asks a reader to compare); `<pin> answered with a valid EDID but carries no DDI` (a Type-C pin, whose DKL PHY §8.8 defers); and the hotplug-reading variants. Per-pin detail comes from §11.1's failure modes, named in `intel-gmbus:` lines: `SATOER`, `STALL_TIMEOUT`, `ACTIVE`, all bytes `0xff` (the bus floated), a checksum that does not sum to zero modulo 256. A well that did not come up is **not** fatal here: the firmware may already hold it on, so the step records it and lets the bus answer. |
| **What a wrong result explains** | Everything from §11 phase 3 on. With no connector there is no DDI, no validated EDID and no plan, so `modeset_at_boot` logs one line and returns — the panel keeps the firmware's picture, and that outcome is a phase 2 result rather than evidence about the pipe, the plane, the PLL or the DDI. |

### 3.5 Phases 3.2 to 6 — the modeset

`intel::modeset_at_boot` is the only caller of `modeset::set_mode` in the product build, and it is
§11 phases 3.2 to 6 as one boot-time step. It takes the first connector whose device came up in
phase 1 — there is one display engine and one console, so a second monitor has nowhere to go — and
in order:

1. **map the GTT array** from the BAR 0 aperture the probe mapped (`gtt::Gtt::map`); a BAR that is
   not the GTTMMADR aperture, or has no documented size, is refused by name;
2. **allocate the surface for `max(mode layer's choice, 1920x1080)` XRGB8888.** The mode is not known
   yet, because `choose_mode` runs inside `set_mode`, and those are the only two modes it can return,
   so a surface that covers both covers the choice. A frame that cannot be allocated is refused by
   name with the firmware's console still up;
3. **read §8.5's voltage-swing values back out of the PHY the firmware programmed**
   (`swing::read_firmware_swing`). This is `docs/design/intel-swing.md`: the numbers are the board's,
   not ours, and a register dump of a working configuration is §13.4's route where a copied table
   would be a guess. A refusal is logged and the request carries no values (§6.4);
4. **`modeset::set_mode`**, which computes everything before it writes anything:
   * read CDCLK (`clk::observe`) — one pixel per clock, so CDCLK is also the mode choice's ceiling —
     and refuse if there is no usable one;
   * `choose_mode`: the mode layer's answer, unless it cannot be programmed at all, in which case
     §11 phase 3.1's 1920×1080@60 and the log says what was set aside and why; a refusal, never a
     guess, when the EDID gave no usable timing;
   * paint the pattern into the surface (§11 phases 3.2 and 6.5, before anything can scan it out);
   * compute the pipe program (`pipe::compute`: timings, DDB, watermarks, plane) and the output
     program (`OutputProgram::plan`: PLL dividers, DDI mapping, swing, transcoder);
   * take the **pre-sample** of `PIPESTAT`, `PLANE_SURFLIVE` and `DDI_BUF_CTL` — the registers phase 6
     will read again, read *before* the first write, so a failure can say whether its reading is
     attributable to this modeset at all;
   * **`pipe::program`**: the 25 shadow registers (timings, DDB, watermarks, plane stride/position/
     size/offset/colour, `PIPE_MISC`, `PIPE_ARB_CTL`). None of them has taken effect yet: every one
     is double buffered, and the arm is what latches them;
   * **`output::program`**: §11 phase 5 in §8.6's order, ending at `DDI_BUF_CTL` and the `IS_IDLE`
     poll. This is also what enables the transcoder, and therefore what makes a vblank exist;
   * **`pipe::arm`**: §11 phase 4.3's two writes, `PLANE_CTL` then `PLANE_SURF`, adjacent — the
     commit;
   * **`prove_it`**: phase 6's four checks (§3.7), including the wait for the plane to latch.
5. **the report** — `outcome.log()` to the kernel log, and `ModeOutcome::render()` into the static
   that `/sys/kernel/debug/dri/0/intel_gpu` serves, so the whole modeset is readable after the log
   has scrolled (§4.2);
6. **`scanout::register(surface, verdict)`** — offer the surface to the console, with phase 6's
   verdict as the gate (§3.8).

**Why the arm comes after the output, one step out of the reference's numbering.** `PLANE_SURF` is
only the arm: the shadow registers `pipe::program` writes are latched at the plane's update event,
which is the pipe's vblank, and while the transcoder is disabled there is no vblank to latch them at
— *"Until the pipe starts `PIPEDSL` reads will return a stale value"* (`[I915]`
`display/intel_display.c:478-486`). A `PLANE_SURF` written before `TRANSCONF` therefore cannot take
effect until after it, and this driver does not depend on a pre-enable write latching later:
`[I915]` enables the CRTC first (`intel_enable_crtc`, `:7200`) and arms the plane afterwards
(`intel_update_crtc`, `:7249`). §11 phase 4.3 arms the plane inside phase 4, which is the order
coreboot's libgfxinit ships; the reference is followed everywhere else, and its own step 4.3 text
("if `ENABLE` is set but `SURFLIVE` is 0, the surface address was rejected") is exactly the
misreading this order avoids — a zero `SURFLIVE` before the pipe runs means *not latched yet*, not
*rejected*, and §3.7's two 6.2 failures are how a reader tells those apart.

**The panel.** Until the arm's `PLANE_SURF` write the panel shows whatever the firmware put there,
and from that write on it shows the pattern: the pipe no longer scans the firmware's aperture, so the
kernel log is no longer on the panel either. If the verdict passes, the console moves onto the Intel
surface when `/dev/fb0` is published and fbcon repaints the log over the pattern; if it does not, the
panel keeps the firmware's picture (or whatever the half-programmed output left on it) and the
console stays where it was.

### 3.6 The log, line by line

In boot order, with what each line means and what a wrong value looks like. `<mode>` is the mode
that was programmed; the values are examples for 1920×1080@60 at 8 bpc.

| Line | Means | A wrong value looks like |
|---|---|---|
| `intel-modeset: allocating <W>x<H> XRGB8888 for <bdf> (phase 3.2), then programming pipe A through DDI A (phases 3.4 to 5.7)` | the surface is sized and the port is named before any register is touched | `<W>x<H>` smaller than the programmed mode cannot happen by construction; a `DDI` other than the one the connector named is the port-selection fault §6.3 starts from |
| `intel-modeset: no monitor answered on any DDC pin, so no mode is set and the firmware's framebuffer keeps the console (reference section 11 phase 3.1)` | there was no connector to program | terminal: the run ends here, and §3.4 is the phase to read |
| `intel-modeset: display <bdf> never came up in phase 1, so it is not programmed; the power failure above is the finding, not this line` | phase 1 failed for the device the connector named | the power block above it is the finding |
| `intel-modeset: the window mapped for <bdf> is <aperture> (BAR <n>), not the GTTMMADR aperture …` / `… no documented size for <aperture> …` / `intel-modeset: <GttError>` | the register window cannot host the GTT array | terminal; the probe's BAR lines (§3.2) say why |
| `intel-modeset: <FbError>` | the framebuffer could not be allocated | `the page allocator could not supply <n> physically contiguous pages (<bytes> bytes) for the framebuffer`, or a stride/size refusal — terminal, console untouched |
| `intel-modeset: DDI A has no buffer-translation values to replay: <SwingSource>` | the swing read-back refused; phase 5 will refuse too, before its first write | see §6.4: this is the named refusal for a port the firmware never brought up |
| `intel-modeset: the mode was not set: <inner error> (<Debug of it>).  The firmware's framebuffer keeps the console (reference section 11 phase 6)` | **the fatal wrapper**: anything `set_mode` refuses or fails at ends here, once, with the inner error in full — a phase 3.1 refusal, an unusable CDCLK, a pipe write that was refused, any phase 5 failure, or `arming the plane failed: <PipeError>` | which inner error it is decides the next step: `no usable CDCLK …` is phase 1, `the mode layer had no usable EDID …`/`NothingProgrammable` is phase 2's EDID, `DDI A is not a combo-PHY port …` is port selection, `MissingBufferTranslation` is §6.4, `DDI A never left idle: DDI_BUF_CTL_A still reads IS_IDLE set (0x…) 500 us after writing 0x…` is §6.3, and a `PipeError` names the register it could not write. This same line is what `/sys/kernel/debug/dri/0/intel_gpu` carries when the modeset did not produce an outcome |
| `intel-modeset: mode: <mode> -- the mode layer's own choice (<reason>), which this driver can program` | whose decision the mode was | `-- reference section 11 phase 3.1's preference.  The mode layer chose <other>, which cannot be programmed because …` is an override, and it is logged rather than silent; `mode: not set -- …` is a refusal and ends the run |
| `intel-modeset: pattern: <W>x<H> xrgb8888, stride <n> B, frame 0, marker <side>x<side> at (0, 0)` | what was painted, and where the marker is | a stride that is not a multiple of 64, or a marker not at `(0, 0)` on frame 0, contradicts §3.9 |
| `intel-modeset: mode <mode> (<vtotal> lines, <refresh> Hz refresh, <clock> kHz pixel clock)` | the timing the registers were computed from | `<clock>` above CDCLK or above the 300 MHz no-scrambling ceiling is refused before this line |
| `intel-pipe: pipe A programmed with 25 shadow writes (reference section 11 phases 3.4 and 4)`, then `intel-pipe: pipe A PIPE_MISC 0x… -> 0x…`, `… PIPE_ARB_CTL 0x… -> 0x…`, then one `intel-pipe:   <REGISTER> <- 0x…` per write | phase 3.4 and 4's shadow half, with every value. Nothing has taken effect yet | a timing field that is not `value − 1`, a `PLANE_STRIDE` that is not `pitch / 64`, or a watermark level 0 with `PLANE_WM_EN` clear |
| `intel-pipe: pipe A armed with 2 writes (PLANE_CTL then PLANE_SURF)`, then `intel-pipe:   PLANE_CTL_A <- 0x…` and `intel-pipe:   PLANE_SURF_A <- 0x…` | the arm, after the output is up: the two writes that latch everything the shadow half wrote | `PLANE_CTL_A` without `ENABLE`/`FORMAT_XRGB8888`/`TILED_LINEAR`, or a `PLANE_SURF_A` that is not the surface's GGTT address. This pair is the last write before phase 6, and a missing pair means the plane is not armed at all |
| `intel-output: DDI A on combo PHY A, HDMI, 4 lane(s), pixel clock <n> kHz` | the port and the clock phase 5 will drive | a DDI/PHY pair that does not match the connector's, or a lane count the sink does not have |
| `intel-output: reference <n> kHz from SKL_DSSM, <n> kHz into the DCO arithmetic, dco_fraction halved per WA #22010492432` | the reference clock the arithmetic used | `left as computed` where the strap says 38.4 MHz, or a reference that contradicts §3.3's `CDCLK:` line — every divider below is then wrong by a factor |
| `intel-output: (P, Q, K) = (…), total divider …, DCO … kHz aimed at … kHz (… centipercent away), symbol rate … kHz, achieved … Hz, error … ppb` | §11 phase 5.1's arithmetic, printed before the write | a symbol rate that is not the mode's pixel clock, or a `DOES NOT AGREE`-sized error; at 1080p60 the symbol rate is 148 500 kHz |
| `intel-output: PLL CFGCR0 = …, CFGCR1 = … under the named encoding` | the two divider registers | the other encoding (`Skylake` codes) puts the same numbers in the wrong fields |
| `intel-output: ICL_DPCLKA_CFGCR0: DDI_CLK_SEL = …, then the DDI_CLK_OFF bit … cleared in a separate write` | the DDI-to-PLL mapping, and the separate clock-off clear §11 phase 5.2 requires | a `DDI_CLK_SEL` naming the other PLL, or a clock-off bit that was not cleared, is 6.3's failure |
| `intel-output: voltage-swing level <n> from read back from the PHY the firmware programmed, DDI A level <n> (i915's table is 'icl_combo_phy_trans_hdmi'; …), DW2 per lane […], DW4 per lane […], DW7 per lane […]; 4 lane(s) -> PWR_DOWN_LN_MASK field 0x0` | the values phase 5.3 replays, and where they came from | a `source` that does not name the PHY read-back, or per-lane values that are all equal where the read showed otherwise |
| `intel-output: PHY_LINK_RATE = 0 (no sourced HDMI encoding in reference section 8.6; this is the field's reset value and section 13.4's dump-diff settles it)` | the one field written by inference | a non-zero value here would need a source; there is none in the tree |
| `intel-output: TRANS_CLK_SEL(A) = …, TRANS_DDI_FUNC_CTL(A) = …, TRANSCONF(A) = …, DDI_BUF_CTL(A) = …` | the transcoder and buffer control words as planned | `TRANSCONF` without `ENABLE`/`STATE_ENABLE`, or a `TRANS_CLK_SEL` naming no port, is 6.1's failure |
| `intel-output: DDI A up: PLL CFGCR0 … (LOCK set), <DDI-IO well>, DDI_BUF_CTL 0x… (IS_IDLE clear, read back 0x…), TRANS_CLK_SEL(A) …, …` | phase 5 finished, including the `IS_IDLE` poll §11 phase 5.7 calls the best "is my DDI alive" bit | a missing `LOCK set`, or `IS_IDLE` still set: phase 5 then fails and the fatal line carries `DDI A never left idle: DDI_BUF_CTL_A still reads IS_IDLE set (0x…) 500 us after writing 0x…` — §6.3 |

### 3.7 Phase 6 — the four checks, and what a failure means

Phase 6 logs four lines, then the verdict. A passing check is `info`, a failing one `warn`; every
failure carries the reading it was decided from. The check's number is the reference's, so a line
matches a row of §11 directly.

| Check | Passing line | Failing line (abridged) | Means and next |
|---|---|---|---|
| **6.1** `PIPEDSL is advancing` | `intel-modeset: 6.1 PIPEDSL is advancing: 4 samples over 3000 us, line rate 67500 Hz against the mode's 67500 Hz (agrees); changed: 0x… -> 0x…` | `6.1 PIPEDSL is advancing: FAILED -- PIPEDSL read the same line on all 4 samples over 3000 us: 0x…@…us, 0x…@…us, 0x…@…us, 0x…@…us` | the pipe is not scanning, so nothing downstream matters. Remedy in the verdict: the PLL (5.1, including `ref` and the `(P,Q,K)` used), then the DDI clock mapping (5.2), then `TRANSCONF`'s `ENABLE` and `STATE_ENABLE` (5.6), in that order. The line rate is evidence, not a verdict: it is §12.3's scope-less check of the PLL arithmetic, and the mode's own rate is printed beside it |
| **6.2** `PLANE_SURFLIVE reads back the surface` | `intel-modeset: 6.2 PLANE_SURFLIVE reads back the surface: 0x… == 0x…` | either `NotLatched` — `PLANE_SURFLIVE still read zero <n> us after the arm, so 0x… has not latched.  Read the 6.1 verdict first: an arm latches at a vblank, and a pipe that is not scanning has none` — or `WrongAddress` — `PLANE_SURFLIVE read 0x… where 0x… was written: the plane is scanning a different surface` | the two are different faults and the log says which: see the row below. A passing 6.2 can also carry `; the register already named this surface before the modeset wrote, so the read-back does not attribute the arm to this write` — a true reading that is not evidence |
| **6.3** `DDI_BUF_CTL.IS_IDLE is clear` | `intel-modeset: 6.3 DDI_BUF_CTL.IS_IDLE is clear: DDI_BUF_CTL = 0x…` | `6.3 DDI_BUF_CTL.IS_IDLE is clear: FAILED -- DDI_BUF_CTL = 0x… still has IS_IDLE set, and it was idle before this modeset wrote anything too: it never came up` (or `… and it was already out of idle before this modeset wrote anything, so it came up and went back to idle`) | read once, deliberately: it is a live bit, and a retry that passed after a first failure would hide the fault. Remedy: the DDI-to-PLL mapping (5.2) then the PLL (5.1), in that order — and then §6.3 of this document, which is the same poll as i915 bug #10932 |
| **6.4** `PIPESTAT has no FIFO underrun` | `intel-modeset: 6.4 PIPESTAT has no FIFO underrun: PIPESTAT = 0x…` | `6.4 PIPESTAT has no FIFO underrun: FAILED -- PIPESTAT = 0x… has PIPE_FIFO_UNDERRUN_STATUS set, and it was already set before this modeset wrote anything, so it may predate it -- the bit is sticky and this kernel's register table declares PIPESTAT read-only` (the second clause is `pre_existing`, and is absent when the bit was clear before) | the watermarks or the DDB are wrong: §11 phase 6.4 says to go back to 4.2 before changing anything else. Without `pre_existing` the bit is attributable to this modeset; with it, the honest reading is "an underrun is latched, and it may or may not be from this run" |
| verdict | `intel-modeset: phase 6 verdict: the display engine is scanning out the surface this mode set programmed` | `intel-modeset: phase 6 verdict: the mode was programmed but the display engine is not scanning out: 6.x <title> failed -- <reading>; ….  <first failure's advice>` | one value, and it is the console gate: `ScanningOut` only when all four agree. Every failing check is in the line, not just the first, and the last sentence is the first failure's §11 remedy |

**A black screen: `WrongAddress` or `NotLatched`.** Because the arm is its own step (§3.5), a 6.2
failure has two distinct meanings and a reader should be able to pick between them from the log
alone:

| Reading | What it means | What to do |
|---|---|---|
| `WrongAddress` — a different, non-zero address | the plane **is** armed, on a buffer that is not this surface: `PLANE_SURFLIVE` holds a live address and it is not the one `PLANE_SURF` was written with. This is not a late latch — a plane whose arm had not latched would read zero | first check the arm pair is in the log at all: if it is not, the arm never ran (§3.1's second grep) and the live address is the firmware's. If it is, the writes did not take, and §11 phase 4.3's causes apply — a `PLANE_SURF` that is not the surface's GGTT address, or a plane the firmware re-armed afterwards |
| `NotLatched` — zero at the deadline (about two frame times) | the plane never latched, which is what a pipe with no vblank produces: the shadow writes are waiting for an update event that a disabled transcoder never generates. The arm wrote `PLANE_SURF`, and the pipe is not running | read 6.1 first: if the pipe is not scanning, this is that fault, not a plane fault. Only with a scanning pipe does a zero mean the surface address was rejected (§11 step 4.3: alignment, or a GGTT entry that is not valid) |

`{register} could not be read, so check 6.x could not run: the register is outside the mapped window`
is the one failure that is about this kernel rather than the machine: the window the probe mapped is
2 MiB and a register outside it cannot be read at all.

### 3.8 The console handover

Programming is not handover. The surface becomes the console's only through `scanout::register`,
and `drm::screen` consults its candidates when `/dev/fb0` is published — after this whole sequence
has run. The candidate the Intel driver registers is `intel-display` at rank 0 (the firmware
aperture is rank 200), so if the verdict passed it wins and the console moves; if it did not, it
says why and the firmware's aperture keeps the screen.

| Line | Means |
|---|---|
| `scanout: the Intel surface will not take the console: the modeset did not prove the display engine is scanning this surface out: <verdict>.  The firmware framebuffer keeps the screen, and this reason is reported through the candidate list (reference section 11 phase 6)` | the `warn` from `scanout::register`, printed at boot when the verdict was not `ScanningOut` |
| `scanout: candidate 'intel-display' (rank 0) selected: <W>x<H> pitch <p>, because the display engine reads this framebuffer through the GGTT, and a plane this kernel programmed is scanning it out` | the handover happened; from here the kernel's own console draws into the Intel surface |
| `scanout: candidate 'intel-display' (rank 0) has nothing to offer: the modeset did not prove …` | the console asked and the refusal above is why; the next line names the winner |
| `scanout: candidate 'firmware-aperture' (rank 200) selected: <W>x<H> pitch <p>, because the firmware programmed this display and nothing in the kernel did` | the console is still on the firmware's surface: the panel is unchanged and the log keeps working |
| `scanout: candidate 'firmware-aperture' (rank 200) not consulted: 'intel-display' already won` | the handover happened, and this is the confirmation that the firmware aperture lost rather than being unavailable |

### 3.9 The two outputs that must agree

The pattern is a measuring instrument only if the panel and the log can be compared, and they can:
the marker's position is a pure function of the frame counter, and the log prints both. The pattern
is §11 phase 6.5's: **eight vertical colour bars, left to right white, yellow, cyan, green, magenta,
red, blue, near-black**, with a marker drawn as the complement of the bar underneath it, at
`marker_rect(width, height, frame)`. Every visible pixel is non-zero, including the darkest bar
(`0x0010_1010`) and the marker over the white bar (mid grey `0x0080_8080`), so a black screen can
never be the pattern (`kernel/src/drm/intel/pattern.rs`).

* The log line is `intel-modeset: pattern: <W>x<H> xrgb8888, stride <n> B, frame <n>, marker
  <side>x<side> at (<x>, <y>)`. The rectangle is in surface coordinates, in pixels, origin top left.
* On the panel, the marker is a square of that side at that position, drawn as the complement of the
  bar beneath it. At `frame 0` it is at the top-left corner by construction — the position is
  `(index % columns) * side, (index / columns) * side` with `index = frame % positions` — so it sits
  over the white bar and is mid grey. `modeset_at_boot` sets `request.frame = 0`, so a boot without a
  repaint always reports frame 0.
* **Agreement is the acceptance**: the square on the panel is where the log says it is, and the bar
  boundaries in the photograph fall where integer division of the width by eight puts them.
* **Disagreement is a finding.** A marker that is not where the log says, or bars that are sheared or
  shifted, means the surface the engine is scanning is not the surface that was filled, or its
  stride is not the stride the pattern was written with. Both are visible by eye and both are what
  §11 phase 6.2's address comparison would report in numbers.
* **A marker that never moves is not proof of anything yet.** Nothing advances the frame counter: the
  surface is painted once, at frame 0, and the repaint `docs/design/intel-modeset.md` §7.4 describes
  is not implemented. "Is the pipe scanning" is therefore answered by §11 phase 6.1's `PIPEDSL`
  samples, not by the panel; the panel answers "is the framebuffer path wired up at all".

---

## 4. Collect this before asking for help

### 4.1 The evidence set

A report about a display failure on this machine is only actionable with all four of these:

1. **What was built.** The build command and the image's digest:
   `sha256sum <state>/out/x86_64/n305/shell/mem1g/kernel-x86_64.esp`.
2. **The panel, photographed in order**, from the firmware logo to the last thing on it. Photograph
   every distinct screen, not only the last one: the early screen's `THEKERNEL  <step>` row and a
   panic banner are different findings, and the first frame that differs from a known-good boot is
   the interesting one.
3. **The log lines**, transcribed or read off the photographs. They are the same text as the debug
   file, so a photograph and a `cat` are interchangeable as evidence.
4. **Where the run stopped**, named as a phase of §11 — the acceptance table in §3 is written so
   that "it stopped at phase 1" is a complete sentence.

### 4.2 The paths on the running system

These are the files and commands that carry the display evidence, all read-only, all reachable from
the shell profile once a channel to the machine exists:

| Path or command | What it holds |
|---|---|
| `/sys/kernel/debug/dri/0/intel_gpu` | everything the display work learned, in one file, in the order it ran: the probe's report, the power step's log, the connector's report (the wells, the per-pin answers, the mode layer's choice, the hotplug reading), **the whole modeset** — the mode chosen, the pattern's frame and marker, the shadow write list, the arm pair, the output program and its read-backs, and phase 6's four readings with the verdict — and the after-boot hotplug watch's events and last re-probe. On a failure it carries the `intel-modeset: the mode was not set: …` line instead of the outcome, so the file is never silent about a run that did not happen (`kernel/src/drm/intel/debugfs.rs`, `kernel/src/drm/intel/mod.rs::report_text`) |
| `/sys/kernel/debug/dri/0/thekernel_metrics` | the DRM resource snapshot: whether a primary device is registered, framebuffers, resources (`kernel/src/pseudofs/graphics_metrics.rs`) |
| `dmesg` | the retained kernel log ring, which is what the screen mirrors. The same display text as the file above, plus everything else the boot logged |
| `/proc/sys/kernel/log_stats` | retention and loss counters for that ring. **Read this before concluding an event did not happen**: a full ring overwrites and a full diagnostic queue drops, and the counters say which |
| `/proc/sys/kernel/log_filter` | the live capture filter; writable with `CAP_SYSLOG` (`docs/debugging.md`) |
| `/proc/iomem`, `/proc/cmdline` | the physical memory map and the boot command line, for the addresses the display work maps |
| `lspci -nn` | the PCI functions, including the display function's device id and BARs (the capture image carries `lspci`; the product rootfs is BusyBox) |

**On today's machine none of these can be read by typing**, because console input has one source —
the UART — and there is none (§1.1). What is on the panel is what exists. Three ways to change that,
the last of which does not exist yet:

* **The screen-capture dongle** (not owned yet; §6.1) turns the panel into files on the development
  host. That is the intended channel for a display acceptance run.
* **The capture image** of `docs/design/n305-bringup.md` §1 boots a throwaway Linux environment from
  the same stick and writes a hardware-facts bundle to a FAT partition **on the stick**, which is
  then read back on the development host with `python3 scripts/ci/hw_facts_bundle.py <bundle-dir>`.
  That is the offline route for the machine facts the kernel cannot yet print: the graphics BARs,
  the OpRegion and VBT, i915's own debug state, the framebuffer format and **the decoded EDID**
  (which the DUT cannot decode: `edid-decode` is not packaged for that Alpine release). It says
  nothing about TheKernel's log.
* **The network** is the third path and is still not available, although its lowest layer has
  landed: the target's NIC is an Intel i225/i226, and `feat/nic-igc` (merged as `2284b239`) brings a
  driver for it (`docs/design/nic-igc.md`), built by the `--net-igc` variant because the product
  variant's static NIC type is `virtio-net` and one non-`dyn` build cannot be both. What is missing
  is everything above the driver: nothing in the guest puts the KTAP transcript on the wire, and the
  driver itself has never run on real silicon -- its only automated evidence is host tests over a
  synthetic device, plus a QEMU boot that exercises the one path QEMU can exercise, the verdict for
  a machine that has no such part. So the Ethernet cable is still for that workstream, and
  `docs/design/n305-bringup.md` §3 applies once it lands.

### 4.3 The minimal reproducer

One cold boot of one image, with nothing else changed:

```sh
THEKERNEL_STATE_DIR=… python3 tools/thekernel.py build --profile shell --platform n305
sha256sum …/kernel-x86_64.esp
# write the ESP to a stick, power on, photograph, power off
```

A report is reproducible when it names that digest, the phase the run reached, the exact log lines
from the phase that failed, and what the panel showed at that moment. A run that changed two things
— a different EDID, a different stick, a re-plugged monitor — is a new run, not a repeat.

---

## 5. What is verified, and what is not

The repository's convention is that an unverified claim is labelled as one; each Intel workstream's
design record carries its own status line, and they all say the same thing: `intel-power.md` "none
of this has run on the target machine", `intel-gpu-probe.md` "No register value in any report is
verified against real hardware", `intel-gmbus.md` "Nothing in this workstream has run on that
machine", `intel-modeset.md` "No part of this workstream has run against a display engine",
`intel-pipe.md` and `intel-scanout.md` likewise, `intel-output.md` "nothing in this module has run
against silicon", `intel-hotplug.md` "Nothing in this workstream has run on hardware". This is where
a reader finds out which claim is which. Two of those records go further than the summary below and
should be read with it: `intel-scanout.md` §8 lists the memory half's unverified facts one by one
with the reading that would settle each, and `docs/design/intel-modeset.md` §8 does the same for
phases 3 to 6.

| Claim | How it is verified today | What that is worth |
|---|---|---|
| PCI configuration-space walk, BDF arithmetic, BAR decoding, the device table, the identity decision | host tests over a synthetic bus (`kernel/src/drm/intel/testbus.rs`) and synthetic configuration space; QEMU exercises the same walk against emulated configuration space and reaches the verdict `no Intel display device present` | proves the code path and the decoding. It says nothing about this machine's bus, its BARs or its revision |
| The probe's register reads and their interpretation | host tests over a mock register file and a synthetic aperture | the offsets and decodings are the reference's; no value has ever been read from a real Gen12 part |
| Power wells, combo PHY, CDCLK, raw clock, DBUF, the workarounds | host tests over `regs::mock::MockRegisters`, which can be made to fail in named ways | the sequence and the arithmetic. Whether `PW_1` comes up on this board, and whether the fuse bits are where the reference says, is unknown until §3.3 runs |
| GMBUS transactions, EDID validation, the §11.1 failure modes | 27 host tests with a controller that can be made to fail, plus a fake clock (`kernel/src/drm/intel/gmbus/tests.rs`) | the protocol and the error attribution. No transaction has ever run on a real DDC bus |
| The AUX/DDC well handshake, the connector's composition, and every named refusal | host tests over the mock and a modelled controller (`kernel/src/drm/intel/connect/tests.rs`, `sink/tests.rs`) | the order and the diagnosis. Whether the firmware leaves the wells on is hardware's answer |
| Hotplug enable, the live connect read, the after-boot watch and its re-probe | host tests over bit positions, transitions and the "a failed read is not a state" rule (`kernel/src/drm/intel/hpd/tests.rs`, `mod.rs`) | the encoding and the edge-detection policy. Which DDI the monitor is on is hardware's answer |
| The mode layer (EDID → mode) | host tests plus fixtures (`kernel/src/drm/modes/`) | that a given EDID selects a given timing. Real monitors send real EDIDs, which is a different test |
| Timings, DDB, watermarks, plane, PLL dividers, the DDI sequence, the pattern, the phase 6 verdicts, `set_mode` end to end | host tests over the mock, including the write order and `set_mode` as one sequence (`kernel/src/drm/intel/modeset/tests.rs`, `pattern/tests.rs`, `pipe/tests.rs`, `output/tests.rs`, `pll.rs`) | arithmetic, ordering, byte-exact pattern geometry and the verdict logic. QEMU writes none of these registers: it has no Gen12 display engine to write them to |
| Reading the swing values back out of the firmware's PHY, and its six named refusals | host tests over a modelled port (`kernel/src/drm/intel/swing/tests.rs`) | that a plausible-looking register dump is refused when the port was never enabled. Whether this machine's firmware leaves such a dump is the first thing the hardware run finds out |
| Framebuffer allocation, GGTT entries, the console candidate gate | host tests (`fb.rs`, `gtt.rs`, `scanout.rs`), inventoried in `docs/design/intel-scanout.md` §9 | the address arithmetic and the refusal path. §8 of that document lists what they cannot show, starting with "the host tests themselves prove nothing about device memory" |
| The call sequence itself — that `bring_up_at_boot` reaches `set_mode`, that the plane is armed between the output and phase 6, and that the surface is offered to the console | **not verified by any test.** These are call sites, and they are the newest part of the tree | §3.1's three greps are the check; a boot log that stops after the connector, or a 6.2 `WrongAddress` naming the firmware's surface, says which call is missing |
| The console handover keeping the firmware's surface on a failed verdict | **reasoned, not observed** (`docs/design/intel-modeset.md` §8) | a prediction |
| Anything on the target machine | **0 boots.** Nothing in this repository records TheKernel running on this hardware | nothing at all |

Two commands are easy to mistake for target verification, and neither is it:

* `python3 tools/thekernel.py verify --tier daily` runs the repository's own stages — dependency
  layers, host tests, a product build, Clippy, a TCG guest suite, and the firmware-framebuffer
  console suite — on the development host (`tools/verification.py` `plan`). It proves the tree is
  consistent; it does not touch the N305.
* `python3 tools/thekernel.py verify --tier hardware` runs the **host's** KVM CPU suite
  (`test --suite cpu --smp 4 --accel kvm`) and the complete Linux ABI differential
  (`test --suite abi --smp 4 --accel kvm`), gated on the development host having `/dev/kvm`.
  It is not a hardware-in-the-loop tier and it says nothing about the display.

The "verified today" column can be reproduced with `python3 tools/thekernel.py test --suite host`,
which runs the kernel crate's host tests including every Intel module's, and `python3
tools/thekernel.py lint` for the product configuration's Clippy policy. Neither needs the target
machine, and neither can substitute for it.

The one QEMU result that is worth something to this document is negative and complete: because QEMU
cannot present a `8086:46d0`-class device, a QEMU boot can never execute phases 1 to 6, so no QEMU
result can confirm or refute a single register write in the Intel path
(`docs/design/intel-gpu-probe.md`, "What it does not prove").

---

## 6. What this procedure cannot cover yet

### 6.1 No capture dongle is owned, so the panel is read by eye

The plan is two steps. **First**, the run in this document: the person at the machine photographs
the panel, and the acceptance of each phase is a human judgement against §3's table. That is enough
to catch the failures that matter — a well that never comes up, no EDID, no bars, bars with the
marker in the wrong place — because each of them is a large, visible difference. It is not enough
for anything that needs frames in sequence or a second pair of eyes later.

**Later**, a driver-free UVC HDMI capture dongle on the development host replaces the person. The
capture script reads it through `v4l2` with `ffmpeg`, so the dongle needs no driver of its own and
the host needs no kernel work:

```sh
scripts/ci/n305-screen-capture.sh capture --out ~/n305-display-run1   # start before power-on
# power on; wait until the screen has stopped changing; interrupt the capture
scripts/ci/n305-screen-capture.sh verdict --dir ~/n305-display-run1 \
    --completion-frame <N> --marker THEKERNEL_SHELL_READY --checker "<who read it>"
```

The frames are P6 PPM files at one per second, and the verdict names the first frame in which the
marker is legible plus a later frame, so that "the capture kept looking" is evidence rather than an
assumption. The gate drives the same capture through its hooks — each hook is protected runner
configuration, not gate input, and the screen hook must write `frame-0000.ppm`, `frame-0001.ppm`, …
into `$THEKERNEL_DUT_SCREEN_DIR` and the verdict to `$THEKERNEL_DUT_SCREEN_VERDICT`, which
`n305-screen-capture.sh` does:

```sh
THEKERNEL_DUT_POWER_CYCLE_CMD='…' \
THEKERNEL_DUT_BOOT_ONCE_CMD='…' \
THEKERNEL_DUT_SCREEN_CAPTURE_CMD='…' \
python3 scripts/ci/dut_gate.py --artifact-dir <out> --state-dir <state> \
    --dut n305 --observation screen --runs 3
```

Two limits are worth stating before someone runs it: the gate's own screen marker is
`# THEKERNEL_SYSTEM_TEST_COMPLETE`, which the shell profile never prints (the capture above passes
`--marker` explicitly), and screen evidence cannot prove per-test KTAP results — the gate says so on
stdout rather than implying otherwise (`docs/design/n305-bringup.md` §2.5).

What the dongle adds for *this* work specifically is time: the marker's position as a function of
the frame counter can only be checked across frames if the frames exist, and a capture at one frame
per second is the cheapest instrument that can tell a scanning pipe from a frozen image by eye.

### 6.2 The marker does not advance

Per §3.9: `modeset_at_boot` sets `request.frame = 0` and nothing repaints, so a successful modeset
shows a static pattern at frame 0. The panel then proves the framebuffer path is wired up and shows
where the marker is for the frame that was written; it does not prove the pipe is *still* scanning.
§11 phase 6.1's `PIPEDSL` sampling is the proof, and on this machine it is a log line, not a picture.
The repaint is `docs/design/intel-modeset.md` §7.4; the frame counter and `ModesetOutcome`'s
`frame` field exist for it.

### 6.3 The known real-hardware risk: `DDI_BUF_CTL.IS_IDLE` never clearing

`docs/design/intel-display-registers.md` §11 phase 5.7 records the closest thing to a field report
for this exact bring-up: **i915 bug #10932**, an N200 / `46d0` machine under coreboot + EDK2 failing
with *"Timeout waiting for DDI BUF D to get active"* — the DDI buffer idle poll never clearing. The
reference's own advice, repeated in `kernel/src/drm/intel/output.rs`'s `DdiNeverIdle` message and in
phase 6.3's verdict, is the order of suspicion: **check the port and the `aux_ch` mapping, and which
DDI is physically wired, before suspecting the PLL.** `IS_IDLE` stays set while the DDI has no clock,
and a wrong DDI is a far more common cause than a wrong divider.

A run that ends here ends with `intel-modeset: the mode was not set: DDI A never left idle: DDI_BUF_CTL_A still reads IS_IDLE set (0x…) 500 us after writing 0x….  Reference section 11 phase 5.7: … (DdiNeverIdle { … })` during phase 5, or — if the poll passed and the DDI later went back to idle — phase 6.3's `still has IS_IDLE set`. Either way the line carries the DDI
name, the raw `DDI_BUF_CTL`, what was written and the 500 µs timeout, so the checks below can be done
from the log without the machine. In this order:

1. **Which DDI the monitor answered on.** The phase 2 line names it: `intel-connect: display
   0000:00:02.0: pin 1 (dpa, DDI A) carries DDI A, …`, and the modeset's own allocation line repeats
   it (`… then programming pipe A through DDI A …`). §11 phase 2.3 makes the answering pin the
   authority on the physical port; if the output stage programmed a different DDI, that is the fault
   and the PLL arithmetic is not implicated.
2. **Which DDIs report a live connection.** `intel-hpd: DDI <X>: HPD enabled (… latched detect
   field …)` is one line per DDI, and §12.2 calls `SHOTPLUG_CTL_DDI` the authority on which ports are
   wired. A board whose inversion is unresolved is reported both ways rather than picked, so read
   the field's raw value (in the same line) and not only the prose.
3. **Whether the pin's DDI is one this driver can even drive.** DDI C and D are not combo-PHY ports
   and Type-C needs the DKL PHY, which §8.8 defers: the output stage refuses those before writing
   anything, with `UnsupportedDdi`, and the swing read-back refuses them first with its own named
   reason. A monitor that answered on a Type-C pin is a port-selection problem, not a clock problem.
4. **Then the DDI-to-PLL mapping and the PLL**, in §11's order: `ICL_DPCLKA_CFGCR0` (step 5.2,
   including the separate write that clears `DDI_CLK_OFF`), then step 5.1's `LOCK` and the logged
   `ref`, `(P, Q, K)` and symbol rate.

### 6.4 The swing values are the machine's, and a machine whose firmware never brought the port up is refused by name

§8.5's voltage-swing and pre-emphasis numbers are the one `[GAP]` phase 5 cannot derive, and that is
why `docs/design/intel-swing.md` exists: the values are **read back out of the PHY the firmware
programmed** (`swing::read_firmware_swing`) and phase 5 replays them, bit for bit, per lane. On the
target this is the normal case — the firmware has been driving the panel since power-on, which is
what the console has been showing — so the read is a dump of a working configuration rather than a
guess (§13.4's route).

The other case is the one to recognise: **a port the firmware never brought up has no such values,
and the read refuses before the first write.** `read_firmware_swing` checks the state a working port
must be in and refuses with a named `SwingSource` for each:

| Refusal | What was read | What it means |
|---|---|---|
| `PhyNotInitialised` | `PORT_COMP_DW0.COMP_INIT` clear | §8.3's initialisation never ran on that PHY; the TX registers hold a reset value, not a program |
| `PortNotEnabled` | `DDI_BUF_CTL.ENABLE` clear | the firmware never enabled the port |
| `PortStillIdle` | `DDI_BUF_CTL.IS_IDLE` set | the port is not driving |
| `LanesAllPoweredDown` | `PORT_CL_DW10.PWR_DOWN_LN_MASK` all ones | §8.6's lane power step never ran |
| `TrainingNotEnabled` | `PORT_TX_DW5.TX_TRAINING_EN` clear | §8.5 step 6, the write that commits the batch, never happened |
| `PhyNotResponding` | the swing words all read as an absent block | the block is not there, or is not clocked (the §12.2 absent-block test) |

What happens then is deliberate and worth knowing before it is seen: `modeset_at_boot` logs
`intel-modeset: <DDI> has no buffer-translation values to replay: <reason>`, hands the request no
values, and phase 5 refuses with `MissingBufferTranslation` **before its first write**. Nothing is
programmed, the firmware's picture stays on the panel, and the reason is on the panel with it. What
to do about it: this is a statement about the machine's state, not a bug to work around — the port
has to be driven before its values can be read, so boot the machine with the firmware's own video
path intact (the default: do not disable the firmware framebuffer), or take the values from a
register dump of a working configuration on the same board (§13.4). Do not transcribe i915's table:
two Intel PRMs disagree about those numbers for the same nominal level because they are
board-tuned, which is the whole reason for the `[GAP]`.

Two smaller values remain unsourced rather than read:

* **`PHY_LINK_RATE` has no sourced HDMI encoding** (§8.6). The plan writes the field's reset value,
  zero, and says in the log that it is an inference.
* **The plane's maximum dimensions are not checked.** `sink.rs` passes `Constraints::unlimited()` to
  the mode layer; the reference gives no maximum plane dimension for ADL-N and the code refuses to
  invent one. A mode that fits the plane on paper and not in the register field would be found by
  §11 phase 6.2, not by the mode choice.

One gap phase 6 now *closes* rather than carries: `PIPESTAT` is still declared read-only, so a
latched FIFO underrun cannot be cleared, but `set_mode` takes a pre-sample of it (and of
`PLANE_SURFLIVE` and `DDI_BUF_CTL`) before the first write, so the verdict can say whether the bit
was already set — `pre_existing` in §3.7 — instead of asserting attribution it does not have.

### 6.5 Nothing re-modesets on hotplug

The after-boot watch (step 8 of §3.1) detects a change, logs it, and **re-probes the sink**. It does
not program a pipe, a PLL, the DDB or a watermark, and it does not move the console
(`docs/design/intel-hotplug.md` §4). So a monitor plugged in after boot will be noticed, named in
the log —

```text
intel-hpd: watching 0000:00:02.0 for hotplug: one SDEISR read every 250 ms, and the poll itself writes no register
intel-hpd: hotplug at <millis> ms: DDI A: disconnected -> connected (SDEISR 0x… (live connect bit set), SOUTH_CHICKEN1 0x… (board inversion …))
```

— and its EDID read again, but the panel will stay dark until the next boot. The reason is the
console: the sequence that programs the pipe is the sequence that drives the only output device the
machine has, so a re-modeset has to answer who owns the screen while the pipe is being taken down
and reprogrammed, and that is the workstream's next feature rather than a detail of this one. A
person with a monitor and a stick should therefore attach the monitor before power-on (§1.1) and
treat a post-boot plug as a diagnostic of the watch, not as a display acceptance step.

### 6.6 What no run of this document can decide

Whether a failure belongs to this kernel or to the board, on a machine whose firmware has been
driving the display since power-on. The kernel never reads the firmware's timings, plane registers
or DDI state before overwriting them, so there is no "before" to compare against. The comparison
that would answer it is the hardware-facts bundle of `docs/design/n305-bringup.md` §1 — i915's own
view of the same device, captured from a throwaway Linux environment on the same stick — which is
why that bundle is worth taking before the first TheKernel display run rather than after it.
