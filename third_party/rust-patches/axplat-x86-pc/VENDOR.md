# Vendored source record: `axplat-x86-pc`

## Immutable published baseline

- Registry package: `axplat-x86-pc` `0.3.1-pre.6`
- crates.io archive: `axplat-x86-pc-0.3.1-pre.6.crate`
- crates.io archive SHA-256: `9df26719c444ca8302e9366b8dc5abe8735933ea756ff094e3ac5ce3b64c41a1`
- Archive URL: <https://static.crates.io/crates/axplat-x86-pc/axplat-x86-pc-0.3.1-pre.6.crate>
- Repository declared by the package: <https://github.com/arceos-org/axplat_crates>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `811837d8c699941f43665510b6e30700faa0e633` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `a1b4f6c90f8119a881b19e0a7aca863b37c44213cfe40c70d91ec87a9e07724e`)
- Cargo source record: `.cargo_vcs_info.json` (SHA-256 `a6c697ca35be9d23f07e69b904667ef54ff6b599263d43bb4ca5709ed41e0dac`)

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the
published archive contains no license text. This is recorded as a
distribution anomaly; no license file has been synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test
assets to restore. The local focused vector mapping tests cover the maintained
IRQ change.

## TheKernel patch ledger

- Vendored the published platform source so TheKernel can maintain the x86
  console IRQ route locally.
- Initialized IOAPIC redirection entries masked, edge/high/fixed, targeting the
  BSP; COM1 uses GSI/pin 4 and vector `0x24`.
- Mapped external x86 vectors to IOAPIC pins without changing LAPIC vector
  handling.

The exact rebase baseline is the archive checksum above; the live patch is the
diff between that archive and this directory.
