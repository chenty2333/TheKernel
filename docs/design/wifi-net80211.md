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
Peer rate setup bounds Supported/Extended Supported Rates to the 15-entry
upstream set, records truncated extensions, infers ERP/11g from 2.4-GHz OFDM
rates, and delegates mutual-rate selection to the protocol layer. Legacy ABG
mode resolves from a fixed local choice or the peer's band/ERP state.
Inline node predicates now determine HT/VHT/HE support from both capability
presence and usable first-stream MCS masks, and gate SGI and wide-channel
operation on both peer capabilities and the accepted operation IE.
RSN policy selection now intersects peer/local suites and applies OpenBSD's
RSN-over-WPA, SHA-256 AKM, CCMP-over-TKIP preferences, enterprise PMKID reuse
and MFP intersection. Link-rate/RSSI accessors preserve fixed-rate and band-
specific roaming thresholds.
Scanned-BSS admission now reports OpenBSD association-failure bits for channel,
ESS/IBSS mode, privacy, mandatory rate, SSID/BSSID, CSA and RSN/MFP mismatch.
Its background-scan path preserves the existing candidate-failure state when
an unrelated SSID is seen.
Candidate selection matches the scan table traversal, skips/ages prior
association failures, tracks the current BSS, and uses the source all-band
policy that prefers 5 GHz above its roaming threshold before strongest-RSSI
fallback.
Scan primitives begin active scans outside hostap mode and select channels in
source array order with wraparound, leaving passive-only channels pending until
passive scan and clearing only the channel actually submitted to scan.
Input header parsing now accounts for address-4, QoS and HTC fields, rejects
unsupported control-frame header-length queries, validates truncation, and
extracts QoS control in little-endian order.
EDCA/WMM parsing retains the four AC record order and low-nibble update-count
suppression; changed parameters request a driver update only when QoS is enabled.
Both standard EDCA and vendor WMM element offsets are checked before decoding.
Output helpers now encode hidden/visible SSID and the first-eight/extended
legacy rate IE split with explicit length bounds, ready for scan and association
request frame assembly.
The scan-node cache is a bounded 512-entry MAC-keyed table with explicit node
lifecycle and source-invalid sequence sentinels. Borrowed Rust lookups replace
manual `ni_refcnt` management; the current BSS record remains separate from
cached scan entries.
The iwx scan-probe DMA builder now calls this crate's SSID and legacy-rate IE
encoders before assigning OpenBSD firmware segment boundaries, replacing the
prior duplicate serializer for those elements.
Output IE builders include HT/VHT/HE capability elements with the exact packed
little-endian layouts; HT Operation uses the source reserved-zero Basic MCS
field, while HE MCS/NSS maps follow advertised 160/80+80 width bits.
RSN/WPA output IEs preserve source suite ordering, WPA-v1 restrictions, replay-
counter bits, station PMF advertisement rules, optional PMKID and BIP group
management cipher. Wire output round-trips through the crate's RSN parser.
EDCA/WMM transmit elements now carry the source 11b versus OFDM AC tables,
AC-order identifiers, congestion-window encodings, TXOP limits and station
U-APSD AC/service-period bits.
Management frame helpers also encode Capability Information, DS channel and
ERP NonERP/protection/Barker fields from station/AP mode, channel and local
preamble state.
The station association-request body builder now applies source channel/mode
predicates, fixed reassociation BSSID and listen interval, then emits the
ordered SSID/rates/security/QoS/HT/VHT/HE elements. It is ready for the later
nl80211 connect adapter; four-way key exchange remains userspace-owned.
Open-system authentication and deauth/disassociation reason bodies now use the
source field order and little-endian encodings.
A reusable Probe Request IE builder now selects WMM+HT, 5-GHz VHT and
HE-extension elements using the same channel/PHY predicates as OpenBSD; iwx's
firmware-specific probe serializer reuses its individual IE encoders.
The rate-fix policy now matches local and peer rates, optionally sorts and
removes unaccepted entries, restores local Basic bits, and returns the source
failure sentinel for unsupported mandatory AP rates or mismatched fixed rates.
The proto layer now computes 11g protection reset and short-slot/preamble
selection from PHY mode, band, AP mode and local capability bits; a transition
helper reports when the driver-facing short-slot state changes.
The beacon-miss timer scales its watchdog threshold by the negotiated beacon
interval, preserves the previous value for a zero interval, and never sets a
threshold below one missed beacon.
The station Open System AUTH branch validates state and transaction sequence,
clears RSN protection/port/replay state before applying status, counts failed
peers or requests trying another BSS, and advances successful AUTH to ASSOC.
The actual user-visible EAPOL 4-way exchange remains with wpa_supplicant.
HT/VHT/HE negotiation now validates mode/channel support, mandatory peer and
local MCS maps, forbids WEP/TKIP for HT, and selects common SGI flags before
recording negotiated PHY state.
Failed authentication can age the current AP, reset the scan mode to AUTO for
all-band selection, choose a different compatible candidate and refuse to
re-select the same BSSID.
The station protocol state planner now emits ordered scan/auth/association/run
actions, cleanup and BA-stop events, keeps RUN on the open-auth retry path, and
defers link-up while RSN authorization remains pending.
Hardware-decrypted CCMP/TKIP frames now update per-TID receive sequence counters
after firmware/driver reorder, allow equal PN only for the explicit same-PN
reorder case, clear Protected and remove the retained IV before Ethernet
conversion. Frames whose IV was already stripped trust hardware replay status.

Station beacon and probe-response receive processing follows OpenBSD's fixed
field and information-element walk, active-channel/mismatch filters, hidden
SSID recovery, peer PHY/QoS/RSN state updates, 5-GHz probe RSSI preference,
and RUN-state current-BSS timer/protection/slot/DTIM effects. Interface-specific
callbacks and counters are represented as explicit `BeaconUpdate` effects for
the caller; peer records retain the associated node and beacon state.

The station authentication receive adapter bounds-checks the management header
and fixed algorithm/sequence/status fields, accepts only Open System
authentication, and passes the parsed response into the translated station
state handler; actual response transmission remains a caller effect.

The station association response handler validates mode/state and subtype,
updates the BSS association ID and negotiated rates, applies EDCA/WMM/U-APSD
state, negotiates HT/VHT/HE from local and peer capability state, and returns
RUN/protection/slot/RSN-port effects for the driver/SME to apply.

Deauthentication and disassociation receive paths decode the fixed reason
field and translate station background-scan/stay-authentication exceptions and
hostap peer-removal decisions into explicit protocol effects.

Block-Ack action receive helpers decode ADDBA/DELBA/BAR layouts and surface
agreement, timeout, refusal and reorder-window effects; actual DMA reorder
buffers and queue stop/start remain owned by the wireless device adapter.

Station MFP SA Query request/response handlers preserve the peer transaction
identifier, request the matching response, and clear the active query only for
a matching response; the management timer remains a caller-managed effect.

The action dispatcher recognizes BA ADDBA/DELBA and SA Query request/response
subtypes and returns typed dispatch tags for the translated receive handlers;
unsupported action categories remain explicit ignored events.
