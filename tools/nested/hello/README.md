# `tools/nested/hello` — Phase 2a freestanding x86_64 hello kernel

A minimal, self-contained, freestanding x86_64 boot artifact for the
nested-QEMU plan (`docs/design/guest-toolchain-and-nested-qemu.md`, Phase 2a).
`qemu-system-x86_64 -kernel` loads it with **no firmware and no bootloader
disk**; it switches the CPU from 32-bit protected mode into x86_64 long mode,
prints an `INNER_`-prefixed banner on COM1, and terminates QEMU through
`isa-debug-exit`.

Purpose: separate *"TCG works inside the guest userspace"* from *"booting a
real Linux kernel is slow"*. Total host wall time from QEMU start to QEMU exit
is **~0.09 s** under both TCG and KVM. Success is observable entirely from
outside QEMU: a serial banner and the QEMU process exit status.

* Build output (persistent, not in the repo):
  `$HOME/.cache/thekernel-targets/guest-toolchain/nested-hello/`
* No libc, no libgcc, no dynamic linker, no network, no cross toolchain.

---

## Obtained result

| | |
|---|---|
| Loader format | **Multiboot v1, ELF32 / EM_386** |
| QEMU exit status | **33** (`(0x10 << 1) \| 1`), TCG and KVM |
| Serial banner | `INNER_HELLO_OK` and 14 further `INNER_` lines, all CR-free |
| Wall time, TCG | 0.082 – 0.096 s (5 runs) |
| Wall time, KVM | 0.087 – 0.126 s (5 runs) |
| `hello.elf` size | 46 376 bytes |
| `hello.elf` sha256 | `98cffb98e1f4a8567399a6405188e15cbd9d923145906ef064d6cef4891a6651` |

---

## Build

One script, one argument (the output directory). It needs no network access
and links no library — only the host's `gcc`, `as`, `ld` and `llvm-objcopy`.

```sh
tools/nested/hello/build.sh "$HOME/.cache/thekernel-targets/guest-toolchain/nested-hello"
```

That produces `hello.elf` in the given directory. Every compile/link flag is
pinned in `build.sh`; the build prints each command it runs, then verifies its
own layout and fails loudly (leaving no `hello.elf` behind) if anything drifts.

```
$ file hello.elf
hello.elf: ELF 32-bit LSB executable, Intel i386, version 1 (SYSV), statically linked, not stripped
$ stat -c %s hello.elf
46376
```

Two clean builds into different directories are byte-identical.

### Layout produced

```
0x100000  .boot       Multiboot header (12 B) + 32-bit stub + GDT
0x101000  .kernel64   the embedded x86_64 payload (2401 B flat binary)
0x101970  .bootdata   PML4/PDPT/PD, 16 KiB boot stack, saved magic/mbi
0x10a000  __image_end
```

The image is one contiguous run of bytes with no `SHT_NOBITS` section,
because QEMU copies it as a single flat blob (see below).

---

## Run recipe

The exact command (this is the command whose output is quoted below):

```sh
timeout 30 qemu-system-x86_64 -machine pc -accel tcg -m 128M -smp 1 -display none \
  -serial stdio -device isa-debug-exit -kernel \
  "$HOME/.cache/thekernel-targets/guest-toolchain/nested-hello/hello.elf"
```

`-device isa-debug-exit` is **required**: without it the final port write goes
nowhere and the kernel spins in `cli; hlt` until the timeout fires.
`-serial stdio` may be replaced with `-serial file:...` when stdout is not a
terminal.

### Literal serial output (stdout, verbatim)

```
INNER_HELLO_OK
INNER_HELLO_BUILD=phase2a-multiboot1-elf32-plus-elf64-payload
INNER_BOOT_MAGIC=0x2badb002 (multiboot1, expected 0x2badb002)
INNER_BOOT_MODE=32-bit-protected-mode-at-entry,now-64-bit-long-mode
INNER_MBI_PTR=0x0000000000009500
INNER_MBI_FLAGS=0x000000000000024f
INNER_MEM_LOWER_KB=639 INNER_MEM_UPPER_KB=129920
INNER_BOOTLOADER_NAME=qemu
INNER_CPU_MODE=x86_64-long-mode
INNER_CPU_CS=0x0018 INNER_CPU_SS=0x0020
INNER_CPU_CR0=0x0000000080000011
INNER_CPU_CR4=0x0000000000000020
INNER_CPU_EFER=0x0000000000000500
INNER_CPU_MODE_EVIDENCE=efer.lme=1 efer.lma=1 cr0.pg=1 cr4.pae=1 cs.l=1
INNER_HELLO_EXIT port=0x0501 value=0x10 expected_qemu_status=33
```

