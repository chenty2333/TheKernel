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

### Original DbC transport (2026-10-04)

`tk-axdriver-dbc` is original Apache-2.0 Rust; the license text is
`LICENSES/Apache-2.0.txt`. Linux debug VID/PID compatibility constants and Intel
xHCI §7.6 register/DMA facts are not a copied driver implementation or license
grant for a USB vendor identity. No Linux code/excerpts were added; no firmware
blob is needed by DbC. See `docs/design/usb-dbc.md` for scope and references.

### Optional inspect-tool payload (2026-10-05)

`scripts/build-inspect-payload.sh` stages unmodified signed Alpine 3.24 x86_64
packages, using the existing 3.24.1 minirootfs pin and its trust keys. The exact
package versions and SPDX expressions are in `config/inspect-apks.lock`, also
shipped as the payload MANIFEST. No APK scriptlets run on the host. This is an
optional userspace payload, not Rust linked into the kernel or a host install.

Main licenses: BusyBox GPL-2.0-only; procps/sysstat/htop/pciutils/usbutils and
iproute2/net-tools GPL family; util-linux programs GPL/BSD/public-domain and
libraries LGPL (per-package expressions in the lock); musl MIT; ncurses MIT.
Packaged data/notices in usr/share are retained. Redistributors must also supply
the applicable license texts and corresponding GPL/LGPL sources, including
Alpine's packaging changes (Alpine aports v3.24 APKBUILDs and their referenced
sources). The version/license lock is not a substitute for corresponding source.
TheKernel's PTY runner and probe script are original Apache-2.0 work.

The inspect payload also includes eudev's signed hardware-name data and an
isolated compiled `etc/udev/hwdb.bin`. APK scripts remain disabled; the pinned
staging `udevadm hwdb --update --root STAGING` performs only offline database
compilation. It does not run udevd, control/trigger host devices, or install host
configuration. eudev/eudev-hwids are GPL-2.0-or-later; kmod-libs LGPL-2.1-or-later;
xz-libs has the exact mixed SPDX expression in the lock. Hardware database
source files remain packaged under usr/lib/udev/hwdb.d, and corresponding-source
obligations include eudev and Alpine's data/packaging changes.

## CrabUSB cached-observation integration

`crates/vendor/crab-usb` is the existing crates.io CrabUSB 0.11.0 dependency,
selected through one Cargo patch rather than a parallel implementation. Only
read-only addressed-device/cache observation methods were added; the controller
command and class-driver implementations are not replaced. The upstream package
manifest declares Apache-2.0, while its packaged `LICENSE` contains an MIT grant
from Quancheng Laboratory Innovation Center (2024). Both the declared metadata
and the original license/attribution are retained; this discrepancy is recorded,
not silently relabeled. TheKernel additions are original Apache-2.0 code. No
Linux C implementation was copied or translated for this interface.

## Optional B2 container payload

`scripts/build-containers-payload.sh` stages exactly the signed Alpine 3.24.1
x86_64 closure in `config/containers-apks.lock`; each package's SPDX expression
comes from verified APK metadata. Key programs: bubblewrap (LGPL-2.1-or-later),
crun (GPL-2.0-or-later AND LGPL-2.1-or-later), podman/conmon (Apache-2.0), fuse-overlayfs
(GPL-2.0-or-later), shadow-subids helpers (BSD-3-Clause), musl (MIT), and
BusyBox (GPL-2.0-only). The complete lock records dependencies and mixed grants;
shipped upstream license/data files are retained in the optional payload.
These distribution binaries are not linked into the kernel. Installer scripts,
triggers and host ownership changes are disabled; no host container is started.
The separate BusyBox OCI root retains its dynamic loader; the offline Alpine
image is constructed from the already pinned minirootfs release, with only the
content digests required by the image format, not an extra provenance archive.
The original Apache-2.0 builder/tests never replace baseline init or accounts.

The container image restores shadow-subids' original signed-package binary
file-capability attributes inside its offline ext4 image. The host staging tree
is not made privileged, and the original helper binaries remain unmodified.

## Optional Alpine debug payload (Codex A, 2026-10-05)

The `debug` payload stages unmodified signed Alpine v3.24 x86_64 packages using
an Alpine 3.24.1 minirootfs bootstrap. `config/guest-debug-apk-pins.tsv` records
the exact package versions, source URLs, reproducibility checksums, SPDX license
expressions and origins from Alpine's signed package metadata. Native apk verifies
signatures before extraction; no package scripts or host installation run.

