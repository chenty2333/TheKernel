# Limit async submit by descriptor budget

Async block submission will use descriptor-aware admission instead of a fixed request-count cap. This keeps RISC-V from being underfed when indirect descriptors make SG requests cheap, while preventing LoongArch64 from exhausting direct VirtIO descriptors when conservative DMA and descriptor settings are required.
