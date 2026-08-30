# Vendored source record: `axalloc`

## Immutable published baseline

- Registry package: `axalloc` `0.3.0-preview.2`
- crates.io archive SHA-256: `1a10c400cbdf0f611f92fcdd6e2c658de329085d3156ba65e323da7eaa7c7aca`
- Archive URL: <https://static.crates.io/crates/axalloc/axalloc-0.3.0-preview.2.crate>
- Repository declared by the package: <https://github.com/arceos-org/arceos/tree/main/modules/axalloc>.

## TheKernel patch ledger

- Adds `UsageKind::Kexec` and `replace_pages_at`, which reserve exact pages
  under the page allocator lock for the lifetime of a loaded kexec image.
- Makes post-initialization memory regions enter the page allocator, not the
  byte heap. This preserves free-region boundaries for fixed-page reservation.
