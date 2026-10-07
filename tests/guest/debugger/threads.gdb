set pagination off
set confirm off
set startup-with-shell off
directory /opt/thekernel-tests/debugger
break snapshot_ready
run
info threads
thread apply all bt
thread 2
bt
info registers rip rsp rbp fs_base
thread 3
bt
info registers rip rsp rbp fs_base
thread 1
bt
info registers
set variable release_gate = 1
continue
