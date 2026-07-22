# RFC 0007: Bounded AF_PACKET Ordinary-Queue Contract

- Status: implemented
- Profile: ordinary-queue baseline
- Date: 2026-07-20
- Owners: TheKernel maintainers
- Target layers: `axnet-ng`, `thekernel-linux-packet`, and the TheKernel
  network/syscall/security adapter
- Audited implementation: Layer 1 and the Layer 3 ordinary-queue/UDP closure
  through `6491f98846205e5db14f0f20292f6eff08cf1281`, Layer 2 through
  `43f58fa0075c564a2a8a8e4caddca473cf0be1b7`, and the exact-source evidence
  gates through `6b9db6efccd248d893a489647a802d1a06440c5f`; the maintained
  task/readiness dependency is pinned at
  `5c34536fd766b5f84f2fb8e6b18a2ab340659582`

## Problem

`AF_PACKET` crosses a device tap, a namespace-local subscription, Linux
protocol/address rules, an open-file description, readiness, usercopy, and a
privileged syscall entry. Treating it as one socket switch arm would obscure
which lock owns a packet, which state a bind publishes, and which layer must
refund memory after failure.

This RFC freezes a deliberately small first slice: ordinary copied
`SOCK_RAW`/`SOCK_DGRAM` queues with bounded retention. It is useful for
diagnostics, compatibility, and later differential testing, but is not the
high-throughput ring design documented by Linux as `PACKET_MMAP`.

## Evidence reviewed

The public behavior was cross-checked against:

