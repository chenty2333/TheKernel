#__clone(func, stack, flags, arg, ptid, tls, ctid)
#         a0,    a1,   a2,    a3,  a4,  a5,   a6
# sys_clone(flags, stack, ptid, ctid, tls)
#            a0,    a1,   a2,    a3,  a4

.text
.global oscomp_raw_clone
.hidden oscomp_raw_clone
.type oscomp_raw_clone,@function
oscomp_raw_clone:
	bstrins.d $a1, $zero, 3, 0
	addi.d $a1, $a1, -16
	st.d $a0, $a1, 0
	st.d $a3, $a1, 8
	or $a0, $a2, $zero
	or $a2, $a4, $zero
	or $a3, $a6, $zero
	or $a4, $a5, $zero
	ori $a7, $zero, 220
	syscall 0

	beqz $a0, 1f
	jirl $zero, $ra, 0
1:
	ld.d $t8, $sp, 0
	ld.d $a0, $sp, 8
	jirl $ra, $t8, 0
	ori $a7, $zero, 93
	syscall 0

.size oscomp_raw_clone, .-oscomp_raw_clone
