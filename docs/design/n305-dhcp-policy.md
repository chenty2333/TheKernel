# N305 starts without QEMU's static IPv4 configuration

The 2026-10-04 hardware report established that the kernel preconfigured eth0
with 10.0.2.15/24 and gateway 10.0.2.2 before DHCP. This was the product tool's
unconditional AX_IP/AX_GW Slirp defaults, not an address supplied by dnsmasq.
The N305 build now supplies **empty** boot address and gateway; q35-uefi keeps its
existing static Slirp defaults. User-configured DHCP/manual addressing owns N305
eth0. No host network, firewall or DHCP service is changed.

The network stack supports absent boot IPv4: the Ethernet ARP sender has an
internal unspecified placeholder, but this is not published as an interface
address, smoltcp local address or subnet/default routing rule. DHCP AF_PACKET
still has the real MAC and an up interface. A gateway without an address is
rejected; explicit static address without a gateway remains a distinct supported
configuration. IPv4 loopback and IPv6 loopback behavior is unchanged.

The lease callback flushes **only the leased interface's IPv4 routes before**
flushing/replacing its addresses, then installs the new connected route and an
optional gateway. This has the same per-interface ownership as its existing
address flush, and never flushes unrelated NIC routes. Old lease subnet routes
must disappear too when the lease's address changes. An absent router option
never reinstalls a default.

Why the old `ip route del default` failed: the current kernel's route mutation
API requires OIF and an exact rule; the bare command lacks interface/gateway,
and after address replacement it also supplies the new preferred source rather
than the old route's source. Standard BusyBox `route flush dev` obtains exact
route attributes from a dump. Flushing before source replacement makes those
attributes match. This change does not pretend to implement generic Linux
wildcard route deletion or repair unrelated netlink APIs.

## Measured validation

Host tests cover empty/static/no-gateway parsing and target-specific tool
settings. A stubbed guest lease hook checks interface scope, ordering and absent
vs multiple router options. The real KVM regression uses N305 with VirtIO NIC:
no IPv4/default initially, real Slirp DHCP to 10.0.2.15, two actual host replies,
then a no-router lease change to 192.168.10.15 with no default/old Slirp address
or subnet route. Only this last step is a simulated lease callback; it is not a
claim that QEMU's DHCP server offered a router-free lease. Run:

```sh
THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/wt-dev \
python3 scripts/ci/n305-dhcp-qemu-smoke.py
```

**The revised policy/hook has not been retested on N305 hardware.** Rebuild the
paired kernel and rootfs (the hook is a rootfs cache input), prepare PXE with
`n305.net=dhcp`, then check `ip -4 addr show dev eth0` and `ip route` after the
reported lease. With dnsmasq's no-router offer, there must be no `default via
10.0.2.2`, no 10.0.2.15 address, and no QEMU subnet route. Same-subnet ping and
netconsole must still deliver real packets. Do not compensate by adding a guessed
Windows/NVMe route or changing host networking from this agent.
