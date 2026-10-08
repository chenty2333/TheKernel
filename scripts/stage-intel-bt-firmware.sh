#!/usr/bin/env bash
set -euo pipefail

if (($# != 3)); then
    printf 'usage: %s SOURCE_DIR DEST_DIR LICENSE_FILE\n' "$0" >&2
    exit 2
fi
source_dir=$(realpath -e "$1")
destination=$2
license_file=$(realpath -e "$3")

[ -f "$license_file" ] || { printf 'missing Intel firmware license: %s\n' "$license_file" >&2; exit 1; }
install -d "$destination"
installed=0
for file in "$source_dir"/ibt-*.sfi "$source_dir"/ibt-*.sfi.xz "$source_dir"/ibt-*.ddc "$source_dir"/ibt-*.ddc.xz; do
    [ -f "$file" ] || continue
    name=${file##*/}
    if [[ "$name" == *.xz ]]; then
        name=${name%.xz}
        xz -dc -- "$file" > "$destination/$name.tmp"
        mv "$destination/$name.tmp" "$destination/$name"
    else
        install -m 0644 "$file" "$destination/$name"
    fi
    installed=$((installed + 1))
done
[ "$installed" -gt 0 ] || { printf 'no Intel ibt SFI/DDC files found in %s\n' "$source_dir" >&2; exit 1; }
install -m 0644 "$license_file" "$destination/LICENSE.intel"
printf 'staged %s Intel Bluetooth firmware files with LICENSE.intel in %s\n' "$installed" "$destination" >&2
