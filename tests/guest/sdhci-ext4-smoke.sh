#!/bin/sh
# Run only with a disposable GPT/ext4 image attached to sdhci-pci as /dev/mmcblk0.
set -eu
mkdir -p /mnt/mmc
mount -t ext4 /dev/mmcblk0p1 /mnt/mmc
printf 'SDHCI guest disk test\n' > /mnt/mmc/probe
sync
[ "$(cat /mnt/mmc/probe)" = 'SDHCI guest disk test' ]
umount /mnt/mmc
echo SDHCI_EXT4_RW_OK
