# Vendored: starry-vm

## Upstream

- Source: `starry-vm` v0.3.0
- Repository: <https://github.com/Starry-OS/starry-vm>
- Original manifest: not present in the current tree

## History

| Commit | Description |
|--------|-------------|
| `7beb011c` | build(toolchain): trim vendored compatibility patches |
| `21394231` | build(toolchain): patch nightly-2025-05-20 compatibility |

## Changes

This fork was added for compatibility with the pinned Rust nightly and the
kernel's virtual-memory dependencies. Later history trimmed vendored
compatibility metadata and tests. Since the current tree has no
`Cargo.toml.orig`, syncs should compare with the published `starry-vm` source
and confirm allocator/thin-vector APIs still match the kernel memory subsystem.
