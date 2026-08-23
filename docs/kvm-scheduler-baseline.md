# KVM scheduler baseline

`tools.kvm_scheduler_baseline` is the first small, repeatable scheduler
comparison lane. It runs the same statically-linked helper in a TheKernel
guest and, when explicitly supplied, a Linux guest. The lane fixes the
virtual topology at x86_64 `q35` + OVMF + KVM + `-cpu host`, retains every raw
latency observation, and computes nearest-rank P50, P99, and P99.9 on the
host.

The guest helper covers:

- private futex ping-pong on one guest CPU and across two guest CPUs;
- pipe ping-pong on one guest CPU and across two guest CPUs; and
- a deterministic CPU worker on one or two guest CPUs.

Each invocation has an explicit warmup count. `--repeat` starts a fresh guest
for every repetition and workload/placement pair. Results are written below
`--output` as `raw-samples.tsv`, `summary.tsv`, `summary.json`, per-run serial
logs, and a `manifest.json` describing the exact topology. Raw samples are
not inferred from the guest's summary line.

## TheKernel lane

Build the x86_64 shell image and rootfs (the rootfs installs
`/opt/thekernel-tests/bin/thekernel-scheduler-baseline`) and run:

```bash
./tools/thekernel.py build --profile shell
./tools/thekernel.py rootfs
PYTHONPATH=. python3 -m tools.kvm_scheduler_baseline run \
  --target thekernel \
  --kernel .state/out/x86_64/q35-uefi/shell/smp4-mem1g/kernel-x86_64 \
  --rootfs .state/out/rootfs/x86/rootfs-x86.img \
  --esp .state/out/x86_64/q35-uefi/shell/smp4-mem1g/kernel-x86_64.esp \
  --output .state/kvm-scheduler/thekernel \
  --cpus 4 --warmup 200 --iterations 2000 --repeat 5 \
  --vcpu-cpus 4,5 --io-cpus 6
```

`--vcpu-cpus` pins vCPU threads round-robin to the listed host CPUs;
`--io-cpus` pins the dedicated virtio-blk iothread. Both are optional. The
pinning wrapper records observed thread IDs and whether each requested class
was actually seen in `thread-pinning.json`; an unobserved thread is reported
as such, never as a successful pin.

The host must expose `/dev/kvm`, and every requested host CPU must be inside
the runner's current affinity mask. If KVM is unavailable the command exits
78 (`UNSUPPORTED`) without producing benchmark results.

## Linux adapter

The Linux adapter is symmetric but deliberately does not discover or download
an image. Supply a Linux kernel, rootfs, and ESP yourself:

```bash
PYTHONPATH=. python3 -m tools.kvm_scheduler_baseline run \
  --target linux \
  --linux-kernel /path/to/linux-kernel \
  --linux-rootfs /path/to/linux-rootfs.img \
  --linux-esp /path/to/linux-esp.img \
  --output .state/kvm-scheduler/linux \
  --guest-program /opt/thekernel-tests/bin/thekernel-scheduler-baseline \
  --ready-marker LINUX_SHELL_READY
```

The supplied Linux image must expose an interactive shell after the chosen
ready marker and contain the same helper path (or use `--guest-program` to
select another compatible binary). Missing Linux artifacts are an explicit
unavailable condition; the TheKernel lane remains runnable independently.

To recompute statistics without launching guests:

```bash
PYTHONPATH=. python3 -m tools.kvm_scheduler_baseline stats \
  --input .state/kvm-scheduler/thekernel/raw-samples.tsv \
  --output /tmp/kvm-summary.json \
  --summary-tsv /tmp/kvm-summary.tsv
```

These measurements compare this specified workload and topology only. They
are not a claim about bare-metal tail latency or overall performance.
