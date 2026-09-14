#!/usr/bin/env bash
#
# build.sh -- reproducible, network-free build of the Phase 2a "hello" kernel.
#
#   usage: tools/nested/hello/build.sh <output-dir>
#
# Produces <output-dir>/hello.elf: a freestanding x86_64 boot artifact that
#   * is loadable by `qemu-system-x86_64 -kernel` as a Multiboot v1 ELF32,
#   * switches the CPU from 32-bit protected mode into x86_64 long mode,
#   * prints an INNER_-prefixed banner on COM1, and
#   * terminates QEMU through isa-debug-exit with host status 33.
#
# The build needs no network, no libc, no libgcc and no cross toolchain: only
# the host's gcc/binutils/as/ld/objcopy.  No *library* is ever linked -- the
# only input objects are the three compiled from the sources next to this
# script.
#
# Layout overview
# ---------------
#   hello.c, entry64.S  ->  ELF64 payload linked at KERNEL64_BASE
#                       ->  flattened to payload64.bin by objcopy
#                       ->  embedded by .incbin in boot.S
#   boot.S              ->  ELF32 Multiboot v1 image, linked at LOAD_BASE
#
# The intermediate format dance is forced by QEMU: hw/i386/multiboot.c
# rejects an EM_X86_64 -kernel image outright ("Cannot load x86-64 image,
# give a 32bit one") and reloads the file with load_elf(..., I386_ELF_MACHINE).
# So the outer container has to be ELF32 even though the kernel is x86_64.

set -euo pipefail

# --------------------------------------------------------------------- setup

readonly SRC_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <output-dir>" >&2
    exit 2
fi

OUT_DIR="$1"
mkdir -p -- "$OUT_DIR"
OUT_DIR="$(cd -- "$OUT_DIR" && pwd)"

# Host tools.  Overridable so a caller can pin an exact toolchain, but the
# defaults are the plain host binutils/gcc.
CC="${CC:-gcc}"
AS="${AS:-as}"
LD="${LD:-ld}"

# llvm-objcopy and GNU objcopy are interchangeable here; prefer llvm-objcopy
# (the documented host tool) and fall back to the binutils one.
if [ -z "${OBJCOPY:-}" ]; then
    if command -v llvm-objcopy >/dev/null 2>&1; then
        OBJCOPY=llvm-objcopy
    else
        OBJCOPY=objcopy
    fi
fi

# Read-only inspection tools used for the layout assertions below.
READELF="${READELF:-readelf}"
NM="${NM:-nm}"

# ------------------------------------------------------- pinned layout values
#
# Single source of truth: these are passed to the assembler and to both linker
# scripts with --defsym, so boot.S's jump target, linker.ld's section address
# and kernel64.ld's link address cannot drift apart.

# 1 MiB: the classic Multiboot load address, well clear of real-mode memory.
readonly LOAD_BASE=0x100000
# 1 MiB + 4 KiB: the 64-bit payload's slot, right after the 32-bit stub.
readonly KERNEL64_BASE=0x101000

# --------------------------------------------------------------- pinned flags
#
# hello.c is the only C translation unit.  Every flag that matters for a
# freestanding, position-dependent, interrupt-free image is spelled out:
#
#   -ffreestanding        no hosted environment; -fno-builtin implied
#   -nostdlib             do not link the C library or startup files
#   -nostdinc             do not even search the C library headers
#   -fno-pie -fno-pic     position-dependent code for a fixed load address
#   -static -no-pie       link-time counterparts, kept here for the record
#   -mno-red-zone         no 128-byte red zone below RSP (no interrupts yet,
#                         but the ABI concession must not be assumed)
#   -mgeneral-regs-only   no SSE/MMX/x87: the kernel never enables them, so a
#                         stray %xmm use would be a #UD
#   -fno-zero-initialized-in-bss
#                         keep zero-initialised data in .data; the payload is
#                         flattened with objcopy, which drops SHT_NOBITS
#   -fno-asynchronous-unwind-tables -fno-unwind-tables
#                         no .eh_frame in a kernel that cannot unwind
#   -fno-stack-protector  no __stack_chk_fail to link against
#   -fcf-protection=none  no CET endbr64 padding and no .note.gnu.property,
#                         which the linker would otherwise place ahead of the
#                         code and shift _start64 off KERNEL64_BASE
#   -fno-tree-loop-distribute-patterns
#                         do not synthesise memset/memcpy calls from loops
#   -mcmodel=small        the image lives below 2 GiB
readonly CFLAGS_64=(
    -m64
    -std=gnu11
    -O2
    -g0
    -ffreestanding
    -nostdlib
    -nostdinc
    -static
    -fno-pie
    -fno-pic
    -no-pie
    -fno-stack-protector
    -fcf-protection=none
    -fno-asynchronous-unwind-tables
    -fno-unwind-tables
    -fno-zero-initialized-in-bss
    -fno-builtin
    -fno-tree-loop-distribute-patterns
    -mno-red-zone
    -mgeneral-regs-only
    -mcmodel=small
    -Wall
    -Wextra
    -Werror
)

