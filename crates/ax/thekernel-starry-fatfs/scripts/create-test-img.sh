#!/bin/sh
# Recreate the upstream read fixtures without loop mounts or root privileges.
set -eu
OUT_DIR=${1:?pass a disk-backed build output directory}
mkdir -p "$OUT_DIR"
printf 'Rust is cool!\n' > "$OUT_DIR/short.txt"
: > "$OUT_DIR/long.txt"
i=0
while [ "$i" -lt 1000 ]; do
    cat "$OUT_DIR/short.txt" >> "$OUT_DIR/long.txt"
    i=$((i + 1))
done
create_test_img() {
    name=$1
    blkcount=$2
    fat_size=$3
    truncate -s "$((blkcount * 1024))" "$name"
    mkfs.vfat -a -s 1 -F "$fat_size" -n 'Test!' -i 12345678 "$name"
    mcopy -i "$name" "$OUT_DIR/long.txt" ::long.txt
    mcopy -i "$name" "$OUT_DIR/short.txt" ::short.txt
    mmd -i "$name" ::very ::very/long ::very/long/path
    mcopy -i "$name" "$OUT_DIR/short.txt" ::very/long/path/test.txt
    mmd -i "$name" ::very-long-dir-name
    mcopy -i "$name" "$OUT_DIR/short.txt" ::very-long-dir-name/very-long-file-name.txt
}
create_test_img "$OUT_DIR/fat12.img" 1000 12
create_test_img "$OUT_DIR/fat16.img" 2500 16
create_test_img "$OUT_DIR/fat32.img" 34000 32
rm "$OUT_DIR/short.txt" "$OUT_DIR/long.txt"
