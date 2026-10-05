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
Hardware branch stepping (SINGLEBLOCK) remains unsupported.
Real gdb/strace acceptance and task-exact multithread state are recorded below.

## Stop report readiness

A stop request is not yet a reportable stopped context. Ptrace wait status and
SIGCHLD notification require the stopped task owner's value-image publication. Owners notify
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

## Real Alpine tools

The optional debug payload uses signed pinned Alpine GDB16.3 and strace6.19, with
its complete runtime closure and a debug-info C target. Native and guest GDB
batch runs establish breakpoint/run/bt/registers/print/variable mutation,
step/next/finish and normal continuation; changed input yields DEBUG_RESULT=24.
The ordinary smoke also validates strace file syscall parameters and returns.
The default driver now exercises fork/exec/file tracing and checks the ordered
file syscall arguments and return values; --files-only keeps the direct fixture.
The earlier guest run exposed ECHILD at traced-child teardown despite correct
syscalls and target output. The terminal wait repair below addresses that cause;
real-tool acceptance passed in guest; the final notification-readiness tightening
passed revalidation rather than being inferred from that earlier run.
Independent multithread stops and real Alpine GDB thread-stack commands now have guest coverage, as detailed below.

## Terminal traced-process handoff

Before clearing a final tracee's live relationship, exit transfers its exact
session generation into the existing preallocated durable group-leader owner.
The tracer reverse link survives and wait resolves the authoritative core process
registry, not merely the live runtime table. No task, ProcessData or address space
is retained for this purpose, and final exit allocates no new owner.

Natural-parent wait cannot consume or WNOWAIT-observe a terminal report held by
a tracer. Repeated tracer WNOWAIT calls leave it intact. A consuming tracer wait
claims its exact session once: a different natural parent receives handoff and
its own SIGCHLD/autoreap policy; a direct-parent tracer performs the sole reap
and usage accounting. Tracer teardown releases the same durable hold even when
the runtime has disappeared. A handed-off marker prevents duplicate parent
notifications when teardown/acknowledgement races final notification. Reparenting
does not bypass a still-held report. Nonleader exits use the separate task-terminal owner below; the process-final parent handoff remains distinct.

Core zombie publication is not yet ptrace wait readiness: the terminal report
becomes visible only after its configured notification. A parent handoff also
remains non-reportable until notification/autoreap finishes, with an extra wake
after that publication; this prevents a polling waiter from racing notification
or sleeping after observing the in-flight phase. Direct-parent tracers preserve
the configured clone exit signal (including none); other tracers get SIGCHLD.
Own-child tracer teardown does not generate a duplicate exit signal, but honors
ignored/NOCLDWAIT and child-autoreap policy before reparenting.

Native Linux's eight-scenario regression and the earlier guest runs cover
repeated WNOWAIT, parent WNOHANG withholding, tracer acknowledgement then natural
reap, zombie-grandchild tracer death, direct-parent SIGCHLD/SIGUSR1/no-signal clone,
own-child zombie tracer death with default/ignored SIGCHLD, and a non-final
pthread tracer exiting with a terminal report held while its process stays live.
The complete repair passed native Linux, kernel2591, lint, baseline61/debug62 KVM
guest and the paired ptrace+wait35/266 subset. Full ABI only retained the unchanged
Linux oracle socket timeout failure; no full ABI pass is claimed.

The existing exact task-parent publication gate serializes terminal parent
selection, notification and report-ready publication with reparenting. Non-final
tracer exit already holds that gate and passes its borrowed guard through the
reverse-link drain; final exit acquires it only after releasing ptrace actions.
This adds no parallel lock/graph and preserves lifecycle->parent->action ordering.

The parent waiter is captured before report-ready publication, so an immediate
winning reap cannot clear the child-parent link and suppress the wake needed by
another waiting thread. This reference is transient; zombie ownership still
retains no live runtime or address space.


## Exact task tracing and real multithread debugger acceptance

Ptrace relation/option/action/signal/private-stop state now belongs to `Thread`,
not `ProcessData`. Shared job control and cgroup freezer state remain group-owned.
A ptrace stop parks only its exact task; its own GPR/FP image is the readiness
boundary. Each CLONE_THREAD child has an independent inherited relation, initial
stop and reverse link before its first user instruction. Sorted tracer/tracee
publication gates are taken after clone construction releases its lifecycle
ownership, while the child remains off-runqueue and exec publication excluded.

Wait candidates render exact namespace-visible TIDs, select each private report
and consume it once. A preallocated nonleader terminal slot moves into the
already-reserved reverse node. It retains namespace, credentials, status and
accounting values, never a live task, process runtime or address space. Its TID
binding survives until acknowledgement or tracer teardown. Notification precedes
terminal readiness; repeated WNOWAIT does not consume it. Detached relationship
credentials stay owned until lifecycle/action gates have dropped. Reverse links
also retain the immutable core TGID as lookup metadata, so nonleader exec can
adopt the group PID without making final wait depend on a dead scheduler TID.

