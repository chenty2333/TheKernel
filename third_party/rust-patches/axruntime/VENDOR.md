# Vendored: axruntime

## Upstream

- Source: `axruntime` v0.3.0-preview.2
- Repository: <https://github.com/arceos-org/arceos/tree/main/modules/axruntime>
- Original manifest: not present in the current tree

## History

| Commit | Description |
|--------|-------------|
| `c098032f` | fix: stabilize oscomp la evaluation flow |
| `af9d9127` | fix: align evaluator output and quiet default logs |
| `5f433f06` | fix(timer): program early monotonic wakeups |

## Changes

This fork carries runtime startup and evaluator-facing output behavior. The
recorded changes include quieter default logs, evaluator output alignment, early
timer wakeup programming, and LA/RV evaluator stabilization. Syncing should
preserve boot ordering, timer initialization, and console output expected by
the competition runner.
