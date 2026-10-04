# Ptrace register transport

The x86-64 general-register image is shared with core dumps in
`kernel/src/task/registers.rs`: 27 native words in Linux `user_regs_struct`
order, including FS/GS bases and `orig_rax`. Facts were checked against Linux
7.2.3 `arch/x86/kernel/ptrace.c` and `kernel/ptrace.c`; Rust is original.

A thread publishes a value image immediately before parking at a stop boundary,
and consumes it on resume. Remote writers hold the existing ptrace action gate,
wait for scheduler inactivity, and never retain a pointer into another task's
stack. Faulting copies happen without holding the snapshot spin lock. Invalid
input does not partially commit a register image. GETREGSET uses native-word
alignment, clamps its iovec length, and supports an empty transfer. Raw PEEK
requests write the word through `data`, not the syscall return register.

EFLAGS writes preserve privileged bits. FS/GS bases must be in the lower
canonical user range. CS/SS must match the kernel's ring-3 descriptors; nonzero
legacy selectors are currently rejected (no user LDT/selector restore). This
is a documented limitation, not full Linux selector compatibility. Changes to
`orig_rax` update or cancel only the interrupted-syscall restart candidate
belonging to the stopped frame. Argument writes also replace replay arguments,
even if `orig_rax` did not change; an ancestor signal handler's saved restart
is not discarded.

## Validation and remaining work

General-register layout/privilege filtering have host tests. The guest regression
checks GET/SETREGS, short/empty/alignment-sensitive NT_PRSTATUS, PEEKUSER,
POKEUSER, raw memory PEEK/POKE, and that a changed RAX affects child execution.
Actual test results are recorded in the task progress file after execution.

Floating state uses an owner-published, heap-aligned XSAVE image. Getters normalize
init x87/SSE components and emit the standard regset software-reserved xfeature
mask, without the signal-frame magic/trailer. Setters validate MXCSR and the
uncompacted XSAVE header before commit. FP writes activate x87/SSE and preserve
other components; XSTATE writes require a complete CPU-sized image. Restore is
performed by the stopped owner after wake, never through a remote context pointer.

GET/SETSIGMASK use the native eight-byte signal set and exclude SIGKILL/SIGSTOP.
Temporary suspend/poll masks retain their eventual restore value in the signal
manager; a debugger replacement updates it so a local wait guard cannot restore
stale data. PEEKSIGINFO reads private/shared queues without consuming records.
The original signal queues keep per-signal delivery priority; independent arrival
tokens order the siginfo view across signal numbers. Allocation, sorting and
faulting usercopy are outside pending spin locks.

Seized INTERRUPT stops carry PTRACE_EVENT_STOP and synthetic siginfo. LISTEN
keeps the owner parked while making ordinary ptrace requests return ESRCH;
INTERRUPT or SIGCONT re-publishes an event stop. An INTERRUPT arriving during an
already traced stop is retained until resume instead of being silently lost.

Syscall entry/exit stops,
single-step, LISTEN, hardware watchpoints and real gdb/strace acceptance remain
pending. Existing relationship and stop storage is process-wide; multithreaded
debugging requires task-exact stop/relationship semantics, not just registers.
