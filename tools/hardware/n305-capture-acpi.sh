#!/bin/sh
# Pure read-only decoders for the Alpine capture. POSIX sh/od/awk only.
# Never cast a 64-bit ECAM address through awk printf's integer conversion.
set -eu
mode=${1:-}
shift || exit 2
case "$mode" in
mcfg|fadt)
    [ "$#" -eq 1 ] && [ -r "$1" ] || exit 2
    od -An -tu1 -v "$1" | awk -v mode="$mode" '
    function hex_le(offset, count, trim,   k, text) {
        text = ""
        for (k = count - 1; k >= 0; k--) text = text sprintf("%02x", b[offset + k + 1])
        if (trim) { sub(/^0+/, "", text); if (text == "") text = "0" }
        return "0x" text
    }
    function u16(offset) { return b[offset+1] + b[offset+2]*256 }
    { for (i=1; i<=NF; i++) b[++n]=$i }
    END {
        if (n < 36) { print "truncated ACPI header"; exit 2 }
        table_len=b[5]+b[6]*256+b[7]*65536+b[8]*16777216
        if (table_len != n) { print "declared ACPI length differs from bytes"; exit 2 }
        if (mode == "mcfg") {
            if (b[1]!=77 || b[2]!=67 || b[3]!=70 || b[4]!=71 || n<44 || (n-44)%16) {
                print "invalid MCFG signature/geometry"; exit 2
            }
            printf "MCFG length=%d revision=%d\n", table_len, b[9]
            for (offset=44; offset<n; offset+=16) {
                if (b[offset+11] > b[offset+12]) { print "reversed MCFG bus range"; exit 2 }
                printf "entry %d: ecam_base=%s segment=%d bus=%02x-%02x\n", (offset-44)/16,
                    hex_le(offset,8,1), u16(offset+8), b[offset+11], b[offset+12]
            }
        } else {
            if (b[1]!=70 || b[2]!=65 || b[3]!=67 || b[4]!=80 || n<116) {
                print "invalid/truncated FADT"; exit 2
            }
            # FADT PWR_BUTTON bit 4 is one for a control-method button.
            method=int(b[113]/16)%2
            printf "FADT flags=%s PWR_BUTTON=%d button_kind=%s SCI=%d\n",
                hex_le(112,4,0), method, method ? "control-method" : "fixed-pm1", u16(46)
            printf "PM1a_EVT_BLK=%s PM1a_CNT_BLK=%s PM1b_CNT_BLK=%s\n",
                hex_le(56,4,0), hex_le(64,4,0), hex_le(68,4,0)
        }
    }'
    ;;
audit)
    [ "$#" -eq 1 ] && [ -d "$1" ] || exit 2
    for table in "$1"/*; do
        [ -f "$table" ] || continue
        signature=$(od -An -tc -N4 "$table" | tr -d ' \n')
        bytes=$(wc -c < "$table" | tr -d ' ')
        if [ "$signature" = FACS ]; then
            printf 'n/a   %-10s %8s bytes  (FACS has no checksum field)\n' "$(basename "$table")" "$bytes"
            continue
        fi
        sum=$(od -An -tu1 -v "$table" | awk '{for(i=1;i<=NF;i++) s=(s+$i)%256} END {print s+0}')
        if [ "$sum" -eq 0 ]; then
            printf 'ok    %-10s %8s bytes\n' "$(basename "$table")" "$bytes"
        else
            printf 'BAD   %-10s %8s bytes  (sum mod 256 = %s)\n' "$(basename "$table")" "$bytes" "$sum"
        fi
    done
    ;;
power-devices)
    [ "$#" -eq 1 ] && [ -d "$1" ] || exit 3
    count=0
    for device in "$1"/*; do
        [ -f "$device/hid" ] || continue
        hid=$(cat "$device/hid")
        [ "$hid" = PNP0C0C ] || continue
        count=$((count+1))
        printf '%s hid=%s\n' "$(basename "$device")" "$hid"
        for attribute in path status modalias; do
            [ -f "$device/$attribute" ] || continue
            printf '%s: ' "$attribute"
            cat "$device/$attribute"
        done
    done
    printf 'PNP0C0C enumerated_count=%s\n' "$count"
    ;;
codec-paths)
    [ "$#" -eq 1 ] && [ -d "$1" ] || exit 3
    count=0
    for codec in "$1"/card*/codec#*; do
        [ -f "$codec" ] || continue
        printf '%s\n' "$codec"
        count=$((count+1))
    done
    [ "$count" -gt 0 ] || exit 3
    ;;
kernel-ecam)
    [ "$#" -ge 1 ] && [ "$#" -le 2 ] && [ -r "$1" ] || exit 2
    log=$1
    # A kernel may not retain its early PCI log. The independently labelled
    # iomem fallback reports a kernel-owned ECAM window, not an invented log.
    if grep -iE 'ECAM|MCFG|MMCONFIG|PCI:.*bus|pci_bus' "$log"; then
        printf 'source: retained kernel log\n'
    else
        result=$?
        [ "$result" -eq 1 ] || exit 2
        if [ "$#" -eq 2 ] && [ -r "$2" ] && grep -iE 'ECAM|MMCONFIG' "$2"; then
            printf 'source: kernel /proc/iomem (early ECAM log not retained)\n'
        else
            printf 'UNAVAILABLE: no ECAM/MCFG log or readable ECAM iomem window\n'
            exit 3
        fi
    fi
    ;;
*) printf 'usage: %s {mcfg|fadt|audit|power-devices|codec-paths|kernel-ecam} INPUT [IOMEM]\n' "$0" >&2; exit 2 ;;
esac
