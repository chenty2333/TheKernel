# Intel LPSS I2C / DesignWare controller

The controller core is a no-std register engine based on FreeBSD
`sys/dev/ichiic/ig4_iic.c`, `ig4_reg.h`, `ig4_var.h`, `ig4_pci.c`, and
`ig4_acpi.c`. It exposes the standard/faster-speed settings, combined
read/write transactions, bounded FIFO polling, transmit-abort handling,
ACPI `I2cSerialBusV2` child addresses, and the N305 `8086:54e8` / `8086:54ea`
IDs. The PCI/ACPI resource attachment, device model registration, I2C-dev
character-device ioctls and `/dev/i2c-N` publication remain untranslated: the
platform resource and devfs lifecycle interfaces required to do these safely
are not yet exposed to this crate. Consequently this commit provides a
controller core, not yet a product-bound I2C driver.
