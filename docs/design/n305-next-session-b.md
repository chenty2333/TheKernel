# N305 next session — Codex B, 2026-10-04

Only main workspace `/home/ava/Desktop/TheKernel`. This is a **future user-run
procedure**, not a record of physical validation. Do not touch A's worktree,
services or network session. Midday physical results in
`/home/ava/.cache/thekernel-targets/tier1-2026-10-04/HWTEST-2026-10-04.md`
confirm the older polling NVMe read-only path and ALC269 codec/route enumeration.
New MSI-X/GPT nodes, native ALSA playback, native modesetting and rollback remain
**未在硬件上验证**.
Do not write the internal BitLocker Windows NVMe. Do not enable GT writes.
Native modesetting is an explicitly enabled, guarded hardware test; never enable it in the baseline.

## 1. Prepare, without host network changes

```
cd /home/ava/Desktop/TheKernel
export THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets
python3 tools/thekernel.py build --platform n305 --profile shell
```

Expected: successful ELF/ESP paths printed under the main state directory.
Inspect the prepared GRUB command line: **no `nvme.allow_write=1`**, no native
modeset request for the baseline. Use the same known-good USB/UEFI or existing
PXE handoff as the last hardware session. Firmware fallback should remain
800x600. Do not modify BIOS settings or write the NVMe to create boot media.

For PXE preparation, following `n305-tonight.md`, choose a *fresh B output*
and the actual dedicated interface only after coordinating with the user/A:

```
scripts/n305-netboot.sh prepare --mode kernel \
  --out /home/ava/.cache/thekernel-targets/tier1-2026-10-04/b-next-kernel \
  --interface enp0s31f6 --kernel PRINTED_ELF --rootfs PRINTED_ROOTFS --loglevel warn
```

Substitute the build's actual ELF/rootfs paths; do not pass the literal
placeholders. Expected: prepared directory/start instructions. Inspect its
GRUB file and confirm the absence of write enable. Preparing does not start
host services. The generated privileged start script, if needed, is for the
**user** to run after confirming no other PXE session is active; B has not run
sudo, changed networking or started it. Avoid `loglevel=7` on the N305 until
A resolves the previously observed verbose-console stall.

## 2. First physical boot — NVMe reads only

In the guest:

```
ls -l /dev/nvme0n1 /dev/nvme0n1p*
blockdev --getro /dev/nvme0n1
dmesg | grep -i nvme
dd if=/dev/nvme0n1 bs=512 count=2 | hexdump -C
```

Expected: block node; getro prints **1**; controller identifies UMIS
`1cc4:6a13`, up to two queues, read_only=true; raw MBR/GPT bytes answer. Do not
mount the whole Windows disk, run mkfs/fsck, invoke write tests, change BLKROSET,
or append `nvme.allow_write=1`. Valid primary GPT entries should appear as
`/dev/nvme0n1p1`, etc., preserving GPT entry numbers; each must also report RO=1.
Do not mount BitLocker partitions. Malformed/unsupported GPT keeps the whole
namespace available but reports discovery failure instead of guessing offsets.
The MSI-X log should name the owned vector and later report a completion wake.
If interrupts do not arrive, bounded polling remains armed. A separate read-only
boot with `nvme.poll=1` explicitly exercises the fallback. QEMU destructive tests must stay on
explicit disposable host image files, never physical devices.

If the node is absent or a read fails, stop storage acceptance. Record the
visible error/controller status with the existing capture setup. Do not
retry a write, change firmware, or interpret this as a successful SSD test.
A controller timeout should disable further driver requests rather than hang
the kernel; the existing boot-module root/screen must remain usable.

## 3. Analog HDA output

The physical driver enumerated codec `10ec0269` (ALC269 family), with headphone
pin `0x21` -> mixer `0x0c` -> DAC `0x02`. Keep generic widget enumeration; do not
replace it with a hard-coded model route. Enumeration was verified, playback
was not.
In TheKernel:

```
dmesg | grep -i hda
ls -l /dev/dsp /dev/snd/controlC0 /dev/snd/pcmC0D0p
```

Expected: actual codec ID, generic analog route, S16LE stereo 48000 Hz and an
OSS and native ALSA nodes. Prepare unmodified static `aplay`, the standard
hardware-plugin configuration `tests/guest/alsa-hw.conf`, and a stereo S16LE
48000-Hz known waveform in a copied boot rootfs (never on internal NVMe).
Run at a safe **external** volume:

