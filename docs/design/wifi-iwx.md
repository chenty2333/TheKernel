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
The firmware-event classifier covers the remaining UAPSD, thermal, MCC,
session-protection, channel-switch, statistics, RLC/TLC, and ignorable command
branches so the platform dispatcher can apply side effects without losing ACKs.
Critical-temperature handling, matching time-event completion, UAPSD disable,
session-protection completion, and station-only channel-switch recovery now
have explicit state-policy updates.
