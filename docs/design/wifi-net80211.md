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

Management subtype dispatch now selects typed beacon/probe, authentication,
association, disconnect, action and hostap-request receive paths from frame
control; unsupported subtype values remain visible to the caller.

Per-TID BAR handling checks active agreements, preserves PBAC's no-window-move
rule and reports required DELBA, inactivity timeout refresh and forward window
movement for the owning RX reorder engine.

The station receive pipeline now preserves OpenBSD's BA-before-fragment/duplicate
ordering, 12-bit sequence handling, retry/same-sequence drops, RSSI updates,
FromDS/BSSID/simplex filters and RX privacy checks. Hardware reorder and
hardware/software decryption remain caller-owned stages. A-MSDU payloads are
split into individually validated Ethernet frames, including the station DA
check and SNAP conversion.

A-MSDU output is represented as a batch of Ethernet frames rather than one
mis-decapsulated packet; the validated subframes preserve exact DA/SA/type
and reject truncation, invalid station destinations and malformed padding.

Beacon nodes retain complete advertised RSN and WPA information elements using
the input.c save-IE replacement semantics (copy the encoded element and resize
only when its declared byte length changes); malformed/truncated element
spans are rejected by the Rust boundary before the cached copy is modified.

## Translation boundaries checked against the reference files

The Fuchsia ieee80211 Rust library was evaluated as a direct dependency. Its
GN target depends on FIDL-generated WLAN types, `std`, `anyhow`, `zerocopy`, and
other Fuchsia build targets; the RSN target additionally depends on Zircon,
Fuchsia synchronization, and Fuchsia/BoringSSL crypto targets. Those crates are
not drop-in `no_std` Cargo dependencies for this kernel. The translated station
frame and RSN paths therefore stay in `tk-net80211`; no Fuchsia source files
are copied. The RSN four-way handshake remains with the standard userspace
wpa_supplicant path, while iwx firmware handles hardware key installation and
packet crypto offload.

For OpenBSD `ieee80211_input.c`, station-used beacon/auth/association,
management, disconnect, action/BA, security-IE, and inputm paths are ported.
The remaining source functions are intentionally mapped/excluded: `defrag`
and its timeout are under upstream `#ifdef notyet`; software BA window
buffering/gap timers are replaced by iwx's RX BAID/NSSN firmware reorder path;
`enqueue_data` is the ifnet/mbuf delivery wrapper mapped to Ethernet frames and
axnet; probe-request/association-request/PS-Poll handlers are AP-only (OpenBSD
iwx defaults to station and rejects hostap mode). Exact function coverage is
recorded in progress-W.md.

OpenBSD's `ieee80211_begin_bgscan()` gates background scans on RUN state,
scan re-entry, management timer and an authorized RSN port. The ported planner
runs the driver callback only after those guards and returns explicit cache
clear/background-scan effects when the callback succeeds; the timeout wrapper
uses the same planner. The controller/firmware scan callback is still a
separate iwx runtime integration step.

The generic management watchdog helper preserves the countdown and timer-rearm
behavior, moves any expired management state to SCAN, and adds station peer
failure/auto-join deselection effects only for AUTH/ASSOC timeouts. The timer
scheduler and actual state transition remain owned by the caller.

OpenBSD `ieee80211_reset_scan()` is represented by `reset_scan_channels()`:
it copies the active-channel bitmap back to the pending bitmap, and when the
BSS channel is ANY it positions the channel cursor one entry before the first
slot so the next-channel walk wraps to channel zero.

The station-mode end-of-scan planner represents active-scan cleanup, inactive
node cleanup, no-candidate reset/scan-count behavior, background-scan AP
retention/backoff, roam management-only queue gating, and driver-flush callback
selection as explicit effects. BSS/ESS choice is delegated to the existing
translated node selectors; hostap and IBSS end-scan paths are omitted because
iwx does not support those operation modes. iwx still needs to consume these
effects in its runtime scan/roam driver task.

Station BSS join now copies the selected scan node into the BSS record, carries
association-failure history only when the selected BSSID or desired ESS
matches, runs the translated rate and RSN selectors, and returns the PHY mode
and AUTH/DEAUTH-triggered state transition plan. Timeout cancellation and
firmware/node callbacks remain caller effects.

The BSS join path composes already-translated node rate/RSN policy and
newstate() effects: a selected scan node becomes the BSS record, matching
BSSID/ESS association-failure history is retained, rates are fixed, RSN suites
are intersected, and the station transitions to AUTH with the correct
background-roam or AUTH-retry trigger. This is a caller-facing state plan;
firmware association/key command execution remains a driver integration step.

Station leave-VHT and leave-HE helpers clear the cached node capability maps
and feature flags using the same reset functions used by the translated node
parser. Hardware PHY reconfiguration remains a caller effect.

The current untranslated net80211 function sets are not complete: remaining
`ieee80211_node.c` code includes AP/IBSS peer lifecycle, tree/refcount,
inactivity timers and ioctl/autoconf integration; remaining `ieee80211_output.c`
code includes AP beacon/response, power-save, hostap and ifqueue/BPF wrappers;
remaining `ieee80211_proto.c` includes AP/IBSS and key-rekey/timer paths. The
software CCMP/TKIP/WEP and BIP engines are not ported; the iwx design relies on
firmware key/cipher offload for data frames and MFP support still needs the
hardware key/control bridge. The explicit PAE files remain with wpa_supplicant.
See the function-count snapshot in progress-W.md for per-file marker totals.

RSN-node leave cleanup is represented as explicit effects: initialize state,
clear PMK/rekey/protection/authorized-port flags, cancel EAPOL/SA Query timers,
delete the pairwise key, and complete rekey only when the departing peer was
rekeying and no other rekey peers remain. The driver owns timer/key application.

HT-node leave now clears the cached HT capability state and returns explicit
BlockAck teardown/RX-reorder-buffer release effects; the RX queue/storage owner
performs the actual hardware reorder cleanup.

The 11g station-leave helper returns short-slot, ERP-protection and short
preamble changes only when the departed peer was the final incompatible
station; IBSS retains long slot time. The driver applies the returned PHY
reconfiguration effects.

Station scan nodes now carry the OpenBSD inactivity age, increment only while
unreferenced, cap at INACT_SCAN, and are removed at the caller's selected age
only when no node references remain. The hardware/refcount owner supplies the
reference predicate to the cache collector.

`ieee80211_node_copy()` now performs a deep owned record replacement (including
saved RSN/WPA IE buffers), requests timeout reset, BA/reorder teardown and
unreference-callback retirement, and leaves AP-only power-save queue
initialization disabled for the station-only iwx use path.

The `ieee80211_begin_scan()` plan now also returns the station-only BSS cleanup,
scan-node inactivity aging, AUTO/current mode reset, scan-count reset and
next-channel dispatch effects. The caller composes it with the node-cache age
and channel cursor helpers; hostap still begins passively.
