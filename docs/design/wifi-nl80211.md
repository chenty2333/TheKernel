# nl80211 generic-netlink surface

## User-space command audit

The reference audit uses the BSD-licensed wpa_supplicant nl80211 backend at
Google's source mirror, revision `5460547`: `src/drivers/driver_nl80211.c`,
`driver_nl80211_scan.c`, and `driver_nl80211_event.c`.

The backend first resolves generic-netlink family `nl80211`, subscribes to the
returned multicast groups, and queries GET_INTERFACE and GET_WIPHY. Its station
path uses TRIGGER_SCAN followed by GET_SCAN and scan-result/scan-aborted
notifications; connection paths use CONNECT or AUTHENTICATE/ASSOCIATE, followed
by NEW_KEY/SET_KEY/GET_KEY/DEL_KEY as applicable. Teardown uses DISCONNECT or
authentication/disassociation commands. Status and policy queries include
GET_STATION and REG_GET. The event dispatch consumes scan, connect/roam,
disconnect, auth/assoc/deauth/disassoc, MIC-failure, and regulatory events.
The exact path varies with the selected nl80211 capability and key-management
offload mode.

Sources:

- <https://android.googlesource.com/platform/external/wpa_supplicant_8/%2B/5460547/src/drivers/driver_nl80211.c>
- <https://android.googlesource.com/platform/external/wpa_supplicant_8/%2B/5460547/src/drivers/driver_nl80211_scan.c>
- <https://android.googlesource.com/platform/external/wpa_supplicant_8/%2B/5460547/src/drivers/driver_nl80211_event.c>
- UAPI layouts and numeric values: Linux 7.2.3 `include/uapi/linux/nl80211.h` (ISC grant; full text in `kernel/LICENSES/ISC.txt`).

## Current implementation boundary

The kernel registers the family id/name and multicast-group names and encodes
GET_INTERFACE/GET_WIPHY records from the wireless-link registry. Both dump
requests return a multipart NLMSG_DONE when there are no wireless devices,
which is the empty-radio path used by `iw dev` and `iw phy`. The registered-radio
record path currently carries interface identity, station type, and valid 2.4/5
GHz NVM frequencies with NO_IR flags. Legacy 2.4/5 GHz bitrate tables and
NVM/antenna-derived HT/VHT capability, MCS, and A-MPDU attributes are emitted;
HE maps and supported cipher suites are not yet advertised. Rates use the
matching 100-kbit/s UAPI units and 2.4 GHz short-preamble flags.
The wiphy advertises a one-SSID scan limit and the GET_WIPHY,
GET_INTERFACE, TRIGGER_SCAN, ABORT_SCAN, GET_SCAN, GET_REG, CONNECT,
DISCONNECT, GET_STATION, GET_KEY, SET_KEY, NEW_KEY and DEL_KEY commands that
currently have handlers. TRIGGER_SCAN validates the interface, the single-SSID limit and
frequency list, then asks the iwx controller to send the firmware UMAC scan
request. GET_SCAN returns only beacon/probe-response observations parsed from
firmware RX notifications and retained by the driver's bounded station scan
cache; it never synthesizes a BSS. During an active scan, the axnet bounded
polling worker drains firmware notifications and feeds that cache. The result
records carry nested BSSID, frequency, TSF, capability, IEs, signal, and age
attributes.
ABORT_SCAN sends the source UMAC abort command when a foreground scan is active.
The cache pump runs while waiting for the command ACK, from the bounded axnet
RX poll, and while a scan dump is queried. Firmware completion/abort emits
NEW_SCAN_RESULTS/SCAN_ABORTED on the nl80211 `scan` multicast group. Events are
deferred out of the RX service lock before listener delivery. GET_REG returns
the global world alpha2 value; regulatory rule tables and per-phy domains are
not yet emitted. CONNECT admits either an open BSS or WPA2-PSK/CCMP on an
observed RSN BSS and fails closed for unsupported AKM/cipher combinations.
The userspace supplicant owns the EAPOL four-way handshake; NEW_KEY/SET_KEY/
GET_KEY/DEL_KEY install software CCMP keys and GET_KEY returns the packet
sequence without disclosing key bytes. SET_PMKSA/DEL_PMKSA/FLUSH_PMKSA validate
standard peer/PMKID attributes but do not cache key material in the kernel;
the supplicant owns PMKSA state and supplies any selected PMKID in CONNECT IEs.
CONNECT also accepts the nl80211 frequency selector and the wpa_supplicant
control-port tuple for EAPOL (0x888e); EAPOL remains unencrypted on the
Ethernet-compatible station port. BSSID/frequency hints are treated only as
hints, while a requested frequency is matched against the cached BSS. MFP
requests other than disabled are rejected because management protection is not
implemented.
Successful CONNECT and DISCONNECT
queue their standard command events on the `mlme` group; CONNECT carries the
association request/response IEs retained by the driver. GET_STATION encodes
the associated BSSID and signed signal value from the live driver record.
Other station/authentication events and regulatory rule operations remain
incomplete.
The required no-radio QEMU acceptance was run on q35-UEFI with the signed
wireless payload and no fake radio: `/usr/sbin/iw dev` and `/usr/sbin/iw phy`
both returned empty output, then the guest reached the test marker. The first
command-file attempt used `/usr/bin/iw` (the APK stages it in `/usr/sbin`) and
did not invoke either utility; the corrected acceptance run is the one that
counts.
