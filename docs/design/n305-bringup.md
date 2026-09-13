# N305 bring-up: capture, acceptance, and the evidence each step returns

The DUT is an Acer 蜂鸟 mini (SQM2270): Intel i3-N305 (Alder Lake-N), 16 GB
RAM, 512 GB NVMe, no operating system installed, **no serial port**. The screen
is the only output channel the machine has, and for now the only way to read
that screen from the development host is a driver-free USB HDMI capture
dongle.

Everything below is written so that the person at the machine does as little as
possible, and so that every step ends in a file that someone else can check.

## 1. Capture ground truth from the machine

The kernel's assumptions about this machine are guesses; this replaces them
with bytes read from the machine. Two things in particular:

* the **PCI ECAM base**, which TheKernel currently takes from
  `axconfig::devices::PCI_ECAM_BASE` (a QEMU value, consumed at
  `crates/ax/thekernel-axdriver/src/bus/pci.rs:225`) while another workstream
  adds ACPI MCFG discovery. The capture carries the raw MCFG table, a decode of
  its bytes done on the machine with `od` and `awk` only, the kernel's own
  `PCI: ECAM ...` line from dmesg, and the PCI windows from `/proc/iomem`, so
  the value can be corroborated three independent ways;
* everything about the **integrated graphics**: BARs, the driver's debugfs
  state, the OpRegion (which carries the VBT), the framebuffer format and
  contents, and the EDID of whatever sink is attached.

### 1.1 Build the stick (development host, no root needed)

```sh
scripts/ci/n305-capture-image.sh --out ~/n305-capture.img
```

This downloads Alpine's standard live ISO (352 MiB, checksum verified against
the copy the CDN publishes beside it) and appends one 256 MiB FAT partition
carrying the capture payload, an offline tool set, and a dump directory. The
vendor ISO is not modified: the build proves with `cmp` that every byte from
the start of the ISO's filesystem to the end of the vendor image is unchanged,
and that the new partition is recorded in both the hybrid MBR and the GPT the
ISO already carries. Alpine is the live environment because it is the smallest
mainstream one (~350 MiB), boots to a root shell with no installer and no
prompts, and has a documented unattended hook: its initramfs scans every
partition for `alpine.apkovl.tar.gz`, which is what starts the capture with no
kernel command line edit and no console interaction.

Write it to a stick of at least 1 GiB, then check that you are pointing at the
stick and not at the machine's internal NVMe:

```sh
lsblk -o NAME,SIZE,TRAN,MODEL          # confirm the device: TRAN=usb, the size you expect
sudo dd if=~/n305-capture.img of=/dev/sdX bs=4M conv=fsync status=progress
sync
```

`bs=4M` is only throughput. `conv=fsync` matters: it makes `dd` wait until the
data is on the device, which USB sticks otherwise cache and lose when the
machine is switched off. `oflag=direct` is not needed and fails on some
sticks.

### 1.2 Run it (at the machine)

Plug the stick in, power the machine on, and do nothing else. The screen shows
`n305-capture:` progress lines, then `CAPTURE COMPLETE`, and the machine
**powers itself off**. That power-off is the success signal: a machine that
stays on has hit an error and is displaying it.

Two firmware preconditions, because the boot chain is unsigned GRUB →
Multiboot2:

* **Secure Boot must be off.** `EFI/BOOT/BOOTX64.EFI` in Alpine's live image is
  unsigned, and on a screen-only machine a Secure Boot refusal looks exactly
  like nothing happening. If the firmware hides the option, clearing the
  platform keys is the usual route;
* the firmware must be in **UEFI** mode, not legacy/CSM.

Attach the display or the capture dongle before powering on: the EDID in the
bundle is the EDID of whatever sink is connected, which is the same thing the
kernel will see.

### 1.3 Read it back

The bundle is a directory on the FAT partition, so any host can read it:

