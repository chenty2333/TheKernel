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