A paused/sigsuspended task visits the private stop boundary using its real syscall
context, and remains asleep after a signal-free CONT. Exec retirement must unwind
that blocked wait even though it enqueues no signal. Other interruptible syscall
waits likewise recognize exact ptrace stops and exec retirement. The paired
thread fixture checks independent peer progress, distinct RSP/FSBASE, independent
resume, once-only exits and nonleader exec with a paused leader. Repeated WNOWAIT passed in the full TheKernel ABI guest; paused-leader attach/exec also passed the paired subset. Exact-session/readiness and reverse-node TID-release helpers passed host tests.

A seized group-stop reports its real stop signal plus EVENT_STOP, not SIGTRAP;
LISTEN/SIGCONT leave shared job control separate from the task's private report.
GDB 16.3 initializes every new LWP by writing DR7=0 even for software-only debug.
That inert reset is accepted; enabling hardware comparators still fails closed
and hardware-watchpoint acceptance remains the next A1 item. No hardware-enabled
state is silently accepted by this software-only stage.

The signed Alpine 16.3 batch thread script passed in a complete64-case KVM debug
suite: info threads, all-thread backtraces, switches2/3, distinct worker stacks
with markers11/22, the main stack marker33, per-thread registers, release of the
workers and THREADS_RESULT=12,23,34 followed by normal exit. Basic GDB variable
mutation/single-step/finish and real strace-f fork/exec/files also passed there.
Earlier ECHILD, recursive lifecycle-lock panic, DR7 setup hang and LISTEN signal
mismatch runs were failures, not acceptance. The DR7 requirement was checked in
upstream GDB16.3 nat/x86-linux-dregs.c:x86_linux_update_debug_registers.

This is not complete Linux ptrace parity. SINGLEBLOCK, hardware comparators,
general protected-private FOLL_FORCE, group-leader terminal delay across unreaped
ptraced nonleaders, and late final group-rusage refresh after runtime teardown
remain unsupported or incomplete. Pending shared signal routing still chooses
the retained leader rather than a general task recipient. No /proc implementation
was changed as part of this task. The full post-migration host suite passed (652 Python tests,3 skips; selected Rust/kernel2594), then the affected kernel was refreshed to2596 after the restart fix. All50 programs completed successfully in the TheKernel ABI guest; the Linux oracle again failed only the unchanged socket timeout jiffy-roundtrip assertion, so no full paired pass is claimed. Final baseline62/debug64 KVM guest and q35/n305 lint revalidation passed; the subsequent affected kernel host run passed2596 and the paired ptrace+wait subset passed31/267.

A blocked restartable wait interrupted only for a ptrace stop must restart when
CONT suppresses the record or default-ignore prevents any handler callback. The
final no-handler edge resolves a still-pending restart only while the return
value remains EINTR; an explicitly debugger-written return value wins. The new
raw regression passed native Linux but exposed premature wait4 return in the
guest before this fix. The raw regression then passed the paired subset; host2596 and complete baseline62/debug64 KVM guest passed, including real strace-f. The tracer
still sees EINTR rather than Linux's internal ERESTART* sentinel for this
interruption; exact internal syscall-exit rendering remains a parity gap.

## Hardware comparators

POKEUSER/PEEKUSER admit DR0–DR3, virtual DR6 and raw DR7. Enabled
comparators support execution, aligned 1/2/4/8-byte write and read/write
watchpoints; I/O comparators and kernel addresses are rejected. Reserved DR7
bits are retained for reads but removed from the physical image. Invalid writes
do not commit. User entry installs a task-owned image with IRQs disabled; every
return snapshots DR6 and restores the kernel/perf image before IRQ dispatch.
SIGTRAP reports TRAP_HWBKPT and the trapping RIP (TRAP_TRACE wins if BS is set).
Exec and logical exit clear the image and release ownership.

Known difference: ptrace hardware comparators and perf breakpoint descriptors
exclude each other globally with EBUSY, not Linux's shared per-CPU slot admission.
Disabled DR7 releases the ptrace lease; a perf lease lasts with the descriptor.
This conservative admission avoids silently masking existing perf breakpoints.
No hardware registers change by default. Native N305 behavior is unverified.

Validation: real Alpine GDB 16.3 `watch watched` stopped on both writes,
reported 0→7→19 and exited normally in the 64/64 debug guest suite
(`system-ohd44s06`). Raw POKEUSER hardware writes, virtual DR6, TRAP_HWBKPT
and si_addr=RIP passed on both Linux and TheKernel (`abi-h6lylt08`).
Kernel host tests passed 2596; q35 lint passed with existing warnings.
