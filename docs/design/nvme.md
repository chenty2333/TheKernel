# NVMe PCI bring-up (2026-10-04)

## Facts and safety

N305 capture identifies UMIS `1cc4:6a13` at 02:00.0 with a BitLocker Windows
installation. **未在硬件上验证**. Never use its disk for any destructive test.
Rust is original; consulted Linux 7.2.3 `drivers/nvme/host/{pci,core}.c`
(reset, queue setup, Identify and PRPs) and NVMe Base 1.4b §§3–7. Reference
PDF/text is outside the repository in `~/.cache/thekernel-targets/refs/nvme/`.

Only exact last-value `nvme.allow_write=1` admits writes. Missing, zero or any
other value leaves `/dev/nvme0n1` read-only. The driver checks admission before
copying data or publishing commands; the device registry also reports RO and
rejects raw writes with EROFS. BLKROSET cannot override the driver's lock.
A write-enabled boot emits a prominent warning. Read-only Flush is a no-op:
there are no writes to make durable. No format, discard, sanitize, firmware
update, write-zeroes, or security commands are implemented.

## Implementation and limitations

`tk-axdriver-nvme` owns controller registers, descriptors, DMA, reset, admin
queue, Identify controller/active namespace list/namespace, Number of Queues,
Create CQ/SQ, NVM read/write and Flush. The x86 adapter binds each allocation
to its PCI requester through the existing VT-d DMA contract. Queue depth 32,
up to two negotiated I/O queues, at most 31 owned requests per queue, and a
per-request bounce buffer capped by controller MDTS and 128 KiB. PRP1/PRP2 or
one PRP-list page describe that buffer. Whole
512–4096-byte LBAs only; metadata/protection formats are rejected. Only the
first active namespace of one controller is exposed, with a stable n1 name.
Identify uses the full six-bit FLBAS format index and validates it against
NLBAF before reading the selected format. MDTS scaling checks multiplication
overflow as well as shift count; large limits retain the bounded bounce-buffer
limit rather than wrapping to zero. A namespace must fit at least one LBA
within the effective transfer limit.

CC reset waits for CSTS.RDY to clear, CAP.TO bounds enable/reset, and I/O
completion waits are bounded to five seconds. Timeout/protocol identity failure poisons further
commands. Owned DMA survives until reset proves RDY=0; an unresponsive device
quarantines allocations rather than letting a late DMA corrupt reused memory.
No retry that could duplicate an uncertain write is performed.

MSI-X uses an owned dynamic IRQ registration when a validated capability/table
and requester-bound vector are available. The callback only publishes the
device's generation and bounded wake notification; it never touches CQ or DMA
owners. Serviceable task context drains CQ only after an IRQ generation changes;
bootstrap ownership with interrupts masked explicitly inspects CQ. A lost
interrupt reaches the deadline and fails closed rather than silently polling
an IRQ-owned queue. Route-less devices and `nvme.poll=1` explicitly use bounded
polling. Teardown masks and reads back the MSI-X entry before synchronizing and
freeing its IRQ action and interrupt-remapping owner.

## Owned batches

The queue state follows TGOSKits `nvme-driver`'s stage/commit/drain model, adapted
to the existing `BlockDriverOps` and `SharedBlockDevice` task-side runtime.
All fallible prefix preparation occurs before publication. Each accepted CID
owns its data and PRP allocations, and a monotonic handle prevents stale callers
from consuming a reused CID. Writes are copied before acceptance; read segment
leases remain with their handle until the task copies a successful result.
One doorbell commits each hardware queue's batch. Failed or malformed completions
never copy read data; unproven reset quarantines DMA backing.

A flush is admitted only after earlier hardware work has finished and prevents
later data admission until its terminal completion. Exact waits retain unrelated
completion owners; bounded drains return concrete handle/status records to the
existing shared runtime. Unsupported physical zero-copy paths remain unsupported:
this change does not invent a second raw-DMA ownership model.

The static block wrapper already had BootModule/Existing/USB variants. NVMe
adds a peer variant and independent feature-gated probe; it deliberately does
not replace `BLOCK_DEV_FEATURES`' selected Existing type. Thus virtio-blk, NVMe
and USB coexist without forcing every block device into a global dynamic
model. The bootloader module remains root `/dev/vda`; NVMe is registered as
`/dev/nvme0n1`, with immutable hardware RO propagated to the registry.

