# First-party Rust unsafe boundaries

This is the review index for source-owned Rust in `kernel/`, `crates/`, and
root `src/`.  It deliberately excludes `third_party/`, `target/`, and ArceOS
sources.  It is an index of intended boundaries and their evidence, **not** a
claim that every unsafe operation has been independently proved correct.

## Reproduce the inventory

Run these commands at the repository root.  The `unsafe` keyword count includes
comments, attributes, and declarations; the following three count only the
requested syntax forms and do not overlap on the current tree.

```sh
rg --files -g '*.rs' kernel crates src | wc -l
rg -n '\\bunsafe\\b' -g '*.rs' kernel crates src | wc -l
rg -n 'unsafe[[:space:]]*\\{' -g '*.rs' kernel crates src | wc -l
rg -n 'unsafe[[:space:]]+fn\\b' -g '*.rs' kernel crates src | wc -l
rg -n 'unsafe[[:space:]]+impl\\b' -g '*.rs' kernel crates src | wc -l
```

At this inventory's creation, the results are 250 Rust files, 385 keyword
lines, and 342 blocks + 8 `unsafe fn` declarations + 31 `unsafe impl`
declarations = 381 construct lines.  The four remaining keyword lines are one
unsafe attribute and three comments.  Counts are a review trigger, not a risk
score; a single raw-pointer ownership transfer can matter more than many
layout conversions.

## Islands and review contracts

