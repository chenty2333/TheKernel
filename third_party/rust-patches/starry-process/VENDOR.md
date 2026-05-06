# Vendored: starry-process

## Upstream

- Source: `starry-process` v0.2.0
- Repository: <https://github.com/Starry-OS/starry-process>
- Original manifest: not present in the current tree

## History

| Commit | Description |
|--------|-------------|
| `84ce9fbd` | fix: unblock rv oscomp path through ltp |
| `1504f93a` | feat(wait): add durable child accounting and wait snapshots |

## Changes

This fork supports Linux-compatible process-group, session, and wait behavior
used by the kernel. The local history shows durable child accounting and wait
snapshots, followed by RV OSCOMP/LTP compatibility work. Upstream syncs should
preserve wait-state snapshots and child accounting semantics expected by
`wait4`, job control, and process reaping.
