# nl80211 generic-netlink boundary

The family resolver advertises `nl80211` (UAPI family name/version and scan,
regulatory, mlme, config, vendor, nan and testmode multicast groups). The
no-radio `GET_WIPHY` and `GET_INTERFACE` dump paths terminate with an empty
multipart `NLMSG_DONE`, which is the expected result for `iw phy` and `iw dev`
when no radio is registered. A registered radio is deliberately not reported
as an empty dump: concrete wiphy/interface records and command-backed state
transitions remain to be implemented. Userland WPA handshakes remain assigned
to unmodified wpa_supplicant rather than kernel PAE.
