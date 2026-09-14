# N305 bring-up: capture, acceptance, and the evidence each step returns

The DUT is an Acer 蜂鸟 mini (SQM2270): Intel i3-N305 (Alder Lake-N), 16 GB
RAM, 512 GB NVMe, no operating system installed, **no serial port**. The screen
is the only output channel the machine has, and the only way to read that
screen from the development host is a driver-free USB HDMI capture dongle.

Everything below is ordered: do the steps in the order they appear. Each one
ends in a file that someone else can check.

Two procedures are described and it matters which is which:

* **§1 captures ground truth about the hardware** with a throwaway Linux
  environment. It ends by powering the machine off, and that power-off is its
  success signal;
* **§2 and §3 accept TheKernel on the machine.** §2's boot deliberately does
  *not* end, so that the evidence stays on the screen; §3 replaces screen
  reading with a network transcript.

The Intel display work has its own acceptance procedure,
[`n305-display-acceptance.md`](n305-display-acceptance.md): it takes §1 and §2
of this document as given — the stick, the firmware preconditions, the shell
profile, the photographs — and adds what each phase of the display bring-up
must show on the panel, which log lines confirm it, and how to falsify it.

## 1. Capture the machine's hardware facts

The kernel now reads the one fact firmware can tell it: the **PCI ECAM base**
comes from the ACPI MCFG table at boot, and it falls back to the configured
`pci-ecam-base` only when firmware publishes nothing usable (`n305.toml` ships
the documented Alder Lake default as that fallback). The capture is therefore
the *check* on that discovery rather than its replacement: it carries the raw
MCFG bytes, an independent decode of them, and the kernel's own ECAM line, so
discovery can be confirmed against the machine instead of trusted. Everything
firmware does **not** tell the kernel is in the bundle too — above all the
**integrated graphics**: BARs, the OpRegion with its VBT, the driver's debug
state, the framebuffer's format and the EDID of the attached sink.

### 1.1 Build the capture image (development host, no root needed)

```sh
scripts/ci/n305-capture-image.sh --out ~/n305-capture.img
```

Downloads Alpine's standard live ISO (352 MiB, checksum verified against the
copy the CDN publishes beside it) and appends one 256 MiB FAT partition
carrying the capture payload, an offline tool set, and a dump directory. The
vendor ISO is not modified: the build proves with `cmp` that every byte from
the ISO9660 filesystem to the end of the vendor image is unchanged, and that
the new partition is recorded in both the hybrid MBR and the GPT — in the first
MBR slot, so that the initramfs finds the payload however slowly the machine
enumerates devices. Build dependencies are listed at the top of the script
(`p7zip` and `mtools` are the unusual ones).

Write it to a stick of at least 1 GiB. **Check which device is the stick**: the
target machine has a 512 GB NVMe, and writing to that by mistake destroys it.

```sh
lsblk -o NAME,SIZE,TRAN,MODEL      # the stick is the one with TRAN=usb
sudo dd if=~/n305-capture.img of=/dev/sdX bs=4M conv=fsync status=progress
sync
```

`bs=4M` is only throughput. `conv=fsync` matters: without it `dd` returns
while the stick still holds the data in its own cache. `oflag=direct` is not
needed and fails on some sticks.

### 1.2 Run it at the machine

Plug the stick in, power the machine on, and do nothing else. Two firmware
preconditions, because this boot chain is unsigned:

* **Secure Boot must be off.** If the firmware hides the option, clearing the
  platform keys is the usual route. A Secure Boot refusal looks exactly like
  nothing happening, which is the worst failure mode on a screen-only machine;
* the firmware must be in **UEFI** mode, not legacy/CSM.

On the glass, in order: the firmware's own logo, then `n305-capture:` progress
lines, then `n305-capture: CAPTURE COMPLETE`, then the machine **powers itself
off**. The power-off is the success signal.

If instead the screen ends at `localhost login:` with no `n305-capture:` lines,
the live environment did not find its payload partition; power-cycle and try
once more, and if it repeats, rebuild the stick. The payload cannot print a
diagnostic in that case, because the code that would print it is what was not
found.

That failure is a race, and it is worth knowing why it can happen at all.
Alpine's initramfs scans block devices as their kernel events arrive, stops as
soon as it holds both a package repository and the apkovl, and shortens its
patience from five seconds to 250 ms the moment it finds the ISO's repository.
On a machine that enumerates its disk quickly, the payload partition's event
can arrive after it has given up. The image narrows the race from this side —
the payload partition sits in the first partition slot, so its event is
generated first — but the race cannot be removed from outside. Removing it
means naming the apkovl on the kernel command line
(`apkovl=LABEL=HWDUMP:alpine.apkovl.tar.gz`), which needs a boot configuration
this image does not own: the ISO's is embedded in its bootloader, and a
replacement GRUB image built on the development host is several megabytes,
against 590 KiB free in the ISO's 1.4 MiB EFI partition.

