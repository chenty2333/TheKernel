# Intel LPSS I2C / DesignWare controller

`tk-i2c` contains a function-level translation of FreeBSD
`sys/dev/ichiic/ig4_iic.c`, `ig4_reg.h`, `ig4_var.h`, `ig4_pci.c`, and
`ig4_acpi.c`: controller transfer/error/timeout/suspend/resume paths, all 140
PCI device-table rows, ACPI matching/resource hooks, and LPSS private-register
handling. The PCI walker binds the Intel LPSS IDs by default, maps BAR0, and
registers the controller; the ACPICA resource parser decodes I2cSerialBusV2
child addresses/speeds and the controller ACPI companion provider associates
them with the PCI function. Devfs publishes `/dev/i2c-N` with the Linux i2c-dev
ioctls, bounded I2C_RDWR, and SMBus byte/word/I2C-block operations. The x86 adapter requires MSI for attach and retains
bounded polling when the caller cannot block. The translated interrupt path
masks and wakes the transfer waiter before it clears controller status.
