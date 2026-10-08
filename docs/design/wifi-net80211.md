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
The output-side user-priority map and non-AP ACM downgrade loop are also
translated; AP mode preserves the requested AC without downgrade.
Ethernet classification maps VLAN PCP or IPv4/IPv6 DSCP into source EDCA
classes and applies the source per-window video/voice TXOP fallback policy.
Management action body builders encode source ADDBA request/response, DELBA
and SA Query response layouts; the transmit BA bitmap/window advances to the
requested 12-bit sequence while retaining the source zero-bitmap stop rule.
Station action dispatch returns no frame for AP-only SA Query requests or
unsupported action categories, matching the `IEEE80211_STA_ONLY` build.
Transmit BlockAck negotiation now plans the source INIT-to-REQUESTED
transition, token and 64-frame window, immediate/delayed policy, response
timeout, and the optional driver-offload completion/refusal branch.
Local DELBA request handling clears the selected Tx or Rx agreement and emits
direction-specific firmware-stop, timer-cancel, reorder-retirement and optional
management-send effects.
Tx/Rx inactivity callbacks preserve source retry counters, statistics and
setup-required/timeout DELBA reasons, including the bounded 30-second request
backoff counter.
Station leave also traverses all 16 Tx TIDs and preserves the source offload
rule: firmware-owned agreements get a stop callback without host-state reset;
software agreements are cleared and optionally transmit DELBA.
Supplicant PTK-negotiation failures now persist the WPA-key association failure
bit on the current BSS and its independent scan-cache copy.
The station transmit-node selector consistently resolves unicast as well as
multicast traffic to the active BSS node, matching the STA-only source branch.
Station keyrun requires RUN plus RSN and moves the handoff state to PTKSTART;
the userspace wpa_supplicant owns the subsequent four-way handshake and keys.
The transmit AMPDU admission predicate additionally requires HT, local TX
support, the active BSS peer in station mode, and RSN protection.
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
uses the same planner. The iwx callback now sends the firmware UMAC scan
command and its RX poll feeds validated beacon/probe-response frames into the
station scan cache; end-of-scan effects and userspace multicast notification
remain unconnected.

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
The software CCMP AES-CCM key setup, packet-number, encrypt and decrypt/MIC
paths are translated in `crypto_ccmp.rs`; AES block operations use the RustCrypto
`aes` crate. The software BIP IGTK/MMIE AES-CMAC encap/decap path is also
translated in `crypto_bip.rs`, WEP RC4/ICV in `crypto_wep.rs`, and station TKIP
key mixing/RC4/Michael/ICV/replay in `crypto_tkip.rs`. The two TKIP functions
remaining untranslated are HostAP-only peer-deauth and MIC timer callbacks.
OpenBSD iwx hardware key offload covers CCMP/IGTK while upstream falls back to
net80211 software crypto for other data ciphers. The Ethernet-compatible iwx
path now installs WPA2-PSK/CCMP pairwise/group keys from nl80211 and routes
protected station data through the translated software CCMP encrypt/decrypt
dispatch; EAPOL remains on the unprotected userspace-supplicant port. TKIP/WEP
are not admitted by the current key control adapter. The explicit PAE files
remain with wpa_supplicant.
The common net80211 cipher dispatcher and pairwise/group RX/TX key selectors
now choose the translated cipher contexts; the iwx control path populates
CCMP keys from nl80211 and routes protected Ethernet frames through these
helpers.
See the function-count snapshot in progress-W.md for per-file marker totals.

## Station-only omission register

The per-file source scan is against OpenBSD's `sys/net80211` files and each
function below has a specific station-iwx boundary; these are not claimed as
translated:

- `ieee80211.c`: `ieee80211_ifattach`, `ieee80211_ifdetach`,
  `ieee80211_media_init`, `ieee80211_media_change`, and
  `ieee80211_media_status` are ifnet/ifmedia registration and ioctl wrappers
  mapped to TheKernel's netdev and nl80211 paths.
