# Vendored: lwext4_rust

## Upstream

- Source: `lwext4_rust` v0.2.0
- Original manifest: `Cargo.toml.orig`

## History

| Commit | Description |
|--------|-------------|
| `84ce9fbd` | fix: unblock rv oscomp path through ltp |
| `1fda473c` | fix: use distro cross toolchains for lwext4 |
| `5eb799ad` | fix: harden oscomp kernel semantics and compatibility |
| `d4456f44` | fix: restore repo-local make all build parity |
| `c098032f` | fix: stabilize oscomp la evaluation flow |

## Changes

Heavily patched (5 commits). Changes cover cross-compilation toolchain setup,
ext4 filesystem semantics hardening, and build parity fixes.
The current `Cargo.toml` is Cargo's auto-normalized form of `Cargo.toml.orig`.
