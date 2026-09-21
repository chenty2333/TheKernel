# Module coupling inside the kernel crate, and why `drm` is not a crate

`kernel/src` is 408 521 lines of Rust in one package, and the layer rules in
`scripts/ci/check_cargo_dependency_layers.py:11-16` can only see edges between
cargo packages.  Inside that one package any module may `use crate::<other>`
without a manifest line changing, so nothing in the existing gates could stop
the coupling this document measures: 89 module-level `use crate::…` edges across
18 modules, 15 of them reciprocal.  `scripts/ci/check_kernel_module_edges.py`
pins that set to `config/kernel-module-edges.toml`; this document records what
the measurement says, and in particular why `kernel/src/drm` — 61 274 lines —
cannot be lifted into `crates/drm` today.

It is written against the tree as it stands; every statement carries the
`path:line` it was read from.

## 1. What the gate counts

The unit is a `use crate::…` *statement*, not every path expression:

- `edges()` (`scripts/ci/check_kernel_module_edges.py:91`) walks every `*.rs`
  under `kernel/src`, masks comments and string literals with the same
  `mask_rust_noncode()` the ABI gate uses
  (`scripts/ci/linux_abi_gate.py:121`), and takes the first identifier after
  each `crate::` in a `use` head (`:72-83`).  A use tree therefore yields the
  module it names — `use crate::mm::{self as memory, SharedPages};` is the edge
  `mm`, not two edges — and `use crate::{self as k, syscall};` is `syscall`.
- The source module is the first path component under `kernel/src`
  (`:39-44`), so `drm/intel/fb.rs` and `drm/gem.rs` are both `drm`.
- A module reaching its own subtree is not coupling and is dropped (`:99`).

Because the unit is the import, retiring a baseline edge proves the `use` line
is gone; it does not prove the dependency is gone.  Two files in `drm` are
coupled to `mm` purely through inline paths and contribute nothing to the
measured edge — `kernel/src/drm/ioctl.rs:33` takes
`crate::mm::map_usercopy_error` as a function argument and
`kernel/src/drm/device.rs:1751` names `Arc<crate::mm::SharedPages>` in a
signature, neither importing it.  Deleting every `use crate::mm` line from
`drm` would therefore retire the edge and leave the coupling untouched, which
is why the gate reports a retired edge instead of trusting one.  It is a
stop-widening tripwire with a reviewable diff, not a layering proof.  To surface
these unimported dependencies, `scripts/ci/check_kernel_module_edges.py --audit-inline`
scans all inline `crate::<target>::` paths and reports coupling edges that bypass
`use` declarations.

`main()` (`scripts/ci/check_kernel_module_edges.py:125`) then compares the
measured graph against the committed one in both directions: a new edge is a
failure with the edge printed in exactly the form that would be pasted into the
baseline (`:137-153`), and an edge that has disappeared is reported as one that
"may leave the list" while still passing (`:154`) — a baseline that can only
grow is not a baseline.  Malformed entries are rejected before any of that,
because a silently unparseable edge would look like a retired one: an entry
must be one module, `->`, one module, must not repeat the crate root, and may
not repeat itself (`:104-118`).

## 2. The measured shape

`config/kernel-module-edges.toml` is the whole list, grouped by source module.
The distribution is not even:

| Module | Outgoing edges | Note |
| --- | --- | --- |
| `syscall` | 15 | the widest single module |
| `pseudofs` | 13 | including `drm` |
| `file` | 12 | reaches `syscall`, `mm`, `task`, `mounts`, `readiness`, … |
| `task` | 10 | |
| `mm` | 6 | |
| `drm` | 5 | `file`, `mm`, `pseudofs`, `task`, `test_support` |

Eight modules are sinks — `async_operation`, `config`, `jit_memory`,
`pmu_registry`, `rcu`, `seccomp_jit`, `test_support`, `time` use no other
module at all, so they are the parts of the crate a split could reuse without
taking the graph with them.

The 15 reciprocal pairs are the reason the file is called a baseline rather
than a layer map: `bpf ↔ file`, `drm ↔ pseudofs`, `file ↔ mm`, `file ↔ task`,
`file ↔ syscall`, `mm ↔ syscall`, `mm ↔ task`, `syscall ↔ task` and seven more.
Those are cycles today, tolerated and enumerated, not aspirational architecture.

## 3. Why `kernel/src/drm` is still in the kernel crate

Lifting `drm` would mean a package that `thekernel` depends on, because
`kernel/Cargo.toml:188-189` already declares the kernel package as the top
`integration` layer (`scripts/ci/check_cargo_dependency_layers.py:15`) — the
one layer allowed to depend on everything, so it can never be a dependency of
something it contains.  All five recorded edges point that way, and one of them
runs both directions at once:

- `crate::file` — `kernel/src/drm/dmabuf.rs:12` and
  `kernel/src/drm/syncobj.rs:10` both `use crate::file::{FileLike, Kstat}`,
  where `FileLike` is the kernel-wide file-object trait
  (`kernel/src/file/types.rs:612`) and `Kstat` its stat shape
  (`kernel/src/file/types.rs:30`).  A DRM object *is* a file object in this
  kernel.
- `crate::mm` — `kernel/src/drm/gem.rs:3` imports `SharedPages`, the
  address-space backend's shared page pool
  (`kernel/src/mm/aspace/backend/shared.rs:117`); `ioctl.rs:33` maps a
  `pub(crate)` error translator (`kernel/src/mm/usercopy.rs:345`).
- `crate::pseudofs` — `kernel/src/drm/screen.rs:45` and
  `kernel/src/drm/fbdev.rs:19-24` implement `ScanoutSurface`
  (`kernel/src/pseudofs/dev/scanout.rs:33`) and consume `bootfb`/`DeviceMmap`.
  `pseudofs` implements DRM traits back the other way
  (`kernel/src/pseudofs/dev/fb.rs:1162-1177`,
  `kernel/src/pseudofs/dev/tty/seat.rs:296`) and calls into the display stack
  to pick a console (`kernel/src/pseudofs/dev/mod.rs:556`) and to suspend and
  resume KMS for a seat change (`kernel/src/pseudofs/dev/tty/vt.rs:190`).  This
  pair is a cycle, and cargo would reject it at the package level.
- `crate::task` — `kernel/src/drm/file.rs:1073-1076` uses `AsThread`
  (`kernel/src/task/thread.rs:3037`) and the signal-pending check, so an
  interruptible DRM close is coupled to the scheduler.
- `crate::test_support` — `kernel/src/drm/intel/regs/mod.rs:1171` imports
  `scheduler_test_context()`, a `pub(crate)` helper
  (`kernel/src/test_support.rs:82`) that 46 sites inside `drm` name; the same
  visibility blocks the console restore call at
  `kernel/src/pseudofs/dev/fb.rs:773`, used from `kernel/src/drm/file.rs:564`.

So the extraction is not a file move plus a `Cargo.toml`.  It requires, in
order: a package for the shared page abstraction, `FileLike` and `Kstat`;
`ScanoutSurface` and its console-selection role lifted out of `pseudofs` into
that third place or inverted behind a registration call; `map_usercopy_error`
and `scheduler_test_context` promoted from `pub(crate)` to a designed public
surface; and only then a `crates/drm` whose layer declaration the dependency
gate would accept.  That is a boundary-design project, not a refactor of
`drm`, and it is why this change set ships the allowlist and not the crate.

What the gate does buy for `drm` is a floor: the five edges are now recorded, so
a sixth — a `drm` that starts using `crate::uprobe`, say — fails
`verify --tier daily` at `tools/verification.py:55` instead of arriving with a
clean `cargo build`.
