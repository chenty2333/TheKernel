# Licensing map

Measured on the `dev` worktree, commit `d38a2db7` plus uncommitted WIP,
2026-09-21. This file states what the repository actually contains and what a
redistributor must do. It does not change any license expression, feature
wiring, or dependency edge; where a change is needed, it says who owns it.

## 1. License map

| Component | Declared license | Text shipped | Reality check |
| --- | --- | --- | --- |
| `kernel/` (`tk-kernel`), the product image | inherits `license.workspace` = `Apache-2.0` (`[workspace.package]` in the root `Cargo.toml`) | root `LICENSE` | **Apache-2.0 only describes the Rust sources.** The default feature set links GPL-2.0-or-later C (section 2), so the shipped binary is not an Apache-2.0-only work. |
| `crates/linux/*` (28 crates) | `Apache-2.0` | `crates/linux/LICENSE` (Apache-2.0), plus `LICENSES/Apache-2.0.txt` in all 28 | Rust is original. Comments quote Linux C verbatim (GPL-2.0-only) as cited documentation — 110 lines / 49 blocks / 20 files at the ≥ 40 scan threshold inside fenced doc blocks, 211 / 59 / 22 at ≥ 25, and 25 more verbatim lines in 5 files outside fences (`io-uring` 17, `process` 4, `fsnotify` 3, `mm` 1), all of it output of `scripts/ci/scan_linux_excerpts.py`. Nothing of Linux is compiled or linked. Each package that holds such text states it in its own `NOTICE` (11 packages), indexed in `docs/upstream-provenance.md`. |
| `crates/ax/*` ArceOS-lineage forks (30 of the 45 crates declare the ArceOS triple license) | `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0` | `LICENSES/{Apache-2.0,GPL-3.0-or-later,MulanPSL-2.0}.txt` per crate and in root `LICENSES/ax/` | Matches upstream's triple option; each crate ships all three texts, so a recipient can take the Apache-2.0 option — except that this is *not* true for the ext4 path below. Three of these crates also carry 7 verbatim Linux comment lines at ≥ 40 across 4 files, none fenced: 3 in `tk-axfs-ng/src/fs/ext4/inode.rs:1227-1229`, 1 in `tk-axfs-ng-vfs/src/mount.rs:2007`, 2 in `tk-axnet-ng/src/tcp.rs:801-802` and 1 in `tk-axnet-ng/src/unix/stream.rs:1224`; the same scan finds 8 further matches in this tree that are Rust `code` (ABI identifiers and hex arrays), not text. At ≥ 25 `tk-axfs-ng-vfs/src/nullfs.rs:8-15` adds the tree's only **fenced** Linux hunk, cited to `fs/nullfs.c`:12-34. None of those three packages carries a `NOTICE`, and `docs/upstream-provenance.md` inventories them. |
| `crates/ax/tk-lwext4-rust` | `GPL-2.0` (`Cargo.toml:30`) | `LICENSE.GPLv2` only | Two problems. (a) The vendored files say "version 2 or (at your option) any later version", i.e. `GPL-2.0-or-later`, so the declaration under-states the grant. (b) Most of the C is BSD-3-Clause per `c/lwext4/README.md:41-48` and no BSD-3-Clause text is shipped. |
| `crates/ax/tk-starry-fatfs` | `MIT` | `LICENSE.txt` | Consistent. Upstream: `Starry-OS/rust-fatfs` ← `rafalh/rust-fatfs`. |
| `crates/ax/tk-starry-smoltcp` | `0BSD` | `LICENSE-0BSD.txt` | Consistent. Upstream: `Starry-OS/smoltcp` ← `smoltcp-rs/smoltcp`. |
| `crates/ax/tk-virtio-drivers` | `MIT` | `LICENSE` | Consistent. Upstream: `rcore-os/virtio-drivers`. |
| `crates/ax/tk-scope-local` | `MIT OR Apache-2.0` | `LICENSE-MIT`, `LICENSE-APACHE-2.0` | Texts present; upstream unrecorded. |
| `crates/ax/tk-axfs-ng-vfs` | `MIT OR Apache-2.0` | `LICENSES/Apache-2.0.txt` only | An `OR` is satisfiable by shipping one option, but the description claims ArceOS lineage while the expression is StarryOS-style. Unresolved; owner needs to record provenance. |
| `crates/linux/{process,signal,usercopy}` | `Apache-2.0` | crate `LICENSE` (Apache-2.0) | Migrated from StarryOS `starry-process 0.2.0`, `starry-signal 0.3.0`, `starry-vm 0.3.0`, which are `MIT OR Apache-2.0` / Apache-2.0. Electing Apache-2.0 is allowed by an `OR`; the omitted MIT text and the unpinned follow-up commit are noted in `crates/linux/process/NOTICE`. |
| `crates/ax/tk-ax{bpf,cbpf,exec,fault,gpu,pmu,random,rcu,tlb}` | `Apache-2.0` | `LICENSES/Apache-2.0.txt` | Original; local-only. |

