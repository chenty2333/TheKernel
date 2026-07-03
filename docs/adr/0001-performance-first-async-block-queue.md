# Performance-first async block queue

The next I/O milestone will optimize for OSComp-relevant iozone throughput, but the implementation will be a maintainable async/batch block queue rather than a one-off benchmark shortcut or a full Linux-style block layer rewrite. This keeps the work focused on measurable queue depth, lower busy polling, and lower notification overhead while preserving the pin/unpin and filesystem correctness guarantees established by the previous I/O boost work.
