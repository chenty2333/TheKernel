# Native block observations for diskstats and sysfs stat

## Scope and sources

Linux 7.2.3 `block/genhd.c` supplies field order/units, not implementation code.
One allocation-free native ledger supplies `/proc/diskstats` and each registered
block device's sysfs `stat`. The existing shared owner records only operations
that actually enter its legacy provider or published requests accepted by the
lower queue. Ordinary/synchronous batch and physical batch publication share a
fixed 128-entry observation slab, matching the existing completion-credit bound.
The existing task-context lower drain accounts each authenticated concrete
completion once, before mailbox delivery; later waiter delivery is not counted
again. IRQ notification, queue admission, buffers, pin ownership, cancellation
and device state are unchanged. Range geometry is observed only inside the
existing admitted legacy owner, never by an extra pre-admission device lock;
a held-lower/quarantined regression covers discard and zero-write rejection. No hardware request is issued by reading stats.

Legacy callbacks count successful operations and their actual buffer lengths;
loop callbacks use the actual short return length. Async success counts the
accepted block request's data length (used-ring length includes protocol status
and is not a payload byte count). Bytes accumulate before division into 512-byte
sectors. FUA/zero-write callbacks count as writes; supported discards and flushes
have separate counters. Separate requests are not merged by this native owner,
so merge counters are zero, not estimated from SG segment coalescing.

Times are raw monotonic native observation intervals, not a physical hardware
latency measurement: legacy entry/return, accepted publication/lower task drain,
and intervals with observed requests pending. Completed operation durations
supply the weighted time column. Snapshot reads project the current busy interval
without modifying the ledger or polling a device. Namespace clock offsets do not
alter these global device observations.

## Failure and lifetime

Unknown/duplicate identities or slab exhaustion make statistics unavailable,
never change I/O admission/custody and never substitute successful zero counts.
A quarantined shared owner also reports statistics unavailable. Quarantined
completions do not prove DMA retirement; their pending observations remain until
the existing Quiesced reset outcome proves quiescence. Proven reset cancellation removes pending work
without inventing successful transfers; historical completed counters persist.
A loop uses a zero-entry slab because its provider is synchronous, avoiding a
large per-loop queue allocation. Shared queue clones retain the same ledger.

## Known differences

This is not a Linux BIO/request accountant. Failed operations are excluded from
successful operation/sector totals; callback/publication/worker observation
boundaries can differ from Linux insertion/IRQ completion timing. Native loop
counters follow the lifetime of the fixed native loop node, not Linux dynamic
loop-disk reallocation. There is no physical busy-time or failed-byte claim.
Advanced queue/discard capability fields and the earlier iostat CPU-parser
spacing discrepancy remain outside this tool-level closeout item.

## Validation

Related driver tests cover observation timing, sector units, real inflight,
completion-versus-mailbox lifetime, quarantine/reset and fail-closed identities.
The formatter has 17 decimal fields with no header; the host Linux shape
regression passes without opening any host block device. The guest regression
compares proc/sysfs sources and device IDs, performs aligned read-only root-device
reads and requires actual completion/sector deltas. Its optional tools mode runs
signed Alpine `iostat -dx` and requires a real root disk row, not an empty header.
Related host driver 59/59, filesystem 177/177 and kernel 2621/2621 tests passed;
lint passed with 784 existing kernel warnings. KVM guest passed 70/70 without
skips and shut down normally (`system-wtpgl_4e`), with observed read/sector deltas
47/376. Actual signed Alpine iostat displayed a real `vda` extended disk row
without diagnostics (`shell-wbzjq40w`, tools result 0); its read-only fixture
observed deltas 46/368. These are effect/count checks on QEMU's virtual root
storage, not hardware throughput/latency benchmarks or native N305 acceptance.
