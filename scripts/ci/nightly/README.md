# Nightly guest adapters

The executable nightly entry points are direct, zero-argument leaf scripts:

```bash
scripts/ci/nightly/fs-powercut.sh
scripts/ci/nightly/nonloopback-network.sh
scripts/ci/nightly/smp-tlb-shootdown.sh
scripts/ci/nightly/mm-performance.sh
```

`lib.sh`, `mm-performance-boundary.sh`, and `network-peer.py` support those
leaves and are not separate entry points.

Every leaf exits with one of these outcomes:

- `0`: its requested checks completed successfully.
- `1`: a requested check failed, its inputs were invalid, or an operational
  error occurred after the adapter started.
- `78`: required host capability or tooling is unavailable.

`78` is unavailable, not a pass. Each script is independently runnable.

## Adapter coverage

- `fs-powercut.sh` writes to an ext4 image, terminates the first guest after a
  durable-write phase, boots a recovery phase, and checks the resulting image
  with `e2fsck`.
- `nonloopback-network.sh` verifies a nonce-authenticated TCP exchange between
  the guest VirtIO NIC and a one-shot host peer over QEMU user networking.
- `smp-tlb-shootdown.sh` exercises the requested x86_64 CPU matrix (default
  `4 8`) through wait-boundary and TLB-shootdown guest programs.
- `mm-performance.sh` runs its requested x86_64 CPU matrix (default `4 8`) on
  a homogeneous host CPU set and records the MM workload measurements.

`THEKERNEL_NIGHTLY_ARCHES` accepts only `x86_64` (or the `x86` alias). Guest
logs and temporary run outputs are placed under
`${THEKERNEL_NIGHTLY_LOG_DIR:-.state/ci/nightly/adapter}`.

## Performance boundary

`mm-performance.sh` is the explicit performance consumer. It requests a
product-CLI performance receipt for each measured run so the measurement can
be associated with its run inputs. The filesystem, network, and SMP semantic
adapters do not request performance receipts.

Use `THEKERNEL_MM_PERF_CPUS`, `THEKERNEL_MM_PERF_ITERATIONS`,
`THEKERNEL_MM_PERF_VMAS`, `THEKERNEL_MM_PERF_PIN_ITERATIONS`, and
`THEKERNEL_MM_PERF_HOST_CPUS` to select a bounded MM experiment. A successful
run is evidence only for the selected QEMU workload and host allocation; it is
not a hardware latency claim.
