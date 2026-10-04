# Booting and reading rootfs from USB

Original GPT partition reader, USB BOT root view and host image/writer tools.
UEFI GPT layout is defined in [UEFI 2.11 section 5](https://uefi.org/specs/UEFI/2.11/05_GUID_Partition_Table_Format.html).
The builder reuses `scripts/build-x86-uefi-esp.sh` for GRUB/EFI and FAT. It copies
that ESP partition into a new protective-MBR/GPT image with primary and backup
CRC-protected headers/arrays, then copies the existing ext4 rootfs into a separate
x86-64 Linux root partition (GUID 4f68bce3-e8cd-4db1-96e7-fbcaf984b709). No loop
mount, sudo, raw host device or filesystem rebuilding is involved. Output must
be a new on-disk file; existing outputs are refused rather than overwritten.

USB GRUB uses `root=usb` and **does not** pass a Multiboot rootfs module. The
existing USB mass-storage implementation reads the primary GPT and validates
header/array CRCs, 128-byte entry size, bounded entry count (at most 128), media
and usable-LBA bounds, unique root GUID and no overlapping other partitions.
The driver publishes a partition-relative USB block view; every read/write is
range-checked and translated before SCSI READ/WRITE(10). Explicit root=usb
requires this view: it never falls back to the first/internal/NVMe disk. Only
512-byte-sector BOT/LUN0 media currently supports this boot mode. Unsupported
or corrupt root media is reported rather than guessed. GPT backup recovery,
UAS, hot-unplugged rootfs and multiple-root selection are not implemented.

The existing root registry calls its first device `/dev/vda` regardless of bus;
this alias **does not imply VirtIO**. Driver `USB rootfs` and boot topology,
not the alias, identify USB transport. Internal NVMe write policy is unchanged.

## Build and QEMU

```sh
export THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/wt-dev
python3 tools/thekernel.py build --platform n305 --profile shell
python3 scripts/build-usb-boot.py \
  --kernel "$THEKERNEL_STATE_DIR/out/x86_64/n305/shell/mem1g/kernel-x86_64" \
  --rootfs "$THEKERNEL_STATE_DIR/out/rootfs/x86/rootfs-x86.img" \
  --output "$THEKERNEL_STATE_DIR/n305-usb-new.img"
python3 tools/thekernel.py run --platform n305 --profile shell --accel kvm \
  --no-build --usb-disk "$THEKERNEL_STATE_DIR/n305-usb-new.img" --usb-boot
```

`--usb-boot` is additive: ordinary `--usb-disk` still attaches a data disk.
USB boot removes the SATA ESP and VirtIO root drive, supplies a bootindex to
usb-storage, and snapshots its image. Kernel arguments come from USB GRUB,
not a separate per-run ESP; the builder accepts `--kernel-cmdline` for e.g.
`quiet` and DHCP/netconsole options, while keeping root=usb fixed. Conflicting `--kernel-cmdline` is refused. Keep the
built image paired with its kernel/rootfs; rebuild after updating the kernel.

`python3 scripts/ci/usb-boot-qemu-smoke.py` builds disposable media, boots using
only USB, writes/syncs literal content, performs a real guest reboot, and reads
identical content in the new kernel before clean power-off. Actual Q35 KVM
validation: OVMF twice loaded UEFI QEMU USB HARDDRIVE; fresh second uptime
0.64 seconds, USB-written contents survived and were reread, ext4 root was rw,
QEMU exited 0. No SATA ESP, VirtIO root or Multiboot module was attached. This is
not just a successful class-driver initialization message. Host tests cover GPT
integrity/ranges and writer refusal/dry-run behavior. **N305 USB boot and media
I/O are not hardware-validated.** Firmware-framebuffer testing should use
`--graphics-profile firmware-fb`, not the headless VirtIO display topology.

## User-operated writing only

Follow the UEFI boot/media flow in `docs/design/n305-bringup.md`. Identify the
USB stick carefully, unmount **all** its partitions, and inspect this dry run:

```sh
python3 scripts/write-usb-boot.py --image /absolute/path/n305-usb-new.img --device /dev/sdX
# Only after checking the device, the user runs:
sudo python3 scripts/write-usb-boot.py --image /absolute/path/n305-usb-new.img --device /dev/sdX --yes
```

The writer refuses `/dev/nvme*`, NVMe aliases/sysfs identities, non-block files,
partitions, sysfs removable!=1, non-USB buses, mounted disk/partitions, active
holders/swap and undersized devices. Dry run only prints the destructive command.
`--yes` also requires root privileges; it revalidates identity and exclusively
pins the whole block inode before writing/fsync. No privileged writer was run
by Codex. Some USB enclosures report removable=0 and are intentionally refused;
do not bypass that protection. All data on the approved USB stick is destroyed.
