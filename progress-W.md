# W progress

- Device match/config and firmware API89 mapping: complete, commits `30271f67` through `2c3513b1`; Linux-firmware rootfs bundle script remains pending (W-1 partial).
- Firmware parser / PCI-ID table review W-2..W-18: fixed and committed as `fb4dbdbb`.
- `if_iwx.c` driver foundation: context image layout and CSR/PRPH access committed (`060919d9`); monitor/debug/LTR support committed (`acc88536`); AX210 RX/TX rings, host-command TFD serialization, CSR/MSI-X interrupt-mask phases committed (`3e9d987e`), `tk-axdriver-iwx` tests 38/38 pass.
- RX metadata and command lifetimes: signal/noise processing committed (`b1cf7ef7`); bounded command response storage, ACK completion and generation reset committed (`5bd1bb53`); source-order legacy/HT rate selection committed (`dfe2819f`); Gen2/Gen3 frame TX command/TFD serialization now implemented.
- Firmware path: ordered NIC/start interrupt phase committed (`1720012e`); PNVM contiguous/fragmented DMA staging and Gen3 scratch link now implemented.
- APM/NIC readiness and interrupt topology: 10-tries preparation, hardware-ready handshake, software reset, APM init/stop, AX power gating, and single-vector MSI-X routing/snapshot implemented; see `bf466bea` and current segment.
- Firmware start path: init command payloads (`fec20b0d`), APM/persistence/MSI-X setup (`bf466bea`, `3a04a94b`, `ae19f8f1`), ALIVE/Init MVM sequencing (`f351c60e`), and NIC config/RX startup functions just added.
- NIC queues: generation-specific scheduler command bytes, command queue selection, ring-size code and response validation implemented.
- Remaining per coordinator W-19: continue the driver body in `if_iwx.c` function order and integrate PCI probe before doing further net80211 work.