- Linux man-pages [`packet(7)`](https://man7.org/linux/man-pages/man7/packet.7.html)
  and [`recvmsg(2)`](https://man7.org/linux/man-pages/man2/recvmsg.2.html) for
  creation privilege, network byte order, RAW/DGRAM views, `sockaddr_ll`, bind,
  `MSG_PEEK`, and `MSG_TRUNC`;
- Linux v6.12
  [`include/uapi/linux/if_packet.h`](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/if_packet.h)
  and
  [`net/packet/af_packet.c`](https://github.com/torvalds/linux/blob/v6.12/net/packet/af_packet.c)
  for the UAPI values, create/error ordering, send/receive, and socket options;
- Linux v6.12
  [`net/socket.c`](https://github.com/torvalds/linux/blob/v6.12/net/socket.c),
  [`net/core/dev.c`](https://github.com/torvalds/linux/blob/v6.12/net/core/dev.c),
  [`drivers/net/loopback.c`](https://github.com/torvalds/linux/blob/v6.12/drivers/net/loopback.c),
  [`net/ethernet/eth.c`](https://github.com/torvalds/linux/blob/v6.12/net/ethernet/eth.c),
  and
  [`include/linux/etherdevice.h`](https://github.com/torvalds/linux/blob/v6.12/include/linux/etherdevice.h)
  for accept-hook ordering, outgoing-tap, origin suppression, cooked-header,
  loopback, and address classification behavior; and
- the official Linux
  [`Packet MMAP` documentation](https://docs.kernel.org/networking/packet_mmap.html)
  to define the ring, TPACKET, mmap, and fanout boundary that this RFC does not
  claim; and
- the official Linux
  [`NAPI` documentation](https://docs.kernel.org/networking/napi.html) for the
  generic precedent that one scheduled owner processes a fixed budget and is
  serviced again when work remains. TheKernel borrows that bounded ownership
  rule, not Linux's softirq, IRQ-masking, queue-pair, or userspace busy-poll API.

The portable oracle and bounded receipt generator are committed as
`tests/guest/tools/packet-socket-smoke.c` and
`scripts/ci/packet-host-differential.sh`. Every accepted run compiles a helper
snapshot materialized from the starting commit tree, records the starting HEAD,
tree, helper and script hashes, compiler, host kernel, namespace mode, and two
independent logs, then revalidates HEAD, tree, cleanliness, and both live input
hashes before writing PASS. The oracle program was written locally; no Linux
selftest or GPL implementation text was copied. TheKernel reimplements
observable contracts and public UAPI values in Rust.

## Decision

### 1. Keep three authority layers

| Layer | Owner | Authority |
| --- | --- | --- |
| Layer 1 | `axnet-ng` | Per-network-stack packet broker, link-device capabilities, ingress/egress taps, immutable shared frame storage, bounded endpoint queues, capture-time selector/filter epochs, source suppression, drop accounting, and the readiness source. It contains no Linux syscall, capability, fd, `sockaddr_ll`, byte-order, or errno policy. |
| Layer 2 | `thekernel-linux-packet` | Dependency-free `no_std`, unsafe-free Linux value and transition rules: RAW/DGRAM type, protocol and address normalization, bind generation/prepare/publish, outgoing policy, ordinary receive decisions, option classification, and typed destructive statistics mapping. It receives copied values and caller-owned facts; it owns no packet buffer, live counter, task, namespace, device registry, waiter, or user pointer. |
| Layer 3 | TheKernel | Usercopy and raw layout validation, socket security hooks, `CAP_NET_RAW` in the user namespace governing the target network namespace, namespace/OFD/FD lifetime, device lookup, lower-selector publication, blocking and signal integration, errno conversion, send/receive copyout, and final descriptor publication. |

No layer may create a second live queue, statistics owner, or readiness source.
In particular, Layer 2 describes queue disposition but Layer 1 alone removes
records and resets counters.

### 2. Publish creation and bind state transactionally

Creation follows this order:

1. parse the generic socket type and creation flags;
2. validate the address-family range and run the socket-create security hook;
3. for `AF_PACKET`, check `CAP_NET_RAW` in the user namespace governing the
   captured network namespace;
4. validate the family-specific `SOCK_RAW` or `SOCK_DGRAM` type;
5. allocate the Layer 2 state, Layer 1 endpoint, and unpublished OFD;
6. run the post-create security hook; and
7. publish the fd.

Failure before step 7 publishes no fd. The capability check intentionally
precedes AF_PACKET-specific in-range type rejection, while an out-of-range
generic type fails before entering the family. The namespace is retained by
the OFD; capability is not silently re-evaluated in a different namespace on
each data operation.

The host oracle fixes the corresponding error precedence:

| Creation case | Result |
| --- | --- |
| generic out-of-range type `0x7f`, with or without capability | `EINVAL` |
| `SOCK_RAW` or `SOCK_DGRAM` without `CAP_NET_RAW` | `EPERM` |
| in-range `SOCK_STREAM` without `CAP_NET_RAW` | `EPERM` |
| in-range `SOCK_STREAM` with `CAP_NET_RAW` | `ESOCKTNOSUPPORT` |

`socketpair(AF_PACKET, ...)` preserves the same generic-type and capability
precedence, but it must not jump directly to the final unsupported error. For
RAW and DGRAM, Layer 3 runs two complete unpublished creation leaves in Linux
order:

```text
create -> capability/type -> private OFD -> post-create
create -> capability/type -> private OFD -> post-create
pair -> EOPNOTSUPP
```

The pair hook observes two distinct OFD identities in the same retained
network namespace. Any create, post-create, or pair denial drops both private
descriptions and their lower endpoint registrations; no fd is published. The
test probe repeats a pair denial beyond the 64-endpoint broker limit to prove
that rollback refunds registrations rather than merely hiding descriptors.
Linux's generic socketpair path reserves descriptor numbers and writes them to
the output array before the backend pair operation. TheKernel's existing
socketpair publication path does not yet reproduce that failed-call output
side effect; this baseline deliberately leaves the caller array untouched and
records the difference as a nonclaim below.

The socket protocol and `sockaddr_ll.sll_protocol` are network-order UAPI
fields and become one canonical host-order selector: disabled zero,
`ETH_P_ALL`, or one exact protocol. Creation protocol keeps Linux's low-16-bit
cast behavior. Bind consumes only family, protocol, and interface index;
ignored `sockaddr_ll` fields cannot make bind stricter. Interface zero is a
wildcard, and bind protocol zero retains the current live protocol.

Layer 2 bind is a non-wrapping prepare/publish transaction. A changed plan
contains the complete expected binding and the next nonzero generation; a
no-op does not advance it. The adapter holds the OFD publication mutex while
it validates an exact device, installs the lower selector, and publishes the
prepared state. Thus lower failure leaves the Linux state unchanged and no
concurrent adapter writer can make the final plan stale. Future adapters that
prepare outside this mutex must roll back their lower lease when publication
reports `StaleBindPlan`; they must not retry without a bound.

This binding generation is distinct from Layer 1 capture sequence numbers and
the broker's subscription epoch. Each endpoint retains at most eight selector
epochs and eight filter epochs so an already staged frame is classified by the
state live at capture time. Sequence allocation and the matching reservation or
accounted-drop publication are one capture-mutex transition. Quiescence also
remains false while a drainer owns an already-dequeued delivery. These
linearization rules prevent epoch history from collapsing around a frame which
still needs it. A selector/filter setter never drains packet work synchronously;
it performs only its bounded transition. Genuine quiescence collapses history,
while excessive concurrent rebind/filter churn returns `ENOBUFS` rather than
growing memory or reclassifying an old frame.

The final endpoint unregister advances the non-wrapping subscription epoch and
clears already published capture/drop backlog before a new endpoint can enter
the registry. A reservation records its subscription epoch. If allocation
finishes after the old endpoint era ended, publication observes the mismatch,
refunds its frame charge, and cannot deliver that frame to a later subscriber.
Subscription-epoch exhaustion closes future admission instead of wrapping an
old reservation into a live generation.

Layer 1 returns typed mechanism failures rather than Linux errnos. Layer 3
maps allocation to `ENOMEM`, sequence exhaustion to `EOVERFLOW`, bounded
capacity exhaustion to `ENOBUFS`, and invalid copied input to `EINVAL`.
Broker teardown or failure of the broker's sole deferred-work owner enters an
explicit, permanent Layer 1 terminal state. The transition clears staged
packet/drop work, invalidates every in-flight reservation, closes endpoint
readiness, reports `HANGUP`, and rejects future subscriptions as `Detached`.
Already queued endpoint records may still drain; an empty receive returns
`BadState`, while ordinary network capture becomes a fast no-op. A normal
Layer 3 packet socket retains its network namespace, which retains the broker,
so this boundary is ordinarily not reached through the current Linux ABI. It
remains defined for independent mechanism consumers and teardown/failure races
rather than being treated as unreachable. Layer 3 does not expose `Detached`
as a new Linux ABI category.

### 3. Preserve the ordinary packet view

- RAW receive exposes the complete link frame; DGRAM receive starts after the
  advertised link header.
- RAW send preserves the complete caller-supplied frame. DGRAM send asks the
  selected device to construct the link header from protocol and destination.
- Protocol zero receives nothing. Exact protocols match ingress only.
  `ETH_P_ALL` also receives the outgoing tap unless
  `PACKET_IGNORE_OUTGOING` is set.
- An injecting endpoint is excluded from its own outgoing clone. A loopback
  ingress copy has independent ownership and may be delivered back to it.
- `getsockname` reports Linux's variable true length: 12 bytes while no exact
  device is selected and 12 plus the selected device address length otherwise
  (18 bytes for the current six-byte loopback/Ethernet devices). Ordinary
  `recvmsg` reports the complete 20-byte current-device `sockaddr_ll`, including
  one-based interface index, network-order protocol, ARPHRD type, packet
  classification, and a canonical zero-filled inline address tail.

An explicit send address is authoritative even when its protocol or interface
field is zero; those fields never inherit the bind. Linux permits a declared
`sll_halen` greater than the eight inline bytes when the supplied address
buffer extends through the declared length. The adapter validates that full
declaration while the current devices consume their exact six-byte destination
from the copied prefix. It does not claim support for a device whose native
address itself exceeds eight bytes.

RAW frame bytes and send protocol are separate facts. The outgoing tap uses
the explicit/bound send protocol without rewriting the caller's Ethernet
header. Ingress is classified again from the received frame: EtherType headers
at least `0x0600` are preserved; length-form frames become `ETH_P_802_3` when
the payload starts with `ff ff`, and `ETH_P_802_2` otherwise. Loopback ingress
therefore cannot accidentally inherit the outgoing request protocol.

The host oracle additionally fixes the loopback baseline: ARPHRD_LOOPBACK is
772, the ordinary address is six zero bytes, RAW has a 14-byte pseudo-Ethernet
header, and DGRAM removes exactly that header. Nonzero pseudo-MAC bytes,
trailing bytes beyond an IPv4 length, and an unknown EtherType survive RAW
injection; DGRAM destination bytes are used to construct the header.

### 4. Bound every retained resource

All limits are per network stack or endpoint, not boot-global hidden policy.

| Resource | Bound |
| --- | ---: |
| live endpoints per network stack | 64 |
| staged capture records | 128 |
| staged drop-accounting records | 128 |
| packet/drop events delivered by one drain invocation | 32 |
| selector epochs per endpoint | 8 |
| filter epochs per endpoint | 8 |
| queued records per endpoint | 256 |
| default logical endpoint bytes | 256 KiB |
| maximum lower endpoint byte budget | 4 MiB |
| accounted shared payload/object bytes per broker | 16 MiB |
| one complete retained frame | 64 KiB |

One allocated immutable frame is shared across matching endpoints. The broker
charges retained vector capacity plus the in-object frame record and releases
that charge with the final owner. This is an accounting contract, not a claim
to know allocator metadata or the `Arc` control-block size. Pre-reserved queue
backing is bounded separately by the count limits above. Each endpoint charges
the complete frame plus record metadata against its logical budget,
intentionally bounding fanout amplification even when frame bytes are shared.

Device code only stages a bounded capture while the network-service mutex is
held. Selector matching, filter execution, endpoint admission, statistics,
and wakeups occur after that mutex is released. Capture allocation or queue
pressure records a bounded drop and cannot reject the ordinary IP path.
Overflow of the bounded drop ledger increments one diagnostic counter. The
drainer alternates pending frames and accounted drops whenever both classes
exist, preventing class starvation without claiming a cross-class global FIFO.
One invocation consumes at most 32 packet/drop events and returns an explicit
`Continuation` when work remains. Each bare-metal `NetStack` owns one sleeping
deferred drainer. A producer performs its first bounded drain only after
releasing the service mutex; `Continuation` schedules that owner, which yields
between fixed-credit invocations and uses check-arm-check before sleeping.
Socket `poll`, readiness registration, and receive never carry broker work.
There is no hidden drain-until-empty loop in a syscall or readiness path.

If either deferred-owner wait fails, the worker does not silently exit or
retry without a bound. It performs the single fail-closed terminal transition
described above, using registry-then-capture lock order and a fixed endpoint
snapshot, then exits. An allocation already represented by a reservation may
finish, but its terminal recheck can only refund the charge; it cannot publish
new backlog or join a later subscription era.

Single-drainer handoff closes the owner-release race. After its final empty pop
or credit exhaustion, the owner releases `draining` and rechecks staging. A
producer that staged work and coalesced before that release is observed by the
recheck and produces `Continuation`; a producer arriving after release can
become the next drainer. The deferred owner is notified only for broker work,
not through endpoint readiness. Therefore bounded draining may defer work, but
cannot strand it solely at the final-empty-pop/owner-release boundary.

These rules do not make capture lockless or allocation-free. A capture may
allocate one frame, and broker/endpoint mutexes remain. The claim is bounded
retention, no global packet hot lock, and a fanout drainer which never owns the
broader network-service mutex.

### 5. Give readiness and completion one owner

Layer 1 reports `READABLE` exactly when the endpoint queue is nonempty and
`HANGUP` after broker detach. Readable/HUP registration uses check-arm-check;
an empty-to-nonempty transition or detach wakes the same bounded source. A
source-close race is reclassified as the already published HUP terminal state,
not exposed as an internal registration error. Broker continuation never wakes
this source. Ordinary dequeue removes the record and refunds its logical charge
before usercopy; `MSG_PEEK` clones the queue head without removing it. Layer 3
combines OFD `O_NONBLOCK` and per-call `MSG_DONTWAIT` with the common readiness
wait path.

For a short receive buffer, both paths copy the available prefix and report
output `MSG_TRUNC`. Without input `MSG_TRUNC`, success returns copied length;
with it, success returns the complete socket-visible record length. Queue
disposition is independent of truncation:

- ordinary receive claims one record before copyout, so `EFAULT` consumes it;
- `MSG_PEEK` never claims the record, so the same `EFAULT` retains it.

There is no OFD-wide receive lock spanning usercopy. Concurrent ordinary
readers claim distinct records; a peek holds only its cloned frame owner.

Write readiness is currently optimistic: it means a synchronous submit may be
attempted, not that device admission cannot race or fail. After copying the
bounded destination and running the send security hook, Layer 3 prepares a
side-effect-free device/protocol/layout plan before reading the payload. This
preserves Linux's invalid-device and size-error precedence over a payload
fault. It then copies the complete ordinary send into kernel-owned memory and
submits that prepared plan once. The device owns an accepted transmit; the
syscall returns the submitted visible length once. A failed usercopy performs
no submit. No deferred ring status, second completion, fence, or completion
credit exists in this baseline.

### 6. Keep statistics single-owned and destructive

The endpoint is the only live counter owner. It saturatingly counts accepted
packets and drops; filter rejection/error remain separate diagnostics. One
`PACKET_STATISTICS` query takes and resets exactly one endpoint snapshot,
Layer 2 maps it without recounting, and Layer 3 narrows/copies the UAPI result.
No second reset or invented queue-versus-staging attribution is permitted.

`accept` is unsupported, but not before policy. Layer 3 constructs a typed
bare accepted-socket security projection with backend and namespace but no
fabricated OFD identity, runs `security_socket_accept`, and only then returns
`EOPNOTSUPP`. It allocates no endpoint that could never be published.

The same socket-policy rule applies to the ordinary file-I/O aliases. Nonzero
`read`/`readv` and every `write`/`writev` dispatch receive/send policy against
the already retained OFD before queue claim, payload copy, or submission; they
never repeat a numeric-fd lookup. Linux's deliberate zero-length distinction is
preserved: `read`/`readv` return zero without entering the socket receive hook
or consuming a record, while `recv` with a zero-length payload can consume one
datagram/packet and a zero-length write still reaches send policy.

The ordinary baseline also exposes the read-only generic introspection needed
to identify the OFD (`SO_TYPE`, `SO_ERROR`, `SO_DOMAIN`, and `SO_PROTOCOL`) with
Linux's short-`optlen` copy behavior. It does not route packet OFDs through a
different network backend merely to obtain these values.

## Explicit nonclaims

This baseline does not implement or claim:

- `PACKET_RX_RING`, `PACKET_TX_RING`, TPACKET v1/v2/v3, packet mmap, zero-copy,
  block retirement, or ring ownership;
- `PACKET_FANOUT`, rollover, multicast/promiscuous memberships, auxiliary data,
  timestamps, qdisc bypass, virtual-net headers, or AF_XDP;
- a classic/eBPF socket-filter attachment ABI, JIT, or a complete `SOL_PACKET`
  option surface;
- `SO_RCVBUF`/`SO_SNDBUF` tuning or a complete `SOL_SOCKET` surface; a later
  receive-budget ABI must change the real endpoint budget transactionally
  rather than acknowledge a value which has no effect;
- complete Linux blocking-send/backpressure parity before Layer 1 supplies a
  device completion-credit and writable-wake contract; a racing admission
  failure may currently return `WouldBlock` to either socket mode;
- Linux's failed `socketpair` output-array prewrite of reserved descriptor
  numbers; the unsupported AF_PACKET path publishes no fd and writes no output;
- hotplug-safe device generations, revocation, or a long-lived device lease;
- lockless, wait-free, or zero-allocation capture;
- high-performance packet capture, line-rate throughput, or a performance
  result inferred from loopback; or
- Linux-internal driver ABI compatibility.

Known packet options outside the implemented ordinary-queue subset fail
honestly; recognizing a UAPI number is not support. The official Linux docs
explicitly motivate PACKET_MMAP because ordinary copied AF_PACKET has high
per-packet syscall and copy cost. A future ring must add its own page/mapping
lease, bounded memory charge, generation/revocation, producer-consumer
ownership, and teardown proof rather than weakening this queue contract.

## Acceptance evidence

The source-level contract is exercised in:

- `crates/axnet-ng/src/packet.rs` and
  `crates/axnet-ng/src/device/{ethernet,loopback}.rs` unit tests;
- `../thekernel-linux-abi/crates/packet/tests/public_contract.rs` and the
  package's internal tests; and
- `kernel/src/file/packet_socket.rs` plus syscall adapter tests.

The guest helper must emit these stage markers exactly:

- `THEKERNEL_PACKET_UDP_PRECONDITION_OK`;
- `THEKERNEL_PACKET_CREATE_OK`;
- `THEKERNEL_PACKET_RECEIVE_OK`;
- `THEKERNEL_PACKET_FAULT_OWNERSHIP_OK`;
- `THEKERNEL_PACKET_SEND_FLAGS_OK`;
- `THEKERNEL_PACKET_SEND_OK`;
- `THEKERNEL_PACKET_OPTIONS_OK`; and
- final `THEKERNEL_PACKET_OK`.

Any failed stage emits `THEKERNEL_PACKET_FAIL <stage>`. The composed system
gate emits `THEKERNEL_SYSTEM_TEST_PACKET_OK` only after the helper's final
marker. Acceptance requires exact-revision host/package tests and RISC-V 64
and LoongArch64 build/boot receipts; marker presence without exact source and
artifact hashes is not release evidence.

The PR gate's finalizer writes one source-set, artifact manifest, checksum
manifest, and PASS/FAIL receipt. A source-mode PASS binds the unchanged clean
TheKernel, `thekernel-ax`, and `thekernel-linux-abi` commits and trees to the
release and shell kernels, both rootfs images, boot QEMU receipts/consoles, and
both semantic-system consoles. It also requires the packet marker contract in
each semantic console, rehashes every present artifact, and performs a second
three-repository HEAD/tree/clean check immediately before deciding PASS. An
early build or boot failure still leaves a FAIL receipt with present and missing
artifacts distinguished; it cannot be promoted to PASS by the finalizer.
`--skip-build` is explicitly reuse mode and never claims release evidence for
the current source set.

The broker evidence harness runs five fixed-shape cases twice and records schema
2 observations, the starting identities of all three source repositories, host
identity, artifact checksums, queue/drop invariants, and a final
`charged_shared_bytes=0` teardown condition. It writes PASS only after all three
HEAD/tree pairs and clean worktrees are revalidated. It defines no portable
throughput or latency threshold. These observations can detect accounting leaks
and large regressions on a comparable host, but cannot establish line-rate,
lock-free, allocator-total-memory, or cross-machine performance claims.

The repository workflow runs the dual-architecture job for PRs, `main` pushes,
and manual dispatches when `THEKERNEL_QEMU_CI=1`, but it is not currently a
universal required check. It depends on a configured `thekernel-qemu`
self-hosted runner, published exact sibling commits, and branch-protection rules
which live outside this repository. Removing the workflow condition before
those external requirements exist would produce an indefinitely queued or
deterministically failing check, not stronger evidence. Local and self-hosted
receipts therefore remain bounded evidence rather than a claim that every
public PR or `main` update has passed the dual-architecture gate.
