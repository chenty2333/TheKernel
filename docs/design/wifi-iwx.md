# Intel CNVi Wi-Fi (iwx) implementation notes

## OpenBSD target identification

Reference snapshot: OpenBSD `sys/dev/pci/if_iwx.c` revision 1.230 (2026-09-19), with `if_iwxvar.h` from the same tree. The source carries an ISC notice (and a separate BSD/GPL notice for upstream Linux-derived portions); any copied code must be audited at the individual-source/section level before translation.

PCI `8086:54f0` is OpenBSD `PCI_PRODUCT_INTEL_WL_22500_16` (AX211). It is present in the PCI attach match list. The requested `8086:4070` is the subsystem vendor/device pair; it is not an exact `iwx_dev_info_table` row. `iwx_find_device_cfg()` extracts RF ID, 160-MHz capability, and core count from the subsystem device ID, then scans the device configuration table in reverse. The applicable fallback is the generic **So-F with GF** row (`MAC_TYPE_SOF`, `RF_TYPE_GF`, 160 enabled, no CDB), selecting `iwx_2ax_cfg_so_gf_a0`; OpenBSD names its aliases `iwx-so-a0-gf-a0-77` and `iwx-so-a0-gf-a0.pnvm`. For TheKernel rootfs, use Linux-firmware paths instead: `/lib/firmware/iwlwifi-so-a0-gf-a0-89.ucode` and `/lib/firmware/iwlwifi-so-a0-gf-a0.pnvm`. Linux 7.2.3's `cfg/ax210.c` pins AX210 API min=max=89 and forms the `.ucode` suffix from that API; `iwl-config.h` forms the matching `.pnvm` name. The PCI product's provisional OpenBSD firmware defaults can be overridden by the runtime config lookup; retain that behavior and confirm the hardware RF/CDB values during driver bring-up rather than binding behavior solely to subsystem `4070`.

The `iwx_2ax_cfg_so_gf_a0` config is the AX211, one-stream (2AX), So-family GF configuration. Exact config definition and firmware aliases are in OpenBSD `sys/dev/pci/if_iwxvar.h`; device IDs and matching are in `if_iwx.c` and `sys/dev/pci/pcidevs`. Linux 7.2.3 `drivers/net/wireless/intel/iwlwifi/cfg/ax210.c` and `iwl-config.h` are used only to map those aliases to current linux-firmware naming and API 89; no GPL code is copied.

## Planned boundaries

- Translate eligible OpenBSD iwx driver logic into `tk-axdriver-iwx`, mapping PCI, DMA, interrupts, firmware and network I/O to TheKernel interfaces. Keep license-prohibited Linux-derived sections out of copied code.
- Implement the iwx-facing subset of OpenBSD net80211 in `tk-net80211`; use Fuchsia Rust WLAN crates only for fitting protocol components where their BSD-3-Clause source/license can be preserved.
- Use userspace `wpa_supplicant` for the standard WPA four-way handshake through nl80211; avoid duplicating the PAE path in the kernel for this initial implementation.
- Expose the station interface through `axnet-ng`, with wireless state via nl80211/sysfs/rfkill. Do not port GPL cfg80211/mac80211; implement protocol behavior from the UAPI/spec instead.

## Firmware payload packaging

`scripts/build-iwx-firmware-payload.sh` stages only the linux-firmware API 89
So/GF ucode and matching PNVM, validates the ucode header/API and size caps, and
copies `LICENCE.iwlwifi_firmware` into `/usr/share/licenses/linux-firmware/`.
`scripts/build-rootfs.sh` invokes it when `THEKERNEL_IWX_FIRMWARE_DIR` points
to a linux-firmware checkout/package tree; firmware binaries remain external
inputs and are not committed.

The `wpa_supplicant` nl80211 command/event set and remaining full driver and
net80211 implementation are still part of the later task items.

## MAC context commands

`tk-axdriver-iwx::mac_context` now builds the legacy packed `MAC_CONTEXT_CMD`
and MLD `MAC_CONFIG_CMD`, including station timing, EDCA FIFO placement,
monitor filters, rates, protection flags, and add/remove active-state checks.
The builders emit firmware payload bytes; caller-owned command transport still
handles queue reservation, doorbell publication, and response dispatch.

Statistics clearing follows the firmware command-version table: unknown
(`99`) uses synchronous legacy `STATISTICS_CMD` with a retained response,
version 1 sends asynchronous `SYSTEM_STATISTICS_CMD` and waits for
`SYSTEM_STATISTICS_END_NOTIF`, and unknown newer versions follow the source's
no-op path.

`iwx_initiate_scan()` selects the v17 UMAC request only when firmware
advertises version 17; otherwise it uses the v14 structure, preserving the
driver's fallback policy.
Its v10/v11 general dwell setup, adaptive budgets, fragment counts, and v6/v7
channel-parameter headers now use the source helper boundaries and wire fields.

The station TLC rate command now serializes v3/v4 wire layouts separately,
including legacy basic-rate indexing, HT/VHT MCS maps, width, antenna chains,
STBC and short-guard-interval capabilities. The surrounding 802.11 peer
capability discovery remains a net80211 input.

Rate-update notifications decode legacy/HT/VHT initial-rate formats by the
firmware notification version and update peer MCS, stream count, or legacy
rate index only for the driver's station ID and rate-update event.

PHY updates now preserve the cross-band CDB remove/add ordering, update-only
path, metadata transition timing, and optional RLC API-v2 receive-chain update.
VHT width inputs use the OpenBSD values (`80 MHz = 1`, `160 MHz = 2`) and are
translated separately to firmware PHY-width values.

