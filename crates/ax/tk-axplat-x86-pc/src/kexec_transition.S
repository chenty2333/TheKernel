.section .text.axplat_kexec_transition,"ax",@progbits
.global axplat_kexec_transition_start
.global axplat_kexec_transition
.global axplat_kexec_transition_end
.type axplat_kexec_transition,@function
axplat_kexec_transition_start:
axplat_kexec_transition:
    cli
    mov %rdi, %cr3
    mov %rsi, %rsp
    xor %rbp, %rbp
    mov %rdx, %rsi
    xor %rdi, %rdi
    jmp *%rcx
.size axplat_kexec_transition, .-axplat_kexec_transition
axplat_kexec_transition_end:

# Enter a copied, identity-mapped terminal copier.  The old high-half shim is
# left behind before any destination page is overwritten.
.global axplat_kexec_copy_enter_start
.global axplat_kexec_copy_enter
.global axplat_kexec_copy_enter_end
.type axplat_kexec_copy_enter,@function
axplat_kexec_copy_enter_start:
axplat_kexec_copy_enter:
    cli
    mov %rdi, %cr3
    mov %rsi, %rsp
    xor %rbp, %rbp
    jmp *%rdx
.size axplat_kexec_copy_enter, .-axplat_kexec_copy_enter
axplat_kexec_copy_enter_end:

# Position-independent terminal copy engine.  %rcx points at an identity-
# mapped control record.  Each linked page contains next/destination/length
# followed by payload bytes.  All list, control, stack, and code pages were
# allocated outside every final image destination.
.global axplat_kexec_copy_start
.global axplat_kexec_copy_end
.type axplat_kexec_copy_start,@function
axplat_kexec_copy_start:
    mov %rcx, %r12
    mov 0(%r12), %r10
    cld
.Lkexec_copy_next:
    test %r10, %r10
    jz .Lkexec_copy_done
    mov 0(%r10), %r11
    mov 8(%r10), %rdi
    mov 16(%r10), %rcx
    lea 24(%r10), %rsi
    rep movsb
    mov %r11, %r10
    jmp .Lkexec_copy_next
.Lkexec_copy_done:
    mov 8(%r12), %rdi
    mov 16(%r12), %rsi
    mov 24(%r12), %rdx
    mov 32(%r12), %rcx
    cmpq $0, 40(%r12)
    jne .Lkexec_copy_32
    mov %rdx, %rsi
    xor %rdi, %rdi
    jmp *%rcx
.Lkexec_copy_32:
    mov 48(%r12), %r8
    jmp *%r8
axplat_kexec_copy_end:

.global axplat_kexec_transition32_enter_start
.global axplat_kexec_transition32_enter
.global axplat_kexec_transition32_enter_end
.type axplat_kexec_transition32_enter,@function
axplat_kexec_transition32_enter_start:
axplat_kexec_transition32_enter:
    cli
    mov %rdi, %cr3
    mov %rsi, %rsp
    xor %rbp, %rbp
    jmp *%r8
.size axplat_kexec_transition32_enter, .-axplat_kexec_transition32_enter
axplat_kexec_transition32_enter_end:

# Position-independent low-memory bzImage handoff.  It is copied verbatim to
# an identity-mapped page before use, so clearing PG cannot strand execution
# in the high-half kernel mapping.
.global axplat_kexec_transition32_start
.global axplat_kexec_transition32_end
.type axplat_kexec_transition32_start,@function
axplat_kexec_transition32_start:
    cli
    mov %rdi, %cr3
    mov %rsi, %rsp
    xor %rbp, %rbp
    sub $24, %rsp
    movw $31, (%rsp)
    lea axplat_kexec_transition32_gdt-axplat_kexec_transition32_start(%r8), %rax
    mov %rax, 2(%rsp)
    mov %rcx, 16(%rsp)
    lgdt (%rsp)
    add $16, %rsp
    pushq $0x10
    lea .Lkexec_compat(%rip), %rax
    pushq %rax
    lretq

.code32
.Lkexec_compat:
    mov $0x18, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    mov %edx, %esi
    xor %edi, %edi
    mov %cr0, %eax
    btr $31, %eax
    mov %eax, %cr0
    mov $0xc0000080, %ecx
    rdmsr
    btr $8, %eax
    wrmsr
    mov %cr4, %eax
    btr $5, %eax
    mov %eax, %cr4
    xor %ebx, %ebx
    jmp *(%esp)

.code64
.align 8
axplat_kexec_transition32_gdt:
    .quad 0
    .quad 0x00cf9a000000ffff # 32-bit code, selector 0x08
    .quad 0x00cf9a000000ffff # compatibility code, selector 0x10
    .quad 0x00cf92000000ffff # 32-bit data, selector 0x18
axplat_kexec_transition32_end:
