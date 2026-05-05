# Vendored: page_table_multiarch

## Upstream

- Source: `page_table_multiarch` (ArceOS workspace crate, version from workspace)
- Original manifest: `Cargo.toml.orig`

## History

| Commit | Description |
|--------|-------------|
| `8bc0ca13` | perf(mm): skip sparse cow leaf scans |
| `ea861256` | perf(mm): batch cow unmaps through page-table drain |
| `b8cf2eb7` | fix kernel affinity, futex, and runner syncs |
| `8f8bc135` | perf(mm): skip tlb flushes for inactive page tables |

## Changes

Performance-oriented patches to page table operations: TLB flush optimization,
COW (copy-on-write) unmap batching, and sparse leaf scan skipping.
The current `Cargo.toml` is Cargo's auto-normalized form of `Cargo.toml.orig`.
