# Intel Bluetooth on the USB transport

The `tk-bt-hci` crate currently provides checked HCI packet framing and channel ownership, plus an initial translation of `iwmbt_fw.c` firmware-name and TLV parsing. USB host-controller discovery is in `crab_usb`/`tk-axdriver`; a Bluetooth class driver is not yet bound to that host, and no AF_BLUETOOTH sockets or `/sys/class/bluetooth` device publisher exist yet.

The source is FreeBSD `usr.sbin/bluetooth/iwmbtfw/iwmbt_fw.c` and `.h` (BSD-2-Clause, 2026-10-08 snapshot). The translated functions are `iwmbt_get_fwname`, `iwmbt_get_fwname_tlv`, and `iwmbt_parse_tlv`. `iwmbt_fw_read`/`iwmbt_fw_free` are deferred: their userspace `open`/`read`/`malloc` lifecycle must be mapped to `axdriver_base::firmware::on_rootfs_ready` and bounded `firmware::request`, not emulated with kernel file descriptors. The larger `iwmbt_hw.c`/`main.c` command orchestration and `ng_ubt.c` USB transfer/control paths are also pending.

The eventual USB driver should bind the Bluetooth HCI interface class, send command packets on endpoint zero, receive events on interrupt IN, and carry ACL over bulk IN/OUT. The socket/UAPI side must use Linux-compatible structures and errno behavior without translating GPL `net/bluetooth` implementation code. Present evidence is crate-level framing/parser tests only; no xHCI device or guest Bluetooth command has been exercised.
