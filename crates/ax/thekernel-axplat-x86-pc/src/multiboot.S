# Bootstrapping from 32-bit with the Multiboot specification.
# See https://www.gnu.org/software/grub/manual/multiboot/multiboot.html

.section .text.boot
.code32
.global _start
_start:
    mov     edi, eax        # arg1: magic: 0x2BADB002
    mov     esi, ebx        # arg2: multiboot info
    jmp     bsp_entry32

.balign 4
.type multiboot_header, STT_OBJECT
multiboot_header:
    .int    {mb_hdr_magic}                      # magic: 0x1BADB002
    .int    {mb_hdr_flags}                      # flags
    .int    -({mb_hdr_magic} + {mb_hdr_flags})  # checksum
    .int    multiboot_header - {offset}         # header_addr
    .int    _skernel - {offset}                 # load_addr
    .int    _edata - {offset}                   # load_end
    .int    _ebss - {offset}                    # bss_end_addr
    .int    _start - {offset}                   # entry_addr

.balign 8
.type multiboot2_header, STT_OBJECT
multiboot2_header:
    .int    {mb2_hdr_magic}                     # magic: 0xE85250D6
    .int    {mb2_hdr_arch}                      # architecture: i386
    .int    {mb2_hdr_length}                    # header length
    .int    {mb2_hdr_checksum}                  # checksum

    # Address tag: all addresses are physical link addresses.  The kernel is
    # linked in the high half, so subtract the linear-map offset exactly as
    # the Multiboot1 header above does.
    .short  2                                   # type
    .short  0                                   # flags
    .int    24                                  # size
    .int    multiboot2_header - {offset}        # header_addr
    .int    _skernel - {offset}                 # load_addr
    .int    _edata - {offset}                   # load_end_addr
    .int    _ebss - {offset}                   # bss_end_addr

    # Entry-address tag.  Its size is 12 bytes, so include the required
    # four-byte pad before the next eight-byte-aligned tag.
    .short  3                                   # type
    .short  0                                   # flags
    .int    12                                  # size
    .int    _start - {offset}                   # entry_addr
    .int    0                                   # padding

    # Framebuffer request tag (type 5).  This is what makes the bootloader hand
    # the kernel a linear framebuffer at all: a kernel that does not ask for a
    # graphical console does not get a type-8 framebuffer tag back, and on this
    # machine the screen is the only place the kernel can say anything.
    #
    # Width, height and depth are zero with the optional flag clear, which asks
    # GRUB for a mode of its own choosing rather than a specific one.  Naming a
    # mode here would be a guess about a monitor nobody has seen; GRUB can ask
    # the firmware for the modes this display actually supports, and the kernel
    # only ever reads back what it was given.  The kernel also cannot draw
    # anything at a depth it did not anticipate, but it does not have to: it
    # accepts 32, 24 and 16 bits per pixel and refuses the rest with a log line.
    #
    # Size 20 makes the next tag land at 4 mod 8, which is already where the
    # four-byte pad above left it, so no further alignment is needed.
    .short  5                                   # type
    .short  0                                   # flags (0 = required, 1 = optional)
    .int    20                                  # size
    .int    0                                   # width: no preference
    .int    0                                   # height: no preference
    .int    0                                   # depth: no preference

    .short  0                                   # end tag type
    .short  0                                   # end tag flags
    .int    8                                   # end tag size

# Common code in 32-bit, prepare states to enter 64-bit.
.macro ENTRY32_COMMON
    # set data segment selectors
    mov     ax, 0x18
    mov     ss, ax
    mov     ds, ax
    mov     es, ax
    mov     fs, ax
    mov     gs, ax

    # set PAE, PGE bit in CR4
    mov     eax, {cr4}
    mov     cr4, eax

    # load the temporary page table
    lea     eax, [.Ltmp_pml4 - {offset}]
    mov     cr3, eax

    # set LME, NXE bit in IA32_EFER
    mov     ecx, {efer_msr}
    mov     edx, 0
    mov     eax, {efer}
    wrmsr

    # set protected mode, write protect, paging bit in CR0
    mov     eax, {cr0}
    mov     cr0, eax
.endm

# Common code in 64-bit
.macro ENTRY64_COMMON
    # clear segment selectors
    xor     ax, ax
    mov     ss, ax
    mov     ds, ax
    mov     es, ax
    mov     fs, ax
    mov     gs, ax
.endm

.code32
bsp_entry32:
    lgdt    [.Ltmp_gdt_desc - {offset}]             # load the temporary GDT
    ENTRY32_COMMON
    ljmp    0x10, offset bsp_entry64 - {offset}    # 0x10 is code64 segment

.code32
.global ap_entry32
ap_entry32:
    ENTRY32_COMMON
    ljmp    0x10, offset ap_entry64 - {offset}     # 0x10 is code64 segment

.code64
bsp_entry64:
    ENTRY64_COMMON

    # set RSP to boot stack
    movabs  rsp, offset {boot_stack}
    add     rsp, {boot_stack_size}

    # call rust_entry(magic, mbi)
    movabs  rax, offset {entry}
    call    rax
    jmp     .Lhlt

.code64
ap_entry64:
    ENTRY64_COMMON

    # set RSP to high address (already set in ap_start.S)
    mov     rax, {offset}
    add     rsp, rax

    # call rust_entry_secondary(magic)
    mov     rdi, {mb_magic}
    movabs  rax, offset {entry_secondary}
    call    rax
    jmp     .Lhlt

.Lhlt:
    hlt
    jmp     .Lhlt

.section .rodata
.balign 8
.Ltmp_gdt_desc:
    .short  .Ltmp_gdt_end - .Ltmp_gdt - 1   # limit
    .long   .Ltmp_gdt - {offset}            # base

.section .data
.balign 16
.Ltmp_gdt:
    .quad 0x0000000000000000    # 0x00: null
    .quad 0x00cf9b000000ffff    # 0x08: code segment (base=0, limit=0xfffff, type=32bit code exec/read, DPL=0, 4k)
    .quad 0x00af9b000000ffff    # 0x10: code segment (base=0, limit=0xfffff, type=64bit code exec/read, DPL=0, 4k)
    .quad 0x00cf93000000ffff    # 0x18: data segment (base=0, limit=0xfffff, type=32bit data read/write, DPL=0, 4k)
.Ltmp_gdt_end:

.balign 4096
.Ltmp_pml4:
    # 0x0000_0000 ~ 0x7f_ffff_ffff (512 GiB)
    .quad .Ltmp_pdpt_low - {offset} + 0x3   # PRESENT | WRITABLE | paddr(tmp_pdpt)
    .zero 8 * 255
    # 0xffff_8000_0000_0000 ~ 0xffff_807f_ffff_ffff (512 GiB)
    .quad .Ltmp_pdpt_high - {offset} + 0x3  # PRESENT | WRITABLE | paddr(tmp_pdpt)
    .zero 8 * 255

# FIXME: may not work on macOS using hvf as the CPU does not support 1GB page (pdpe1gb)
.Ltmp_pdpt_low:
.set i, 0
.rept 512
    .quad 0x40000000 * i | 0x83  # PRESENT | WRITABLE | HUGE_PAGE | paddr(0x4000_0000 * i)
    .set i, i + 1
.endr

.Ltmp_pdpt_high:
.set i, 0
.rept 512
    .quad 0x40000000 * i | 0x83  # PRESENT | WRITABLE | HUGE_PAGE | paddr(0x4000_0000 * i)
    .set i, i + 1
.endr
