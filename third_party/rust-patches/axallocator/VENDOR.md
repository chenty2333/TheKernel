# Vendored source record: `axallocator`

## Immutable published baseline

- Registry package: `axallocator` `0.2.0`
- crates.io archive SHA-256: `3894f6027940d4b013f1d1f9e2e61b47a9e4a7dbf1a0ba10dd33e7bb265ea733`
- Archive URL: <https://static.crates.io/crates/axallocator/axallocator-0.2.0.crate>
- Repository declared by the package: <https://github.com/arceos-org/axallocator>.

## TheKernel patch ledger

- Extends the bitmap page allocator with checked, non-overlapping multi-region
  insertion. A persistent managed bitmap rejects overlap after pages become
  allocated, so fixed kexec reservations cannot reopen an owned range.