```
ALSA_CONFIG_PATH=/alsa-hw.conf aplay -D hw:0,0 /input.wav
```

Expected: `Playing WAVE ... Signed 16 bit Little Endian, Rate 48000 Hz, Stereo`,
normal exit, audible analog output, and working screen/guest after drain/close.
`/hda-smoke` remains an OSS regression check; `/alsa-smoke` (compiled from
`tests/guest/alsa-smoke.c`) checks the native ABI and START/DROP/reopen lifecycle.
Do not run OSS and native clients concurrently: the second opener must be busy.
In QEMU, unmodified aplay's recorded WAV matched all 8192 frames / 32768 bytes;
this is not a claim about the physical codec. Capture analog output externally
if available and compare waveform/timing rather than counting a submission
marker or merely hearing a noise as proof.
If no route/node or drain fails, stop; the codec may require a board-specific
quirk after actual enumeration. Do not substitute HDMI audio: HDMI depends on
future native display power/link/ELD ownership.

## 4. Intel baseline and GT — read only

Boot without `intel.modeset=1` and inspect:

```
cat /sys/kernel/debug/dri/0/intel_gpu
dmesg | grep -i intel
```

Expected: `8086:46d0`, original 800x600 console, no display writes. Midday GT
reads found ACK_GT/RENDER/VDBOX0/VEBOX0 = 0 and BCS0_CTL unavailable with no
forcewake, sample-stable=false. These are gated/unknown observations, **not**
proof that the engines are absent. Do not write forcewake, GT reset, execlist,
GuC/HuC or batch registers. BCS/GEM/GT work remains only the design/readonly probe.

## 5. First recovery exercise — explicit modeset with an injected failure

Use a fresh copied boot image/config, the known working capture/monitor and
external kernel-log capture. Keep `nvme.allow_write` absent. Append:

```
intel.modeset=1 intel.modeset.fail_write=1
```

Expected: admission either refuses **before all writes** (unsupported firmware
pipe/port/calibration/clock/ownership), or the first plane write is deliberately
refused and real restoration runs. The screen must return/stay at the original
800x600 firmware console with readable, updating text. Log/debugfs must report
`ROLLBACK_MMIO_VERIFIED`, original layout/live surface, advancing scanlines and
restored PTEs. This label alone does not verify the monitor picture: confirm the
actual console visibly, type commands, and compare the external capture.

If preflight refuses, it is not a modeset/rollback pass. Read the candidate
before-image and refusal reason; do not bypass admission or substitute guessed
PLL/PHY/power values. The supported path is one firmware primary on pipe A,
combo HDMI/DVI A/B, an already reusable active PHY and a valid firmware-selected
combo PLL/CDCLK. DP/Type-C, other active pipes, overlay/cursor and PHY
recalibration are deliberately not attempted.

If `ROLLBACK_FAILED`, missing picture, black screen, corrupted pitch or stopped
console appears, stop physical acceptance. Use the external log to distinguish
the original failure from recovery failure; do not claim success from register
equality. Reboot using the **baseline** configuration with no modeset option.
DMA is retained, no native console is published and no second modeset is tried.
Do not issue GPU reset or enable GT/SSD writes as a workaround.

## 6. Native 1080p60 and later rollback prefixes

After the first recovery test succeeds visibly, boot the fresh configuration
with only:

```
intel.modeset=1
```

Expected: live EDID advertises 1920x1080@60; selected mode log says
1920x1080, 148500-kHz CEA timing when that exact descriptor is supplied; native
color bars/marker followed by the working console at 1920x1080. `fbset -i` and
the fixed linear DRM mode should reflect the selected geometry. Confirm the
capture/monitor actually receives 1080p60, rows/pitch/colors are correct and
console text updates. The saved dongle EDID is not a substitute for the currently
attached sink. A scanline/SURFLIVE log without the visible pattern is not a pass.

Record the reported forward write count. In separate boots repeat
`intel.modeset=1 intel.modeset.fail_write=N` at later counts through power,
PLL/PHY, pipe and the final plane-arm write. Each must restore the same visible
800x600 console and pass the MMIO/PTE/progression contract. N past the last write
is not an injected-failure test. Never enable automatic hotplug modesetting or
GT submission for this session.

Analog audio can be checked separately after either baseline or successful
native boot. HDMI audio still requires coordinated display-power references,
link/ELD availability and codec converter setup and is not an acceptance item.