- `ieee80211_node.c`: `ieee80211_add_ess`, `ieee80211_del_ess`,
  `ieee80211_deselect_ess`, `ieee80211_ess_clear_wep`,
  `ieee80211_ess_clear_wpa`, `ieee80211_ess_setnwkeys`,
  `ieee80211_ess_setwpaparms`, `ieee80211_set_ess`, `ieee80211_print_ess`,
  `ieee80211_print_ess_list`, and `ieee80211_ess_cmp` belong to the OpenBSD
  ifconfig saved-network profile table; wpa_supplicant supplies that policy
  through CONNECT, while scanned BSS records remain in the station cache.
  `ieee80211_create_ibss` and `ieee80211_ibss_merge` are IBSS-only.
  `ieee80211_node_join`, `ieee80211_node_join_11g`,
  `ieee80211_node_join_ht`, `ieee80211_node_join_rsn`,
  `ieee80211_node_leave`, `ieee80211_node_leave_pwrsave`,
  `ieee80211_count_longslotsta`, `ieee80211_count_nonerpsta`,
  `ieee80211_count_pssta`, `ieee80211_count_rekeysta`, `ieee80211_set_tim`,
  and `ieee80211_notify_dtim` are AP-side peer admission, TIM/DTIM, power-save,
  or authenticator callbacks; the station iwx adapter has no AP role and its
  association response path is separate. `ieee80211_needs_auth` is the AP
  authenticator's 802.1X callback; station PAE/EAPOL belongs to
  wpa_supplicant. `ieee80211_node_addba_request`,
  `ieee80211_node_addba_request_ac_be_to`,
  `ieee80211_node_addba_request_ac_bk_to`,
  `ieee80211_node_addba_request_ac_vi_to`,
  `ieee80211_node_addba_request_ac_vo_to`,
  `ieee80211_node_addba_request_tid4`, `ieee80211_node_addba_request_tid5`,
  `ieee80211_node_addba_request_tid6`, `ieee80211_node_addba_request_tid7`,
  and `ieee80211_node_trigger_addba_req` use net80211 software callouts; iwx
  uses firmware BA queues/reorder state instead. `ieee80211_ba_del` and
  `ieee80211_node_tx_flushed` are BA/callout teardown wrappers mapped to
  iwx's controller-owned BA tables and station queue flush/teardown.
  `ieee80211_clean_cached`, `ieee80211_clean_nodes`,
  `ieee80211_iterate_nodes`, `ieee80211_node_free`,
  `ieee80211_node_free_unref_cb`, `ieee80211_node_cmp`, and
  `ieee80211_node_attach`/`detach`/`lateattach` are RB-tree, refcount, timer,
  or allocation wrappers represented by the bounded `NodeTable`, owned Rust
  records, and driver lifetime; `ieee80211_node_detach` and
  `ieee80211_node_lateattach` are autoconf framework methods. `ieee80211_inact_timeout` and
  `ieee80211_node_cache_timeout` are periodic OpenBSD timeout registrations;
  the iwx scan cache is aged by bounded service polling. `ieee80211_node_set_timeouts`
  is a hostap/EAPOL/SA-Query/BA timer registration wrapper. `ieee80211_release_node`
  is represented by Rust ownership/drop. `ieee80211_do_slow_print` is an
  optional rate-limited diagnostic printer.
- `ieee80211_input.c`: `ieee80211_defrag` and `ieee80211_defrag_timeout` are
  upstream `#ifdef notyet`; `ieee80211_input_ba`,
  `ieee80211_input_ba_flush`, `ieee80211_input_ba_gap_skip`,
  `ieee80211_input_ba_gap_timeout`, `ieee80211_input_ba_seq`, and
  `ieee80211_ba_move_window` are software reorder queues replaced by iwx's
  hardware BAID/NSSN reorder path. `ieee80211_enqueue_data` is the
  ifnet mbuf enqueue wrapper mapped to the Ethernet-compatible axnet device.
  `ieee80211_recv_assoc_req`, `ieee80211_recv_probe_req`, and
  `ieee80211_recv_pspoll` are hostap-only.
- `ieee80211_output.c`: `ieee80211_action_name` and `ieee80211_getmgmt` are
  debug text / mbuf allocation wrappers; `ieee80211_output` is the ifqueue
  entry mapped to axnet plus the station encap helper. `ieee80211_add_ibss_params`,
  `ieee80211_add_tim`, `ieee80211_beacon_alloc`, `ieee80211_get_assoc_resp`,
  `ieee80211_get_probe_resp`, and `ieee80211_pwrsave` are IBSS/hostap paths.
  `ieee80211_add_tie` is an AP beacon-suppression IE. `ieee80211_get_rts`,
  `ieee80211_get_cts_to_self`, and `ieee80211_tx_compressed_bar` wrap the
  legacy if_start/mbuf control-frame callbacks; transmit admission/control is
  owned by iwx firmware queues and the standalone BAR encoder.