Root `LICENSES/ax/README.md` states that the triple-licensed crates keep their
own copies of the three texts — verified: all 30 crates that declare
`GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0` ship
`LICENSES/{Apache-2.0,GPL-3.0-or-later,MulanPSL-2.0}.txt` — and that the other
component licenses "remain in the respective crate directories with their
original notices". That second sentence has two exceptions, which the file now
names: `tk-lwext4-rust` carries no BSD-3-Clause text for the majority of its
vendored C, and `tk-axfs-ng-vfs` declares `MIT OR Apache-2.0` while shipping
only the Apache-2.0 text.

## 2. The ext4 / GPL combination, stated plainly

* What is GPL: the vendored C in `crates/ax/tk-lwext4-rust/c/lwext4` —
  31 `.c` files (21,629 lines) plus 32 headers (7,258 lines), 28,887 lines in
  all (`find c/lwext4 -name '*.c' -exec cat {} + | wc -l`, and the same for
  `*.h`, from the crate root). `src/ext4_xattr.c` and
  `src/ext4_extent.c` are GPL-2.0-or-later; the rest is BSD-3-Clause, and
  upstream states that the GPLv2 files "make whole lwext4 GPLv2 licensed"
  (`c/lwext4/README.md:41-48`).
* How it is reached: `kernel/Cargo.toml` lists `"fs-ng-ext4"` among the features
  of its `axfeat` dependency → `crates/ax/tk-axfeat/Cargo.toml:50-53`
  (`fs-ng-ext4 = ["fs-ng", "axfs-ng/ext4"]`) →
  `crates/ax/tk-axfs-ng/Cargo.toml:32` (`ext4 = ["dep:lwext4_rust"]`, pulling
  the `optional = true` dependency declared at `Cargo.toml:149-154`) →
  `tk-lwext4-rust`, whose `build.rs:30-54` runs `make musl-generic` over the
  vendored tree — that target configures with CMake and builds
  `src/liblwext4.a` (`c/lwext4/Makefile:43-46`) — and links the resulting
  archive (`build.rs:436`).
* Which gate controls it, and what a non-GPL build would take: the single
  `"fs-ng-ext4"` entry in `kernel/Cargo.toml`'s `axfeat` feature list. **There
  is no non-default build recipe that omits this C, and no flag is missing.**
  Workspace-wide, `fs-ng-ext4` occurs in exactly two manifests: that entry and
  its definition in `tk-axfeat` (grep of every `Cargo.toml` on 2026-09-21). It
  is not a named `tk-kernel` or root `thekernel` feature, so
  `--no-default-features` and `--features` cannot select or deselect it; it is
  unconditional in the product build. `tools/thekernel.py` builds the product
  with `cargo build --locked --package thekernel --bin thekernel --target
  x86_64-unknown-none --release --features $(kernel_features)`, where
  `kernel_features()` emits `x86-product` plus variant-selected features and
  none of them controls ext4 (`tools/thekernel.py:259-278,70`,
  `TARGET = "x86_64-unknown-none"` at `tools/product_state.py:16`). So the
  route to an image without lwext4 is a one-line manifest edit — delete the
  `"fs-ng-ext4"` line from `kernel/Cargo.toml` — and then the same command
  builds; that edit belongs to the owner of `kernel/Cargo.toml`, not here, and
  this document does not make it. `tk-axfs-ng` is written so that edit does not
  leave dangling references: `src/fs/mod.rs` has an `else` arm supplying a stub
  `DefaultFilesystem` when neither `ext4` nor `fat` is on, and every
  `lwext4_rust` reference outside `src/fs/ext4/` (which is itself behind
  `#[cfg(feature = "ext4")]`) sits under `#[cfg(feature = "ext4")]`, with
  `#[cfg(not(feature = "ext4"))]` fallbacks at `src/fs/mod.rs:52-60`,
  `src/highlevel/file.rs:7588-7591` and `src/lib.rs:115-119`. I verified those
  guards by reading the source; I did not compile a non-ext4 image, so the
  command above is quoted from the build tool, not asserted as a recipe I ran.
