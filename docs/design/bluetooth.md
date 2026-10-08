# Intel Bluetooth on the USB transport

The `tk-bt-hci` crate provides checked HCI packet framing and channel ownership, plus initial translation of `iwmbtfw` firmware handling. The xHCI probe binds USB class `e0/01/01`, requires HCI event/ACL endpoints, and registers a control/interrupt/bulk transport in the axdriver registry. The kernel recognizes `AF_BLUETOOTH/SOCK_RAW/BTPROTO_HCI`; raw/user binds route command TX and event/ACL RX to attached adapters, monitor frames use the Linux HCI monitor header, and `HCIGETDEVLIST`/`HCIGETDEVINFO` expose controller identity/capabilities/stat counters. HCI_USER excludes RAW owners; monitor currently fans into the single target adapter. Receive is persistent-request polling from socket read, not completion-driven async readiness: poll/epoll wakeups are not integrated. `/sys/class/bluetooth/hciN` publishes address, name, type, bus, manufacturer, and flags; the full Linux attribute set is not implemented.

The sources are FreeBSD `usr.sbin/bluetooth/iwmbtfw/iwmbt_fw.c`/`.h`/`main.c`, `iwmbt_hw.c`, and `sys/netgraph/bluetooth/drivers/ubt/ng_ubt.c` (BSD-2-Clause, 2026-10-08 snapshot). The translated firmware functions include `iwmbt_get_fwname`, `iwmbt_get_fwname_tlv`, `iwmbt_parse_tlv`, `iwmbt_is_supported`, `iwmbt_identify`, fixed/TLV version and boot-parameter queries, RSA/ECDSA header chunks, SFI command chunking/reset, manufacturer mode, patch BSEQ, DDC and event mask operations. Patch-stream expected events are checked by both event code and payload; command/event counters include those successful HCI transfers. HCI_USER ownership excludes RAW sockets while still allowing monitor observers; monitor framing preserves full 1028-byte ACL packets. A monitor bound with HCI_DEV_NONE attaches to the first available controller (the current target has one); multi-controller fan-out is not implemented. The rootfs-ready callback dispatches the identified 7260/8260/9260 family to the corresponding download path; no physical controller has validated those paths. To stage binary firmware into the guest image, set `THEKERNEL_INTEL_BT_FIRMWARE_DIR` to a linux-firmware `intel/` directory; `scripts/build-rootfs.sh` decompresses SFI/DDC/BSEQ files and carries `LICENSE.intel`. The userspace CLI/device enumeration and parts of main's recovery/status behavior remain unmapped. The HCI endpoint split maps control commands, interrupt events and bulk ACL to CrabUSB transfers. A per-adapter receive task now keeps event- and ACL-IN requests submitted concurrently, queues completed packets and wakes poll/epoll readiness; queue depth is bounded. Multiple raw readers still consume from the shared queue rather than receiving independent copies. `HCI_CHANNEL_CONTROL` implements READ_VERSION, READ_COMMANDS, READ_INDEX_LIST, READ_INFO, SET_POWERED, SET_CONNECTABLE, SET_DISCOVERABLE (timeout zero only), SET_BONDABLE, SET_SSP and SET_LE. These six setting commands return settings in Command Complete; scan/SSP/LE commands wait for matching HCI Command Complete, and NEW_SETTINGS is queued after the response on the issuing control socket. The event endpoint is quiesced/rearmed around synchronous commands and unrelated command-complete events remain queued for raw readers. Cross-socket fan-out for HCI-derived device events, nonzero discoverable-timeout expiry, BREDR setting, key loading, device-found translation, pairing, disconnect, connection state and device/key events remain unimplemented. START/STOP_DISCOVERY issue Inquiry or LE scan commands and queue a local DISCOVERING event after HCI acknowledgement; active-controller scan results remain unverified. `HCIGETDEVLIST` emits aligned `dev_opt`/HCI_UP flags and `HCIGETDEVINFO` queries the standard BD_ADDR/features/buffer-size commands and reports command/event/ACL packet+byte counters. Multi-controller monitor fan-out remains incomplete. `/sys/class/bluetooth/hciN` publishes address/name/type/bus/numeric HCI manufacturer ID/flags plus device ID, HCI feature bits, HCI version/revision and a root-writable `reset` attribute backed by HCI Reset; uevent/link topology and further attributes remain unimplemented. Reset is software-validated only, without a physical controller. Firmware blobs are not checked in.

