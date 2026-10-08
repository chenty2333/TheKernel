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
