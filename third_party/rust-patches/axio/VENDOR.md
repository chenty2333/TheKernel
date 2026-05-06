# Vendored: axio

## Upstream

- Source: `axio` v0.3.0-pre.1
- Repository: <https://github.com/arceos-org/axio>
- Original manifest: not present in the current tree

## History

| Commit | Description |
|--------|-------------|
| `7beb011c` | build(toolchain): trim vendored compatibility patches |
| `21394231` | build(toolchain): patch nightly-2025-05-20 compatibility |

## Changes

This fork was introduced for toolchain compatibility with
`nightly-2025-05-20`. Later history trimmed vendored compatibility metadata and
test-only files. The current tree does not keep `Cargo.toml.orig`, so upstream
syncs should compare against the published `axio` version and re-check no-std
I/O trait compatibility used by `axfs-ng`, `axnet-ng`, and the kernel file
layer.