The USB binding uses endpoint zero for commands, interrupt IN for events and bulk IN/OUT for ACL, routed through CrabUSB; it has not been exercised with a physical controller. QEMU q35 without a Bluetooth device passed the guest smoke helper: monitor bind, empty `HCIGETDEVLIST`, `ENODEV` for `HCIDEVUP`/`HCIGETDEVINFO`, `/sys/class/bluetooth`, and HCI management READ_VERSION/READ_INDEX_LIST with poll-readable command completion. Alpine BlueZ command-line/runtime tools are now available in the optional `bluez` payload and a no-controller daemon smoke is recorded below; active-controller behavior remains unverified. The socket/UAPI implementation is original and specification-based; GPL `net/bluetooth` implementation code was not copied.

The optional `--toolchain bluez` payload now stages signed Alpine v3.24 x86_64
BlueZ 5.86-r2 (`bluetoothd`, `btmgmt`, `bluetoothctl`) and its pinned musl,
D-Bus, GLib, readline, json-c, and eudev runtime closure. The APK signatures
and checked-in SHA256 pins are verified during payload staging; the ordinary
`none` image is unchanged. A QEMU q35/KVM no-controller run launched the
system D-Bus daemon and `bluetoothd -n`; the daemon remained alive after
initializing management, `btmgmt info` printed `Index list with 0 items`, and
`bluetoothctl list` exited successfully with no controller entries. The
repeatable guest case is `bluez-no-controller`; it proves userspace no-device
startup only, not pairing, controller discovery, or live HCI operations.

The no-controller QEMU test remains unchanged and verifies the empty-index
BlueZ path. With a controller, six settings commands and START/STOP_DISCOVERY
are advertised alongside READ_INDEX_LIST/READ_INFO; NEW_SETTINGS and
DISCOVERING are listed as implemented local events. Current-settings and local
response framing are covered in source tests, but no physical controller has
validated the command sequences. `SET_DISCOVERABLE` currently accepts only
timeout zero; discovery supports BR/EDR Inquiry or LE scan independently.
Empty LOAD_LINK_KEYS/LOAD_LONG_TERM_KEYS/LOAD_IRKS batches are accepted as no-op loads; non-empty key storage remains unsupported. PAIR_DEVICE, DISCONNECT, BREDR setting and other requested operations remain unsupported. Device Found, Connected/Disconnected and key events are not implemented.
NEW_SETTINGS and DISCOVERING are fanned out to currently bound control
sockets, but raw HCI event to mgmt event translation remains incomplete.

2026-10-09 management follow-up: valid nonempty LOAD_LINK_KEYS, LOAD_LONG_TERM_KEYS and LOAD_IRKS records are validated and retained in the controller's host-side key cache; PAIR_DEVICE issues a BR/EDR or LE connection command and DISCONNECT issues HCI Disconnect for a tracked handle. SET_BREDR (mgmt opcode 0x002a) controls the advertised host state and gates BR/EDR inquiry; it does not switch a physical radio mode in hardware. Raw HCI event fan-out now translates inquiry/LE advertising reports to DEVICE_FOUND, classic/LE connect/disconnect state to DEVICE_CONNECTED/DISCONNECTED, HCI authentication failures to AUTH_FAILED, and classic HCI Link Key Notification to NEW_LINK_KEY. The HCI transport's async event tap preserves events for raw sockets while management subscribers receive corresponding notifications. At this implementation stage, SMP was not integrated. Loaded link keys and LTKs answer matching HCI requests from a volatile host cache; disconnect-reason fidelity is limited to mapped HCI reason codes. No-device BlueZ/QEMU acceptance covers only management framing and invalid-index responses; the later LE SMP section below supersedes the pairing status.

The HCI event tap also handles LE Connection Complete and Inquiry Result with RSSI when those events reach the receive worker; event parameters use the BlueZ management `mgmt_addr_info` / RSSI / flags / EIR framing. These conversion paths have source-level validation but remain unverified on physical Bluetooth hardware.

Controller-backed mgmt status before LE SMP integration: `SET_BREDR`, SET_IO_CAPABILITY, PIN_CODE_REPLY/NEG_REPLY, USER_CONFIRM_REPLY/NEG_REPLY and the management PIN/numeric-confirmation events are routed to HCI. Classic BR/EDR PAIR_DEVICE starts connection/authentication and answers HCI IO Capability Request. Loaded link keys and LTKs answer matching HCI requests from the volatile cache. The no-controller QEMU smoke validates the framing and error responses; physical HCI behavior remains unverified. The current command/event table and LE pairing state are recorded below.