GDB 16.3-r4 declares GPL-3.0-or-later AND LGPL-3.0-or-later; strace 6.19-r1 declares
BSD-3-Clause; musl declares MIT and Python PSF-2.0. Dependencies retain their
original package payloads/notices, with their individual declarations recorded in
the pin file copied into the guest. This is optional userspace, not Rust kernel
source or a change to the kernel license. Redistribution must satisfy each
package's license and corresponding-source requirements where applicable;
Alpine package origins identify the build recipes and upstream source projects.

### CPU power diagnostic payload

The debug guest optionally includes unmodified, Alpine-signed `cpupower`
7.1.5-r0 (GPL-2.0-only, origin linux-tools) and lm-sensors3.6.0-r5
(GPL-2.0-or-later/LGPL-2.1-or-later), with their musl/libcap/libintl/libnl3/
pciutils/sysfsutils runtime closure. Exact package/source-origin/license rows
are in `config/guest-power-apk-pins.tsv`; `scripts/build-power-payload.sh`
verifies pins and Alpine signatures before extraction, using the same checksum-pinned
Alpine3.24.1 bootstrap as the debugger payload. No package maintainer scripts,
host installation, sensors-detect or kernel-linked GPL code are introduced.
Corresponding sources are the Alpine v3.24 aports linux-tools/lm-sensors and
listed dependency origins; upstream versions and Alpine package revisions are
preserved in the manifest. `/opt/thekernel-tools/POWER-PACKAGES.tsv` accompanies
the binaries. The new kernel Rust only implements architectural facts/behavior.

## ACPICA integration (2026-10-05)

