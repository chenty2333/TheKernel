# N305 next session — Codex B, 2026-10-04

Only main workspace `/home/ava/Desktop/TheKernel`. This is a **future user-run
procedure**, not a record of physical validation. Do not touch A's worktree,
services or network session. All three physical drivers are **未在硬件上验证**.
Do not write the internal BitLocker Windows NVMe. Do not enable GT writes.
Native 1080p modesetting remains incomplete and must not be attempted yet.

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
ls -l /dev/nvme0n1
blockdev --getro /dev/nvme0n1
dmesg | grep -i nvme
dd if=/dev/nvme0n1 bs=512 count=2 | hexdump -C
```

Expected: block node; getro prints **1**; controller identifies UMIS
`1cc4:6a13`, up to two queues, read_only=true; raw MBR/GPT bytes answer. Do not
mount the whole Windows disk, run mkfs/fsck, invoke write tests, change BLKROSET,
or append `nvme.allow_write=1`. Partition enumeration is not implemented yet,
so no `/dev/nvme0n1p*` node is promised. QEMU destructive tests must stay on
explicit disposable host image files, never physical devices.

If the node is absent or a read fails, stop storage acceptance. Record the
visible error/controller status with the existing capture setup. Do not
retry a write, change firmware, or interpret this as a successful SSD test.
A controller timeout should disable further driver requests rather than hang
the kernel; the existing boot-module root/screen must remain usable.

## 3. Analog HDA output

A's next Alpine capture should load the HDA driver and report the actual
codec IDs/widget graph; subsystem `10ec:12ec` alone does not name the codec.
In TheKernel:

```
dmesg | grep -i hda
ls -l /dev/dsp
```

Expected: actual codec ID, generic analog route, S16LE stereo 48000 Hz and an
OSS node. There is **no native ALSA PCM endpoint yet**, so direct `aplay -D
hw:0,0` is not an acceptance step. Install the reviewed `hda-smoke` static
helper into a copied boot rootfs ahead of time (never onto internal NVMe),
then run `/hda-smoke` with headphones/analog output at a safe external volume.
Expected: known waveform and successful drain/close. Capture the analog
signal externally if available and compare samples/timing; merely hearing a
noise or seeing a submission marker is not sample correctness proof.
If no route/node or drain fails, stop; the codec may require a board-specific
quirk after actual enumeration. Do not substitute HDMI audio: HDMI depends on
future native display power/link/ELD ownership.

## 4. Intel snapshot and GT observations — no modeset

Baseline guest:

```
cat /sys/kernel/debug/dri/0/intel_gpu
```

Expected: `8086:46d0`, unchanged 800x600 firmware console, read-only GT ACK
observations. If GT is asleep, gated engine/fuse values are **unavailable**,
not proof that BCS or media engines are absent. No forcewake is acquired.

Only after the baseline is visible, a subsequent **diagnostic** boot may add
`intel.modeset=1` to its external GRUB configuration. Current code must print
**REFUSED**, capture known register candidates and perform **no display
writes**; debugfs must say NOT a complete rollback. Screen remains the same
firmware console. This is snapshot collection, **not** a 1080p test. Do not
change the refusal gate on the machine.

If the screen changes/disappears or init never appears, stop that attempt and
return to the known-good boot configuration. A retained framebuffer pointer
is not evidence of recovery. The existing HDMI capture workflow can establish
whether scanout continues; QEMU/fake registers cannot answer that question.

## 5. Separate future acceptance, not enabled by this change

Finish full firmware PLL/PHY/DDI/transcoder/pipe/plane/power/CDCLK/GGTT rollback
first. Obtain live EDID, attempt its advertised 1920x1080@60 mode only with an
explicit parameter, and inject failure after each phase. Expected: either
known 1080p pattern plus advancing counters, or restored firmware picture plus
advancing counters and visible console updates. Compare physical capture,
not just register equality. Follow `intel-firmware-rollback.md` before unlocking.

For GT, follow `intel-bcs-minimal.md`: owned forcewake/reset, mappings/context,
no-op breadcrumb, then disposable BO copy/guard comparisons. No submission,
GT firmware loading or GPU-copy benchmark belongs to the current session.