`SET_PRIVACY` retains the local IRK; when privacy is enabled and the controller is up, the driver clears/repopulates the standard LE resolving list from loaded public/random IRKs and enables controller address resolution. `LOAD_IRKS` refreshes that list only while privacy is enabled. This is controller-backed HCI programming, but privacy-mode acceptance still needs a physical Intel controller.

2026-10-09 no-device CLI follow-up: the optional Alpine payload also includes
the signed `bluez-btmon` and `bluez-deprecated` packages, pinning `btmon` and
`hciconfig` alongside the daemon and current tools. The HCI socket now retains
the `SOL_HCI` data-direction/timestamp controls and the `SOL_SOCKET`
timestamp/pass-credentials toggles required for `btmon` startup. Ancillary HCI receive metadata now emits `HCI_CMSG_DIR` (incoming direction) and
`HCI_CMSG_TSTAMP` (microsecond timeval) when enabled; timestamp capture comes from
the receive/pump path. With `SO_PASSCRED`, `SCM_CREDENTIALS` now carries kernel
credentials `(pid, uid, gid) = (0, 0, 0)` and sets `MSG_CTRUNC` if the ancillary
space cannot fit it. This evidence is strictly the no-controller path: in QEMU, `hciconfig` enumerates no adapters,
`hciconfig hci0` gets `ENODEV`, and `btmon -i 0` starts waiting until the test
stops it. The existing BlueZ daemon, `btmgmt info`, and `bluetoothctl list`
no-controller checks also pass. No active-controller/monitor-frame behavior is
claimed by this smoke.

## LE SMP implementation status (2026-10-09)

The no-std `tk-bt-hci::smp_crypto` module now provides AES-128, RFC 4493
AES-CMAC, legacy `c1`/`s1`, Secure Connections `f4`/`f5`/`f6`/`g2`, resolvable
address `ah`, and P-256 public-key/ECDH helpers. Crypto arrays use the most-
significant-octet-first order from Core Vol 3 Part H Appendix D; SMP wire fields
are little-endian and require explicit conversion at their caller. RFC 4493 and
Core Appendix D vectors cover CMAC, f4-f6, g2, and ah; P-256 ECDH has symmetry
and invalid-point checks. A bounded central-side Just Works state machine now performs the Pairing Feature
exchange, Legacy Confirm/Random/STK or Secure Connections Public Key/Confirm/
Random/DHKey Check and returns an encryption key action. MITM/OOB requests fail
closed. It has no live ACL/CID 6 dispatch yet and does not drive HCI encryption,
post-encryption key distribution, bond persistence, mgmt `PAIR_DEVICE` completion,
or `NEW_LONG_TERM_KEY`/`NEW_IRK` events. Passkey/Numeric Comparison UI and
peripheral-role pairing are also unsupported. Thus this is tested protocol core,
not end-to-end LE pairing; no hardware pairing success is claimed.

2026-10-09 LE pairing integration follow-up: `PAIR_DEVICE` for LE now installs a
central-side SMP Just Works session on successful LE connection, sends the
Pairing Request on ACL/L2CAP CID 0x0006, observes ACL independently of raw HCI
socket delivery, validates phase-2 Legacy or Secure Connections traffic, starts
HCI encryption, and runs phase-3 key distribution. The locally distributed IRK is retained and reused
(or uses the mgmt-configured local IRK); it caches generated/received LTKs and
IRKs in the controller's host key tables, updates the resolving list
when privacy is active, and emits the BlueZ `NEW_LONG_TERM_KEY`/`NEW_IRK` events
only after phase-3 transfer completes. The READ_COMMANDS event list now includes
both key events (22 commands, 11 events). Software checks cover HCI ACL observer,
crypto test vectors and bounded SMP exchange state tests; product-feature kernel
check passes. This is not yet a hardware acceptance: no physical controller was
available. ACL continuation-fragment reassembly, peripheral-role pairing,
Numeric Comparison UI, Passkey/OOB association models, pairing timeout/cancel,
remote RPA-to-identity connection matching, and crash-safe persistence remain
open; invalid/unavailable flows fail without reporting a successful bond.

The latest no-controller QEMU smoke (Q35/KVM, VT-d/intremap) passed after the
management event table update: 22 commands/11 events, empty index list,
no-device ioctl errors, `hciconfig` empty enumeration and BlueZ's no-controller
startup path. This does not exercise ACL SMP against a peer.