# Stage-2 link: a plain non-relocatable ELF64 with no interpreter.
#
# --no-warn-rwx-segments: the payload is flattened to bytes and copied to RAM
# by QEMU; there is no MMU permission model to honour, and splitting it into
# R/X and R/W segments would only add padding to a flat image.
readonly LDFLAGS_64=(
    -m elf_x86_64
    --build-id=none
    -static
    --no-dynamic-linker
    --no-warn-rwx-segments
    -z noexecstack
)

# Stage-1 link: ELF32/EM_386, the only thing QEMU's Multiboot loader accepts.
readonly LDFLAGS_32=(
    -m elf_i386
    --build-id=none
    -static
    --no-warn-rwx-segments
    -z noexecstack
)

# ------------------------------------------------------------------- helpers

run() {
    printf '+ ' >&2
    printf '%q ' "$@" >&2
    printf '\n' >&2
    "$@"
}

fail() {
    printf 'build.sh: FAILED: %s\n' "$*" >&2
    exit 1
}

# Read a symbol's value out of an ELF file and normalise it to 0x%x.
symbol_value() {
    local file="$1" name="$2" raw
    raw="$("$NM" --defined-only "$file" | awk -v s="$name" '$3 == s { print $1; exit }')"
    [ -n "$raw" ] || fail "symbol '$name' not found in $file"
    normalise_hex "$raw"
}

# Accept "101000" or "0x101000" and print "0x101000".
normalise_hex() {
    local value="${1#0x}"
    [ -n "$value" ] || fail "empty hex value"
    printf '0x%x' "$((16#$value))"
}

expect_value() {
    local what="$1" got="$2" want="$3"
    [ "$got" = "$want" ] || fail "$what: got $got, want $want"
}

# --------------------------------------------------------------------- build

cd -- "$OUT_DIR"

# Never leave a stale bootable image behind if a later step fails: the whole
# point of this artifact is that hello.elf existing means "it built".
rm -f -- "$OUT_DIR/hello.elf"

# 1. The x86_64 kernel half: one C translation unit.
run "$CC" "${CFLAGS_64[@]}" -c "$SRC_DIR/hello.c" -o "$OUT_DIR/hello64.o"

# 2. The x86_64 entry trampoline (no C preprocessor; assembled directly).
run "$AS" --64 -o "$OUT_DIR/entry64.o" "$SRC_DIR/entry64.S"

# 3. Link the payload at KERNEL64_BASE.  KERNEL64_BASE is injected with
#    --defsym and consumed by kernel64.ld.
run "$LD" "${LDFLAGS_64[@]}" \
    -T "$SRC_DIR/kernel64.ld" \
    --defsym KERNEL64_BASE="$KERNEL64_BASE" \
    -o "$OUT_DIR/payload64.elf" \
    "$OUT_DIR/entry64.o" "$OUT_DIR/hello64.o"

# 4. Flatten it.  Only PROGBITS bytes survive, hence the NOBITS assertion.
run "$OBJCOPY" -O binary "$OUT_DIR/payload64.elf" "$OUT_DIR/payload64.bin"

# 5. The 32-bit Multiboot stub, with the payload pulled in via .incbin.
#    -I lets the assembler find payload64.bin regardless of the caller's cwd.
run "$AS" --32 -I "$OUT_DIR" \
    --defsym KERNEL64_BASE="$KERNEL64_BASE" \
    -o "$OUT_DIR/boot.o" "$SRC_DIR/boot.S"

# 6. Link the bootable image.  --defsym feeds LOAD_BASE / KERNEL64_BASE into
#    linker.ld; no library or startup object is passed.
run "$LD" "${LDFLAGS_32[@]}" \
    -T "$SRC_DIR/linker.ld" \
    --defsym LOAD_BASE="$LOAD_BASE" \
    --defsym KERNEL64_BASE="$KERNEL64_BASE" \
    -o "$OUT_DIR/hello.elf" \
    "$OUT_DIR/boot.o"

