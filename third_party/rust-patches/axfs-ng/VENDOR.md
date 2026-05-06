# Vendored: axfs-ng

## Upstream

- Source: `axfs-ng` v0.3.0-preview.2
- Upstream family: ArceOS filesystem module
- Original manifest: not present in the current tree

## History

| Commit | Description |
|--------|-------------|
| `1f4c6b41` | fix: restore buildable cache and thread preflight |
| `8e237d39` | fix: address review findings round 3 (build, evict, tmpfs, COW, cache) |
| `f34c5900` | fix: address review findings (pipe close, thread cache, reclaim, flush) |
| `8d85afed` | fix(page-cache): wire coherence into truncate/ftruncate and fallocate |
| `2da3ee55` | feat(page-cache): coherence API and global clean-page LRU with budget |
| `1756413f` | feat(page-cache): writeback dirty pages from registry on sync/syncfs |
| `a890b85f` | feat(page-cache): add global registry for sync/syncfs on close+ dirty caches |
| `b731f090` | fix: preserve dirty flag on truncate and skip sigbus pages in clone_map |
| `0fa20b8b` | fix(mm): batch COW, loader, mmap, and file cache fixes |
| `c098032f` | fix: stabilize oscomp la evaluation flow |
| `8eeab8d8` | fix: batch kernel and support compatibility updates |
| `60d76210` | fix: harden more rv ltp signal and mount semantics |
| `5eb799ad` | fix: harden oscomp kernel semantics and compatibility |
| `ecfbcdd4` | feat: align evaluator runtime and expand syscall coverage |
| `b82f151c` | fix(fs): zero file cache tails for partial pages |
| `5c021a8c` | perf(mm): grow page cache with larger RAM |
| `bcd005b6` | feat(mm): scale caches with physical memory |
| `2d20c081` | fix(fs): normalize root path semantics |

## Changes

This fork has substantial local filesystem and page-cache behavior. The history
shows root path normalization, EXT4/FAT compatibility fixes, larger and
RAM-scaled file caches, partial-page zeroing, global dirty-page discovery,
sync/syncfs writeback, cache coherence hooks for truncate/fallocate, and clean
page LRU budgeting.

Treat this crate as a kernel-facing fork rather than a simple manifest patch.
When syncing, audit `src/highlevel/file.rs`, `src/highlevel/fs.rs`, and EXT4
inode behavior before accepting upstream changes.