- `ieee80211_proto.c`: `ieee80211_proto_attach` and
  `ieee80211_proto_detach` register ifnet callbacks;
  `ieee80211_set_link_state` maps to axnet link state.
  `ieee80211_rtm_80211info_task` is the OpenBSD route-socket metadata worker;
  TheKernel publishes supported wireless state through rtnetlink and nl80211.
  `ieee80211_auth_open_confirm` is under OpenBSD's station-only exclusion and
  is AP-side confirmation. `ieee80211_setkeys`, `ieee80211_setkeysdone`,
  `ieee80211_gtk_rekey_timeout`, `ieee80211_node_gtk_rekey`,
  `ieee80211_sa_query_request`, and `ieee80211_sa_query_timeout` are AP
  authenticator/rekey/timeout paths; EAPOL/SA-Query station protocol messages
  are userspace-owned or represented by station request effects.
  `ieee80211_dump_pkt` and `ieee80211_print_essid` are diagnostics.
- `ieee80211_crypto.c`: `ieee80211_crypto_attach` and
  `ieee80211_crypto_detach` register cipher methods replaced by the Rust
  crypto dispatcher and owned-context drop. `ieee80211_derive_ptk`,
  `ieee80211_derive_pmkid`, `ieee80211_pmkid_sha1`,
  `ieee80211_pmkid_sha256`, `ieee80211_eapol_key_check_mic`,
  `ieee80211_eapol_key_decrypt`, `ieee80211_eapol_key_encrypt`,
  `ieee80211_eapol_key_mic`, `ieee80211_pmksa_add`,
  `ieee80211_pmksa_find`, and `ieee80211_crypto_clear_groupkeys` are EAPOL/PMKSA
  supplicant/authenticator state; wpa_supplicant owns the four-way handshake
  and PMKSA, while nl80211 installs only the selected CCMP traffic keys.
- `ieee80211_pae_input.c` and `ieee80211_pae_output.c`: every PAE state/key
  handler is userspace wpa_supplicant's standard nl80211/EAPOL path. Fuchsia's
  PAE crates depend on FIDL/std/Fuchsia crypto and are not no_std drop-ins.
- `ieee80211_crypto_tkip.c`: `ieee80211_michael_mic_failure_timeout` and
  `ieee80211_tkip_deauth` are hostap peer-countermeasure/timer callbacks;
  the iwx STA path currently admits WPA2-CCMP only.

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

The four scalar ifmedia conversion functions from `ieee80211.c` now map
legacy rates and HT/VHT/HE MCS values through typed `MediaSubtype` variants.
The `ifattach`/`ifdetach`, `channel_init`, `ioctl`, `media_init`,
`media_change`, and `media_status` functions are framework registration/ioctl
wrappers mapped to the kernel wireless netdev/nl80211 surface rather than
copied from OpenBSD ifnet. `release_node` reference drops are represented by
Rust-owned borrow/drop lifetimes.

Station compressed-BAR construction is also translated as a bounded control
frame encoder; queue admission, node retention and `if_start` are caller-side
effects mapped to iwx's management queue.

The station `mgmt_output` header builder now emits receiver/transmitter/BSSID,
12-bit per-node sequence control, and Protected-bit policy for MFP action,
deauth and disassoc frames. The caller still owns node reference transfer and
the actual iwx management queue submission.

The station subset of `ieee80211_send_mgmt()` selects probe/auth/deauth/assoc/
disassoc/action body requests and preserves the five-tick transition timer
only for probe/auth/association; the caller builds the body and queues it.

Duplicate station RX nodes now inherit only the source BSS BSSID and channel
after bounded cache allocation; the kernel owns their lifetimes instead of
OpenBSD node reference callbacks.

The station-only `needs_rxnode`/`find_rxnode` policy now routes ordinary STA
traffic to the BSS record and monitor captures to a referenced peer record,
allocating a duplicate-BSS peer only on cache miss. AP/IBSS address admission
remains outside this station subset.

The station TX Block-Ack clear helper resets one agreement and reports only
the retry-timer cancellation the task scheduler must perform.

Background roam TX-drain and BSS-switch callbacks now produce explicit
send-deauth/switch/restart-scan plans, including cleanup when the current or
selected station node disappeared; command submission and callback lifetime
remain driver-owned.

Node cleanup now clears owned security-IE storage and returns BA/reorder and
unreference-callback retirement effects; only an explicit HostAP caller asks
to purge the saved power-save queue.
