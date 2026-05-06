# Vendored: axcpu

## Upstream

- Source: `axcpu` v0.3.0-preview.8
- Repository: <https://github.com/arceos-org/axcpu/tree/dev>
- Original manifest: not present in the current tree

## History

| Commit | Description |
|--------|-------------|
| `c098032f` | fix: stabilize oscomp la evaluation flow |
| `ecfbcdd4` | feat: align evaluator runtime and expand syscall coverage |
| `58da0c11` | fix(mm): optimize loongarch user-copy path |

## Changes

This fork carries CPU-architecture glue needed by the kernel's evaluator path.
The local history shows LoongArch user-copy/trap work and later evaluator
stabilization changes. Syncing with upstream should compare architecture files
under `src/*/` first, especially `loongarch64` user access and trap handling.
