set pagination off
set confirm off
set startup-with-shell off
directory /opt/thekernel-tests/debugger
break middle
run
bt
info registers rip rax rsp rbp
print input
step
print input
set variable input = 9
next
print local
next
finish
finish
continue
