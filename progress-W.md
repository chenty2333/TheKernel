# W progress

- Device match/config and firmware API89 mapping: complete, commits `30271f67` through `2c3513b1`; Linux-firmware rootfs bundle script remains pending (W-1 partial).
- Firmware parser / PCI-ID table review W-2..W-18: fixed and committed as `fb4dbdbb`.
- `if_iwx.c` driver foundation: context image layout and CSR/PRPH access committed (`060919d9`); monitor/debug/LTR support committed (`acc88536`); AX210 RX/TX rings, host-command TFD serialization, CSR/MSI-X interrupt-mask phases committed (`3e9d987e`), `tk-axdriver-iwx` tests 38/38 pass.
- RX metadata and command lifetimes: signal/noise processing committed (`b1cf7ef7`); bounded command response storage, ACK completion and generation reset committed (`5bd1bb53`); source-order legacy/HT rate selection committed next.
- Remaining per coordinator W-19: continue the driver body in `if_iwx.c` function order and integrate PCI probe before doing further net80211 work.