The iwx association layer models source-order AUTH context creation with
generation-guarded reverse cleanup, DEAUTH removal order, RUN station/MAC/power/
rate setup, and RUN_STOP flush/BA/filter teardown. Platform command adapters
must supply those ordered actions to the builders above.

Key setup uses `ADD_STA_KEY` for legacy firmware, v1/v2 IGTK commands where
applicable, and `SEC_KEY_CMD` for MLD firmware; unsupported ciphers stay on
software crypto. Deferred install bookkeeping opens the RSN port only after
the source-required pairwise/group and (for MFP) integrity-group keys succeed.

Deferred iwx state transitions now preserve RUN task cancellation, SCAN-to-SCAN
behavior, shutdown short-circuiting, AUTH/DEAUTH/RUN/RUN_STOP action order, and
task-reference ownership. Smart-FIFO state commands serialize the source's
watermarks and timeout matrices and are omitted when firmware reports offload.

Init-time BT/SOC/DQA, MCC update/response validation, temperature-threshold,
LTR-tolerant hardware setup ordering, and NIC-lock release branches are also
represented in the driver crate.

Interface initialization now captures generation validation, monitor-vs-scan
startup, and the one-second scan-state wait; shutdown drains/cancels the source
task set before device stop and software-state reset. Multicast filter requests
pass all groups for the active BSSID.

Per-TX-queue watchdog expiry, media-change restarts, and generation-guarded
ioctl/ENETRESET power and interface handling are mapped to callbacks so the
driver core can bind them to TheKernel's network and task APIs.

Firmware LMAC/UMAC error-table word layouts, family-dependent pointer floors,
SYSASSERT descriptions, and status-dump fields are parsed in source order for
diagnostics.

The RX transfer-buffer walker now applies source framing/alignment, first-MPDU
ring replacement, pre-AX210 copy-vs-transfer ownership, AX210 single-packet
handling, command-response retirement, and notification ACK suppression.
The RX_MPDU path now decodes Gen2/Gen3 descriptors, repairs the source
post-header padding/A-MSDU-bit quirks, validates checksum/decryption flags,
checks CCMP packet-number replay windows, tracks duplicate/A-MSDU subframes,
and provides BAID/TID reorder-buffer release. The controller owns those replay,
duplicate, and reorder states; the resulting 802.11 frame still needs delivery
through the not-yet-published net80211/netdev adapter.
Firmware/NVM antenna masks now produce the driver's source HT/VHT MCS and STBC
capabilities, and standard/UHB PHY_CONTEXT_CMD v3/v4 layouts have separate
builders selected by the source dispatcher.
Controller helpers now cover the non-QoS management queue's qid/TID lifecycle,
the PDU/status command wrappers, association session-protection add/remove,
and the legacy interrupt-mask restore path.
The firmware-event classifier covers the remaining UAPSD, thermal, MCC,
session-protection, channel-switch, statistics, RLC/TLC, and ignorable command
branches so the platform dispatcher can apply side effects without losing ACKs.
Critical-temperature handling, matching time-event completion, UAPSD disable,
session-protection completion, and station-only channel-switch recovery now
have explicit state-policy updates.

The MLD station/link command path now encodes the packed LINK_CONFIG_CMD and
STA_CONFIG_CMD v1/v2 variants, source EDCA/protection/rate fields, and the
ordered add/modify/configure and remove/deactivate/delete operations. The
platform still supplies the station/node state consumed by these builders.

Contiguous DMA allocation/free is adapted by the PCI layer to `UsageKind::Dma`
page ownership; Rust-owned firmware, RX, and TX DMA regions are released by
their owner types instead of the explicit OpenBSD `*_free` loops. The checked
register facade also keeps the source NIC-lock assertion for PRPH access.

The netif TX scheduler keeps management traffic eligible outside RUN, blocks
data on queue-full/flush/management-only state, preserves encapsulation and
node-release error paths, and retains the 500ms MFP leave wait.

Queued init recovery, generation checks, suspend/resume, and wakeup paths now
preserve source lock/unlock and interface-state policy.

The static PCI probe chain now uses OpenBSD's complete iwx product list and
its BZ/Wi-Fi-6E RF filter, then applies the runtime subsystem/MAC/RF table.
Matched functions are claimed, but the full hardware-to-wlan0 attach adapter is
still being completed.
Product attach profiles now retain family, integrated/LTR/XTAL defaults,
CSR-address selection, UMAC peripheral offset, and queue modulus before the
runtime device-table overrides.

The core attach allocator now creates Gen2/Gen3 context info, Gen3 peripheral
scratch/info, the 4 KiB-aligned ICT, ten TX queues, and the 512-buffer RX ring
in the upstream allocation order with automatic rollback on failure.
The PCI platform retains an `IwxController` with bounded volatile BAR0 access.
When rootfs firmware becomes available, it runs the Init ucode ALIVE/INIT
sequence by polling and servicing the source interrupt/RX rings, reads the
strap/OTP MAC and NVM_GET_INFO response, then masks device interrupts and stops
the NIC as OpenBSD's preinit path does. PCI INTx is disabled while this
synchronous polling adapter is used. Controller stop now resets RX/TX rings,
clears held NIC access, stops/resets the APM, restores RF-kill routing, and
re-prepares the card after NVM read. PCIe Link Control and Device Control 2
populate the APM L0s/LTR state used by later power policy. The normal runtime ucode/PNVM sequence,
including regular ucode, PNVM doorbell completion, and post-ALIVE setup, is
exposed in source order on the controller but is not yet called by an if-up
hook. The installed runtime interrupt worker and wlan0 publication remain
incomplete.