* Consequence: every default TheKernel image is a work that combines Apache-2.0
  code with GPL-2.0-or-later code. Apache-2.0's `LICENSE` and root `NOTICE`
  grant is not a licence for that combined work. Distributing the image requires
  satisfying GPL-2.0-or-later for the C and for the combined binary: source
  availability for the whole work that links it, license-text retention, and no
  further restrictions on the recipient's right to modify and redistribute it.
* So the split is: the code supports a non-ext4 build, the product's feature
  request does not. Until the manifest line above is removed by its owner,
  every image this repository builds is the combined case.
* The alternative to feature-gating, if the intent is to keep ext4 on by
  default: `c/lwext4/README.md:41-43` says the GPLv2 files can be *removed* to
  use the library as BSD-3-Clause. Both removed files are load-bearing here
  (extents and xattrs), so that route is not available without dropping ext4
  features; it is recorded so the choice is made knowingly rather than by
  default.

## 3. Third-party materials inventory

1. **Linux 7.2.3** (GPL-2.0-only, (C) The Linux Kernel Authors) — quoted C in
   Rust comments of `crates/linux/**` (110 lines / 49 blocks / 20 files at ≥ 40)
   and `kernel/src/**` (89 / 43 / 19, outside any `NOTICE`), and in
   `crates/ax/**` (7 comment lines at ≥ 40 in 3 crates, 11 plus a 4-line fenced
   `nullfs.rs` hunk at ≥ 25), never compiled or linked. Attribution is by Linux file
   in every case; a line range accompanies only part of it (18 of the 49 fenced
   `crates/linux` blocks counted at ≥ 40 carry an `Excerpt: Linux v7.2.3
   <file>:<lines>` marker — 21 marker lines in source, 20 of 59 blocks at the ≥ 25
   threshold — 1 more has a matching range in prose, and 30 name the file and
   Linux's enclosing function only), as `docs/upstream-provenance.md` indexes it.
   Every range stated next to its own quotation — 55 cites in `crates/linux`, 17
   in `kernel/src` — is resolved against the reference tree by
   `scripts/ci/audit_linux_range_cites.py`, and
   `tests/ci/test_linux_excerpt_baseline.py` pins that reach along with the three
   candidate rows the tool still prints, which
   `docs/upstream-provenance.md` names and dismisses. A range with no quotation
   beside it is outside that instrument; those were re-read against the tree by
   hand, and the same file records what kinds of defect that turned up.
2. **lwext4** (GPL-2.0-or-later for two files / BSD-3-Clause for the rest,
   (C) Grzegorz Kostka, Kaho Ng, and contributors) — vendored C, **compiled and
   linked** into the default image. Also `c/ext_images.7z`, a binary test
   archive carrying no license statement of its own.
3. **ArceOS** and the `arceos-org` split repositories (GPL-3.0-or-later OR
   Apache-2.0 OR MulanPSL-2.0) — forked into `crates/ax/tk-ax*`; each keeps its
   own three license texts.
4. **StarryOS** (MIT OR Apache-2.0) — `tk-starry-fatfs`, `tk-starry-smoltcp`
   (forks), and `crates/linux/{process,signal,usercopy}` (migrations).
5. **smoltcp** (0BSD), **rust-fatfs** (MIT), **virtio-drivers** (MIT,
   rcore-os), **kernel-elf-parser** (MIT/Apache-2.0/MulanPSL-2.0, Azure-stars),
   **percpu** and the `ax*` registry crates — the latter group taken from
   crates.io and pinned by `Cargo.lock`.
6. **FreeBSD / Zircon / Asterinas / gVisor / liburing / libpcap** — named in
   `crates/linux/*/NOTICE` as consulted for semantics; none is a build input.
   Whether their text reached the repository is a claim I could not test, and
   this file does not make it: the only upstream source available for scanning is
   the Linux 7.2.3 tree, and no reference copy or revision of these six is
   recorded anywhere, so neither a consulted version nor a verified absence can
   be stated. The per-package denials in `crates/linux/*/NOTICE` are the authors'
   statements about what they read, not scan results;
   `docs/upstream-provenance.md` says the same.

## 4. What a distributor must ship

For any build with `fs-ng-ext4` enabled (today: every build):

* Complete corresponding source for the whole combined work, including the
  `crates/ax/tk-lwext4-rust/c/lwext4` tree and the Rust sources that link it,
  under terms that do not restrict the recipient's GPL rights.
* `LICENSE.GPLv2` plus the lwext4 copyright headers, **and** the BSD-3-Clause
  text (currently missing) for the non-GPL files, with their copyright lines
  intact.
* `LICENSES/ax/{Apache-2.0,GPL-3.0-or-later,MulanPSL-2.0}.txt` and each crate's
  own `LICENSES/`, plus the `NOTICE` files, per Apache-2.0 §4(d).
* The `c/ext_images.7z` blob or an explicit statement that it is not part of the
  distributed build.
* A copy of root `NOTICE` and this file, since the Linux excerpts are attributed
  there.

For a build without `fs-ng-ext4` — not reachable by any flag today; it requires
the one-line `kernel/Cargo.toml` change section 2 describes, which its owner
holds:

* Apache-2.0 covers the Rust tree; the ArceOS triple license still requires
  shipping the option the recipient takes (the Apache-2.0 text is present in all
  30 crates that declare `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, and
  root `LICENSES/ax/` holds the other two).
* The Linux comment excerpts remain GPL-2.0-only attribution obligations:
  ship the `NOTICE` files and `docs/upstream-provenance.md` alongside the
  sources. Quoted comments in source files must not be stripped, since stripping
  them would remove the attribution that makes them lawful citation.
* MIT / 0BSD / BSD components still require their license texts (all present
  except the lwext4 BSD-3 text above, and except the `MIT` half of
  `tk-axfs-ng-vfs`, which ships only its Apache-2.0 option).

## 5. Known license-expression defects (not fixed here)

* `crates/ax/tk-lwext4-rust/Cargo.toml:30` — `license = "GPL-2.0"` should be
  `GPL-2.0-or-later` to match the vendored headers.
* No BSD-3-Clause text anywhere for the majority of the vendored lwext4 C.
* `kernel/Cargo.toml` carries no `license` of its own beyond the workspace
  `Apache-2.0`, which cannot describe a binary that links GPL-2.0-or-later code.
* 22 forked manifests (first lines of `crates/ax/*/Cargo.toml`, e.g.
  `crates/ax/tk-lwext4-rust/Cargo.toml:1`) are cargo-normalized `cargo package`
  artifacts committed into the tree, so their dependency tables describe a
  hypothetical crates.io publication rather than this workspace.
* Exactly 2 of the 75 publishable packages ship no license text at all:
  `crates/process-adapter` (`tk-linux-process-adapter`) and
  `crates/readiness-adapter` (`tk-readiness-adapter`), both
  `publish = ["crates-io"]` and `license = "Apache-2.0"` (`Cargo.toml:7` and
  `:9` in each), neither holding a `LICENSE`, a `LICENSES/` directory, nor a
  `NOTICE`, unlike their `crates/linux` peers. Every other publishable package
  ships at least its own license text. Publishing them would distribute an
  Apache-2.0 grant whose text is absent, so the fix is one file per crate; its
  owner is whoever publishes the manifests. (`kernel/Cargo.toml:2` is
  `publish.workspace = true`, and `[workspace.package] publish = false`, so the
  kernel itself is not in this set.)
* `crates/readiness-adapter/Cargo.toml:15` sets `[lib] name = "axpoll"`, the
  upstream ArceOS crate's own lib name, in a package that declares no upstream
  and ships no notice. A reader who stops at the manifest sees `axpoll` from a
  TheKernel manifest and concludes fork; `docs/upstream-provenance.md` records
  the package as original, so the name and the provenance disagree and only one
  of them can be right.
