# Vendored: memory_set

## Upstream

- Source: `memory_set` (ArceOS workspace crate, version from workspace)
- Original manifest: `Cargo.toml.orig`

## History

| Commit | Description |
|--------|-------------|
| `3fe53147` | perf(mm): coalesce anon vmas and reduce fault churn |
| `6904548e` | perf(mm): add append-biased kernel area placement |
| `96c1fcfa` | fix: complete rv memfd_create semantics |

## Changes

Performance-oriented patches to memory area management: anon VMA coalescing,
append-biased placement, and memfd semantics fixes.
The current `Cargo.toml` is Cargo's auto-normalized form of `Cargo.toml.orig`.