`crates/ax/tk-acpica/vendor/{components,include}` contains unchanged ACPICA
20260930 from the [official release](https://github.com/acpica/acpica/releases/tag/20260930).
The repository now redirects to `open-acpica/acpica`. The release archive is
`acpica-unix-20260930.tar.gz`; its SHA256 was verified against the GitHub
release-asset digest: `aa18901b92e30749be0edc3081c8d550c61fce4fa37546fc6a65d367a4ae71a5`.
The elected option is **BSD-3-Clause**, with the original Intel/contributor
copyright headers, `LICENSE.BSD-3-Clause`, and `NOTICE` retained in the crate.
The Rust adapter and TheKernel C/platform glue are original Apache-2.0 code.
Generated include copies insert the TheKernel platform configuration; vendored
files are not edited. Neither Linux-tree ACPICA nor FreeBSD/Haiku code is copied.
Firmware tables (including OEM AML and MSDM keys) are never repository inputs.

### ACPICA guest inspection payload

`--toolchain acpica` stages static `acpidump` and `iasl` from the same verified
ACPICA 20260930 release archive, with its BSD-3-Clause notice. The builder is
`scripts/build-acpica-payload.sh`; the guest downloads nothing. The build uses
the supplied host C compiler (default GCC) and its static C library, just like
the baseline rootfs tool build. Binary redistribution must also satisfy that
C library's license (the default host glibc is LGPL-2.1-or-later); this payload
is not claimed to be BSD-only. The archive and generated tool sources remain
in the external state cache, not the kernel's vendored runtime tree.
The optional payload and the merged baseline image each use 160 MiB. It contains no OEM firmware tables.

## MIT i915 display translation (Codex D, 2026-10-05)

`crates/ax/tk-intel-display` is MIT Rust translated from the individually checked
MIT files of Linux 7.2.3 i915, not part of the earlier “original Rust only” rule.
Its `NOTICE` lists the exact included functions; each translation file names
its source and retains the upstream copyright. `LICENSE-MIT` ships the full
permission/warranty text. Currently included: ADL-P/N device/stepping selection,
VBT/BDB routing and HDMI capabilities, and read-only OpRegion VBT discovery.
No GPL ACPI/trace implementation, DRM C core, GT or firmware is bundled by this
crate. Private N305 BIOS/EDID captures are external test inputs, not distributed
MIT fixtures. Hardware behavior remains unverified.

The readout slice additionally translates MIT `intel_display.c` timing/source
functions and selected `intel_display_regs.h` register fields, with their
2006–2007 / 2025 Intel copyrights retained in source and crate NOTICE. The
kernel adapter remains original Apache-2.0 Rust; linking the MIT crate does
not change the existing product's GPL obligations described above.

Additional MIT arithmetic translations: selected DKL HDMI/no-SSC functions in
`intel_dpll_mgr.c`, selected fields from `intel_{mg,dkl}_phy_regs.h`, ADL-P
B0+/ADL-N D0 table selection and display-13 pixel-rate minimum in `intel_cdclk.c`.
Each has original attribution and the crate's permission-text reference.
Tests may compile local MIT source into temporary host oracles; no Linux C
source or firmware is added to the repository by that testing workflow.

GT firmware assessment inputs (not part of the repository/product): the external
refs directory contains host linux-firmware `tgl_guc_70.bin`, `tgl_huc.bin` and
comparison-only `adlp_guc_70.bin` with `LICENSE.i915`. These are Intel binary
firmware under that separate grant, not MIT and not loaded. N305's i915 GuC
selection follows its ADL-N→ADL-S override (tgl, not adlp). Temporary C oracle
copies carry the complete crate MIT grant/source copyrights and are removed
when the test completes. No GPL HDA/GT translation is shipped in this work.

The conventional host-oracle `ARRAY_SIZE` sizeof expression produces one
additional scanner code-line match at thresholds25/40, not a copied prose block
or an imported C driver. The measured inventory and its CI baseline are updated
in provenance and the crate NOTICE; the scanner remains enabled and unchanged.

The DKL access/readout slice additionally translates MIT `intel_dkl_phy.c`
(`dkl_phy_set_hip_idx`, `intel_dkl_phy_{read,write,rmw,posting_read}`) and
`intel_dpll_mgr.c::dkl_pll_get_hw_state` into `dkl_phy.rs` / `dpll_mgr.rs`.
The 2022 / 2006–2016 Intel copyrights and permission-text references accompany
source; selected display register fields retain the 2025 attribution in NOTICE.
The local C oracle uses the same temporary-source grant, not a shipped C driver.

Plane reconstruction translates MIT `skl_universal_plane.c` format and initial
plane readout/stride helpers, ADL-P main-plane tile helpers from `intel_fb.c`,
and selected encodings in `skl_universal_plane_regs.h` / `drm_fourcc.h` into
`universal_plane.rs`. Original 2020, 2021, 2024 and 2011 Intel copyrights are
preserved in source, NOTICE and the bundled MIT text. No DRM core is imported.

Color discovery translates individually MIT-licensed `intel_color.c` (2016
Intel) configuration, CSC, LUT readout and packing helpers plus selected
`intel_color_regs.h` encodings (2023 Intel) into `src/color.rs`. Source and
NOTICE retain the exact function inventory and original copyrights; the bundled
MIT grant covers the translation and temporary local C oracles. No color
commit/programming code or GPL tracing is imported by this slice.

The color-oracle shim contributes four normalized scanner matches at25 (one
at40): conventional min/length expressions and MIT color-state declarations.
These are registered in crate NOTICE/provenance and CI inventory, without a
scanner exemption or an added GPL implementation.

Scaler discovery additionally translates MIT `skl_scaler.c` pipe scaler state
and configuration getters (2020 Intel) with selected `intel_display_regs.h`
fields (2025 Intel) into `src/scaler.rs`. Permission/copyright references and
function inventory accompany source/NOTICE; no GPL display trace is imported.

Watermark discovery translates MIT `skl_watermark.c` display13 WM/DDB decoders
and getters plus enabled-DBUF-slice readout (2022 Intel) into `watermark.rs`.
Register fields come from MIT `skl_universal_plane_regs.h`, `intel_cursor_regs.h`
(2024 Intel), and `skl_watermark_regs.h` (2023 Intel). Source/NOTICE preserve
original attribution and the shared MIT grant. No WM programming imported yet.

TC discovery translates selected MIT `intel_tc.c` ADL-P readiness/ownership,
modular-FIA and legacy pin/lane fields (2019 Intel), with display/DKL/MG register
facts (2025/2022 Intel) into `tc.rs`. The 2019 copyright is added to the bundled
MIT grant and source/NOTICE. Extra DKL before-image collection is original glue,
not an imported PHY connect/ownership implementation or GPL code.

DDI discovery translates selected HDMI/DVI control fields and TC clock-enabled /
PLL-selection helpers from MIT `intel_ddi.c` (2012 Intel), with selected MIT
`intel_display_regs.h` fields (2025 Intel), into `ddi.rs`. Source, NOTICE and the
bundled grant retain attribution. No DDI programming or DP software core added.

HDMI discovery translates display13 packet-enable/GCP/DIP reads and the hardware
ECC-hole layout from MIT `intel_hdmi.c` (Dave Airlie 2006; Intel 2006–2009),
with selected `intel_display_regs.h` fields (2025 Intel), into `hdmi.rs`.
`hdmi_packet.rs` translates selected AVI/SPD/vendor/HDR decode/checksum helpers
from MIT `drivers/video/hdmi.c` and `include/linux/hdmi.h` (Avionic Design 2012).
Their exact grant has a non-infringement disclaimer and is bundled separately
as `LICENSE-HDMI-MIT`, not replaced with a different standard-MIT disclaimer.
The C-oracle helper includes both complete grants. Source/NOTICE inventory names
all included functions; no GPL or HDMI/audio programming code is introduced.

Seven new HDMI-oracle scanner matches at25 are conventional min and HDMI size /
OUI constants. Registered in NOTICE/provenance and CI; at40 counts are unchanged.
These do not import a C driver, GPL body or private capture fixture.

The native N305 fastboot call chain uses the MIT pipe/VRR readout translation
in `crates/ax/tk-intel-display/src/pipe_config.rs`; functions/headers and original
Intel grants are listed in its NOTICE. The kernel adapter, power-request pin,
GGTT binding ownership and pageflip recovery policy are original MIT code.
No GPL early-quirks code is copied for the independently decoded GMS facts.

`crates/ax/tk-intel-gt` ports the MIT N305 Gen12 forcewake/reset path from
`intel_uncore.c`, `gt/intel_engine_cs.c`, `gt/intel_reset.c` and selected
`intel_{gt,engine}_regs.h` fields. Its LICENSE-MIT retains the exact uncore
permission grant, including the next-paragraph clause, and Intel original
copyrights; NOTICE lists functions and safety differences. Independent C
oracles extract immutable local MIT functions into temporary builds under
`wt-intel`, remove them afterwards, and preserve the full grant. No firmware,
GPL compatibility layer or foreign capture is bundled.
Its C shim's single conventional `ARRAY_SIZE` match is separately inventoried
at25/40 in NOTICE/provenance and the CI baseline, without exemptions.

The same `tk-intel-gt` grant now covers selected Gen12 system PPGTT, BCS LRC/
indirect/predicate WAs, linear fast-copy and flush/breadcrumb/WA-tail routines
from MIT `gt/{gen8_ppgtt,intel_lrc,gen8_engine_cs}.c/.h`, command/LRC headers,
`gem/selftests/i915_gem_client_blt.c`, and required shared UC/MCR/GT policy from
`gt/{intel_gtt,intel_mocs,intel_workarounds,intel_sseu,intel_gt_mcr}.c`.
NOTICE inventories exact functions and original copyrights (2003-2018,
2014/2014-2018/2015/2019/2020/2022). Native memory/PCI/DMA/result-retirement
adapter is original MIT; no arbitrary user batches, GPL or binary firmware.

`tests/guest/graphics/intel-bcs-smoke.c` is original MIT user acceptance code
using public i915/DRM wire facts. It is installed by the existing graphics
image builder; it does not bundle a driver, binary firmware or a CPU-copy
replacement. Compilation/usage checks are not native hardware validation.

The N305 RCS source context/ring/reset/workaround selections remain under
`tk-intel-gt/LICENSE-MIT` with exact functions/copyrights in NOTICE. The bounded
RCS shader/state page and corresponding acceptance header are from licensed
Intel-hosted IGT backport/v6.17 source, with the full original multi-author
COPYING retained as `tk-intel-gt/LICENSE-IGT`. The original test-only C shim
extracts unchanged selected IGT functions, retains that full grant and performs
no DRM ioctl or GPU action. Mesa26.1.2 Gen120 packet/cache fields are used as
wire facts, not a copied driver/compiler. No firmware or GPL driver is bundled.

GT capability discovery additionally translates selected MIT `intel_sseu.c`
Gen12 fuse readout (©2019 Intel) and `intel_gt_clock_utils.c` Gen11+ clock
readout (©2020 Intel), with full existing `tk-intel-gt/LICENSE-MIT` grant.
The per-file context/UAPI transport is original code using header facts;
no GPL query/context implementation body is copied. Independent temporary C
oracles preserve the existing full grants and source copyrights.

The producer-fence ownership fix in DRM syncobj/fence is original Rust.
Its source-only host oracle compiles unchanged selected MIT `drm_syncobj.c`
functions (Copyright2017 Red Hat,2016 AMD) and GPL-2.0-only dma-fence-chain
functions (Copyright2018 AMD) only into temporary task-local C with both full
license notices. GPL chain code is used to verify subtle dependency ordering;
no GPL implementation body is retained in the runtime or repository.
The conventional temporary-C transport `max(a,b)` shim is one additional kernel
code-line scanner match at25 (none at40), reconciled with provenance/CI totals.

Selected N305 media-idle cache admission extends the existing MIT uncore port
with source forcewake/register/platform-mask facts and `intel_engine_cs.c`
hardware ring-idle checks (©2016 Intel, existing full LICENSE-MIT). There is no
media submission/reset implementation or new firmware payload.
The selected ring-idle C oracle adds one conventional SELFTEST_ONLY transport
macro match at25 and none at40, included in the current GT/provenance totals.

`tests/guest/graphics/intel-mesa-smoke.c` is original MIT acceptance code calling
public GBM/EGL/GLES/i915 interfaces. It contains no imported shader/driver body.
The task-local iris/CLC builds use the existing Mesa26.1.2 archive and its licenses;
LLVM/SPIR-V tool packages remain isolated build dependencies, not kernel code.
The VM/context wire adapter follows Linux7.2.3 MIT i915_gem_context.c VM/proto-
context lifetime functions (©2011–2012 Intel), using existing per-file GEM charging
and RAM pins. It does not replace an opaque GPU state image with guessed data.

The N305 sparse residency adapter `kernel/src/drm/intel/gt/copy_ppgtt.rs`
continues the Linux7.2.3 MIT `gen8_ppgtt.c` port (Intel2020); its full original
grant is retained in `crates/ax/tk-intel-gt/LICENSE-MIT`. Only four-level4K
system-memory allocation/insertion is included; no GPL command-parser body.

`tk-intel-gt/src/cache.rs` retains MIT Intel2015/2020 attribution for the
Linux7.2.3 ADL-N MOCS/L3CC/private-PAT source; grants accompany the crate.
The existing independent temporary-C comparison now includes all104 source
writes. No GPL body or firmware binary was added for these object interfaces.

The minimal CPU PAT/mapping-type adapter is original MIT Rust, referencing
Linux x86 PAT behavior without importing GPL bodies. Display audio translations
retain the MIT Intel2022/2023 grants for intel_audio/drm_edid; the HDA bridge
uses original behavior adaptation of Linux7.2.3 HDMI codec handling. Source C
oracles are temporary external-input builds with original grants preserved.

Powered TC legacy-HDMI mode programming translates the selected Linux7.2.3
MIT dkl_pll_write, DDI clock/function/buffer, timing/pipe and plane-arm
functions, retaining Intel2006–2022 attribution and the display crate's full
MIT grant; module headers and NOTICE identify the exact restricted path.

HDMI HDA port/pin topology references Linux7.2.3
sound/hda/codecs/hdmi/intelhdmi.c (GPL) as behavior only; the original HDA
adapter remains Apache-2.0, with no GPL source body imported.

The tc.rs signal-level port additionally retains MIT Intel2012/2020/2023
source attribution for tgl_dkl_phy_set_signal_levels, intel_ddi_level and
the HDMI DKL table; the full grant remains in the display crate.

The scoped display IRQ/MSI and hardware-counter epoch adapters are original
MIT Rust using source mask/dispatch facts; GPL i915_irq.c and DRM core are
behavior references only. Exact MIT display/hotplug function references are
listed in irq.rs, with original retained-owner lifecycle over existing APIs.

The optional n305-iris-smoke image overlays only target-built Mesa26.1.2
libgallium from the existing same-version source/CLC/toolchain, retaining the
Mesa/Buildroot package license obligations already registered above. No Mesa
binary is committed. The dedicated flavor and loader check are original
project scripts; they do not import a LinuxKPI, GPL driver body or new firmware.

The AHCI register/header translation retains the FreeBSD BSD-2-Clause grant and
attribution; see `crates/ax/tk-axdriver-block/LICENSES/BSD-2-Clause-FreeBSD-AHCI.txt`.


The SDHCI hardware register and quirk constants are translated from FreeBSD
`sys/dev/sdhci/sdhci.h` and retain its BSD-2-Clause attribution in
`crates/ax/tk-axdriver-block/LICENSES/BSD-2-Clause-FreeBSD-SDHCI.txt`.

The FreeBSD SD/MMC protocol and block path retain BSD-2-Clause attribution;
see `crates/ax/tk-axdriver-block/LICENSES/BSD-2-Clause-FreeBSD-MMC.txt`.

The SDHCI PCI binding retains FreeBSD BSD-2-Clause attribution in
`crates/ax/tk-axdriver/LICENSES/BSD-2-Clause-FreeBSD-SDHCI-PCI.txt`.
