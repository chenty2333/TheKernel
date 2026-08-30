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
