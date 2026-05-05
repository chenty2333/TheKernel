# Vendored: lwext4_rust

## Upstream

- Source: `lwext4_rust` v0.2.0
- Original manifest: `Cargo.toml.orig`

## History

| Commit | Description |
|--------|-------------|
| `01834cc5` | build: fix remote make-all kernel targets |
| `bcb901f6` | chore: rebuild development environment around repo-local docker |
| `0faad4f2` | fix: harden dev image downloads and unblock la bootstrap |
| `84ce9fbd` | fix: unblock rv oscomp path through ltp |
| `1fda473c` | fix: use distro cross toolchains for lwext4 |
| `5eb799ad` | fix: harden oscomp kernel semantics and compatibility |
| `d4456f44` | fix: restore repo-local make all build parity |
| `c098032f` | fix: stabilize oscomp la evaluation flow |

## Changes

Heavily patched (8 commits). Changes cover initial remote `make all` parity,
repo-local Docker and LA bootstrap support, cross-compilation toolchain setup,
ext4 filesystem semantics hardening, and build parity fixes.
The current `Cargo.toml` is Cargo's auto-normalized form of `Cargo.toml.orig`.
