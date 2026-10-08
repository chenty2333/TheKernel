# Wi-Fi net80211 protocol layer

`tk-net80211` owns the OpenBSD net80211 station-mode protocol behavior and
exports frames and capabilities to the wireless netdev/nl80211 adapter. Current
coverage includes Ethernet/802.11 data encapsulation and decapsulation, WPA/RSN
suite parsing, OpenBSD legacy RSSI-threshold rate adaptation, and OpenBSD HT
goodput/MCS rate adaptation. `ra.rs` keeps the ordered 20/40 MHz ratesets,
fixed-point goodput/loss statistics, probe selection and peer capability masks;
the caller supplies the current peer/channel capabilities rather than importing
OpenBSD's `ieee80211com` and node framework. The driver passes hardware-decrypt
and channel/RSSI metadata into this layer; authentication/association state and
EAPOL are still to be connected.

The Rust Fuchsia `ieee80211`, `common`, `rsn`, `eapol`, `mlme` and `sme` crates
are the intended reusable BSD-3-Clause source for frame/RSN/EAPOL/SME pieces
where their abstractions fit. Linux cfg80211/mac80211 are not imported. The
4-way handshake uses unmodified `wpa_supplicant` via nl80211; no in-kernel PAE
is planned. The netdevice and nl80211 adapter remain separate kernel-facing
layers.

The regulatory adapter translates OpenBSD's country/domain name and band map
lookups into static Rust tables used by the later `REG_GET` nl80211 response.
It deliberately retains the upstream domain flags and 2/5 GHz decision range;
actual channel admission remains driven by the selected wireless-regdb payload.
The channel helper module translates OpenBSD MHz/channel conversions and its
channel-array index/ANY sentinel mapping, using safe indexes in place of
`ieee80211_channel *` pointers.
Legacy rate handling now includes the source 11a/11b/11g rate sets, per-mode
basic-rate marking, negotiated minimum/maximum basic rates, and the inverse
PLCP SIGNAL mappings for CCK/OFDM. The PHY mode is explicit at the crate API.
Channel state also exposes the OpenBSD mode-capability scan, current-mode
fallback, active-channel selection, first-usable IBSS channel choice and the
AMPDU/QoS capability gate. The adapter receives an active mask and chosen
channel so the later wireless interface layer can issue scan-reset/ERP updates.
The layer also translates rate lookup and the background-scan mode cycle (AUTO
when a fixed media mode or all-band scanning applies; otherwise skip HT/VHT/HE
superset channel sets).
Node/ESS helpers now validate candidate SSID/security, preserve association
failure reasons, apply OpenBSD's RSSI and crypto scoring, select saved-network
scan results (including fixed BSSID/SSID versus auto-join), and filter HT/VHT
40/80 MHz channel centers against the upstream regulatory operating-class sets.
Bounded HT/VHT/HE capability and operation IE parsing now keeps MCS/NSS maps,
channel-width information and peer flags in explicit node capability structs;
clear operations reset the source fields, reserved HT MCS 77-79 are removed,
and HT/VHT width selectors gate on both advertised and local channel support.
VHT operation parsing follows the source ext-NSS bandwidth table to resolve the
secondary 80-MHz center; it widens a 80-MHz report to 160 only when local
channel/capability data allows, and clamps unsupported 160-MHz operation back
to the primary-containing 80-MHz block.
