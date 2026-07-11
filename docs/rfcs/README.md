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