Measured on this host, before and after that partition-slot change: booting the
image with QEMU's own kernel loader (`-kernel`/`-initrd`, which hands the
kernel straight to the machine) finds the payload every time — the initramfs
log shows `Loading user settings from /media/sda1/alpine.apkovl.tar.gz`, the
payload runs and the guest powers itself off. The UEFI path (OVMF, then the
ISO's GRUB, the same kernel and initramfs) missed it **three times out of
three**, run one at a time with nothing else on the host, leaving the guest at
a login prompt. The difference is boot speed: the kernel's console output slows
early boot enough for the initramfs scan to see every partition, and `quiet` on
the ISO's command line removes exactly that delay.

**So this image is not yet reliable on the target.** The N305 will boot the way
the UEFI path does, `quiet` included, which is the fast case that loses. The
fix is not more retries: it is a boot configuration that names the apkovl on
the kernel command line (`apkovl=LABEL=HWDUMP:alpine.apkovl.tar.gz`), which
takes the initramfs scan out of the picture entirely. That needs a GRUB image
of our own, because the ISO's is embedded in its `BOOTX64.EFI` and the ISO's
1.4 MiB EFI partition has 590 KiB free against several megabytes for a
host-built GRUB. The shape of it: append a 16 MiB ESP of our own carrying
`EFI/BOOT/BOOTX64.EFI` built by `grub2-mkstandalone` with that command line,
and point the MBR's `0xEF` entry and the GPT's ESP entry at it. Until that
exists, the capture is a retry-until-it-takes operation, and the retry is
cheap: power-cycle, and read the screen.

Attach the display or the capture dongle *before* powering on: the EDID in the
bundle is the EDID of whatever sink is connected, which is the same thing the
kernel will see.

### 1.3 Read the bundle back

The bundle is a plain directory on a FAT partition:

```sh
mkdir -p /mnt/stick && sudo mount /dev/sdX1 /mnt/stick   # the partition with the marker file
cp -r /mnt/stick/dump/n305-<timestamp> /somewhere/
scripts/ci/hw_facts_bundle.py /somewhere/n305-<timestamp>
```

`hw_facts_bundle.py` prints the machine's identity, CPU, the ECAM base with its
corroboration from dmesg and `/proc/iomem`, the graphics device and driver,
each display's decoded EDID (which the DUT cannot do: `edid-decode` is not
packaged for this Alpine release), and every probe that failed. With
`--expect-ecam <addr>`, passing the `pci-ecam-base` the built profile actually
uses, it fails loudly when MCFG disagrees — which is the check that turns "the
kernel assumes an address" into "the kernel assumes a *wrong* address, and here
is the table that says so".

### 1.4 What the capture cannot contain

* **No serial output** — the machine has no serial port; the capture runs
  headless and its screen messages are not recorded.
* **No decoded EDID on the machine** — decoded on the development host instead.
* **No `intel_reg` MMIO dump** — `intel-gpu-tools` is not packaged for Alpine
  3.24; `graphics/debugfs-*/` carries what i915 reports about itself.
* **No i915 initialisation trace** — tracing must be armed before the driver
  loads, which the vendor boot configuration cannot do.
* Everything else that could not be read is in the bundle's own
  `capture-status.txt` as `FAIL` or `UNAVAILABLE` with the reason.

`scripts/ci/n305-capture-image-selfcheck.sh` boots the built image in QEMU and
asserts the whole pipeline works. It proves nothing about the N305: QEMU is not
that machine.

**Its guest half does not currently pass, and the `--out` guard is not the
reason.** Booting the image under OVMF reaches Alpine's `localhost login:`
prompt, the run ends at its boot timeout, and the image contains no
`dump/n305-*` directory: the capture wrote no bundle and the guest never powered
itself off. An image built from this script before the guard existed behaves
identically -- the two differ only in tar member mtimes and the FAT volume
serial, and their screen frames are byte-identical -- so the failure predates
that change. What is not established is why: the vendor ISO boots with `quiet`,
the capture writes to the screen, and the serial console carries neither the
initramfs apkovl scan nor the service output, so "the apkovl was not applied"
and "the payload failed before its first write" cannot be told apart from this
run. Until that is settled, the image's end-to-end behaviour is asserted by this
selfcheck alone, and it fails.

