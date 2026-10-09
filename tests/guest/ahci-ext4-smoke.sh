#!/bin/sh
# Run only with a disposable GPT/ext4 image on AHCI as /dev/sda.
set -eu
mkdir -p /mnt/ahci
mount -t ext4 /dev/sda1 /mnt/ahci
printf 'AHCI guest disk test\n' > /mnt/ahci/probe
sync
[ "$(cat /mnt/ahci/probe)" = 'AHCI guest disk test' ]
umount /mnt/ahci
echo AHCI_EXT4_RW_OK
