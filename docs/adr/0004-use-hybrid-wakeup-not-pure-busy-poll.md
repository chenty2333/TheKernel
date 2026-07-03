# Use hybrid wakeup instead of pure busy polling

The async block queue will expose a real completion and wakeup contract, while allowing a short spin phase before sleeping to keep QEMU small-I/O latency low. Pure busy polling is not an acceptable final state because the previous I/O work already exposes queue wait polls as a measurable CPU cost, and the next milestone must reduce that cost rather than move it into another loop.