## 2. Acceptance (a): the kernel boots and its log is visible on screen

### 2.1 Build the shell profile, and why not the system profile

```sh
python3 tools/thekernel.py build --profile shell
```

The build emits `kernel-x86_64.esp`: a complete GPT disk image containing
`EFI/BOOT/BOOTX64.EFI`, `TheKernel.elf` and `rootfs-x86.img`. Nothing else has
to be put on the stick.

**Use the shell profile.** The acceptance question is "is the log on the glass,
and can a person still be looking at it a minute later", and only the shell
profile answers yes:

* `--profile shell` boots `/etc/thekernel/shell-init.sh`, which ends in
  `/bin/sh -i`. On this machine there is no keyboard driver, so the shell's
  read from the console never returns (`kernel/src/pseudofs/dev/tty/mod.rs`
  blocks on the READABLE event in `read_with_nonblocking`), and the
  `poweroff -f` at the end of the script is never reached. The log and the
  `THEKERNEL_SHELL_READY` prompt stay on the glass indefinitely;
* `--profile system` runs the KTAP suite and prints its transcript, but it
  cannot be relied on to leave it there: the transcript ends with the
  completion marker and `system-init.c` then returns, and the clean power-off
  the lab observes is the *runner* sending `SYSTEM_TEST_SHUTDOWN_COMMANDS`
  (`/bin/busybox poweroff -f`) after it sees the marker. On a machine with no
  input channel, nothing sends that.

So §2 establishes boot, log and reachability — not test results. The tests are
§3's job, and that is the honest split.

### 2.2 Write the ESP to the stick

Same check and same command as §1.1 — the ESP *is* the disk image:

```sh
lsblk -o NAME,SIZE,TRAN,MODEL
sudo dd if=<build output>/kernel-x86_64.esp of=/dev/sdX bs=4M conv=fsync status=progress
sync
```

### 2.3 Record the screen, then boot

```sh
scripts/ci/n305-screen-capture.sh capture --out ~/n305-run-1     # start this first
```

Then power the machine on. Nothing is typed on the machine, ever. Leave the
capture running until the log has stopped and the prompt has been on screen for
a while — a minute is plenty, because the point of the shell profile is that
there is no rush.

Measured on this host, with the shell profile built by
`python3 tools/thekernel.py build --profile shell`, booted under OVMF with
`-device bochs-display` and a GRUB configuration that sets `gfxterm` and
`gfxpayload=keep`: screen captures at t = 10, 30, 60, 120, 180, 240 and 280
seconds are **byte-identical** (sha256 `eefb2ed3348c9ee6…`) and show
`THEKERNEL_SHELL_READY`. The guest never powered itself off; QEMU exited only
when the test's own timeout killed it. That is the property this profile is
chosen for: the evidence is still on the glass minutes later.

Two things the same run makes plain:

* the glass showed the readiness marker and **no kernel log**. That observation
  is overtaken: it was taken before `fix/klog-loss` (merged as `0e640c92`) made
  the console a reader of the kernel's log ring instead of a consumer of a copy
  queue that dropped records once it filled. The `firmware-fbcon` stage of
  `verify --tier daily` now reads a kernel log line (`Enter user space: ip=0x`)
  off a screendump of the firmware framebuffer, on a profile with no virtio-gpu
  and no serial port. So "the log reaches a serial-less screen" is gated
  evidence — but the *shell* profile above has not been re-run since the fix,
  and the frame that would show its log mirror has not been taken;
* the bootloader has to pass a Multiboot2 framebuffer tag. With GRUB's terminal
  left on the serial port, GRUB prints `WARNING: no console will be available
  to OS` and the glass stays black — the failure in the table below.

### 2.4 What the good outcome looks like on the glass

In order:

1. the firmware's own logo;
2. `GRUB` text, because `config/x86_64/grub.cfg` sends GRUB's terminal to the
   console as well as the serial port;
3. kernel log lines;
4. a prompt preceded by `THEKERNEL_SHELL_READY`, and then **no further
   change**: that stillness is the success signal, not a hang.

Telling a good boot from a bad one, from the screen alone:

