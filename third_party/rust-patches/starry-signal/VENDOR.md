# Vendored: starry-signal

## Upstream

- Source: `starry-signal` v0.3.0
- Repository: <https://github.com/Starry-OS/starry-signal>
- Original manifest: `Cargo.toml.orig`

## History

| Commit | Description |
|--------|-------------|
| `13915025` | fix(signal): centralize syscall restart enrollment — initial vendoring |
| `8eeab8d8` | fix: batch kernel and support compatibility updates |

## Changes

The current `Cargo.toml` is Cargo's auto-normalized form of `Cargo.toml.orig`.
No dependency or structural changes from the original.

Source modifications (from `8eeab8d8`):
- `src/api/process.rs` — +4 lines
- `src/api/thread.rs` — +4 lines
- `src/pending.rs` — +4 lines