GPT views are now discovered by the reusable `tk-axdriver-block::partition`
parser and published through the existing device registry/devfs. It checks
protective MBR, primary GPT header/entry-array CRCs, bounded sizes, geometry
and non-overlap before exposing `nvme0n1pN`. A corrupt table leaves the whole
disk node available but publishes no questionable partitions. This is GPT,
not extended MBR support; recovery from a corrupt primary using backup GPT is
not implemented. Views share the one parent controller queue, enforce relative
bounds and retain immutable hardware RO even if BLKROSET clears software RO.
The root boot module is unchanged; a partition can be mounted as ext4 data.
No Windows filesystem mount or BitLocker decryption is attempted.

## Validation

Host fake emulates Identify, queue creation, DMA payload copies, CQ phases,
70 read/write pairs through queue wrap, PRP offsets, range rejection, RO
admission and timeout poison. QEMU `--nvme-disk IMAGE` appends a standard NVMe
controller to the existing topology. `--kernel-cmdline` builds a run-local ESP
with explicit parameters; the canonical ESP/config is not edited.

`tests/guest/nvme-smoke.c` is destructive only in raw/fs modes. Compile static
and install into a **copy** of rootfs. Run with a disposable 64-MiB image:

```
python3 tools/thekernel.py run --profile shell --accel kvm --rootfs ROOTFS_COPY \
  --nvme-disk DISPOSABLE_IMAGE --commands COMMAND_FILE
```

ro mode checks BLKROGET, an actual EROFS write rejection and a 128-KiB read.
raw mode requires the explicit write parameter, writes seeded pseudorandom
128-KiB data, fsyncs, reads and compares all bytes. Independently compare the
host image at 60 MiB after clean QEMU shutdown. fs mode mounts ext4, writes
and fsyncs a 128-KiB file, compares it and unmounts. Inspect the host file using
debugfs after shutdown. These check QEMU persistence, not power-loss durability
of physical NAND or the N305 controller.

Measured in this change: QEMU KVM RO and 128-KiB raw tests pass; host backing
image matched every generated byte after fsync and shutdown. ext4 mount,
128-KiB file write/fsync/read/unmount pass; debugfs extracted file also matched
every byte independently. Two I/O queues are exercised by round-robin raw
write/read, and host fake repeats 70 pairs across both CQ phase wraps.

Write-enable warning is also sent directly to the foreground and diagnostic
console transports, so the boot log filter cannot hide it. QEMU with the
ordinary default log filter printed the warning before `NVME_RAW_OK`.

Final combined KVM boot with NVMe, virtio-blk and USB storage enumerated all
three disks alongside the boot module and passed the NVMe read-only helper.
Run-local boot-argument ESP/GRUB outputs are checked against all input paths
before writing: matching paths and hardlinks to an input NVMe image are
rejected without invoking the ESP builder. A normal explicit RO-parameter
boot still passed the content/admission helper.


Continuation GPT validation: a disposable 128-MiB QEMU GPT image contains a
119-MiB ext4 partition at 1 MiB. `/dev/nvme0n1p1` reports the exact geometry;
partition-relative superblock bytes match the parent's offset. Default writes
fail with EROFS, and clearing software RO cannot bypass the driver lock. With
explicit write enable, a 128-KiB file write/fsync/read/unmount passes and host
debugfs extraction matches every byte independently. MSI-X completion wake
was observed; forced polling passes the same RO/offset test. Parser/view host
tests reject corrupt CRCs, overlaps, out-of-range I/O and RO writes. All target
N305 behavior remains hardware-unverified.

## Midday physical baseline (user-run, older integration)

`HWTEST-2026-10-04.md` reports UMIS 1cc4:6a13, two polling I/O queues,
read_only=true, blockdev RO=1 and capacity 512110190592 bytes. Protective MBR and
GPT header reads passed with no data writes. This verifies the older base
read-only path, **not** this continuation's MSI-X delivery or GPT partition
nodes; those still need the next physical session. BitLocker Windows protection
and the absent-by-default write-enable parameter remain unchanged.