| what the screen shows | what it means | what to do |
| --- | --- | --- |
| nothing at all, not even the firmware logo | no boot from USB: wrong boot entry, CSM mode, or Secure Boot refusing the stick | firmware setup: UEFI mode, Secure Boot off, boot the USB entry |
| firmware logo, then nothing | the firmware did not execute `BOOTX64.EFI`; Secure Boot is the first suspect | disable Secure Boot, or clear the platform keys |
| GRUB text, then no kernel log at all — and a kernel log read elsewhere says `MB2 framebuffer: absent` | the bootloader passed no Multiboot2 framebuffer tag, so the kernel has no console to draw on. This is *not* the same failure as a kernel that produced no output: here the kernel has nowhere to write, and the fix belongs in the bootloader's video configuration | fix the bootloader's video setup (a mode must be set before handoff); keep the frames as evidence |
| GRUB text, then a dark screen | GRUB ran and passed a framebuffer, but nothing was drawn. The kernel paints this surface from the moment `axhal::init_early` returns and mirrors its log ring into it, so this is either a handoff that never reached the kernel or a stop inside the window that closes a few milliseconds into `rust_main` | keep the frames and report; `docs/design/early-screen.md` §5 names that window |
| log appears, then stops before `THEKERNEL_SHELL_READY` | the kernel stopped — and the screen now separates the two cases: `*** PANIC ***` on a red bar is a panic, its absence is a hang | keep the frames: they are the evidence. `docs/design/early-screen.md` §4 describes the panic screen |
| log, then `THEKERNEL_SHELL_READY` and no further change | acceptance (a) passed | turn the frames into gate evidence, below |

That last line is why the shell profile matters: the frame that proves the boot
is still on the glass when the person gets round to looking at it.

### 2.5 Turn the frames into gate evidence

Find the first frame in which `THEKERNEL_SHELL_READY` is legible and record who
read it:

```sh
scripts/ci/n305-screen-capture.sh verdict --dir ~/n305-run-1 \
    --completion-frame 42 --marker THEKERNEL_SHELL_READY \
    --checker "your name, who read the screen"
```

Then run the gate, which checks the frames itself rather than trusting the
verdict:

```sh
THEKERNEL_DUT_POWER_CYCLE_CMD='...' \
THEKERNEL_DUT_BOOT_ONCE_CMD='...' \
THEKERNEL_DUT_SCREEN_CAPTURE_CMD='...' \
scripts/ci/dut_gate.py --artifact-dir <build output> --state-dir <state> \
    --dut n305 --observation screen --runs 3
```

Each hook receives `THEKERNEL_DUT_*` paths; the screen hook must record
`frame-0000.ppm`, `frame-0001.ppm`, ... into `$THEKERNEL_DUT_SCREEN_DIR` and
write the verdict to `$THEKERNEL_DUT_SCREEN_VERDICT`, which
`n305-screen-capture.sh` does. With no lab controller, the power-cycle and
one-shot-boot hooks are the person: a hook that prints "power on the machine
now", waits for a keypress, and returns is a legitimate implementation.

The gate rejects a frame set whose completion frame is blank (a black screen
cannot show a prompt), whose frames are not real binary P6 PPM files of the
declared resolution, whose indices are not contiguous, or which stops looking
before the banner. It accepts a *dark* last frame, because a machine that
powers itself off after the banner is a success.

**What acceptance (a) proves:** the machine booted from the stick, produced
output on its only display, reached the shell prompt, and stayed there.
**What it does not prove:** that the system-test suite ran or passed. No
screenshot can establish that, and the gate says so on stdout rather than
implying otherwise.

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

This runs the `system` profile: the hook fetches the guest's complete KTAP
transcript over the network into `$THEKERNEL_DUT_TRANSCRIPT` and writes `clean`
to `$THEKERNEL_DUT_SHUTDOWN_STATUS` only after independently observing a normal
shutdown. The gate then enforces exactly what the Panther Lake serial gate
enforces: `KTAP version 1`, one plan, every record present, no `not ok`, no
`SKIP`, and the completion marker. The transport is the guest-side
workstream's business; all the gate requires is that the transcript is the
guest's, not a summary of it.

The gate takes one channel per invocation, so requiring both costs two
invocations (six cold boots). Use the screen channel while the network channel
does not exist, and switch as soon as it does. Its lowest layer now does: the
target's i225/i226 has a driver in this tree (`--net-igc`,
`docs/design/nic-igc.md`), so what is missing is the guest-side transport above
it rather than the driver underneath.

## 4. What is still an assumption

* The MCFG value is confirmed *for this machine as configured*. A firmware
  update, or a different unit of the same model, invalidates it — which is why
  MCFG discovery belongs in the kernel rather than in a constant that this
  capture blesses.
* The screen verdict is a named, recorded human judgement, not a measurement.
  The frames that back it travel with it, so someone else can re-check it.
* Nothing here validates the kernel's behaviour on the NVMe beyond what the
  capture image reports about the device itself.
