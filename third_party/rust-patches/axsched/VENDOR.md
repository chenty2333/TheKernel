# Vendored: axsched

## Upstream

- Source: `axsched` v0.3.1
- Repository: <https://github.com/arceos-org/axsched>
- Original manifest: not present in the current tree

## History

| Commit | Description |
|--------|-------------|
| `c098032f` | fix: stabilize oscomp la evaluation flow |
| `8eeab8d8` | fix: batch kernel and support compatibility updates |
| `5eb799ad` | fix: harden oscomp kernel semantics and compatibility |
| `73f2f01a` | perf(task): safely reintroduce per-cpu stack cache |
| `a6f1fb23` | feat(sched): add RT policy support for scheduler syscalls |
| `f809f7a4` | fix(sched): harden runtime scheduler state updates |
| `104d9884` | feat(sched): add runtime CFS classes and scheduler state |

## Changes

This fork extends the scheduler layer for Linux-compatible scheduler syscalls
and OSCOMP behavior. Local history shows runtime CFS classes, RT policy support,
hardening around scheduler state updates, and compatibility adjustments. Syncs
should review `src/cfs.rs`, `src/round_robin.rs`, and scheduler tests together
with `axtask` run-queue changes.