```sh
mkdir -p /mnt/stick && sudo mount /dev/sdX3 /mnt/stick
cp -r /mnt/stick/dump/n305-<timestamp> /somewhere/
scripts/ci/hw_facts_bundle.py /somewhere/n305-<timestamp>
```

`hw_facts_bundle.py` prints the machine's identity, the CPU, the ECAM base with
its corroboration from dmesg and `/proc/iomem`, the graphics device and driver,
each display's decoded EDID (including the preferred timing, which
`edid-decode` cannot do here because it is not packaged for this Alpine
release), and the list of probes that failed or were unavailable. With
`--expect-ecam <addr>` it fails loudly if MCFG disagrees: pass the
`pci-ecam-base` the profile being built actually uses (the value lives in the
build's axconfig, e.g. `config/x86_64/q35-uefi.toml` for the QEMU profiles),
which is the check that turns "the kernel assumes an address" into "the kernel
assumes a *wrong* address, and here is the table that says so".

### 1.4 What the bundle does not contain, and why

* **No serial output** — the machine has no serial port; the capture runs
  headless and the screen messages are not recorded.
* **No decoded EDID on the machine** — `edid-decode` is not packaged for Alpine
  3.24. The raw EDID bytes are captured and are the authority; decoding happens
  in `hw_facts_bundle.py` on the development host.
* **No `intel_reg` MMIO dump** — `intel-gpu-tools` is not packaged for Alpine
  3.24. `graphics/debugfs-*/` carries what i915 reports about itself instead,
  which includes its display and engine state.
* **No i915 initialisation trace** — tracing has to be armed before the driver
  loads, and the vendor boot configuration cannot do that. `logs/messages.txt`
  usually retains more of the early boot than `dmesg` does.
* Anything else that could not be read is listed in the bundle's own
  `capture-status.txt` as `FAIL` or `UNAVAILABLE` with the reason.

`scripts/ci/n305-capture-image-selfcheck.sh` boots the built image in QEMU and
asserts that the whole pipeline works — vendor boot path, apkovl discovery,
offline tool install, every probe, the bundle landing on FAT, the guest
powering itself off. It says nothing about the N305: QEMU is not that machine.

## 2. Acceptance (a): the kernel boots and its log is visible on screen

### 2.1 What the person at the machine does

1. FLASH the ESP image the build produced to a USB stick — same `lsblk` check,
   same `dd ... bs=4M conv=fsync` as above. Nothing else needs to be written to
   the stick: `tools/thekernel.py build` (with the `n305` platform profile once
   it lands) emits `kernel-x86_64.esp`, and that artifact is already a complete
   GPT disk image containing `EFI/BOOT/BOOTX64.EFI`, `TheKernel.elf` and
   `rootfs-x86.img`.
2. Start `scripts/ci/n305-screen-capture.sh capture --out ~/n305-run-1` on the
   development host with the HDMI dongle connected to the machine's output.
3. Power the machine on and wait. Nothing is typed on the machine.
4. When the kernel log and `# THEKERNEL_SYSTEM_TEST_COMPLETE` are on the
   screen, power the machine off, then stop the capture.
5. Look at the frames, find the first frame in which the completion banner is
   legible, and turn that judgement into evidence:

```sh
scripts/ci/n305-screen-capture.sh verdict --dir ~/n305-run-1 \
    --completion-frame 42 --checker "your name, who read the screen"
```

### 2.2 Tell "firmware problem" from "kernel problem" first

Before accepting anything, establish that **GRUB was reached**, because that is
what separates a USB or firmware fault from a kernel fault, and with
`set timeout=0` the menu does not linger. The screen alone distinguishes them:

| what the screen shows | what it means |
| --- | --- |
| nothing at all, not even the firmware's own logo | no boot from USB: wrong boot entry, wrong mode (CSM), or Secure Boot refusing the stick |
| firmware logo, then nothing | the firmware did not execute `BOOTX64.EFI` — Secure Boot is the first suspect |
| GRUB's own text or menu, then a dark screen | GRUB ran; the kernel did not produce screen output — a kernel-side problem |
| GRUB text, then a kernel log | the chain works; this is what acceptance (a) is about |

GRUB currently writes to the serial port only (`terminal_output serial` in
`config/x86_64/grub.cfg`), so with no serial port present a GRUB failure and a
kernel failure are indistinguishable on the screen. Making GRUB's console
terminal active as well is a prerequisite for this procedure and belongs to the
framebuffer-console workstream, not here.

### 2.3 The gate

Three cold boots, driven by the same gate the Panther Lake DUT uses, with the
screen as the observation channel:

```sh
THEKERNEL_DUT_POWER_CYCLE_CMD='...' \
THEKERNEL_DUT_BOOT_ONCE_CMD='...' \
THEKERNEL_DUT_SCREEN_CAPTURE_CMD='...' \
scripts/ci/dut_gate.py --artifact-dir <build output> --state-dir <state> \
    --dut n305 --observation screen --runs 3
```

The capture hook must record frames as `frame-0000.ppm`, `frame-0001.ppm`, ...
into `$THEKERNEL_DUT_SCREEN_DIR` and write the verdict file at
`$THEKERNEL_DUT_SCREEN_VERDICT`; `n305-screen-capture.sh` does both. With no
lab controller, the power-cycle and one-shot-boot hooks are the person: a hook
that prompts, waits for the machine to be switched on, and returns is a
legitimate implementation of both.

Evidence that comes back, per run, in the state directory: the frame set, the
verdict, and the gate's own verdict line. The gate checks the frames itself —
real binary P6 PPM, the declared resolution, contiguous indices, a completion
frame that is not blank, and a later frame proving the capture kept looking
after the banner appeared.

**What this proves:** the machine booted, put its log on the screen, reached the
completion banner, and stayed up afterwards.
**What it does not prove:** the KTAP contract — that every test ran and passed.
No amount of screenshotting establishes that, and the gate says so on stdout
rather than implying otherwise. Acceptance (b) is how that gap closes.

## 3. Acceptance (b): the automated network channel

Once the guest is reachable on the network, the same gate runs with
`--observation network` and the full contract is back:

```sh
THEKERNEL_DUT_POWER_CYCLE_CMD='...' \
THEKERNEL_DUT_BOOT_ONCE_CMD='...' \
THEKERNEL_DUT_NETWORK_CAPTURE_CMD='...' \
scripts/ci/dut_gate.py --artifact-dir <build output> --state-dir <state> \
    --dut n305 --observation network --runs 3
```

The hook fetches the guest's complete KTAP transcript over the network into
`$THEKERNEL_DUT_TRANSCRIPT` and writes `clean` to
`$THEKERNEL_DUT_SHUTDOWN_STATUS` only after it has independently observed a
normal shutdown. The gate then enforces exactly what the Panther Lake serial
gate enforces: `KTAP version 1`, one plan, every record present, no `not ok`,
no `SKIP`, and the completion marker. The network transport itself is the
guest-side workstream; the gate only requires that the transcript it is handed
is the guest's, not a summary of it.

The gate takes one observation channel per invocation, so requiring both costs
two invocations — six cold boots. Use the screen channel while the network
channel does not exist yet, and switch to the network channel as soon as it
does.

## 4. What is still an assumption afterwards

* The MCFG value is confirmed *for this machine as configured*: a firmware
  update, or the machine being a different unit of the same model, invalidates
  it, which is why MCFG discovery belongs in the kernel rather than in a
  constant that this capture blesses.
* The screen channel's verdict is attested by whoever read the screen. It is a
  named, recorded judgement, not a measurement; the frames that back it are in
  the bundle, so it can be re-checked by someone else.
* Nothing here validates the kernel's behaviour on the NVMe beyond what the
  capture image reports about the device itself.
