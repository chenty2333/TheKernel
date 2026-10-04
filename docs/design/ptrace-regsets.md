# Ptrace register transport

The x86-64 general-register image is shared with core dumps in
`kernel/src/task/registers.rs`: 27 native words in Linux `user_regs_struct`
order, including FS/GS bases and `orig_rax`. Facts were checked against Linux
7.2.3 `arch/x86/kernel/ptrace.c` and `kernel/ptrace.c`; Rust is original.

The actual user GDT selectors match Linux amd64: CS=0x33, SS=0x2b.
Kernel selectors remain unchanged, with TSS/LDT moved beyond the user slots.
This avoids GDB's native CS-based misclassification as i386; it is not a fake
ptrace-only register translation. The guest regression reads actual CS/SS.
Upstream behavior reference: [GDB x86 Linux architecture detection](https://gnu.googlesource.com/binutils-gdb/+/d01e823438c7dc264d6885fbbfeace4d8955dcb7/gdb/nat/x86-linux.c).
No upstream implementation or prose was copied.

A thread publishes a value image immediately before parking at a stop boundary,
and consumes it on resume. Remote writers hold the existing ptrace action gate,
wait for scheduler inactivity, and never retain a pointer into another task's
stack. Faulting copies happen without holding the snapshot spin lock. Invalid
input does not partially commit a register image. GETREGSET uses native-word
alignment, clamps its iovec length, and supports an empty transfer. Raw PEEK
requests write the word through `data`, not the syscall return register.

EFLAGS writes preserve privileged bits. RIP/RSP writes are restricted to the
lower canonical user range: unlike Linux, this kernel has no bad-IRET fixup,
so accepting a noncanonical return frame could fault after SWAPGS in ring 0.
Such debugger inputs fail closed with EIO, without changing the image. This
restriction remains until safe return-fault delivery is implemented.
FS/GS bases must be in the lower
canonical user range. CS/SS must match the kernel's ring-3 long-mode descriptors. Legacy DS/ES/FS/GS
selectors are transported and restored against present GDT/LDT data/readable-code
descriptors. Invalidated LDT selectors become null at the final IRQ-disabled
return edge. FS/GS loads preserve the explicit saved bases, rather than reproducing
all legacy non-FSGSBASE hidden-base interactions. This and the stronger descriptor
admission remain documented Linux limitations. Changes to
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

SYSCALL publishes genuine entry/exit stops and syscall event messages. The entry
image has RAX=-ENOSYS and a separate original syscall number; edits of orig_rax
and argument registers affect dispatch, while a negative orig_rax skips it.
SYSEMU samples its skip decision on entry, so resuming that stop with CONT cannot
execute the suppressed syscall. Exit RAX edits affect the value observed in user
space. GET_SYSCALL_INFO includes actual IP/SP, six native arguments, return/error
and short-buffer sizing. Without TRACESYSGOOD it reports NONE, using provenance
captured at stop publication rather than guessing from later option changes.
Syscall siginfo is synthetic and not a queued signal to consume. Resume mode is
bound to the exact ptrace generation and committed before the job gate exposes
Running, so a reattachment cannot inherit a dead tracer's mode and an unrelated
wake cannot execute the tracee before its requested mode is installed.

Instruction stepping sets TF at the final user-entry edge, preserving it across
unrelated IRQ returns. Debugger-forced TF is hidden from register reads and cleared
by ordinary resume/detach; user-provided TF and POPF/IRET-owned updates are not
claimed by that cleanup. DR6 is captured and acknowledged before IRQ enable or
migration and architectural BS stops carry TRAP_TRACE at the actual return IP.
The synthetic syscall-step stop instead uses Linux x86's TRAP_BRKPT convention.
SYSEMU_SINGLESTEP combines skipped syscall entries with instruction stepping.
The paired guest test executes NOP/store instructions, steps across a real
syscall, verifies skipped SYSEMU_SINGLESTEP side effects, and preserves user-owned
TF. POPF/IRET opcode detection has host helper coverage, not a claim that arbitrary
user return-fault recovery is complete. The test admits at most one additional
same-IP TRAP_TRACE observed on native Linux after the emulation entry, and checks
that no store occurred at that stop before stepping the next instruction.
Hardware branch stepping (SINGLEBLOCK), hardware watchpoints and real gdb/strace
acceptance remain pending. Existing relationship and stop storage is process-wide; multithreaded
debugging requires task-exact stop/relationship semantics, not just registers.

## Stop report readiness

A stop request is not yet a reportable stopped context. Ptrace wait status and
SIGCHLD notification require all owners' value-image publication. Owners notify
after snapshotting, with one notification per stop. The top-of-user-loop gate
also covers a new stop arriving while a prior image retires. Without this barrier,
a waiter could release a shared-memory handshake after EVENT_STOP and let the
tracee execute another syscall before its resume mode was installed.

The paired readiness case uses 16 real seized tracees. It releases their handshake
immediately after wait status, with no GETREGS/GETSIGINFO/PEEK inactivity barrier,
and proves a pipe write stays absent until CONT and appears afterwards. This
checks execution effects rather than merely observing ptrace return success.

## Fork/vfork/clone stop publication

Selected fork/vfork/clone options create an inherited relationship for a new
process, not just a parent notification. Non-seized children get a bare SIGSTOP
with the native inherited-signal siginfo; seized children get EVENT_STOP. The
owner parks before its first user instruction (after CHILD_SETTID's schedule-tail
publication). Child syscall resume mode starts empty, rather than inheriting the
parent's SYSCALL/SYSEMU requests. CLONE_UNTRACED suppresses both inheritance and
the parent event. Event choice uses the supplied exit signal: vfork has priority,
then non-SIGCHLD clone, then fork; a disabled vfork event does not fall back to fork.

Parent clone event stops publish the actual owner context after complete child
publication and before entering the vfork-completion wait. This permits register
inspection of a vfork parent without waiting for a child the debugger has parked.
Stop provenance is cleared on leaving the owner wait, so an inherited SIGSTOP's
synthetic info cannot leak into a later delivery stop.

The paired portable case exercises fork, non-thread clone, vfork, UNTRACED and
seized fork, checks a shared first-instruction side effect stays absent at the
initial child stop, then follows an actual child write entry/exit and detaches it
before exit. Full traced-child exit handoff/reaping, thread-exact relationships,
exec option events and actual gdb/strace still need acceptance; this case does not
claim those workflows are complete.

## Exec reports

TRACEEXEC produces event 4 and an event message containing the former visible
thread ID, captured before identity/alias handoff. Non-seized legacy tracing
without that option produces a plain SIGTRAP with SI_USER siginfo; seized tracing
without TRACEEXEC produces no extra trap. Publication uses the exact admitted
relationship, not a new attach which happens after the image commits.

The paired regression execs the real ELF test program in all four option/seize
combinations, inspects the new image's entry registers and siginfo, then confirms
an actual pipe write and normal exit. Non-leader exec event IDs and thread-exact
wait/exit handoff still require multithreaded debugger acceptance.

## Software breakpoint stores

POKETEXT/POKEDATA first use ordinary writable-memory admission. Protected text
uses the common original private-executable COW patch primitive, shared with uprobes, while
holding the selected address-space handle and preserving its RX user PTE policy.
ELF loader backings do not carry syscall-mmap FileMapping metadata, so private
COW admission includes them as well as private anonymous executable backings.
Uprobes retain their stronger inode-binding admission before the same copy path.
No writable/executable mprotect window is created. Shared mappings and secret
backings cannot pass this admission, so neither inode cache nor another process's
mapping becomes patched. As with Linux's remote copy, a fault can leave a copied
prefix; an eight-byte store is not promised to be transactional across mappings.

The paired regression inserts an actual INT3, verifies the trap's IP/siginfo,
restores the instruction, single-steps it, and continues to normal exit. Parent
text, a shared RX alias, and pread of the executable inode retain original bytes.
A protected shared-alias write returns EIO. General FOLL_FORCE mutation of other
protected private mappings and concurrent debugger/uprobe byte ownership remain
limitations, not claims established by this executable-text test.
