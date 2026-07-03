# Stage async depth by architecture

RISC-V will be the first default-on target for real async/batch block queue depth, while LoongArch64 will share the same block-I/O contract but may default to conservative depth or synchronous fallback until its VirtIO DMA and descriptor behavior are proven stable. This avoids hiding architecture-specific correctness risk inside the new queue contract while still preventing a permanent RISC-V-only design.
