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
GHz NVM frequencies with NO_IR flags. It does not yet emit legacy/HT/VHT/HE
rates or supported ciphers. GET_REG returns the global world alpha2 value;
regulatory rule tables and per-phy domains are not yet emitted. GET_SCAN
accepts a dump request for a registered interface and emits an empty multipart
result until a scan cache is connected; it does not start scans. TRIGGER_SCAN,
scan events, connection/authentication, key, station and regulatory rule
operations remain incomplete. The required no-radio QEMU acceptance is deferred
until the task-5 command surface is complete; no fake radio is used.
