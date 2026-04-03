.macro fixup op rd rj off
100:
	\op	\rd, \rj, \off
	_asm_extable 100b, .Lerr_copy_user
.endm

.section .text
.global user_copy
user_copy:
	add.d	$t5, $a0, $a2
	move	$t0, $t5
	beqz	$a2, .Lout_copy_user

	li.w	$a3, 72
	bltu	$a2, $a3, .Lbyte_copy_tail

	addi.d	$t1, $a0, 7
	srli.d	$t1, $t1, 3
	slli.d	$t1, $t1, 3
	beq	$a0, $t1, .Lskip_align_dst
1:
	fixup	ld.bu $a4, $a1, 0
	fixup	st.b  $a4, $a0, 0
	addi.d	$a0, $a0, 1
	addi.d	$a1, $a1, 1
	bltu	$a0, $t1, 1b

.Lskip_align_dst:
	andi	$a3, $a1, 7
	bnez	$a3, .Lshift_copy

.Lword_copy:
	addi.d	$t8, $t0, -64
	bltu	$t8, $a0, .Lbyte_copy_tail
2:
	fixup	LDD $a4, $a1, 0
	fixup	LDD $a5, $a1, 1
	fixup	LDD $a6, $a1, 2
	fixup	LDD $a7, $a1, 3
	fixup	LDD $t1, $a1, 4
	fixup	LDD $t2, $a1, 5
	fixup	LDD $t3, $a1, 6
	fixup	LDD $t4, $a1, 7
	fixup	st.d $a4, $a0, 0
	addi.d	$a0, $a0, 8
	fixup	st.d $a5, $a0, 0
	addi.d	$a0, $a0, 8
	fixup	st.d $a6, $a0, 0
	addi.d	$a0, $a0, 8
	fixup	st.d $a7, $a0, 0
	addi.d	$a0, $a0, 8
	fixup	st.d $t1, $a0, 0
	addi.d	$a0, $a0, 8
	fixup	st.d $t2, $a0, 0
	addi.d	$a0, $a0, 8
	fixup	st.d $t3, $a0, 0
	addi.d	$a0, $a0, 8
	fixup	st.d $t4, $a0, 0
	addi.d	$a0, $a0, 8
	addi.d	$a1, $a1, 64
	bgeu	$t8, $a0, 2b
	b	.Lbyte_copy_tail

.Lshift_copy:
	srli.d	$t1, $t0, 3
	slli.d	$t1, $t1, 3
	bgeu	$a0, $t1, .Lbyte_copy_tail

	srli.d	$t6, $a1, 3
	slli.d	$t6, $t6, 3
	slli.d	$t3, $a3, 3
	li.w	$t4, 64
	sub.d	$t4, $t4, $t3
	fixup	ld.d $a5, $t6, 0
3:
	srl.d	$a4, $a5, $t3
	fixup	ld.d $a5, $t6, 8
	addi.d	$t6, $t6, 8
	sll.d	$a6, $a5, $t4
	or	$a6, $a6, $a4
	fixup	st.d $a6, $a0, 0
	addi.d	$a0, $a0, 8
	addi.d	$a1, $a1, 8
	bltu	$a0, $t1, 3b

.Lbyte_copy_tail:
	bgeu	$a0, $t0, .Lout_copy_user
4:
	fixup	ld.bu $a5, $a1, 0
	fixup	st.b  $a5, $a0, 0
	addi.d	$a0, $a0, 1
	addi.d	$a1, $a1, 1
	bltu	$a0, $t0, 4b

.Lout_copy_user:
	move	$a0, $zero
	jr	$ra

.Lerr_copy_user:
	sub.d	$a0, $t5, $a0
	jr	$ra
