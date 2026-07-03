# TheKernel I/O Boost

This context names the next-stage I/O performance work for TheKernel. It keeps the planning language precise while separating competition-driven goals from throwaway benchmark shortcuts.

## Language

**OSComp I/O Target**:
The next-stage performance objective: improve OSComp-relevant iozone throughput while preserving kernel correctness and leaving a maintainable block-I/O architecture behind.
_Avoid_: benchmark hack, full storage rewrite

**Architecture Release Profile**:
The per-architecture enablement policy for new block-I/O behavior. RISC-V is the first default-on performance target, while LoongArch64 shares the same contract but may use conservative depth or synchronous fallback until its VirtIO DMA path is proven stable.
_Avoid_: arch hack, temporary fork

**Async Consumer**:
A filesystem or syscall path that can create more than one block request before waiting for completion. The first async consumer is page-cache dirty run flush; user direct I/O and the lwext4 read path are planned later consumers of the same queue contract.
_Avoid_: caller, path, client

**Dirty Run Flush**:
Writing a consecutive run of dirty page-cache pages back to the filesystem or block device as a grouped operation. It is the first target consumer because the kernel owns the page lifetime and can wait for the whole run before clearing dirty state.
_Avoid_: writeback daemon, background flush

**Completion Wakeup Contract**:
The block-queue rule that every submitted request has an observable completion state and a shared wakeup path. Waiters may use a short spin phase for low latency, but pure busy polling is not an accepted completion model.
_Avoid_: done flag loop, wait spin, polling contract

**Hybrid Wait Policy**:
A wait strategy that first drains completions and spins briefly for immediately completed requests, then registers a waiter and yields until the shared completion path wakes it.
_Avoid_: busy wait, interrupt only

**Owned Block Request**:
A block-I/O request whose request header, response status, completion state, token metadata, and resource guards are owned by the block queue until completion. It replaces stack-borrowed pending requests for any path that can have more than one request in flight.
_Avoid_: stack pending request, done flag request

**Request Pool**:
A bounded allocator for owned block requests on the hot I/O path. It exists to avoid turning async queue depth into per-request heap allocation overhead.
_Avoid_: request heap, temporary Box

**Descriptor-Aware Admission**:
The block-queue admission rule that limits submissions by estimated VirtIO descriptor use, not only by request count. It lets RISC-V use deep queues when indirect descriptors are available while keeping LoongArch64 conservative when direct descriptors are the stable path.
_Avoid_: request-count cap, fixed queue depth

**Opportunistic Completion Drain**:
Completing finished block requests from submit, wait, queue-full, or interrupt paths instead of relying on a permanent polling worker. It is the first-stage completion strategy for the async block queue.
_Avoid_: completion daemon, background poller

**Barrier Model**:
The ordering rules for async block writes. Dirty data writes may be batched when their ownership and range are clear, while metadata, truncate, fsync, sync, close, and flush boundaries must fence earlier writes before continuing.
_Avoid_: reorder policy, write scheduler

**Async Block Capability**:
An optional block-device capability that exposes batch submission, completion polling, and wait operations while preserving the existing synchronous block driver API for unsupported devices.
_Avoid_: replacement block trait, forced async driver
