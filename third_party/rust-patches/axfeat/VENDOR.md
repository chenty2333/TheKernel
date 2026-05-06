# Vendored: axfeat

## Upstream

- Source: `axfeat` v0.3.0-preview.2
- Repository: <https://github.com/arceos-org/arceos/tree/main/api/axfeat>
- Original manifest: not present in the current tree

## History

| Commit | Description |
|--------|-------------|
| `c098032f` | fix: stabilize oscomp la evaluation flow |

## Changes

This fork is used to keep feature selection aligned with the local ArceOS patch
set and the OSCOMP evaluator targets. The recorded local change is evaluator
stabilization, so upstream syncs should verify feature names and default
platform wiring against the root `Cargo.toml` `[patch.crates-io]` section.
