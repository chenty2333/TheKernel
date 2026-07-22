# TheKernel RFCs

This directory records architectural decisions that change a subsystem
contract, a public crate boundary, or a default-on mechanism.

RFCs are evidence records, not substitutes for tests. Each accepted RFC must
identify:

- the real subsystem problem and its owning layer;
- the Linux-visible semantics, if any;
- the industrial implementations and research results reviewed;
- the design adopted for TheKernel and the alternatives rejected;
- allocation, accounting, locking, cancellation, and rollback rules;
- architecture-specific constraints;
- semantic, fault-injection, stress, and performance gates;
- the intended crate or repository boundary;
- upstream source versions, commits, papers, and licenses.

Statuses are `draft`, `accepted`, `implemented`, `superseded`, and `rejected`.
An RFC reaches `implemented` only after its required gates pass. A mechanism
must not be described as supported merely because its syscall number or public
type exists.

## Index

- [RFC 0000: TheKernel Modernization Program](0000-modernization-program.md)
  (`accepted`)
- [RFC 0001: Immutable Credentials, User-ID Mapping, and Typed Security Hooks](0001-credential-v2.md)
  (`draft`)
- [RFC 0002: Explicit Linux Path Context over a Policy-Neutral VFS Walker](0002-vfs-path-contract.md)
  (`draft`)
- [RFC 0003: Cancellable FD and Readiness Registration Contract](0003-fd-readiness-contract.md)
  (`draft`)
- [RFC 0004: Accounted VM Pins and Generation-Safe Fault Delegation](0004-mm-pin-fault-contract.md)
  (`draft`)
- [RFC 0005: Bounded io_uring Core and Kernel Adapter Contract](0005-io-uring-contract.md)
  (`draft`)
- [RFC 0006: Bounded Task-Local Seccomp Contract](0006-seccomp-contract.md)
  (`draft`)
- [RFC 0007: Bounded AF_PACKET Ordinary-Queue Contract](0007-packet-socket-contract.md)
  (`implemented ordinary-queue baseline`)
- [RFC 0008: Modern Performance, Driver, and Graphics Program](0008-performance-driver-graphics-program.md)
  (`accepted`)