| Island (coverage) | Primary owner file(s) | Core invariant to re-establish in review | Focused validation |
| --- | --- | --- | --- |
| User-memory capability and ABI copy-in/out (`mm/{access,usercopy}.rs`, `file/userfaultfd.rs`, syscall time/signal/resource/fs/io-mux/task paths) | `kernel/src/mm/usercopy.rs`, `kernel/src/syscall/time.rs` | A checked capability selects the address space; success initializes every output byte; unchecked copy-out has a fully initialized, x86_64 ABI-valid object including padding.  Never form a Rust reference to concurrently mutable user memory. | Usercopy/unit tests; affected syscall's Linux differential and fault/partial-copy case. |
| Socket address and message ABI (`syscall/net/**`, `file/{netlink,packet,af_alg}.rs`) | `kernel/src/syscall/net/addr.rs`, `kernel/src/file/netlink.rs` | Length/family precede casts; raw byte views only cover initialized `repr(C)` values; unaligned reads use the unaligned primitive; outbound lengths never exceed the checked user buffer. | Address family/short-buffer/fault tests and network differential. |
| System V/POSIX IPC and pipe transfer (`syscall/ipc/**`, `file/pipe.rs`, `task/futex.rs`) | `kernel/src/syscall/ipc/msg.rs`, `kernel/src/syscall/ipc/sem.rs`, `kernel/src/file/pipe.rs` | IPC records are zeroed before padding can be copied out; raw member addresses are used only as usercopy addresses; queue nodes retain exactly one `Arc`/`Box` owner through dequeue. | IPC ABI, blocking/wakeup, fault and teardown tests. |
| Timer and task intrusive queues (`task/{timer,futex,thread,ops}.rs`, task scheduling syscalls) | `kernel/src/task/timer.rs` | Per-CPU consumer token gives exactly one mutable cursor; producer publication has release/acquire ordering; every raw `Arc` count is transferred once and nodes stay live until the sole consumer unlinks them. | Existing timer handoff/queue tests; add a focused multi-CPU interleaving test whenever this protocol changes. |
| Descriptor and notification deferred cleanup (`file/{desc,dnotify,fanotify,inotify}.rs`) | `kernel/src/file/desc.rs` | Atomic incoming lists publish only live heap nodes; drain ownership is exclusive; `Box::from_raw` occurs exactly once after successful unlink; deferred file leases retain their descriptions until finalization. | Descriptor close/notification teardown tests, with an allocation-failure or concurrent-close case for changes. |
| Registered buffers and physical I/O (`file/io_uring.rs`) | `kernel/src/file/io_uring.rs` | A registered pin remains valid for the whole descriptor/effect lifetime; `Send`/`Sync` relies on registry serialization; physical publication is one-way, and admission/lease/credit survives until completion or typed quiescence/retirement proof. Quarantine alone retains custody. | Physical-I/O unit/guest tests plus direct/fallback, reset and ring-close teardown cases. |
| VM frames, COW and shared atomics (`mm/aspace/**`, `mm/io.rs`) | `kernel/src/mm/aspace/backend/cow.rs`, `kernel/src/mm/aspace/backend/shared.rs` | Physical-to-virtual mappings are live, sized and non-overlapping for the operation; a frame is initialized before publication; copy/COW preserves ownership and required TLB/I-cache synchronization; shared atomic pointers are aligned and only atomically accessed. | MM/COW tests, mapping-fault paths and x86_64 TLB regression coverage. |
| cBPF interpreter, packet filter and executable memory (`bpf/**`, `syscall/{bpf,seccomp}.rs`, `packet_cbpf.rs`, `jit_memory.rs`) | `kernel/src/bpf/vm.rs`, `kernel/src/jit_memory.rs` | VM ranges are checked before raw load/store; JIT memory is writable only while building, immutable before execution, executable only under W^X policy, and remains mapped throughout the typed entry call. | Interpreter bounds tests; when a translator is enabled, interpreter/JIT differential plus W^X publication/retirement tests. |
| In-kernel network queues (`crates/axnet-ng/**`) | `crates/axnet-ng/src/unix/{stream,dgram}.rs` | Ring indices move only after the corresponding initialized bytes are owned; deferred cleanup uses a single finalizer; explicit `Send`/`Sync` is justified by the queue's synchronization, not by the raw fields. | Stream/datagram/vsock lifecycle and concurrent cleanup tests. |
| Device-like pseudo files and object serialization (`pseudofs/dev/**`, `file/types.rs`, `task/coredump.rs`, `keyring/object.rs`) | `kernel/src/pseudofs/dev/fb.rs`, `kernel/src/task/coredump.rs` | Mapped range is live and exact; `MaybeUninit` is read only after initialization; serialized C records have a valid initialized byte representation; volatile wiping cannot be optimized away. | Device bounds tests, coredump layout tests and targeted ABI assertions. |
| Narrow integration traits (`rcu.rs`, `crates/process-adapter/src/lib.rs`, root `src/main.rs`) | `kernel/src/rcu.rs` | The unsafe trait/attribute contract remains no wider than its platform or process boundary; callers cannot manufacture an aliasing/lifetime promise the implementation does not enforce. | Trait-specific host test and product build/link check. |

The table covers every `unsafe {}`, `unsafe fn`, and `unsafe impl` found by the
commands above; directory entries such as `syscall/fs/**` and `mm/aspace/**`
are intentional groupings, not omitted code.

## Review rule and highest-priority gaps

For a new or materially changed construct, keep the unsafe operation local,
state the concrete preconditions next to it, and update the relevant row and
focused test in the same change.  Do not use this document to bless a broad
module, a raw pointer type, or an `unsafe impl` wholesale.

1. The raw ownership protocols in timer, descriptor cleanup, futex, and
   io_uring are the highest review priority: they combine atomics with
   `Arc::from_raw`/`Box::from_raw`, and require interleaving plus teardown
   coverage rather than only happy-path tests.
2. The physical-I/O boundary needs continued reset/quarantine and close-path
   testing.  A logical CQE or timeout is not evidence that a DMA pin,
   descriptor, or device claim may be released.
3. JIT execution must not expand beyond the present boundary until translator
   validation, W^X publication, x86_64 state policy, and retirement testing
   close together.  This inventory does not establish those properties.
4. ABI copy-out sites remain sensitive to padding and partial-fault order.
   Each new struct transfer needs an explicit initialized-representation and
   layout argument, with a Linux comparison where the ABI is observable.
