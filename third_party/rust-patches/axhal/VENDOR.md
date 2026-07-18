# Vendored source record: `axhal`

## Immutable published baseline

- Registry package: `axhal` `0.3.0-preview.2`
- crates.io archive: `axhal-0.3.0-preview.2.crate`
- crates.io archive SHA-256: `2b721414abb9554522acdc0495cef83eadfc7d47257fe3c979e655c982d79588`
- Archive URL: <https://static.crates.io/crates/axhal/axhal-0.3.0-preview.2.crate>
- Repository declared by the package: <https://github.com/arceos-org/arceos/tree/main/modules/axhal>.
- Cargo records exact source commit `6c6765c05df0550e31edb0ca82d468199f108b3f` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `b57d3209e36677ff8b7ab1189f76dfb208227e55ba8965c6b9a55ebeafe35341`)
- Cargo source record: `.cargo_vcs_info.json`

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`. The
published archive contains no license text; no license file is synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test
assets to restore. Local depth and hook-ownership tests cover the maintained
IRQ-boundary delta; the dev-only `percpu/sp-naive` backend makes those tests
linkable on a host without changing the bare-metal feature graph.

## TheKernel patch ledger

- Adds a bounded Layer 0 IRQ-boundary transport through the maintained `axcpu`
  fork.
- Pins `axcpu` to the exact published `0.3.0-preview.8` baseline so Cargo cannot
  silently select a newer registry implementation that lacks this transport.
- Tracks per-CPU IRQ nesting and a short outermost-exit phase in `axhal`.
- Exposes a one-owner `register_irq_exit_hook` and `in_irq_context` contract to
  the generic scheduler. The callback runs after the platform `NoPreempt` guard
  is released while local interrupts remain masked.
- This patch does not change platform IRQ acknowledgement or the existing
  device hook; Linux ABI and scheduler policy remain above this crate.

## Rebase rule

Start from the verified registry archive and reapply the IRQ-boundary ledger.
Do not treat the patch as a lockless scheduler or infer that an IRQ hook is a
substitute for platform interrupt acknowledgement.
