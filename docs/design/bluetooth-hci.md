# Bluetooth HCI core

`tk-bt-hci` defines bounded HCI packet framing, a USB endpoint transport
contract matching `ng_ubt.c` (command control OUT, event interrupt IN, ACL bulk
IN/OUT), channel ownership, and Linux Bluetooth socket constants. It does not
yet bind xHCI USB devices, port `iwmbtfw` firmware update procedures or load
`intel/ibt-*` firmware, expose AF_BLUETOOTH sockets or implement the complete
HCIGETDEV/HCIDEV ioctl data structures. Thus it is not yet a functioning
Bluetooth controller driver and no guest-visible HCI device is registered.
