# Opt-in xHCI Debug Capability console

**Hardware unverified.** QEMU xHCI has no DbC; absence is a tested fallback,
not a transport test. Original Apache-2.0 Rust implementation in
`tk-axdriver-dbc` (`ids`, `regs`, `desc`, `probe`, `bringup`, `fake`), with
coherent-DMA/PCI seam in `tk-axdriver/src/dbc.rs`. Linux behavior/register facts
were consulted, not translated or quoted.

## Interface and safety

`python3 tools/thekernel.py build --platform n305 --profile shell --usb-dbc`
selects a distinct `mem1g-usb-dbc` artifact variant. Default builds never enable
or access DbC. Feature passes root → kernel → axfeat → axdriver → hardware crate.
The regular USB controller initializes first, avoiding reset of an active DbC.
The BAR-sized, 256-entry bounded, read-only extended-capability walk rejects
truncation, Controller Not Ready, and firmware-owned/enabled DbC. No global xHCI
reset, PCI policy change, or framebuffer replacement is performed by DbC.

Six coherent DMA pages, physically 64KiB-aligned, contain the 192-byte context,
UTF-16 string descriptors, ERST, one 256-TRB event ring, two 255-data-TRB/link rings,
and two 1024-byte buffers. IN TDs carry at most 1023 bytes, guaranteeing a
short packet to complete a host bulk read promptly even if no later log is
written. One outstanding TD per direction; release/acquire
fences and volatile access publish/observe cycle bits. Link cycles toggle at
wrap; event pointers/endpoint IDs/completion codes/residuals are validated before
use. Device IN is log/TTY TX (doorbell target 1); device OUT is console RX (target
0). This distinction is not host xHCI endpoint numbering. DCPORTSC change
acknowledgment preserves its **RW PED**, unlike host PORTSC.

No wait for a cable during boot. A configured connection enables transfers.
Polling is bounded to 256 events per call and runs every 2ms in a scheduler task.
Connection without enumeration times out after 30 seconds. Enable/disable
readback is bounded to 1000 reads. Transfer error, halt, reset, malformed event,
or configured link loss disables DbC and leaves ordinary consoles operational.
No automatic reconnect/ring reuse; **reboot target to reconnect**. The single
24KiB DMA allocation is retained until reboot even on initialization or disable
failure, preventing use-after-free if hardware still owns DMA.

An 8192-byte admission queue applies backpressure to an independent, nonblocking
kernel-log snapshot cursor (all retained priorities, unlike screen loglevel).
It never consumes the shared syslog/netconsole cursor. Pre-worker logs replay
only if still retained; overwrite emits one gap warning. Kernel logs and active
VT output can interleave. TTY mirroring is best effort: overflow/lock contention
drops its bytes, counted in the transport's stop report, and never waits on the
screen path. DMA admission is **not host delivery acknowledgment**.
RX uses the normal active-VT route/flush-generation admission; switching VTs or
flushing input rejects stale batches. First-byte diagnostics mark `UsbDebug`.
NUL bytes are preserved. Input admission runs in a separate worker, so a blocked VT input route cannot
block the independent log/DMA worker. No independent `/dev/ttyDBC`, termios/baud or emulated
BREAK protocol. Worker starts after PID1 publication; this is **not an earlyboot,
panic-safe or scheduler-independent channel**. If the scheduler itself stops,
queued logs cannot be guaranteed to drain. Normal framebuffer remains the fallback.

## Cable / Linux debug host (user-operated)

Use a **USB 3.x SuperSpeed Type-A male-to-Type-A male debugging cable**, with
SuperSpeed crossover and **VBUS disconnected**, explicitly rated for two-host
kernel debugging. Do not connect an ordinary powered A–A cable between hosts,
a USB2-only cable, USB2 data-transfer bridge, or charging cable. Target must use
a direct SuperSpeed root port supporting DbC; host may use a SuperSpeed hub.
This transport is not CH9329 and needs a separate host USB port/cable.
See the [Microsoft debug-cable requirements](https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/setting-up-a-usb-3-0-debug-cable-connection).

On the Linux host, user loads `usb_debug` (requires `CONFIG_USB_SERIAL_DEBUG`),
then checks enumeration and the newly created tty, not an assumed ttyUSB0:

```sh
sudo modprobe usb_debug
lsusb -d 1d6b:0010
ls -l /dev/serial/by-id /dev/ttyUSB*
# Replace ttyUSBX with the verified debug device; permissions are user-managed.
stty -F /dev/ttyUSBX raw -echo
cat /dev/ttyUSBX
# From another terminal, send a harmless shell command after the guest prompt:
printf 'echo DBC_REAL_INPUT_OK\n' > /dev/ttyUSBX
```

Identity `1d6b:0010`, interface protocol 1, is Linux's debug compatibility/test
identity recognized by `usb_debug`; **not a USB-IF VID assigned to TheKernel**.
Do not represent this personal debug device as a certified production product.
A configured host log is insufficient: require actual new kernel log bytes,
TTY prompt/output and the exact echoed command marker at the host. Stop `cat`
after testing. Disconnect intentionally: ordinary screen must remain usable,
transport must fail closed; reboot target before the next connection attempt.
If connection before target enable leaves the USB port inactive, reconnect after
enable or user-operated host warm reset; do not guess target register writes.

## Verification boundaries / references

Host `fake.rs` covers read-only bounded probing, disabled/floating/truncated
capabilities, CNR/firmware ownership, context/register ordering, backpressure,
no doorbell before RUN, actual TX payload and RX NUL/short packet bytes,
one-TD ownership, 520 completions across transfer/event cycles, device-side PED
acknowledgment, malformed pointers/residuals, errors/halt/disconnect,
enumeration/enable timeout and 32-bit address rejection. None models real USB
link training or proves N305's capability exists/routes to a particular port.
QEMU enabled-feature boot validates absence + unchanged shell/keyboard/screen;
real transport, disconnect and host `usb_debug` interoperability remain unverified.

- [Intel xHCI 1.2b specification, §7.6](https://cdrdv2-public.intel.com/625472/625472_xHCI_Rev1_2b.pdf)
  (register, DMA and state-machine facts).
- Linux 7.2.3 `drivers/usb/host/xhci-dbgcap.{c,h}`, `xhci-dbgtty.c`,
  `drivers/usb/serial/usb_debug.c` (behavior / debug-host identity).
- [Linux USB3 debug port documentation](https://kernel.org/doc/html/v6.3/driver-api/usb/usb3-debug-port.html).

Repeat the emulated absence regression (not transport validation):

```sh
THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/wt-dev \
python3 scripts/ci/usb-dbc-qemu-smoke.py
```
