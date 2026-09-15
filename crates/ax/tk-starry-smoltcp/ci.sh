#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd -- "$SCRIPT_DIR/../../.." && pwd)
# The root pin is authoritative for every feature combination, including
# alloc, benchmarks and lint. There is no stable/MSRV or cross-target matrix.
TOOLCHAIN=$(sed -n 's/^channel = "\([^" ]*\)"$/\1/p' "$ROOT/rust-toolchain.toml")
if [[ "$TOOLCHAIN" != nightly-* ]]; then
    echo "missing pinned nightly in $ROOT/rust-toolchain.toml" >&2
    exit 1
fi
cd -- "$SCRIPT_DIR"
export DEFMT_LOG=trace

FEATURES_TEST=(
    "default"
    "std,proto-ipv4"
    "std,medium-ethernet,phy-raw_socket,proto-ipv6,socket-udp,socket-dns"
    "std,medium-ethernet,phy-tuntap_interface,proto-ipv6,socket-udp"
    "std,medium-ethernet,proto-ipv4,proto-ipv4-fragmentation,socket-raw,socket-dns"
    "std,medium-ethernet,proto-ipv4,multicast,socket-raw,socket-dns"
    "std,medium-ethernet,proto-ipv4,socket-udp,socket-tcp,socket-dns"
    "std,medium-ethernet,proto-ipv4,proto-dhcpv4,socket-udp"
    "std,medium-ethernet,medium-ip,medium-ieee802154,proto-ipv6,multicast,proto-rpl,socket-udp,socket-dns"
    "std,medium-ethernet,proto-ipv6,socket-tcp"
    "std,medium-ethernet,medium-ip,proto-ipv4,socket-icmp,socket-tcp"
    "std,medium-ip,proto-ipv6,socket-icmp,socket-tcp"
    "std,medium-ieee802154,proto-sixlowpan,socket-udp"
    "std,medium-ieee802154,proto-sixlowpan,proto-sixlowpan-fragmentation,socket-udp"
    "std,medium-ieee802154,proto-rpl,proto-sixlowpan,proto-sixlowpan-fragmentation,socket-udp"
    "std,medium-ip,proto-ipv4,proto-ipv6,socket-tcp,socket-udp"
    "std,medium-ethernet,medium-ip,medium-ieee802154,proto-ipv4,proto-ipv6,multicast,proto-rpl,socket-raw,socket-udp,socket-tcp,socket-icmp,socket-dns,async"
    "std,medium-ip,proto-ipv4,proto-ipv6,multicast,socket-raw,socket-udp,socket-tcp,socket-icmp,socket-dns,async"
    "std,medium-ieee802154,medium-ip,proto-ipv4,socket-raw"
    "std,medium-ethernet,proto-ipv4,proto-ipsec,socket-raw"
    "alloc,medium-ethernet,proto-ipv4,proto-ipv6,socket-raw,socket-udp,socket-tcp,socket-icmp"
)

FEATURES_CHECK=(
    "medium-ip,medium-ethernet,medium-ieee802154,proto-ipv6,multicast,proto-dhcpv4,proto-ipsec,socket-raw,socket-udp,socket-tcp,socket-icmp,socket-dns,async"
    "defmt,medium-ip,medium-ethernet,proto-ipv6,multicast,proto-dhcpv4,socket-raw,socket-udp,socket-tcp,socket-icmp,socket-dns,async"
    "defmt,alloc,medium-ip,medium-ethernet,proto-ipv6,multicast,proto-dhcpv4,socket-raw,socket-udp,socket-tcp,socket-icmp,socket-dns,async"
    "medium-ieee802154,proto-sixlowpan,socket-dns"
)

run_tests() {
    for features in "${FEATURES_TEST[@]}"; do
        cargo "+$TOOLCHAIN" test --no-default-features --features "$features"
    done
}

netsim() {
    cargo "+$TOOLCHAIN" test --release --features _netsim netsim
}

check() {
    for features in "${FEATURES_CHECK[@]}"; do
        cargo "+$TOOLCHAIN" check --no-default-features --features "$features"
    done
    cargo "+$TOOLCHAIN" check --examples
    cargo "+$TOOLCHAIN" check --benches
}

clippy() {
    cargo "+$TOOLCHAIN" clippy --tests --examples -- -D warnings
}

coverage() {
    for features in "${FEATURES_TEST[@]}"; do
        cargo "+$TOOLCHAIN" llvm-cov --no-report --no-default-features --features "$features"
    done
    cargo "+$TOOLCHAIN" llvm-cov report --lcov --output-path lcov.info
}

usage() {
    echo "usage: $0 {test|check|clippy|coverage|netsim|all}" >&2
}
if (( $# != 1 )); then
    usage
    exit 2
fi
case "$1" in
    test) run_tests ;;
    check) check ;;
    clippy) clippy ;;
    coverage) coverage ;;
    netsim) netsim ;;
    all) run_tests; check; clippy; coverage; netsim ;;
    *) usage; exit 2 ;;
esac
