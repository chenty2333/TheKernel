# I2C HID transport core

`tk-i2c-hid` models the 30-byte HID-over-I2C descriptor, ACPI `_DSM`
descriptor-register result, command register RESET/SET_POWER writes and the
two-byte input-report length framing from FreeBSD `iichid.c`. It is transport-
agnostic and expects its adapter to serialize on `tk-i2c`'s bus. HID report
decoding should reuse the existing USB HID parser and the kernel input layer;
this crate does not yet attach to ACPI `PNP0C50` / `ACPI0C50`, publish an input
device, or implement `hmt.c` multitouch slot tracking and `ABS_MT_*` events.
