# Nightly system adapters

These adapters exercise a real guest path and return one of three outcomes:

- `0`: the named semantic checks ran and passed;
- `1`: infrastructure launched the check, but the check failed;
- `78`: required infrastructure or an explicit capability was unavailable.

Exit `78` is not a pass. `nightly-gate.sh` records it as `unsupported` and
returns `78` after all enabled categories have been considered.

The repository-owned adapters cover:

- `pressure.sh`: bounded concurrent task and root-filesystem pressure while a
  multi-threaded scheduler workload runs, on each selected architecture;
- `oom-failpoint.sh`: deterministic anonymous-mapping admission failure under
  strict overcommit policy, policy restoration, and a successful recovery
  mapping on each selected architecture;
- `fs-powercut.sh`: a writable ext4 image, a durable write, abrupt QEMU
  `SIGKILL` after an exact guest marker, a second recovery boot, clean unmount,
  and host `e2fsck` verification;
- `nonloopback-network.sh`: a nonce-authenticated TCP exchange from the guest
  VirtIO NIC through QEMU user networking to a one-shot host peer.

These gates deliberately do not overclaim. The OOM adapter does not substitute
for a future kernel-allocator failpoint framework or OOM-victim policy test.
The network adapter proves a real non-loopback NIC path but does not substitute
for TAP, packet loss, multi-peer, or physical-NIC testing. The power-cut test
models sudden VM process loss after explicit durable writes; storage devices
with volatile caches still require hardware-appropriate flush/fence testing.

`THEKERNEL_NIGHTLY_ARCHES` accepts `rv`, `la`, or `both` (the default). Missing
QEMU binaries, official images, cross compilers, or filesystem tools cause
exit `78`. A runner may provide a category-specific `*_COMMAND` override for
hardware-only testing; its exit `78` retains the same unsupported meaning.