Line endings are **bare LF (`0x0a`), no CR**. That is deliberate: this banner
exists to be parsed from outside QEMU, so a captured transcript has clean
`^INNER_...$` lines. On an interactive tty served by QEMU's raw-mode stdio
chardev the missing carriage return can make the text staircase.

`stderr` is empty in both accelerator runs.

### Literal exit statuses

```
$ ... ; echo $?
33
```

* TCG (`-accel tcg`): `33` — 5/5 runs.
* KVM (`-accel kvm`, `/dev/kvm` present): `33` — 5/5 runs, and the serial
  output is **byte-for-byte identical** to the TCG run (`cmp` clean).

`isa-debug-exit` maps a written value `v` to host status `(v << 1) | 1`; the
kernel writes `0x10`, so the expected and observed status is `33`. The
kernel's own failure path writes `0x11` and would produce `35`, which is how
the bad-magic case was distinguished during development.

---

## Loader format: Multiboot v1 ELF32, and why

QEMU's x86 `-kernel` handling has exactly two acceptance paths on the `pc`
machine. Both were probed empirically on this host's
`qemu-system-x86_64` 10.2.2 with the same command shape:

| Candidate | Result |
|---|---|
| **Multiboot v1 header in an ELF32** (`hello.elf`) | **boots, exit 33** |
| Multiboot v1 header in an ELF64 | exit 1 — `qemu-system-x86_64: Cannot load x86-64 image, give a 32bit one.` |
| Plain ELF32, no Multiboot header | exit 1 — `qemu-system-x86_64: Error loading uncompressed kernel without PVH ELF Note` |
| Plain ELF64, no Multiboot header | exit 1 — `qemu-system-x86_64: Error loading uncompressed kernel without PVH ELF Note` |
| ELF64 carrying a Xen PVH note (`n_type = 18`) | boots, exit 37 *(separate probe, see below)* |

So a "plain ELF" is **not** loadable: `load_elfboot()` requires the image to
carry a `XEN_ELFNOTE_PHYS32_ENTRY` note, otherwise QEMU aborts. And a
Multiboot image **must** be ELF32/`EM_386`; QEMU rejects `EM_X86_64` outright
before it even tries to load segments.

That is the whole reason this artifact has an unusual two-stage shape:

```
hello.c + entry64.S  --(gcc -m64, ld -m elf_x86_64)-->  ELF64 at 0x101000
                     --(objcopy -O binary)---------->  payload64.bin
                     --(.incbin in boot.S)---------->  embedded in ...
boot.S               --(as --32, ld -m elf_i386)----->  hello.elf  (ELF32)
```

The outer container must be ELF32 to satisfy QEMU's loader, while the kernel
proper is x86_64 code that the 32-bit stub reaches after building page tables
and setting `CR4.PAE` / `EFER.LME` / `CR0.PG`.

Multiboot v1 was chosen over the PVH route because it is the documented,
bootloader-neutral contract and because it lets the kernel *prove its own boot
contract*: QEMU passes `0x2BADB002` in `EAX` and the info-structure pointer in
`EBX`, and `kmain()` verifies the magic and reports it on the serial port
rather than assuming it. The banner's `INNER_CPU_MODE_EVIDENCE` line is
likewise read back from hardware — `EFER.LME`/`LMA`, `CR0.PG`, `CR4.PAE`, and
the `L` bit of the GDT descriptor `CS` is currently executing under — not
printed from a constant.

### QEMU requires a contiguous image

`load_multiboot()` does:

```c
mh_load_addr   = elf_low;
mb_kernel_size = elf_high - elf_low;
rom_copy(mbs.mb_buf, mh_load_addr, mb_kernel_size);
```

and the `multiboot_dma.bin` option ROM then makes one flat copy to `elf_low`.
Any gap between sections is copied as whatever happened to be in the buffer,
so `.bootdata` (page tables, boot stack) is `PROGBITS` with explicit zeros
rather than an `SHT_NOBITS` tail. The build asserts there is no `NOBITS`
section anywhere in the payload.