# ------------------------------------------------------- layout verification
#
# These assertions are the build's own contract checks; a silent layout drift
# would otherwise only show up as a QEMU triple fault.

# The payload must be a fully initialised image: objcopy drops NOBITS bytes.
if "$READELF" -S "$OUT_DIR/payload64.elf" | grep -q 'NOBITS'; then
    fail "payload64.elf contains an SHT_NOBITS section; objcopy -O binary would drop it"
fi

# The payload's entry point must be its first byte, because boot.S jumps to
# the bare address KERNEL64_BASE.
expect_value "_start64 address" \
    "$(symbol_value "$OUT_DIR/payload64.elf" _start64)" "$KERNEL64_BASE"

# The embedded payload must land exactly at KERNEL64_BASE.
expect_value "__kernel64_start" \
    "$(symbol_value "$OUT_DIR/hello.elf" __kernel64_start)" "$KERNEL64_BASE"

# ...and the 32-bit stub must not have run into it.
boot_end="$(symbol_value "$OUT_DIR/hello.elf" __boot_end)"
[ "$((boot_end))" -le "$((KERNEL64_BASE))" ] \
    || fail "__boot_end ($boot_end) overlaps KERNEL64_BASE ($KERNEL64_BASE)"

# The image must start at LOAD_BASE...
expect_value "__boot_start" \
    "$(symbol_value "$OUT_DIR/hello.elf" __boot_start)" "$LOAD_BASE"

# ...with _start inside the stub slot.  It is deliberately not LOAD_BASE: the
# 12-byte Multiboot header has to come first so that QEMU finds it in the
# file's leading bytes.
start_addr="$(symbol_value "$OUT_DIR/hello.elf" _start)"
[ "$((start_addr))" -ge "$((LOAD_BASE))" ] \
    && [ "$((start_addr))" -lt "$((KERNEL64_BASE))" ] \
    || fail "_start ($start_addr) is outside the 32-bit stub slot"

# The ELF entry point QEMU will jump to must be exactly that _start.
entry_hex="$("$READELF" -h "$OUT_DIR/hello.elf" \
    | awk '$1 == "Entry" && $2 == "point" { print $NF }')"
[ -n "$entry_hex" ] || fail "could not read the ELF entry point"
expect_value "ELF entry point" "$(normalise_hex "$entry_hex")" "$start_addr"

# The image is contiguous: the payload ends where .bootdata begins and the
# lowest load address is LOAD_BASE.
expect_value "__image_end > __kernel64_end" \
    "$([ "$(( $(symbol_value "$OUT_DIR/hello.elf" __image_end) ))" -gt \
         "$(( $(symbol_value "$OUT_DIR/hello.elf" __kernel64_end) ))" ] \
       && echo yes || echo no)" "yes"

# ELF class/machine of the outer container: QEMU requires ELF32 / EM_386.
"$READELF" -h "$OUT_DIR/hello.elf" | grep -q 'ELF32' \
    || fail "hello.elf is not ELF32"
"$READELF" -h "$OUT_DIR/hello.elf" | grep -q 'Intel 80386' \
    || fail "hello.elf is not EM_386"

# The Multiboot header must sit inside the first 8192 bytes of the file,
# 4-byte aligned.  QEMU scans exactly that window for 0x1BADB002.
if ! head -c 8192 "$OUT_DIR/hello.elf" \
        | od -An -tx4 -v \
        | tr -s ' \n' '\n' \
        | grep -qx '1badb002'; then
    fail "Multiboot magic 0x1BADB002 not found in the first 8192 bytes of hello.elf"
fi

# ------------------------------------------------------------------- summary

printf '\nbuilt: %s\n' "$OUT_DIR/hello.elf"
file "$OUT_DIR/hello.elf" || true
printf 'size:  %s bytes\n' "$(stat -c %s "$OUT_DIR/hello.elf")"
printf 'payload64.bin: %s bytes\n' "$(stat -c %s "$OUT_DIR/payload64.bin")"
printf '\nlayout:\n'
"$NM" --defined-only "$OUT_DIR/hello.elf" \
    | grep -E ' (_start|__boot_end|__kernel64_start|__kernel64_end|__image_end)$' \
    | sort || true
