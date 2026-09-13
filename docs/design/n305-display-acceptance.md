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