---

## Source map

| File | Role |
|---|---|
| `multiboot.h` | Multiboot v1 constants, the info-structure layout, and the kernel's own fixed-width typedefs (no libc headers anywhere). |
| `boot.S` | Multiboot header; 32-bit protected-mode stub; installs its own GDT; builds a 1 GiB identity map with 2 MiB pages; `CR4.PAE` → `CR3` → `EFER.LME` → `CR0.PG` → far jump into the 64-bit code segment. Assembled by bare `as --32` with no C preprocessor. |
| `entry64.S` | 64-bit entry trampoline; first byte of the payload, so `boot.S` can jump to the bare address `KERNEL64_BASE` with no ELF entry lookup. |
| `hello.c` | COM1 (16550) driver, hand-written port-I/O helpers, Multiboot magic verification, banner, CPU-mode read-back, `isa-debug-exit`. Also defines its own `memset`/`memcpy`/`memmove`. |
| `linker.ld` | Stage-1 ELF32 layout and the assertion that the 32-bit stub fits its 4 KiB slot. |
| `kernel64.ld` | Stage-2 ELF64 layout; pins `_start64` to the first byte of the payload. |
| `build.sh` | The whole build plus its layout assertions. Single source of truth for `LOAD_BASE` / `KERNEL64_BASE`. |

`isa-debug-exit` is written with a **1-byte** store (`outb`). QEMU creates the
device with `iobase=0x501, iosize=1`, so its memory region is exactly one byte
wide; a 32-bit `outl` would straddle the region and the unassigned bytes after
it. Both end up calling `exit((value << 1) | 1)`, but the byte store is the
unambiguous one.

### Two bugs found and fixed during bring-up

Recorded because they are the non-obvious parts of this boot path:

1. **Long-mode page-table entries are 8 bytes wide.** The page-directory loop
   first filled the PD with a 4-byte stride, producing garbage PDEs and a
   triple fault (page fault → double fault → triple fault, no serial output).
   `boot.S` now writes an explicit low dword + zero high dword for every entry
   and strides the PD by 8.
2. **`EDI`/`EBX` do not survive the 32-bit stub.** The PD loop uses `EDI` as
   its destination pointer and `EBX` as the high half of the physical base, so
   the Multiboot magic and info pointer are reloaded from `.bootdata` while
   still in 32-bit code, immediately before the far jump. Doing that load
   after the far jump would need a 64-bit absolute memory operand, which
   `as --32` cannot encode into an ELF32 container.

---

## What was NOT verified

* **Only `-machine pc` was tested.** `q35` was not run, nor was `microvm`.
* **Only `-m 128M -smp 1` was tested.** Other RAM sizes and SMP counts were
  not exercised. The kernel maps only the first 1 GiB and never touches an
  AP, so more vCPUs would leave the APs parked in the BIOS/option-ROM state.
* **`-accel kvm` was tested only on this host** (`/dev/kvm` present, AMD/Intel
  unknown to the artifact). Behaviour was identical to TCG, but that is one
  host, not a portability claim.
* **No nested QEMU was run.** This artifact was booted by the *host's* QEMU
  only. Whether the same ELF boots inside TheKernel's guest userspace — the
  actual Phase 2a claim — was not tested here.
* **The Xen PVH route was only probed as far as "QEMU accepts it and starts
  it"** (a separate throwaway probe that printed one line and exited 37, kept
  under `loader-probe/`). It was not developed into a second real artifact,
  and the PVH `hvm_start_info` contract was not implemented or verified.
* **No bootloader other than QEMU was tried.** GRUB, Limine, etc. were not
  used, so the Multiboot header is only proven against QEMU's implementation.
* **The `-d int,cpu_reset` diagnosis was done with a local QEMU source tree
  (v11.1.1) that is no longer on disk.** The behavioural claims in this README
  and all quoted output come from the host's QEMU 10.2.2 binary, not from that
  source tree; the source was used only to explain the observed behaviour.
* **No `-kernel` variant with `-initrd`, `-append` or a bzImage was tested.**
  Modules and the Linux boot protocol are out of scope for Phase 2a.
