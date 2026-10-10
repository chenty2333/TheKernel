# Upstream provenance

Measured on the `dev` worktree at commit `d38a2db7` plus uncommitted WIP,
re-scanned 2026-09-21, against the Linux 7.2.3 reference tree at
`/home/ava/Desktop/linux-7.2.3` and the repository's own `Cargo.lock`. Nothing
in this file is inferred from naming: every row states the evidence that produced
it, and where no evidence exists the status is `unknown` with the specific
artifact that would pin it. No upstream commit hash is recorded anywhere in
this repository, so none is invented here.

Every verbatim-Linux count below is output of
`scripts/ci/scan_linux_excerpts.py`, which is the definition of the measurement
from now on: if a number here cannot be re-derived by running it, it does not
belong in this file or in a `NOTICE`. `tests/ci/test_linux_excerpt_baseline.py`
runs both instruments over this tree and fails when the numbers below and the
numbers the tools print come apart, so the agreement is checked rather than
claimed; it skips when no reference tree is materialized, as the ABI gate's own
citation check does.

## How the verbatim-Linux scan works

Run it over the two trees:

    python3 scripts/ci/scan_linux_excerpts.py --linux ~/Desktop/linux-7.2.3 --scope crates/linux --threshold 40
    python3 scripts/ci/scan_linux_excerpts.py --linux ~/Desktop/linux-7.2.3 --scope crates/linux --threshold 25
    python3 scripts/ci/scan_linux_excerpts.py --linux ~/Desktop/linux-7.2.3 --scope crates/ax --threshold 40
    python3 scripts/ci/scan_linux_excerpts.py --linux ~/Desktop/linux-7.2.3 --scope kernel/src --threshold 40

with `--show-lines` for the per-site inventory the second table uses,
`--include-rules` to see the drawing lines the filter drops, and `--group-by 1`
for the per-package rollup a `NOTICE` quotes. The rule, stated so a disagreement
can be localized:

* The reference set is every line of every `*.c` and `*.h` file in the Linux tree
  with **all** whitespace removed, kept at the requested length and hashed with
  blake2b to 8 bytes: **12,039,278 lines / 7,657,686 distinct at ≥ 40
  characters**, **19,452,872 / 12,372,214 at ≥ 25**.
* A Rust line is a candidate when, after stripping its leading whitespace, its
  comment marker (`///`, `//!`, `//`, `/*`, ` *`) and any box-drawing quote
  gutter (`├──`, `│`), and re-stripping whitespace, it is byte-identical to a
  reference line.
* A candidate is reported separately depending on where it sits — `fenced`
  (inside a doc-comment code fence), `unfenced-doc` (doc-comment prose or
  `/* */`), `line-comment` (`//`), or `code` (a real Rust statement). A ` ``` `
  delimiter inside a doc comment toggles "inside a fenced block" and is never
  itself a candidate, so a quoted hunk whose blank and `*` lines match nothing
  still counts as one block.
* Two filters are applied and stated wherever a number depends on them:
  * **ASCII rules.** A candidate made only of `- = # * _ + |` and spaces
    (`-----`, `+-+-+-+`) is a drawing, not text, and is excluded from the excerpt
    counts. In `crates/linux` that filter removes exactly 28 ≥ 40 lines: 12 in
    `bpf/src/uapi.rs` and 16 in `futex/src/lib.rs`.
  * **Thresholds.** Three are reported because they answer different questions:
    * **≥ 40 chars** — the conservative scan: distinctive code, no accidental hits.
    * **≥ 25 chars** — adds short-but-verbatim lines (`case MOUNT_ATTR_RELATIME:`,
      `if (unlikely(len > PATH_MAX))`). A crate can be clean at 40 and not clean
      at 25; `seccomp` is exactly that case.
    * **≥ 20 chars** — the "is this crate clean?" test, used once below: the
      reference set is **22,039,749 lines / 13,719,648 distinct**. Below 25 the
      scan starts matching short C idioms that are not excerpts, so this
      threshold is only used to show that a scope has *no* hits at all.
* A fenced block counts as **marked** when the line that opens it is preceded,
  outside the fence, by a doc line carrying `Excerpt:`. The header line of every
  run reports how many matched blocks are marked and how many markers exist in
  the scope, which is what the citation-precision paragraph below restates.

### crates/linux: what the scan finds

At ≥ 40, `crates/linux/**` holds **110 verbatim lines in 49 fenced blocks across
20 files**, plus **25 further verbatim lines in 5 files outside fenced blocks**,
in unfenced `///` prose lists and in `//` comments. No ≥ 40 match anywhere in the
directory is a Rust statement. At ≥ 25 the fenced count is **211 lines / 59
blocks / 22 files**, the count outside fences **41 lines**, and 3 of those are
Rust code: `mm/src/mempolicy.rs:732,838,856` is the `MPOL_WEIGHTED_INTERLEAVE,`
enum variant, an ABI name rather than a borrowed sentence. A `--include-rules`
run moves the ≥ 40 outside-fence count from 25 to 53; the 28-line difference is
the `bpf`/`futex` rules above and nothing else, so the rule filter is the only
place where this scan and a naive one disagree.

The 25 unfenced lines are inventoried site by site in the second table below and
in the per-package `NOTICE` files of `io-uring`, `process`, `fsnotify`, and `mm`.

SPDX headers: the scan of every file type under `crates/ax/**` and
`crates/linux/**` finds **zero** `SPDX-License-Identifier` lines in either
(0 of 495 Rust files under `crates/ax`, and none in the vendored lwext4 C). The
only two in tracked source are `kernel/src/file/epoll.rs:1` and
`kernel/src/syscall/mm/mincore.rs:1`, both declaring Apache-2.0 for
TheKernel-written files. This is a measurement, not an assumption.

Citation precision, block by block: **18 of the 49 matched ≥ 40 fenced blocks
carry the inline `Excerpt: Linux v7.2.3 <file>:<lines>` marker**, and those
markers are the only ranges stated *at* a block — all 18 are in `net`, `mount`,
`aio`, and `random`. The scope holds 21 marker lines (12 in `net`, 5 in `mount`,
3 in `random`, 1 in `aio`); the three that open no counted block are
`mount/src/lib.rs:387` and `:567`, whose hunks are made of lines under 40
characters, and `net/src/lib.rs:1053`, whose `af_netlink.c:1872-1873` hunk is
under 25 as well — which is why the same scan at ≥ 25 reports 20 marked blocks of
59. Of the 31 unmarked blocks exactly **1** has a Linux `file:lines` cite in the
surrounding prose: `io-uring/src/registration.rs:441`, whose cite read
`:1029-1030` until this scan found the quoted pair sits at `:1031-1032` and the
source was corrected. The remaining 30 name only the Linux file and, where Linux
gives one, the enclosing function: every block in `mm` and `fd`, three in `ipc`,
three in `process`. Two cites an earlier revision of this file counted as ranges
are not ranges: `mm/src/userfaultfd.rs:23` gives
`mm/userfaultfd.c:vma_can_userfault()`, and the block at `process/src/wait.rs:51`
is cited by function only — the `kernel/exit.c:1527-1528` range above it belongs
to the inline `if (!ptrace_reparented(p)) ptrace = 1;` statement, not to the
block, and that block is invisible at ≥ 40 in any case.

Ranges were then checked, not counted.
`scripts/ci/audit_linux_range_cites.py` takes every `` `<file>.c:<lo>-<hi>` ``
citation that has verbatim quoted text from that file sitting next to it, since a
cite attached to a paraphrase has nothing to compare, and reports the ones whose
quoted lines do not occur inside the stated range. The rule is per quoted line,
and a range passes if it covers one of several byte-identical C statements, so a
row it prints is a lead rather than a verdict. It reaches **55 cites in
`crates/linux`, 17 in `kernel/src`, and 0 in `crates/ax`**; the wider count is
this pass's change to the tool, which now walks quoted text across ` ``` `
delimiters instead of stopping at them. Without that, the commonest layout in this
tree — a cite on the line above the fence it introduces — left its own quotation
unreachable, and the same scopes reported 29 and 6 checked cites.

The corrected ranges are not enumerated. Once a cite says the right thing the
tree no longer shows what it used to say, and nothing re-derives a count of
superseded values, so this file indexes the measurement rather than its history.
What the pass found is better told as the kinds of defect, each named by a cite
that is now checkable against the reference tree:

* A range one or two lines off at either end, as `kernel/src/syscall/ipc/shm.rs:2639`
  (`ipc/shm.c:711-721`) reads now, or as the `-EOVERFLOW` gate in
  `io_uring/src/registration.rs` reads `io_uring/rsrc.c:445-446` rather than the
  function's own declarations just above it.
* A range that names the right function and the wrong statement inside it, so
  the quoted text falls outside the lines stated. `crates/linux/net/src/lib.rs:333`
  is the example: the call site read `net/socket.c:2003`, a local declaration
  inside `do_accept()` — the accept path, which hands a peer address *out* — where
  the `move_addr_to_kernel()` inside `__sys_connect()` that the prose means is at
  `:2150`.
* A citation copied into a second file and corrected in only one of them: the
  `io_uring/register.c:1031-1032` pair at `io-uring/src/registration.rs:397` and
  `:446` has a kernel-side twin, and `io_uring/query.c:136-151` now reads the
  same in `io-uring/src/error.rs`, `io-uring/src/registration.rs` and
  `kernel/src/syscall/fs/io_uring.rs`.
* A marker claiming two hunks for a block that quotes one:
  `crates/linux/net/src/lib.rs:1110` states `net/netlink/af_netlink.c:1218-1224`,
  the unicast form, and leaves the multicast twin to the prose above it, which
  names it at `:1389-1407`.

A range with no quotation next to it is outside the walk by design — a cite
attached to a paraphrase has nothing to compare — and that class holds most of
the cites in this tree, including every restatement inside a `#[cfg(test)]`
module. Those were checked the other way round: each superseded value was
searched for across the repository, so a cite left behind in a test comment, in a
crate `NOTICE`, or in the kernel-side copy of a `crates/linux` cite surfaces as a
hit and was then re-read against the function it names. An attempt to automate
that by pairing every `file:lines` cite with the Linux function named in the
sentence above it was tried and dropped: call-site and callee-body cites are
innocent by construction, so the sweep printed far more rows than it resolved and
could not gate anything.

Three rows survive the pass, and none of them is a finding. Two are one class: a
prose cite whose walk ran past a fence and collected the quotation belonging to
the *neighbouring* cite. `mm/src/userfaultfd.rs:35`
(`mm/userfaultfd.c:3659-3661`) and `net/src/lib.rs:347` (`net/socket.c:1947`)
were both re-read against the reference tree and are right — `-EINVAL` before
`vma_can_userfault()`, and `move_addr_to_kernel()` inside `__sys_bind()`. The
third, `kernel/src/syscall/fs/io_uring.rs:765`, is the callee-body case named
above. The walk is not tightened to hide them: refusing to cross into a region
that already carries its own attribution would also have hidden `msg.rs:171` and
`:591`, where the marker above the block was right and the prose cite below it
was wrong. What the tool adds is that the class cannot quietly come back:
`tests/ci/test_linux_excerpt_baseline.py` resolves every reachable range against
the reference tree and fails on any row outside this three-name allowlist, and no
`NOTICE` or table row here is allowed to cite a range it has not passed.

Both instruments resolve against the Linux **7.2.3** tree and nothing else, which
matters for one corpus in this repository: the Intel display backend cites
`[I915]`, pinned by `docs/design/intel-display-registers.md` §0 to Linux **v6.12**
tag `adc218676eef25575469234709c2d87185ca223a`. Three file names used there —
`display/intel_fb_pin.c`, `display/intel_plane_initial.c`, and the bare
`intel_plane_initial.c` — are absent from 7.2.3, which has them as
`i915_fb_pin.c` and `display/i915_initial_plane.c` (the "MTL GOP likes to place
the framebuffer high up in ggtt" comment the backend quotes is at
`i915_initial_plane.c:152` in 7.2.3), and they appear at 11 cite sites in
`kernel/src/drm/intel`. That is the version boundary, not an error, and no claim
is made about the v6.12 ranges either way: no v6.12 tree is materialized on this
host, so those cites are unverified rather than measured, exactly like the
Zircon/Asterinas/FreeBSD/gVisor/liburing/libpcap lines in the `NOTICE` files.

## crates/linux — packages that quote Linux verbatim

All of these are Apache-2.0 TheKernel code; the quoted C is GPL-2.0-only,
(C) The Linux Kernel Authors, kept as cited documentation. None of it is
compiled or linked. "Blocks" and "lines" are at the ≥ 40 threshold, fenced
blocks only.

| File | Blocks | Lines | ≥ 25 blocks / lines | Linux source of the quoted hunk | Marker status |
| --- | --- | --- | --- | --- | --- |
| `mm/src/brk.rs` | 5 | 7 | 5 / 12 | `mm/mmap.c`, `include/linux/mm.h` | file + function cites, no ranges; no marker |
| `mm/src/charge.rs` | 2 | 9 | 2 / 13 | `mm/mmap.c`, `mm/vma.c` | file + function cites, no ranges; no marker |
| `mm/src/madvise.rs` | 2 | 2 | 4 / 8 | `mm/madvise.c` (+2 short blocks) | file + function cites, no ranges; no marker |
| `mm/src/memfd.rs` | 2 | 7 | 2 / 16 | `mm/memfd.c` | file + function cites, no ranges; no marker |
| `mm/src/mempolicy.rs` | 2 | 10 | 2 / 16 | `mm/mempolicy.c` | file + function cites, no ranges; no marker |
| `mm/src/mmap.rs` | 2 | 5 | 4 / 11 | `mm/mmap.c`, `mm/vma.h` | file + function cites, no ranges; no marker |
| `mm/src/msync.rs` | 1 | 2 | 1 / 4 | `mm/msync.c` | file + `SYSCALL_DEFINE3` cite, no range; no marker |
| `mm/src/release.rs` | 3 | 9 | 3 / 20 | `mm/oom_kill.c` (`:1205-1231` reproduced line for line at `release.rs:132-158`, checked against the reference tree on this pass) | file + function cites, no ranges; no marker |
| `mm/src/userfaultfd.rs` | 2 | 8 | 2 / 11 | `mm/userfaultfd.c` | file + function cite (`:23`), no range; no marker |
| `net/src/lib.rs` | 7 | 11 | 7 / 18 | `net/socket.c:251-252`, `net/core/sock.c:686-701`, `:641-643`, `:436-453`, `net/unix/af_unix.c:1468-1470`, `net/core/rtnetlink.c:7109-7112`, `net/netlink/af_netlink.c:1872-1873`, `:1218-1224` (prose adds `:1210-1232`, `:1389-1407`) | **8 markers, 7 of them on counted blocks** (`:1053`'s hunk is under 25) |
| `net/src/msg.rs` | 4 | 4 | 4 / 6 | `net/packet/af_packet.c:3452-3454`, `net/ipv4/tcp.c:2680-2682`, `:1483-1485`, `include/net/sock.h:2751-2753` | **4 markers, all on counted blocks** |
| `mount/src/lib.rs` | 3 | 7 | 5 / 16 | `fs/namespace.c:4456-4469`, `:4446-4461`, `:3220-3229`, `:2061-2063`, `:4096-4101` | **5 markers, 3 on counted blocks** (`:387`, `:567`) |
| `random/src/lib.rs` | 3 | 4 | 3 / 6 | `drivers/char/random.c:1385-1393`, `:1395-1401`, `lib/iov_iter.c:1445-1450` | **3 markers, all on counted blocks** |
| `aio/src/lib.rs` | 1 | 1 | 1 / 2 | `fs/aio.c:1783-1786` | **1 marker** |
| `fd/src/pidfd.rs` | 1 | 5 | 2 / 16 | `kernel/signal.c` (module header cites `SYSCALL_DEFINE4(pidfd_send_signal, ...)`), `kernel/pid.c:pidfd_get_task()` | file + function cites, no ranges; no marker |
| `fd/src/epoll.rs` | 1 | 3 | 1 / 3 | `fs/eventpoll.c` | file cite only; no marker |
| `fd/src/setfl.rs` | 1 | 5 | 1 / 7 | `fs/fcntl.c`, `fs/crypto/policy.c` | file cite only; no marker |
| `ipc/src/lib.rs` | 3 | 4 | 3 / 9 | `fs/libfs.c:69-72`, `fs/namei.c:198-206`, `:3094-3101` | file + function cites, no ranges; no marker. `crates/linux/ipc/NOTICE` now records the ranges derived here |
| `process/src/linux_abi.rs` | 3 | 6 | 4 / 12 | `fs/proc/array.c`, `include/uapi/linux/ptrace.h:185-186`, `kernel/ptrace.c:385` | file + function cites, no ranges; no marker |
| `process/src/wait.rs` | 0 | 0 | 1 / 1 (`:53`) | `kernel/exit.c:1371-1375` | the quoted `if (!ptrace && …)` line is 38 characters whitespace-free, so a ≥ 40 scan cannot see it; file + function cite, no range on the block |
| `io-uring/src/registration.rs` | 1 | 1 | 1 / 1 | `io_uring/register.c:1031-1032` (the in-source cite said `:1029-1030` until this pass corrected it) | prose-cited with the right range; no marker |
| `seccomp/src/action.rs` | 0 | 0 | 1 / 3 | `kernel/seccomp.c:2069-2081` | prose-cited with range; no marker |

Totals: **110 lines / 49 blocks / 20 files / 9 crates** at ≥ 40 in fenced blocks;
**211 lines / 59 blocks / 22 files / 10 crates** at ≥ 25. Per package, ≥ 40 then
≥ 25, counting only files that hold a fenced match (`mm/tests.rs` and
`io-uring/src/sqe.rs` match outside fences and are tabled in the next section):
`mm` 59 / 21 / 9 then 111 / 25 / 9; `fd` 13 / 3 / 3 then 26 / 4 / 3;
`net` 15 / 11 / 2 then 24 / 11 / 2; `mount` 7 / 3 / 1 then 16 / 5 / 1;
`process` 6 / 3 / 1 then 13 / 5 / 2; `ipc` 4 / 3 / 1 then 9 / 3 / 1; `random`
4 / 3 / 1 then 6 / 3 / 1; `aio` 1 / 1 / 1 then 2 / 1 / 1; `io-uring` 1 / 1 / 1
then 1 / 1 / 1; `seccomp` 0 then 3 / 1 / 1.

## crates/linux — verbatim lines outside fenced blocks

These are the lines a fence-restricted scan cannot see. None carries an
`Excerpt:` marker; the "cited as" column is what the surrounding comment
actually says. Every Linux location in the third column was re-found on this
pass by searching the reference tree for the quoted line itself, so it names
where the text *is* rather than where a comment claims it is. Sites marked
"≥ 25 only" have a whitespace-free form between 25 and 39 characters, which a
≥ 40 scan cannot see.

| File and sites | ≥ 40 | ≥ 25 | Where the quoted line is in Linux | Cited as |
| --- | --- | --- | --- | --- |
| `io-uring/src/registration.rs` — at ≥ 40: 278, 290, 304, 306, 345, 347, 373, 528, 625, 639, 660, 664, 667, 916, 939, 955; ≥ 25 adds 365, 524, 552, 556, 612, 751, 768 | 16 | 23 | `io_uring/rsrc.c:428,464,483`; `io_uring/register.c:118,221,226,233,786,790,801,805,829,835,967`; `io_uring/tctx.c:326,385`; `io_uring/zcrx.c:1436,1438`; `io_uring/query.c:125` | every ≥ 40 site sits in a hunk whose prose names a range, e.g. `io_uring/rsrc.c:428-429`, `io_uring/register.c:201-235` |
| `io-uring/src/sqe.rs:848` (hunk `:842-849`) | 1 | 1 | `io_uring/io_uring.c:1807` | `io_uring/io_uring.c:1749-1808` |
| `process/src/linux_abi.rs:464-466`; `:468-469` ≥ 25 only | 3 | 5 | `kernel/fork.c:2704-2706` (the comment) + `:2708-2709` | `kernel/fork.c:2703-2711` |
| `process/src/linux_abi.rs:790`; `:787-788` ≥ 25 only | 1 | 3 | `include/uapi/linux/ptrace.h:186`; `:182-183` | file name only (`:786`) |
| `fsnotify/src/lib.rs:471` | 1 | 1 | `fs/notify/fanotify/fanotify_user.c:1996` | `do_fanotify_mark()` at `:459` — function name, no file |
| `fsnotify/src/lib.rs:501`; `:500` ≥ 25 only | 1 | 2 | `…fanotify_user.c:1961-1962` | `…fanotify_user.c:1958-1963` |
| `fsnotify/src/lib.rs:548` | 1 | 1 | `…fanotify_user.c:1803` | `fanotify_events_supported()` at `:545` — function name, no file |
| `fsnotify/src/lib.rs:599` | 0 | 1 | `…fanotify_user.c:1655` | file name only (`:591`) |
| `mm/src/tests.rs:231` | 1 | 1 | `mm/mmap.c:189` | `mm/mmap.c:184-191` |
| `mm/src/mempolicy.rs:732,838,856` — Rust `code`, not a comment | 0 | 3 | the `MPOL_WEIGHTED_INTERLEAVE` enumerator of `include/uapi/linux/mempolicy.h` | n/a; an ABI name in a Rust enum |
| **Total** | **25** | **41** | | |

`bpf/src/uapi.rs` and `futex/src/lib.rs` produce 28 further raw ≥ 40 matches, all
of them `// ----------` rule lines; they are excluded by the ASCII-rule filter and
are not Linux text. `crates/linux/{bpf,futex}/**` scan clean at ≥ 20 once rules
are filtered, and hold no Linux source text.

## crates/ax — what the scan finds

`crates/ax/**` at ≥ 40 with the ASCII-rule filter on (the default) produces
**15 lines: 8 of them Rust `code` and 7 genuine Linux comment lines, in 3
crates**. The same run with `--include-rules` produces 100 lines, and 85 of them
(43 fenced + 42 in `//` comments, every one in `tk-starry-smoltcp`) are RFC
bit-layout diagram rows (`0 1 2 3 4 5 6 7 …`, `+-+-+-+…`) that Linux network
drivers reprint from the same IETF figure. That boilerplate, not Linux source,
is what an unfiltered scan of this tree reports; the "186 matches in 14 files"
figure this section used to carry was that number, and it is superseded.

The 7 comment lines, each re-found in the reference tree on this pass:

| File:line | Linux source | Cited as |
| --- | --- | --- |
| `tk-axfs-ng/src/fs/ext4/inode.rs:1227-1229` | `fs/ext4/extents.c:4878-4880` | `ext4_fallocate()` and `fs/ext4/extents.c` at `:1224`, no range |
| `tk-axfs-ng-vfs/src/mount.rs:2007` | `fs/namespace.c:4728` | `fs/namespace.c`:4726-4732 at `:2011` |
| `tk-axnet-ng/src/tcp.rs:801-802` | `net/ipv4/af_inet.c:943-944` | `inet_shutdown()` and `net/ipv4/af_inet.c:899-953` at `:788` |
| `tk-axnet-ng/src/unix/stream.rs:1224` | `net/unix/af_unix.c:3208` | `unix_shutdown()` and `net/unix/af_unix.c:3193-3243` at `:1219` |

At ≥ 25 the scope holds 18 fenced lines in 15 blocks and 28 outside fences (17 of
them `code`), and the first genuine **fenced** Linux hunk in `crates/ax` appears:
`tk-axfs-ng-vfs/src/nullfs.rs:8-15`, inside a ` ```c ` fence spanning `:7-16`,
cited at `:17` as `fs/nullfs.c`:12-34 — a range that does contain the quoted
`make_empty_dir_inode(inode);`, `simple_inode_init_ts(inode);` and
`inode->i_flags |= S_IMMUTABLE;` (nullfs.c:30, 31, 34). Four of the 18 ≥ 25
fenced lines are that block; the other 14 are smoltcp diagram rows. Of the
comments, `tk-axfs-ng/src/fs/ext4/inode.rs:1226` (≥ 25 only) is the
`/* Return error if mode is not supported */` line of the same extents.c hunk
(extents.c:4877) and `tk-axfs-ng-vfs/src/mount.rs:2009` the `/* mount old root on
put_old */` line of the same namespace.c hunk (namespace.c:4730).

Everything else the scan flags in this tree is coincidence of a stated kind: hex
byte-array literals in `tk-starry-smoltcp` tests and `wire/ipv6.rs`, the
`memcpy`/`memcmp`/`strcmp` prototypes `tk-lwext4-rust/build.rs:203-207` writes
itself into a freestanding `string.h` shim, and the
`sum = (sum & 0xffff) + (sum >> 16)` checksum idiom at
`tk-axnet-ng/src/fragment.rs:463` and `sctp.rs:456` — all reported as `code`,
none of them Linux text copied into a comment. At ≥ 20 the totals move to 19
fenced / 44 outside / 25 code, with no new genuine comment line.

So `crates/ax/**` does hold Linux source text — 7 comment lines at ≥ 40 in 4
files across 3 crates, and at ≥ 25 those 7 become 11 outside fences next to the
4-line fenced `nullfs.rs` hunk — and the earlier claim in this file that it holds
none was wrong. None of `tk-axfs-ng`, `tk-axfs-ng-vfs`, or `tk-axnet-ng` carries
a `NOTICE`, which is the code-side follow-up for their owners; this file
inventories their sites so a distributor is not relying on a missing notice to
learn about them.

## Rewritten instead of quoted

* `sched/src/lib.rs` — the two `sched_getaffinity()` admission tests,
  `kernel/sched/syscalls.c:1313-1316`, restated as pseudo-code.
* `vfs/src/getcwd.rs` — the `getcwd()` verdict chain, `fs/d_path.c:437-444`,
  restated as a decision list.

Measured now, `crates/linux/{sched,vfs}/**` hold no Linux line at ≥ 25 with rules
filtered, and neither does `crates/linux/{cred,packet,signal,usercopy,time,rseq,
keyring,landlock,drm,perf,profile,syslog,arch-x86-64}/**`; at the stricter ≥ 20
threshold every one of those crates still scans clean, so their `NOTICE` files'
"no source text is copied" statements are true. `crates/linux/{bpf,futex}` have
no `NOTICE` and need none by that test.

## crates/linux — notices

`crates/linux/NOTICE` (the directory-level notice) now states the excerpt policy
instead of denying excerpts: excerpts exist, each is attributed to a Linux file,
and the required inline form is on 18 of the 49 matched ≥ 40 fenced blocks (20 of
59 at ≥ 25). Those 18 sit on 21 marker lines in source — `net` 12, `mount` 5,
`random` 3, `aio` 1 — and the three markers that open no counted block are named
in the citation-precision paragraph above. The remaining 31 unmarked blocks are
cited by file and, where Linux gives one, by enclosing function, with no range at
all. The marker-plus-range treatment therefore still has to be extended in source
in `mm`, `fd`, `ipc`, `process`, `io-uring`, `fsnotify`, and `seccomp`.

Per-package notices exist for every `crates/linux` package that holds Linux
source text: `aio`, `fd`, `fsnotify`, `io-uring`, `ipc`, `mm`, `mount`, `net`,
`process`, `random`, `seccomp`. `ipc` and `fsnotify` were added by this pass
(`ipc` previously had none while holding 4 verbatim lines; `fsnotify` had none
while holding 3 more outside fences, 5 at the ≥ 25 threshold). Every count in
those notices is now the same scan output as the two tables above, re-derived on
this pass; where a notice previously stated a different number, the notice was
changed rather than the scan. The superseded figures match no run of the tool:
every scan output kept under `/home/ava/.cache/thekernel-targets/excerpt-scan/`,
from the first run onward, reports **110 / 49 / 20** at ≥ 40 and **211 / 59 / 22**
at ≥ 25, so the "125 lines in 52 blocks in 21 files" and per-package counts the
notices carried were hand-tallied before the instrument existed. That is the
whole justification for the rule at the top of this file. `crates/linux/{cred,
packet,signal,usercopy,vfs}` hold no Linux text and their notices say so
truthfully; `sched` holds none and has no notice.

`crates/linux/random` had no `LICENSES/` directory although every other
`crates/linux` package ships `LICENSES/Apache-2.0.txt`; it now does, byte-identical
to the copies in `crates/linux/{mount,net,ipc,fsnotify,futex}`.

## crates/linux — forked or migrated packages

| Crate | Upstream project | Upstream version / rev | Upstream license | Local license | SYNC STATUS |
| --- | --- | --- | --- | --- | --- |
| `tk-linux-usercopy` | StarryOS `starry-vm` — <https://github.com/StarryOS/Starry> | `0.3.0` per `crates/linux/usercopy/NOTICE`; rev unknown | MIT OR Apache-2.0 (upstream crate) | Apache-2.0 | unknown — pin by recording the StarryOS commit that held `starry-vm 0.3.0` |
| `tk-linux-signal` | StarryOS `starry-signal` — <https://github.com/StarryOS/Starry> | `0.3.0` per `crates/linux/signal/NOTICE`; rev unknown | Apache-2.0 per NOTICE | Apache-2.0 | unknown — as above |
| `tk-linux-process` | StarryOS `starry-process` — <https://github.com/StarryOS/Starry> | `0.2.0` per `crates/linux/process/NOTICE`; rev unknown. NOTICE adds that the bundled Apache-2.0 text came from "an immediate upstream follow-up commit", which is itself unpinned | MIT OR Apache-2.0 | Apache-2.0 | unknown — pin by recording both the `0.2.0` commit and the follow-up license-commit hash |
| `tk-linux-{aio,arch-x86-64,bpf,cred,drm,fd,fsnotify,futex,io-uring,ipc,keyring,landlock,mm,mount,net,packet,perf,process,profile,random,rseq,sched,seccomp,signal,syslog,time,usercopy,vfs}` | none — new extractions of TheKernel's own ABI work. Linux 7.2.3 (and, per each `NOTICE`, FreeBSD/Zircon/Asterinas/gVisor/liburing/libpcap) consulted for semantics only | n/a | GPL-2.0-only (Linux, consulted; quoted in comments as tabled above) | Apache-2.0 | local-only — the consulted Linux version is 7.2.3 and that is the pin that matters; the reference trees for the other consulted projects are not recorded anywhere, so no scan for their text was possible |

## crates/ — workspace adapters

| Crate | Upstream project | Upstream version / rev | Upstream license | Local license | SYNC STATUS |
| --- | --- | --- | --- | --- | --- |
| `tk-linux-process-adapter` (`crates/process-adapter`) | none — TheKernel-original adapter. `src/lib.rs` calls itself "TheKernel-specific payload adapter for `tk-linux-process`", `Cargo.toml:8` "TheKernel zombie payload and type aliases over the explicit Linux process domain", and the only dependency edge is `tk-linux-process.workspace = true`. `README.md` states the same. Its whole history is in-repo: 11 commits, all by the workspace author, starting at `a4521c75` "process: adapt the explicit Linux process domain"; `Cargo.toml:22` records `layer = "integration"` | n/a | n/a | Apache-2.0 (`Cargo.toml:9`), `publish = ["crates-io"]` | local-only — not a fork, so nothing to pin. **Gap:** the package ships no `LICENSES/` text and no `NOTICE`, unlike its `crates/linux` peers |
| `tk-readiness-adapter` (`crates/readiness-adapter`) | none — TheKernel-original adapter over the `tk-axpoll` contract. `src/lib.rs`: "TheKernel errno and product-future bridge for generic readiness objects… deliberately contains only TheKernel product policy built on that neutral contract"; dependencies are `axerrno 0.2` and `tk-axpoll` (renamed `axpoll-core`). 8 commits, all in-repo, same `a0e267aa` origin | n/a | n/a | Apache-2.0 (`Cargo.toml:9`) | local-only — not a fork. Two notes worth recording: `[lib] name = "axpoll"` reuses the upstream lib name, which reads as a fork of ArceOS `axpoll` to anyone who stops at the manifest, and the package ships no `LICENSES/` text and no `NOTICE` |

Both scan clean: 0 matches at ≥ 25 against the Linux reference set, and no
vendored, forked, or licensed third source file in either directory.

## crates/ax — ArceOS-lineage forks

Upstream licenses are the ones the shipped `LICENSES/` texts and upstream
`Cargo.toml` expressions carry (`GPL-3.0-or-later OR Apache-2.0 OR
MulanPSL-2.0` for ArceOS; 30 of the 45 `crates/ax` manifests declare exactly
that triple). "Registry duplicate" = the same component also appears in
`Cargo.lock` from crates.io at the version shown; that proves only
that the upstream crate is in the graph, **not** that the fork started there.

| Crate | Upstream project | Upstream version / rev | Registry duplicate in lock | Local version | Local license | SYNC STATUS |
| --- | --- | --- | --- | --- | --- | --- |
| `tk-axalloc` | ArceOS `modules/axalloc` — <https://github.com/arceos-org/arceos> | unknown | `axalloc 0.3.0-preview.2` | 0.1.0 | GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0 | unknown |
| `tk-axallocator` | ArceOS `crates/allocator` | unknown | `axallocator 0.2.0` | 0.1.1 | triple | unknown |
| `tk-axcpu` | `arceos-org/axcpu` (homepage recorded in its `Cargo.toml`) | unknown | `axcpu 0.3.1` | 0.1.0 | triple | unknown |
| `tk-axdisplay` | ArceOS `modules/axdisplay` (claimed by license + name; no upstream URL recorded) | unknown | none | 0.1.0 | triple | unknown |
| `tk-axdriver` | ArceOS `modules/axdriver` | unknown | none | 0.1.0 | triple | unknown |
| `tk-axdriver-base` | `arceos-org/axdriver_crates` (README) | unknown | none | 0.1.0 | triple | unknown |
| `tk-axdriver-block` | `arceos-org/axdriver_crates` | unknown | none | 0.1.1-preview.1 | triple | unknown |
| `tk-axdriver-display` | `arceos-org/axdriver_crates` (no README; name + license only) | unknown | none | 0.1.0 | triple | unknown |
| `tk-axdriver-input` | `arceos-org/axdriver_crates` (no README; name + license only) | unknown | none | 0.1.0 | triple | unknown |
| `tk-axdriver-net` | `arceos-org/axdriver_crates` | unknown | none | 0.1.0 | triple | unknown |
| `tk-axdriver-pci` | `arceos-org/axdriver_crates` | unknown | none | 0.1.0 | triple | unknown |
| `tk-axdriver-virtio` | `arceos-org/axdriver_crates`, wraps `rcore-os/virtio-drivers` | unknown | none | 0.1.0 | triple | unknown |
| `tk-axdriver-vsock` | `arceos-org/axdriver_crates` | unknown | none | 0.1.0 | triple | unknown |
| `tk-axfeat` | ArceOS `modules/axfeat` | unknown | none | 0.1.0 | triple | unknown |
| `tk-axfs-ng` | ArceOS `modules/axfs` (fs-ng variant) | unknown | none | 0.1.0 | triple | unknown |
| `tk-axhal` | ArceOS `modules/axhal` | unknown | `axhal 0.3.0-preview.2` | 0.1.0 | triple | unknown |
| `tk-axinput` | ArceOS `modules/axinput` | unknown | none | 0.1.0 | triple | unknown |
| `tk-axio` | `arceos-org/axio` (README badge) | unknown | none | 0.1.0 | triple | unknown |
| `tk-axmm` | ArceOS `modules/axmm` | unknown | `axmm 0.3.0-preview.2` | 0.1.0 | triple | unknown |
| `tk-axnet-ng` | ArceOS `modules/axnet` | unknown | none | 0.1.0 | triple | unknown |
| `tk-axplat-x86-pc` | `arceos-org/axplat_crates` (README) | unknown | `axplat 0.3.1-pre.6` | 0.1.0 | triple | unknown |
| `tk-axpoll` | `arceos-org/axpoll` — README: "This maintained fork of upstream `axpoll` is packaged as `tk-axpoll`" | unknown | none | 0.1.1 | triple | unknown |
| `tk-axruntime` | ArceOS `modules/axruntime` | unknown | none | 0.1.0 | triple | unknown |
| `tk-axsched` | `arceos-org/axsched` — README: "is a fork of upstream `axsched`" | unknown | none | 0.1.0 | triple | unknown |
| `tk-axsync` | ArceOS `modules/axsync` (README + lib doc) | unknown | none | 0.1.0 | triple | unknown |
| `tk-axtask` | ArceOS `modules/axtask` — `src/lib.rs` names it explicitly | unknown | none | 0.1.0 | triple | unknown |
| `tk-memory-set` | `arceos-org/axmm_crates` → `memory-set` (README badge) | unknown | none | 0.1.0 | triple | unknown |
| `tk-page-table-entry` | `arceos-org/page_table_multiarch` → `page_table_entry` (README badge) | unknown | `page_table_entry 0.6.1` | 0.1.0 | triple | unknown |
| `tk-page-table-multiarch` | `arceos-org/page_table_multiarch` (README badge) | unknown | `page_table_multiarch 0.6.1` | 0.1.0 | triple | unknown |
| `tk-kernel-elf-parser` | `Azure-stars/kernel-elf-parser` (README badge; author is also a workspace author) | unknown | none | 0.1.0 | triple | unknown |

## crates/ax — non-ArceOS forks and vendored C

| Crate | Upstream project | Upstream version / rev | Upstream license | Local license | SYNC STATUS |
| --- | --- | --- | --- | --- | --- |
| `tk-lwext4-rust` | `gkostka/lwext4` — <https://github.com/gkostka/lwext4>, vendored C at `c/lwext4` (31 `.c`, 32 headers, 21,629 lines of `.c`) | `1.0.0` self-declared at `c/lwext4/Makefile:10-12`; commit unknown. Rust glue points at `elliott10/arceos` branch `ext4-starry-x86_64` per `README.md` | Mostly BSD-3-Clause; `src/ext4_xattr.c` and `src/ext4_extent.c` are GPL-2.0-or-later ("version 2 or any later"), which upstream states makes the whole library GPL-2.0-or-later | `GPL-2.0` (= GPL-2.0-only), `crates/ax/tk-lwext4-rust/Cargo.toml:30` | unknown — pin by recording the lwext4 tag/commit the tree was copied from; ship the BSD-3-Clause text as well |
| `tk-starry-fatfs` | `Starry-OS/rust-fatfs` (homepage recorded) → `rafalh/rust-fatfs` | unknown; upstream fatfs line 0.4.x | MIT (`LICENSE.txt` shipped) | MIT | unknown — pin by recording the Starry-OS fork commit; diff against `rafalh/rust-fatfs` tags |
| `tk-starry-smoltcp` | `Starry-OS/smoltcp` (homepage recorded) → `smoltcp-rs/smoltcp` | unknown; `smoltcp 0.12.0` is in the lock as a registry crate for other consumers | 0BSD (`LICENSE-0BSD.txt` shipped) | 0BSD | unknown — pin by recording the fork point relative to a smoltcp release tag |
| `tk-virtio-drivers` | `rcore-os/virtio-drivers` (README badge) | unknown | MIT (`LICENSE` shipped) | MIT | unknown — pin by tag/commit |
| `tk-scope-local` | not recorded. Depends on `percpu 0.2.3-preview.1` (an ArceOS-ecosystem crate), which suggests ArceOS lineage, but no `homepage`, `repository`, README, or notice names an upstream | unknown | unknown (declares `MIT OR Apache-2.0` and ships both `LICENSE-MIT` and `LICENSE-APACHE-2.0`) | MIT OR Apache-2.0 | unknown — pin by naming the upstream repo and the copied commit, or by declaring the crate original |
| `tk-axfs-ng-vfs` | not recorded. `Cargo.toml:27` describes it as "Virtual filesystem layer for ArceOS" while `Cargo.toml:35` declares `MIT OR Apache-2.0` — ArceOS's own expression is the triple license, and `MIT OR Apache-2.0` with authors Mivik and 朝倉水希 is the StarryOS convention. Neither claim is stated as a provenance record | unknown | unknown — ArceOS triple license if it is an ArceOS module, MIT OR Apache-2.0 if it is StarryOS | MIT OR Apache-2.0, ships only `LICENSES/Apache-2.0.txt` | unknown — pin by naming the upstream repo/commit and reconciling the description with the license expression |

## crates/ax — TheKernel-original (no upstream)

`tk-axbpf`, `tk-axcbpf`, `tk-axexec`, `tk-axfault`, `tk-axgpu`, `tk-axpmu`,
`tk-axrandom`, `tk-axrcu`, `tk-axtlb` declare `Apache-2.0`, ship only their own
`LICENSES/Apache-2.0.txt`, and record no upstream claim in `Cargo.toml`, README,
or lib doc. **SYNC STATUS: local-only.** One caveat worth stating rather than
glossing: the BPF crates restate UAPI constant values and
`include/uapi/linux/bpf.h` layouts, which are ABI facts rather than copied prose —
the scan finds no Linux lines in these crates at ≥ 20 with rules filtered.

## Registry crates used unmodified (genuinely pinned)

These come from crates.io, so `Cargo.lock` is the pin: `axconfig 0.3.0-preview.2`,
`axlog 0.3.0-preview.2`, `axerrno 0.1.2` and `0.2.2`, `axbacktrace 0.1.2`,
`ax-crate-interface 0.5.8`, `axdma 0.3.0-preview.2`, `axklib 0.3.0`,
`axplat-macros 0.1.0`, `axconfig-gen 0.2.1`, `axconfig-macros 0.2.1`,
`ax_slab_allocator 0.4.0`, `page_table_entry 0.6.1`, `page_table_multiarch 0.6.1`,
`smoltcp 0.12.0`, `percpu 0.2.3-preview.1`. They are ArceOS-ecosystem crates and
carry the same triple-license or MIT terms as their `LICENSES/` files state.

## What would close the `unknown` rows

For each fork, one line in that crate's `Cargo.toml` or `NOTICE` recording
`upstream = <url>`, `upstream-rev = <40-hex>`, and `synced = <date>`, plus the
same fields on the ArceOS forks' parent sync commit. Anything less leaves the
fork unattributable to a reviewable upstream state, and `repository.workspace =
true` in every forked manifest currently replaces the upstream URL with
TheKernel's own — so a published crate would point readers at the fork, not at
what it was forked from.

## Out of scope here, flagged for the kernel owner

`kernel/src/**` is not covered by any `NOTICE`, and this pass did not triage it
site by site. Measured, so the owner starts from numbers rather than an
estimate: **86 verbatim Linux lines in 42 fenced blocks across 20 files at
≥ 40**, **159 lines / 52 blocks / 26 files at ≥ 25**. The 2026-09-28 re-scan
accounts for the split of `mm/aspace/mod.rs` quotations between `map.rs` and
`query.rs`: file counts increase by one at both thresholds, while line and
block counts stay unchanged. The `io_uring.rs` allowlisted citation above also
moved from line 773 to 765 after the typed-usercopy cleanup; its text and Linux
ranges are unchanged. By subtree, where `files`
counts any category and so includes comment-only files:

| Subtree | ≥ 40 fenced lines / blocks / files | ≥ 25 fenced lines / blocks / files | ≥ 40 outside fences: doc / `//` / `code` |
| --- | --- | --- | --- |
| `syscall/` | 62 / 30 / 29 | 109 / 36 / 34 | 5 / 108 / 8 |
| `task/` | 9 / 5 / 4 | 16 / 5 / 5 | 0 / 1 / 0 |
| `mm/` | 9 / 3 / 3 | 14 / 4 / 4 | 0 / 0 / 0 |
| `mounts.rs` | 5 / 3 / 1 | 13 / 3 / 1 | 0 / 3 / 0 |
| `file/` | 0 / 0 / 2 | 3 / 1 / 4 | 0 / 5 / 0 |
| `drm/` | 1 / 1 / 1 | 2 / 2 / 2 | 0 / 0 / 0 |
| `time.rs` | 0 / 0 / 0 | 2 / 1 / 1 | 0 / 0 / 0 |
| `bpf/` | 0 / 0 / 0 | 0 / 0 / 1 | 0 / 0 / 2 |
| **`kernel/src` total** | **86 / 42 / 20** | **159 / 52 / 26** | 130 lines, of which 8 `code` |

The 2026-10-05 B2 create-authority correction removed the obsolete
`mount_capable` documentation excerpts asserting no userns-mountable types.
Re-scanning original Rust changes against the same pinned release leaves
fenced/block/file/code totals unchanged, while outside-fence matches decrease
132→130 at ≥40 and 302→299 at ≥25. The declaration is not a claim of new Linux
implementation translation; removed comments are no longer counted. `kernel/src`
has no NOTICE file to edit. The unchanged raw ≥40 count includes104 drawing
matches under the current scope.

Three things about that table need stating plainly. **Not one** of the 42 blocks
(52 at ≥ 25) carries an `Excerpt:` marker, while `crates/linux` puts 18 of 49 in
that form: kernel-side citation is uniformly weaker. The ≥ 40 outside-fence count
is **130 with the ASCII-rule filter and 234 without it**, so 104 of the raw
matches are `// -----` drawing lines. And unlike `crates/linux`, this scope does
have verbatim Linux text that compiles: 8 `code` matches at ≥ 40, all of them
identifiers or format strings rather than borrowed prose —
`MEMBARRIER_CMD_REGISTER_{GLOBAL,PRIVATE,PRIVATE_SYNC_CORE}_EXPEDITED` at
`syscall/sync/membarrier.rs:754-801`, `FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE`
at `syscall/fs/io.rs:4649`, and the `sysvsem -s` column header at
`syscall/ipc/sem.rs:837`, which is 96 characters of Linux output format sitting in
a Rust literal. The first two are ABI enumerator names; the third is the one that
is text rather than a name, and its owner should decide whether to keep it
attributed. Those files are owned elsewhere; the same marker-plus-`NOTICE`
treatment applied above should be repeated there.

## Original xHCI DbC transport (Codex A, 2026-10-04)

`crates/ax/tk-axdriver-dbc` is TheKernel-original Apache-2.0 Rust, local-only,
with no copied upstream implementation. Intel xHCI 1.2b §7.6 register/layout
facts and Linux 7.2.3 `xhci-dbgcap.c`, `xhci-dbgtty.c`, `usb_debug.c` behavior/
identity facts were consulted. No Linux quotations or translated code were
introduced. Hardware transport remains unverified; see `docs/design/usb-dbc.md`.

## 2026-10-05 main: existing CrabUSB dependency observation surface

The existing crates.io CrabUSB 0.11.0 source is kept at `crates/vendor/crab-usb`
with its original manifest/README/LICENSE attribution. The local change exposes
an addressed device's actual owned xHCI output-context address, existing
configuration cache and discovered parent-port route through immutable probe
metadata. It adds no USB command or descriptor request. Kernel integration
retains existing raw configuration descriptor bytes and encodes the standard
device-descriptor fields already decoded by the dependency. This is dependency
reuse with original Rust observation additions, not a Linux-code translation.
The package/license-file discrepancy is recorded in `docs/licensing.md`.

### Interface short-flags accessor audit location (B container work)

The original ifreq short-input accessor/regression moved the already-registered
Linux7.2.3 sockaddr import excerpt marker in crates/linux/net/src/lib.rs from
line347 to360. The range-citation audit row follows that source movement;
excerpt content, allowed ranges and scan-category totals are unchanged. The
period65 host check detected this drift; no threshold or assertion was relaxed.

## Ptrace syscall-stop implementation (Codex A, 2026-10-05)

Replacing the obsolete GET_SYSCALL_INFO commentary removed an existing quoted
Linux block. The scanner-backed host inventory measured kernel/src totals of
86 fenced lines / 42 blocks / 20 files at >=40 and 159 / 52 / 26 at >=25;
outside-fence totals are 132 (8 code) and 301 (13 code), respectively. The
range-citation audit remains unchanged. No new Linux quotation or translated
implementation was added; the new runtime module uses original Rust.

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
The optional inspect payload uses 192 MiB for its signed storage/partition tools;
the merged baseline image remains 160 MiB. It contains no OEM firmware tables.

## MIT i915 ADL-P/N display translation (Codex D, 2026-10-05)

Source authority: local Linux 7.2.3, `drivers/gpu/drm/i915/display/`.
`crates/ax/tk-intel-display/NOTICE` is the function inventory, with the original
copyright and `LICENSE-MIT`. Current source-to-Rust mapping:

- `intel_display_device.c` → `src/device.rs`: ADL-P/N default display identity
  and revision/stepping lookup; PCI ID facts from `include/drm/intel/pciids.h`.
- `intel_bios.c` / `intel_vbt_defs.h` → `src/intel_bios.rs`: VBT/BDB extent and
  raw-block lookup, general features/definitions, child records, XELPD DVO,
  HDMI/DP caps, port/presence and AUX mappings, driver/power flags, DSC records,
  eDP and PSR settings, LFP pointer validation/generation, panel timings and
  backlight settings, BDB zero-extended block initialization, plus DSI MIPI
  configuration and sequence validation. MIPI layouts use MIT
  `intel_dsi_vbt_defs.h` (2025 Intel). More `intel_bios.c` functions remain to
  translate; no GPL source was used.
- `intel_opregion.c` → `src/opregion.rs`: header/ASLE layout facts, external
  RVDA address/size and mailbox VBT lookup only. No ASLE/ACPI/SWSCI writes.

Other display platforms and old/future BDB semantic versions are omitted.
Checked byte access, accessed-section validation and stricter ambiguous-input
admission are documented divergences, not alleged exact equivalence to unsafe
C inputs. The rest of the D1 inventory is planned, **not translated**.

Additional slice: `intel_display.c::intel_get_transcoder_timings` (non-DSI,
version 13) and `intel_get_pipe_src_size` → `tk-intel-display/src/display.rs`;
register masks/offsets from MIT `intel_display_regs.h`. All seven timing reads,
interlace correction and SET_CONTEXT_LATENCY override follow source order.
`kernel/src/drm/intel/i915_port.rs` is original TheKernel glue, limited to an
already-admitted powered pipe A and denying every write. No GPL helper port
or full fastboot state-equivalence claim is introduced.

Clock slice: `intel_dpll_mgr.c::{icl_mg_pll_find_divisors,icl_calc_mg_pll_state,
icl_ddi_mg_pll_get_freq}` → `src/dpll_mgr.rs` (DKL HDMI, no SSC only).
`intel_cdclk.c::adlp_cdclk_table`, display-13 pixel-rate minimum and
`bxt_calc_cdclk` table search → `src/cdclk.rs` (ADL-P B0+ / ADL-N D0).
Selected field definitions come from MIT `intel_{dkl,mg}_phy_regs.h`.
No PLL/PHY/power/clock writes or workarounds are silently declared complete.
The optional C oracles load unmodified bodies/headers from the external Linux
7.2.3 tree, build temporary host programs and remove them. Private captured
EDID→selected timing→DKL arithmetic→preserved CDCLK is exercised in an explicit
kernel host test, without bundling BIOS/EDID or asserting full clock policy.

Kernel identity glue now delegates ADL-P/N stepping lookup to that MIT crate;
ADL-N's source-derived display version is 13 and revision0 is D0. An inexact
next/future lookup remains unknown in the hardware admission layer. Native
boot admission additionally requires the characterized `8086:46d0` exact D0,
not merely a family ID. This changes no default MMIO write policy and does
not assert the stepping/workaround/TC hardware port is complete.

GT route assessment consulted local Linux 7.2.3 `i915/intel_step.c` (ADL-N
revision0 graphics/media A0, distinct from display D0), `gt/uc/intel_uc_fw.c`
(ADL-N selects tgl GuC via the ADL-S override), xe device/firmware tables, and
cached Mesa 26.1.2 iris i915/xe backends. No GT/HDA code was translated by that
assessment. The FreeBSD LinuxKPI route reference is the upstream drm-kmod
repository linked from `docs/design/intel-gpu-acceleration.md`.

The 2026-10-05 re-scan finds **one** new normalized code-line match in
`tk-intel-display/tests/upstream_clock.rs`: the conventional C `ARRAY_SIZE`
sizeof expression in the original oracle shim. It adds no fenced/prose quote.
At ≥40, `crates/ax` totals are now `(0,0,0,16,9,0,0)`; at ≥25 they are
`(18,15,4,29,18,0,0)` in the scanner's seven-category order. Other scope totals
and range-citation counts are unchanged. `test_linux_excerpt_baseline.py` is
reconciled to these measured totals rather than disabling/exempting the scan.
The earlier inventory tables remain explicitly dated historical measurements.

DKL readout/access extension: MIT `intel_dkl_phy.c` helpers → `src/dkl_phy.rs`,
`intel_dpll_mgr.c::dkl_pll_get_hw_state` → `src/dpll_mgr.rs`. ADL-P TC PLL enables
start at **0x46038, stride 8**, not the TGL/ICL MG_PLL_ENABLE registers. Backend
contracts pin power and serialize all HIP access. Firmware-only readout encloses
all eight reads in one lock, preserves the full shared selector and verifies its
restoration on every fallible prefix; failure requires quarantine. This is an
explicit safety divergence from i915's driver-owned helper (one lock per read).
`intel_de_rmw` ultimately always writes through `intel_uncore_rmw`, contrary to
the DKL helper's stale unchanged-value-elision comment. Port follows executable
upstream behavior. Optional `upstream_dkl` compiles unmodified local C helpers,
state-readout body and register masks: 192 four-operation traces and 12 masked
PLL states/read traces. The oracle ignores only the documented preservation
wrapper's three extra operations, not any upstream register access.

Plane slice: `skl_universal_plane.c::{skl_format_to_fourcc,
skl_get_initial_plane_config,skl_plane_stride_mult}` and ADL-P main-plane
`intel_fb.c::intel_tile_{size,width_bytes,height}` / `intel_fb_align_height` →
`src/universal_plane.rs`. Source format fallback is preserved for readout, but
native admission separately requires the exact XRGB8888 raw format, linear,
opaque/unrotated/unreflected layout, bypassed plane color and complete pitch.
ADL-P field5 is **Yf**, not DG2 4-tile. Five universal planes are exposed per
pipe (display13 runtime has four sprites plus primary). Main size is checked
u64, unlike upstream u32 multiplication. Auxiliary/compression/DPT ownership
is not proven by this slice. Optional `upstream_plane` compiles the unchanged
format, full initial-plane reconstruction and tile/stride bodies with external
register/format definitions: 1792 format/alpha/order/tiling/rotation/layout
states and exact seven-read traces. No physical memory or private fixture added.

Color slice: `intel_color.c` display13 `skl_get_config`, `icl_read_csc` and
`icl_read_luts` paths, CSC matrix helpers and all used LUT read/packing helpers
→ `src/color.rs` (exact list in NOTICE); fields from MIT `intel_color_regs.h`.
Caller-owned LUT buffers avoid a large kernel stack object. Decoded CSC offsets
are u16 like i915; the complete original dwords are also retained for safety.
All indexed palettes preserve and verify firmware selectors on every error
prefix, under the same lock as color commits. Extra wrapper operations are the
only trace exclusions in the compiled-C oracle. Disabled tables are not read.
The upstream multi-segment FIXME is preserved as an explicit nine-entry-only
state, not silently zero-filled or accepted as a complete transform. ICL-only
Wa_1406463849 does not apply to ADL-P; TGL+ CSC reads do not disarm updates.
Measured: 192 configuration/CSC/LUT states, decoded entries and upstream MMIO
traces agree with unchanged local C bodies; all indexed fault prefixes tested.

Color-oracle inventory reconciliation: scoped scanner finds four new code-line
matches in `tests/upstream_color.rs` at25: conventional `min`, LUT length256,
CSC matrix members and pre/post LUT blob pointers. Only the final declaration
also matches at40. Alongside ARRAY_SIZE, the Intel crate now has five such
code matches at25 / two at40, no fenced/comment/marked matches. Current whole
`crates/ax` totals supersede the pre-oracle figures above: at40, 0 fenced and
17 outside fences (10 code); at25, 18 fenced /15 blocks /4 files and33 outside
fences (22 code). Other scopes and citation counts are unchanged; CI baseline
is reconciled, not suppressed. Temporary imported bodies retain the MIT grant.

Scaler slice: MIT `skl_scaler.c::{skl_pipe_scaler_get_hw_state,
skl_scaler_get_config}` → `src/scaler.rs`, display13/two scalers, no CASF.
Offsets use **pipe stride0x800**, not transcoder/plane stride0x1000. Window
sizes are direct pixels, not +1 fields. PANEL_FITTER power is pinned separately
and never woken for discovery. Original extra ownership readout checks both
controls, including active plane/reserved bindings which the pipe-only getter
skips. Active filter/scaling state is not admitted for plane-only fastboot.
48 compiled-C configurations and exact read traces agree; dark-domain,
missing-register and plane/reserved-binding regressions pass.

WM slice: `skl_watermark.c` display13 pipe WM/DDB getters and decoders plus
`intel_enabled_dbuf_slices_mask` → `src/watermark.rs`. ADL-P has **six** latency
levels and dedicated SAGV/transition offsets, not eight ordinary levels. Both
five exposed planes and the cursor are included in upstream read order.
DDB end0 remains disabled; nonzero inclusive ends become exclusive+1. Raw
values are retained separately from decoded fields; safety validation is not
upstream decode and must resolve MBUS-relative offsets/enabled slices before
ownership. Global readout retains four irregular DBUF controls plus MBUS_CTL;
this is not full global bandwidth reconstruction or policy. No WM/PCODE writes.
64 compiled-C states with all decoded fields and65-register traces agree;
missing/dark domains, DDB bounds and dedicated SAGV/cursor offsets are tested.

TC readout slice: `intel_tc.c` ADL-P ready/owned predicates, display13 ownership
condition and modular-FIA mapping/legacy pin/lane fields → `src/tc.rs`. TCSS
status registers stride4; TC1 DDI is PORT_D (0x64300), DDI stride0x100. ADL-P
always has two ports per modular FIA, at0x163000/0x16e000. **Display13 pin
assignment still comes from DFLEXPA1**, not TCSS_DDI_STATUS's display20+ field.
Core/port/legacy AUX cold-block power must already be pinned; no waking domains,
no ownership/cold writes. Additional original DKL before-image collection reads
19 setup-related words under the preserved-selector mechanism, with all43
fallible MMIO prefixes checked for restoration/quarantine on all four ports.
48 compiled-C readiness/ownership/FIA states and exact read offsets agree.
This is not `adlp_tc_phy_get_hw_state` (which acquires power/cold), nor complete
HPD-derived TC-mode discovery or encoder fastboot admission.

DDI readout slice: selected HDMI/DVI fields in `intel_ddi_read_func_ctl`,
display13 four-lane `intel_ddi_read_func_ctl_dvi`, `icl_ddi_tc_is_clock_enabled`
and `icl_ddi_tc_get_pll` → `src/ddi.rs`. TGL port encoding uses `(port+1)<<27`;
TC1=PORT_D, selector0x4610c. TC gates are bits12/13/14/**21**, not four adjacent
bits. Unknown clock muxes return no PLL, never a guessed DKL path. Only HDMI
mode interprets scrambling/high-TMDS flags; DVI lane count is four regardless
of the DP width field. Combined raw clock evidence is original glue; full
encoder/DP/audio/infoframe state is not claimed by the control decoder.
4096 compiled-C HDMI/DVI BPC/sync/scrambling and TC mux/gate states/read traces
agree; missing/dark domains and unknown encodings are covered.

HDMI slice: display13 packet-enable/GCP/DIP read helpers in MIT `intel_hdmi.c`
→ `src/hdmi.rs`; exact included functions in NOTICE. Enabled GCP only is read,
then enabled AVI/SPD/vendor/DRM packets in encoder get_config order. DIP has
an ECC/reserved hole at byte3, not a hole at the byte4 checksum. Raw control
(including filtered-out PPS/reserved bits) and raw bytes remain available to
strict admission; reading a packet is not proof it is valid. 256 compiled-C
hardware-enable/software-index/GCP/data states and exact MMIO traces agree.
Selected MIT `drivers/video/hdmi.c`/`include/linux/hdmi.h` decode helpers →
`src/hdmi_packet.rs`: all AVI fields/bars, SPD text/SDI, vendor VIC/3D metadata,
and HDR u16 fields, checksum/version/length checks. Full Avionic Design grant
is in LICENSE-HDMI-MIT. Known safety difference: SPD initializer's unchecked
string scan is replaced with field-bounded prefix/zero-pad behavior; C oracle
input buffers have explicit trailing zeros, not undefined string accesses.
4800 packet-field/rejection cases agree with unmodified local C functions.
No claim of full fastboot ownership, packet programming, audio or hardware output.

HDMI-oracle excerpt inventory: seven new code matches at25 in
`tests/hdmi_packet.rs`: conventional min, five packet-size constants and HDMI
IEEE OUI. Their normalized lengths are28–34, so at40 totals are unchanged.
Current Intel crate totals:12 code matches at25 /2 at40, no fenced/comment/marked
matches. Whole crates/ax at25 is now18 fenced lines /15 blocks /4 files and40
outside fences (29 code); at40 remains0 fenced and17 outside (10 code).
Other scopes/citation counts unchanged; the baseline is reconciled, not bypassed.

### Native N305 fastboot wiring

`tk-intel-display/src/pipe_config.rs` translates display13
`intel_display.c::bdw_get_pipe_misc_output_format`, the scalar readout steps of
`hsw_get_pipe_config`, and `intel_vrr.c::intel_vrr_get_config`, with selected
MIT `intel_{display,vrr,vdsc}_regs.h` fields. Original Intel copyrights
2006–2007/2020/2025/2024/2023 and the full MIT grant are in the crate NOTICE and
LICENSE-MIT. Active DSC/joining is refused, not described as a decoded PPS.
`kernel/src/drm/intel/fastboot.rs` is original adapter/ownership/recovery policy
around these translated getters. Power-map and UC non-GuC GGTT ordering refer
to local i915; the GMS size decoder independently implements published field
facts, with no GPL text/translation. No GT reset, GuC, DMC, physical display
acceptance or Mesa rendering is implied by this native KMS adapter.

### Independently opted-in N305 GT entry

The native `intel.gt=1` boot hook is independent of display fastboot/HDMI/HPD/
DMC/audio. `tk-intel-gt` translates only Gen12 GT forcewake ownership/reset,
source fallback-ACK workaround, BCS CS-stop/prefetch/pending-MI-forcewake and
prepare/cancel/hardware-domain reset. MIT sources/functions/copyrights and the
exact original uncore grant are in its NOTICE/LICENSE-MIT. Kernel MMIO adapter
and terminal-owner policy are original MIT. Source `intel_step.c` maps N305
revision0 to GT/media A0; display D0 is not reused. The compiled-i915 oracle
checks BCS domain selection (Gen11 bit2, not old bit3), reset prepare/cancel,
double GDRST and50us settle. Model tests are not physical reset/copy evidence.

The GT C oracle shim's conventional `ARRAY_SIZE` definition adds one code-line
match at both excerpt thresholds25/40. The combined Intel display+GT test
inventory is15 code matches at25 and3 at40, with no fenced/comment/marker
matches and no scan exemption. The CI totals are reconciled accordingly.

### Kernel-owned N305 BCS execution chain

`intel.gt=1` now continues from source forcewake/reset into owned SharedPages,
39-bit physical checks, direct-DMA admission, shared scoped GGTT bindings,
private four-level PPGTT, source Gen12 BCS LRC/indirect/predicate image, UC cache
policy and applicable GT workarounds, execlists load, flush/breadcrumb wait,
source stop/reset retirement, exact copied bytes/source/guard verification.
Source/function and copyright inventory is in `tk-intel-gt/NOTICE` and its full
original MIT grant. Unmodified compiled C compares whole register/WA images,
PDE/PTE fields and all batch/ring words; it caught predicate WA and WA-tail
omissions before commit. The native kernel does not have a CPU-copy fallback:
the interpreter is host-test-only. No physical GPU execution has been observed;
BCS is not RCS rendering, and no i915 execbuf/Mesa capability is advertised.

The BCS/context C shims add two conventional `INVALID_MMIO_REG` definitions
at25 only; GT totals are3 at25/1 at40, combined Intel15 at25/3 at40. CI
`crates/ax` at25 changes only outside/code41/30→43/32. No exemption is used.

The original kernel `intel/gem_exec.rs` adapter uses published x86_64 i915 UAPI
facts, not a GPL execbuf body. Local Linux7.2.3 headers compiled independently
confirm eight sizes, eight ioctl encodings, WB mapping flag and five offsets.
Existing GEM/PRIME/mmap/reservation/binary-sync infrastructure is reused; user
commands are bounded, decoded and rebuilt rather than run privileged. No full
Mesa/RCS execution or physical BCS acceptance is claimed.

### Bounded RCS shader chain

N305 render-domain reset, source14-page context/2WA pages, whole-slice RPCS,
command-buffer/GPR/timestamp restore, mandatory instruction-state invalidation,
source RCS engine/context WAs and RCS flush/breadcrumb are translated from MIT
i915 sources/functions inventoried in `tk-intel-gt/NOTICE`. A bounded licensed
IGT Gen12 shader rectangle is adapted from Intel-hosted backport/v6.17 source
and its original full COPYING is preserved as LICENSE-IGT. Source-only compiler
oracles compare complete images/rings/WA lists/state pages. Original native
memory/submit/result/retirement and strict rebuilt user-page admission reuse
GEM reservations/binary sync. Source Mesa26.1.2 Gen120 MOCS/packet facts provide
an explicit UC-policy/full-SBA safety adaptation. This is not EU emulation or
physical GPU/Mesa acceptance; no host DRM node is opened in these validations.

The RCS additions add two conventional C-shim INVALID_MMIO_REG matches at25
and73 identical all-zero data-array rows at25/40 from the compiled IGT page.
They are measured textual matches, not proof of copied Linux bodies or GPU
execution. Current GT totals79/74 and combined Intel91/76; crates/ax baseline
outside/code118/107 at25,91/84 at40, no fenced/comments/markers or exemptions.

N305 GT information runtime: `tk-intel-gt/src/info.rs` selects MIT
`gt/intel_sseu.c::gen12_sseu_info_init/gen11_compute_sseu_info` and
`gt/intel_gt_clock_utils.c` reference/crystal/divider readout, copyrights2019/
2020 Intel, full grant in existing LICENSE-MIT. The existing i915-wire dispatcher
calls these for real fuse topology and timestamp frequency; it does not use the
product's advertised EU count as a hardware observation. Per-file context/query
transport is original, with independently compiled x86_64 UAPI sizes/commands.

DRM completion ownership: original syncobj/fence fix follows observed Linux7.2.3
binary SIGNAL replacement and chain dependency ordering, not producer mutation.
The ignored explicit host oracle reads selected source `drm_syncobj_replace_fence`,
`drm_syncobj_assign_null_handle` and `dma_fence_chain_{init,find_seqno,signaled}`,
then compiles only a temporary licensed C transport model. MIT Red Hat/AMD and
GPL-2.0-only AMD notices/full grant accompany that temporary source. No runtime
GPL chain translation is present. It tests captured identity, terminal/dependency
ordering and late points; it is not evidence of GPU execution or all fence errno.

The syncobj oracle's conventional test-only `max(a,b)` macro adds one normalized
code match at threshold25, none at40. Kernel outside-fence totals are now303/
14code at25 and unchanged132/8code at40; fenced totals/range cites are unchanged.
This is a short transport shim, not a retained Linux chain implementation;
the scanner is not exempted or disabled.

Shared-cache admission calls selected ADL-P/N media forcewake helpers and source
`intel_engine_cs.c::ring_is_idle` head/tail/MODE_IDLE checks before existing
PAT/MOCS/L3 writes. Platform masks/fuse and MMIO definitions are read from the
local i915 reference; unknown/live consumers refuse. Corresponding unchanged-C
register/idle predicates and fault/ownership models validate software only.
The media idle C transport adds one conventional `I915_SELFTEST_ONLY(x) 0`
match at25, none at40; current GT79/74, combined Intel91/76 and crates/ax119/
108code at25 (unchanged91/84code at40) are reconciled without an exemption.

Intel VM/context transport: Linux7.2.3 gem/i915_gem_context.c
`i915_gem_vm_{create,destroy}_ioctl`, `get_ppgtt`, `set_proto_ctx_vm` and
`create_setparam` define handle/reference/proto-context behavior; existing
selected gen8_ppgtt.c encodings back the runtime PPGTT. Kernel lifetime/storage
adaptation is original Rust over GEM charging/SharedPages, with no GPL body.
Actual userspace reference stays Mesa26.1.2, including intel/common/i915/intel_gem.c
and iris/i915/iris_{batch,bufmgr,kmd_backend}.c. Same-source iris build and the
original real EGL/GLES client are preparation, not initialization/render evidence.

Target iris context/clock continuation: Linux7.2.3 `set_proto_ctx_engines` and
`i915_reg_read_ioctl` whitelist; `intel_uncore_read64_2x32` upper/low/upper with
three attempts (MIT ©2013/2022 Intel, existing full grant). The native owner
already holds forcewake; unknown/torn state returns an error. Mesa26.1.2's
actual RCS/RCS/BCS+RECOVERABLE=0+VM create chain selects immutable engine slots.
Scheduling/recovery, hardware/default saved images and general batch support
are still not claimed. Compiled unmodified C is a trace oracle, not GPU evidence.

Opaque context-image continuation selects Linux7.2.3 intel_lrc.c
`lrc_update_regs`, `init_ppgtt_regs`, `__reset_stop_ring`, WA image builders and
execlists port ordering/Gen11 SW-context tags (MIT ©2014 Intel, existing grant).
The kernel adapter owns pins/charging, uses a distinct idle context to save the
request image, and gates validity on two breadcrumbs plus source reset retirement.
GPU-generated streams are not rebuilt from invented values. `intel_gt.c::__engines_record_defaults` and `intel_lrc.c::lrc_init_state`
now guide reset-default recording/clone initialization. Physical capture remains
unverified; general nonprivileged user batches remain unfinished.

Source Gen12.0 RCS/BCS register whitelist: Linux7.2.3 intel_workarounds.c
`tgl_whitelist_build`, `allow_read_ctx_timestamp`, `_wa_add` encoded-address
ordering and `intel_engine_apply_whitelist`; intel_engine_regs.h supplies12
slots/read-only/range4/NOPID fields. Same source functions compile into the
existing temporary-C oracle, covering all24 RCS/BCS writes, plus landed-store
and readback fault cases. The original kernel native ownership gates are kept;
this does not itself open general Mesa batches or establish GPU execution.

- Intel N305 standard residency: `kernel/src/drm/intel/gt/copy_ppgtt.rs`
  adapts Linux7.2.3 MIT `gt/gen8_ppgtt.c` `__gen8_ppgtt_alloc` and
  `gen8_ppgtt_insert_pte` (Intel2020), using existing GEM charging/pins and
  stopped-engine serialization. Four-level4K only; full grant in
  `crates/ax/tk-intel-gt/LICENSE-MIT`. Native ordinary batches reuse the
  existing MIT `gen8_emit_bb_start_noarb` translation and Gen12 hardware
  nonprivileged admission; no GPL command-parser body imported.

- `tk-intel-gt/src/cache.rs`: Linux7.2.3 MIT `intel_mocs.c` Gen12 table,
  unused-index selection/global-control/paired-L3 initialization (Intel2015),
  `intel_gtt.c` private PAT initialization (Intel2020); full grant in GT license.
  Original GEM adapter follows source domain/cache/advice/aperture behavior
  over existing TheKernel reservations, pins and storage/PRIME ownership.

- Necessary GPU CPU-map adaptation is original MIT Rust in
  `tk-axplat-x86-pc/src/intel_cpu_cache.rs`, existing file/shared-mmap backend
  and x86 PTE conversion. Linux7.2.3 x86 PAT initialization is a behavior
  reference only; no GPL source body was copied. Opt-in/all-CPU proof and
  immutable WC/UC/WB VMA types preserve the default and ownership boundaries.
- HDMI audio C-oracle declarations add5 normalized matches at25 and2 at40
  (ARRAY_SIZE, DIV_ROUND_UP, min and2 structure member declarations). Current
  crates/ax counts:25=(18,15,4,124,113,0,0),40=(0,0,0,93,86,0,0); other
  scopes unchanged. Combined Intel display+GT test shim counts96/78. These
  are reconciled in NOTICE/baseline; no source/scanner exemptions introduced.

- Powered TC legacy-HDMI modeset: dpll_mgr.rs adds source dkl_pll_write;
  kernel tc_modeset.rs ports Linux7.2.3 MIT intel_ddi.c transcoder clock/
  function/buffer enable-disable, intel_display.c timing/pipe and
  skl_universal_plane.c primary arm (Intel2006–2022). Existing fastboot owns
  exact original-image restoration, power and DMA lifetime. No GPL body,
  cold-TC/PCODE/CDCLK or new WM computation was imported. Grant: display
  LICENSE-MIT, function inventory in display NOTICE and module header.

- HDMI audio: tk-intel-display/audio.rs translates Linux7.2.3 MIT
  intel_audio.c clock/N/M-CTS and HSW/DDI enable/disable plus drm_edid.c ELD
  assembly; full attribution/grant in header and display NOTICE/LICENSE-MIT.
  CTA validation and kernel audio.rs/HDA integration are original adapters.
  sound/hda/codecs/hdmi/intelhdmi.c (GPL) is a behavior/register-topology
  reference only: no GPL implementation body copied into the Apache HDA crate.

- Preserved-WM profile admission is an original conservative predicate, not
  new PCODE/latency policy. The independent temporary C oracle uses unchanged
  Linux7.2.3 MIT skl_compute_wm_params/skl_compute_plane_wm and intel_fixed.h
  arithmetic for exact linear-XRGB4K30->1080p60. At258 normal latencies
  target block/line/minimum-DDB demand is no worse; the same-profile method2
  argument covers SAGV equal latencies. Actual latency is not guessed/read.
  Conventional WM oracle shims add5 code matches at25 and2 at40; current
  crates/ax totals25=(18,15,4,130,119,0,0),40=(0,0,0,95,88,0,0).
  Combined Intel display+GT inventory101/80, with no scanner exemptions.

- Powered TC HDMI signal programming: tc.rs ports Linux7.2.3 MIT
  intel_ddi.c::tgl_dkl_phy_set_signal_levels and intel_ddi_level with
  intel_ddi_buf_trans.c::_tgl_dkl_phy_trans_hdmi, selected ADL-P HDMI branch
  (Intel2012/2020/2023; full grant in display LICENSE-MIT). N305 D0 matches
  intel_display_wa.c Wa_16011342517 applicability. The literal source RMW
  set argument1 at594MHz is preserved, not reinterpreted as bit12. Native
  target clocks148500/297000 use the source0 branch and VBT level5.
  The source helper is called before DDI_BUF enable; complete expected-PHY
  readback and original8-word/HIP restoration feed the existing transaction.

- Native display IRQ/PCI adapter (kernel intel/irq.rs, pci.rs): original MIT
  Rust around existing MSI/WaitQueue facilities, using Linux7.2.3 MIT
  intel_display_irq.c / intel_hotplug_irq.c source masks, W1C/selected-pin
  behavior and register layouts. Function inventory/boundary in irq.rs header.
  GPL i915_irq.c master dispatcher is behavior-only, not copied. Hardware
  counter epochs adapt source intel_crtc_vblank_off/on behavior to existing
  KMS; no GPL DRM core body imported. No new firmware binary added.

- Native iris payload: same cached Mesa26.1.2, Buildroot2026.05.2 GCC14.4 /
  glibc2.43 / libdrm2.4.131 target cross-build. Common shared-DSO platform
  options match the existing recipe; iris added without source version mixing.
  Native flavor reuses existing Buildroot overlay and graphics runner. No
  binary/source driver body imported into this repository; image and tools
  remain task-owned build outputs. Guest immediate symbol binding is measured;
  no physical GPU initialization/rendering or software fallback result claimed.

### Combined A/B/D/E inventory (2026-10-07)

The merged tree retains A's removal of the obsolete ptrace quotation and
B's removal of obsolete mount-authority comments alongside D's MIT oracle
shims. The scanner measures kernel/src as (86,42,20,130,8,0,0) at >=40 and
(159,52,26,299,14,0,0) at >=25. The merged baseline uses these measured totals;
individual-branch historical counts above are not additive. Other scopes
retain the latest Intel inventory, with no scanner exemptions.

`crates/ax/tk-intel-display/src/dmc.rs` translates the display-12/13 path and
size selection from Linux v7.2.3 `drivers/gpu/drm/i915/display/intel_dmc.c`
`dmc_firmware_default()` (MIT, Copyright © 2014 Intel). Firmware blobs are
external Buildroot inputs; the package carries their separate license notice
and does not commit binary firmware to this worktree.

The same file now also translates the display-12/13 main/pipe package parser
from `intel_dmc.c` `parse_dmc_fw()`, `parse_dmc_fw_header()`, and
`fw_info_matches_stepping()` (MIT, Copyright © 2014 Intel). The kernel adapter
selects by PCI revision after the rootfs-ready callback and retains the parsed
program records. It also translates the event-handler fixups, disabled-event
policy, payload/MMIO upload and readback verification; the kernel performs the
upload only after the opt-in power-ready boundary. This supersedes the earlier
statement above that no firmware parser or loader was added.

`crates/ax/tk-intel-display/src/power_map.rs` translates the display-12/13
power-well domain lists and descriptor groups from Linux v7.2.3
`drivers/gpu/drm/i915/display/intel_display_power_map.c` (`tgl_power_wells`,
`rkl_power_wells`, `adls_power_wells`, and `xelpd_power_wells`; MIT, Copyright
© 2022 Intel). `power_domains.rs` consumes these descriptors for synchronous
domain accounting, and `kernel/src/drm/intel/power.rs` requests Pipe-A/PW_A.

`crates/ax/tk-intel-display/src/power_well.rs` translates the HSW-style
requester, fuse, enable/disable, and state-query helpers from Linux v7.2.3
`intel_display_power_well.c` (MIT, Copyright © 2022 Intel). The typed kernel
register adapter consumes the translated handshake for PW_1/PW_A; IRQ-coupled
wells and DDI/AUX operation groups remain.

`crates/ax/tk-intel-display/src/dc_state.rs` translates the display-12/13
`gen9_dc_mask()` and `gen9_write_dc_state()` logic plus the field read-modify-
write part of `gen9_set_dc_state()` from `intel_display_power_well.c`, plus
`get_allowed_dc_mask()`, `sanitize_target_dc_state()`, the target setter, and
current-state readout from `intel_display_power.c` (MIT, Copyright © 2022 Intel).
`kernel/src/drm/intel/power.rs` uses the write retry for initial DC disable;
DMC-controlled DC5/6/9 transitions are not yet wired.

The display-12/13 subset of `intel_bios.c` and its MIT-licensed
`intel_vbt_defs.h` helpers in `crates/ax/tk-intel-display/src/intel_bios.rs`
now includes BDB block initialization/fixups, panel index and PnP selection,
SDVO/VBT helpers, platform DDC routing, MIPI sequence repair, and display-12/13
panel parsing. Linux's panel object allocation/lifetime (`intel_bios_init_panel_early/late`
and `intel_bios_fini_panel`) is represented by the owned `PanelVbtData` result
and Rust drop rather than importing DRM panel lifecycle APIs.
The VBT byte getter is exposed for an adapter; DRM debugfs registration and
log-only DDI port printing remain framework diagnostics and are not copied.
`kernel/src/drm/intel/fastboot.rs` consumes `intel_bios_init()` for the N305
route and AFC override; general DDI cold-start admission still needs broader
platform integration.

`crates/ax/tk-intel-display/src/power_domains.rs` also translates
`intel_display_power_domain_str()` from `intel_display_power.c` (MIT, Copyright
© 2022 Intel), and `power_map.rs` now exposes the full source domain enum,
including display-core, eDP/DSI transcoder, DDI lane A/F, port-other, GMBUS,
and GT-IRQ identifiers.

`dc_state.rs` includes a tracked `gen9_set_dc_state()` request path from
`intel_display_power_well.c` (MIT, Copyright © 2022 Intel). Its observer maps
the source's PSR and DMC DC6-count side effects; boot-time disable uses the
helper during initialization with those side effects intentionally suppressed.

`power_well.rs` translates `hsw_power_well_sync_hw()` from
`intel_display_power_well.c` (MIT, Copyright © 2022 Intel), transferring a BIOS
request to the driver before clearing BIOS ownership; requester reads follow
BIOS, driver, KVMR, then debug order.

The kernel HSW power-well adapter gates Wa_16013190616 on Alder Lake-P/N and
PG1, rather than PG1 alone, matching `intel_display_power_well.c`.

`kernel/src/drm/intel/power.rs` associates HSW, ICL AUX and ICL DDI wells with
the source's separate BIOS/driver/debug request registers; only HSW has a KVMR
request register (`intel_display_power_well.c`, MIT, Copyright © 2022 Intel).

The HSW wait helper documents the upstream `fixed_enable_delay` branch as
DG2-only (600–1200 us); DG2 is not in the display-12/13 platform maps, while
its descriptor flag remains faithfully represented.

The kernel maps `PowerWellInstance::irq_pipe_mask` into the HSW power-well
sequence. While the parent IRQ is offline the upstream hooks are no-ops; while
it is live the unsupported pipe transition fails closed instead of replacing
the descriptor mask with zero.

`power_domains.rs` adds the mapped `sync_domain()` traversal from
`intel_display_power.c`; the kernel invokes it for Pipe-A before acquiring that
domain, and `MappedPowerWellIo` maps the HSW group to
`hsw_power_well_sync_hw()`. Other power domains and their well operations remain
unintegrated.

`dc_state.rs` translates `sanitize_disable_power_well_option()` from
`intel_display_power.c` (MIT, Copyright © 2022 Intel); negative values select
the source default of disabling power wells.

The kernel mapped power-domain enabled query calls translated
`hsw_power_well_enabled()` and therefore tests both the driver request and
state bits, rather than state alone (`intel_display_power_well.c`).

`power_domains.rs` translates the asynchronous domain put path from
`intel_display_power.c`: current/next masks, non-final immediate puts, pending
get reuse, max next delay, worker batch completion, and synchronous flush.
The surrounding kernel workqueue/runtime-PM adapter is not yet connected.

`power_well.rs` translates the TGL Type-C cold-block request and ICL TC cold
exit PCODE retry/timing helpers from `intel_display_power_well.c` (MIT,
Copyright © 2022 Intel); it returns source-shaped reports so the kernel can
map firmware transport and diagnostics. The TGL map callback is not yet wired.

The `intel_display_device.c` port in `tk-intel-display/src/device.rs` now also
maps TGL, RKL, ADL-S, ADL-P and ADL-N IDs to display version, DMC platform,
default ports and the source's pre-GMD_ID stepping tables. PCI ID values follow
`include/drm/intel/pciids.h` (MIT, Copyright © 2013 Intel). This pure classifier
does not expand the kernel's current N305-only native PCI binding/modeset path.

The kernel `PowerState` adapter exposes map-backed `get_domain()`,
`put_domain()`, `is_domain_enabled()` and `get_domain_if_enabled()` wrappers
around the translated power-domain manager. Pipe-A boot sync/get and an AUX-A
reference path exercise those APIs; connector/output call sites are not yet
fully migrated.

`tk-intel-display/src/cdclk.rs` adds source-shaped CDCLK transition predicates
and crawl/squash midpoint calculation from Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_cdclk.c` (MIT, Copyright © 2006-2017
Intel); see `docs/design/intel-cdclk.md` for the translated functions and
remaining runtime adapter work.

`kernel/src/drm/intel/clk.rs::transition()` adds the display-12/13 runtime
CDCLK ratio/enable/lock and PLL crawl/request/ack MMIO steps from the same
`intel_cdclk.c` source. It is not yet called by an atomic modeset path; PCode,
audio/PSR and AUX/GMBUS lock ordering remain caller-side gaps.

`tk-intel-display/src/dpll_mgr.rs` adds ICL/TGL combo PLL parameter search,
fixed DP/TBT tables, CFGCR state encode/decode, and the 38.4-MHz fraction
workaround from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_dpll_mgr.c`
(MIT, Copyright © 2006-2016 Intel); register-field definitions follow
`display/intel_display_regs.h` (MIT, Copyright © 2006-2018 Intel). See
`docs/design/intel-pll.md` §7. PLL resource allocation and MMIO manager
lifecycle remain separate gaps.

The same DPLL-manager translation also covers ICL/TGL combo-PHY candidate
masks, TC-port/MG-PLL identity mapping, active-port DPLL selection and a
shareable hardware-state/pipe-mask allocator corresponding to
`icl_get_combo_phy_dpll()`, `icl_tc_port_to_pll_id()`,
`icl_update_active_dpll()` and `intel_find_dpll()` plus reference edges. It is
not yet wired into the kernel's modeset atomic-state lifecycle.

`tk-intel-display/src/dpll_mgr.rs::icl_dpll_descriptors()` translates the
per-platform DPLL ID/type/alt-port inventories and source ordering for TGL,
RKL, DG1, ADL-S, ADL-P/N and EHL/JSL from Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_dpll_mgr.c` (MIT, Copyright © 2006-2016
Intel). IDs are kept platform-scoped because numeric ID 2 aliases different
PLL types between source profiles.

`tk-intel-display/src/dpll.rs` translates generic CRTC DPLL dispatch/state
preparation and ±1 kHz clock comparison from Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_dpll.c` (MIT, Copyright © 2020 Intel).
The dispatcher is exposed as a pure helper and not yet wired to the kernel
atomic modeset path.
The same file also translates `intel_dpll_init_clock_hook()` platform order and
`hsw_crtc_compute_clock()` dispatch/dotclock update; display-12/13 profile tests
select the HSW shared-DPLL family.

`kernel/src/drm/intel/pll.rs::enable_combo_pll()` and
`disable_combo_pll()` translate the combo DPLL0/1 power-state, CFGCR write,
lock and power-off sequence from Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_dpll_mgr.c` (MIT, Copyright © 2006-2016
Intel). The adapter retains i915's warning-only timeout outcome and is not yet
called by atomic modeset.

`kernel/src/drm/intel/pll.rs::enable_tbt_pll()` and `disable_tbt_pll()`
translate `icl_tbt_pll_enable()`/`icl_tbt_pll_disable()` power-state, TBT
CFGCR0/1 posting read, PLL-enable/lock and power-off flow from the same MIT
`intel_dpll_mgr.c` source. `TBT_PLL_{ENABLE,CFGCR0,CFGCR1}` register entries
follow `intel_display_regs.h` (MIT, Copyright © 2006-2018 Intel). The native
modeset does not call these functions yet.

`kernel/src/drm/intel/pll.rs::enable_tc_dkl_pll()` and
`disable_tc_dkl_pll()` connect the DKL PHY writer to a checked dynamic-register
backend and perform the TC PLL power/lock sequence. The selector is serialized
and the adapter currently admits only known TGL/ADL-P/N TC1/TC2 enable offsets;
the caller must hold display/PHY power references. The function order follows
`mg_pll_enable()`/`mg_pll_disable()` and `icl_pll_power_enable()`/
`icl_pll_disable()` in the same MIT source file; no native modeset call site is
connected yet.

`tk-intel-display/src/dpll_mgr.rs` extends the DKL MG PLL calculation to
source-shaped DisplayPort 8.1-GHz DCO and HDMI `[7992,10000]`-MHz window
selection, preserving `icl_mg_pll_find_divisors()` search priority and
`icl_calc_mg_pll_state()` fixed-point state generation. Source is Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_dpll_mgr.c` (MIT, Copyright © 2006-2016
Intel); targeted tests cover 162/540-MHz DP and 1080p60 HDMI.

`tk-intel-display/src/ddi.rs` adds TGL/ADL DDI helpers for clock-select,
buffer PHY link-rate/stagger fields, idle/active wait policy, and transcoder
function-control generation from Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_ddi.c` (MIT, Copyright © 2012 Intel).
See `docs/design/intel-ddi.md` for the implemented subset and the ports still
refused.

`kernel/src/drm/intel/ddi.rs` adapts the platform-generated combo DPCLKA RMW
plan to typed MMIO with a serialized two-write enable and one-write disable.
`kernel/src/drm/intel/output.rs::program()` now consumes it for the supported
N305 A/B output path; the sequence follows `_icl_ddi_enable_clock()` and
`_icl_ddi_disable_clock()` from the same MIT `intel_ddi.c` source.

`crates/ax/tk-intel-display/src/ddi_buf_trans.rs` translates display-12/13 DDI buffer-translation table data and platform selection from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_ddi_buf_trans.c` (MIT, Copyright © 2020 Intel Corporation); `LICENSE-MIT`.

`kernel/src/drm/intel/phy.rs::combo_phy_power_up_lane_mask` follows the DSI, lane-count and lane-reversal cases of Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_combo_phy.c::intel_combo_phy_power_up_lanes()` (MIT, Copyright © 2018 Intel); `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/tc.rs` additionally translates the TC mode-name/query, HPD-glitch routing, DP lane-count decoding, and mode/version dispatch helpers from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_tc.c` (MIT, Copyright © 2019 Intel); `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/hdmi.rs` now also translates `intel_write_infoframe()`'s byte-3 ECC-hole packing and `hsw_write_infoframe()`'s transcoder DIP write order from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_hdmi.c` (MIT, Copyright 2006 Dave Airlie and © 2006-2009 Intel); `LICENSE-MIT`.

The same `hdmi.rs` translation now includes the HSW deep-color GCP phase predicate/state builder, GCP payload write and HSW AVI/SPD/vendor/DRM infoframe enable sequence from Linux 7.2.3 `intel_hdmi.c` (MIT, Copyright 2006 Dave Airlie and © 2006-2009 Intel); `LICENSE-MIT`.

`kernel/src/drm/intel/combo_phy_full.rs` translates all 14 functions in Linux v7.2.3 `drivers/gpu/drm/i915/display/intel_combo_phy.c` (MIT, Copyright © 2018 Intel Corporation); `LICENSE-MIT`.

The HSW HDMI module also translates Intel's SPD infoframe defaults and DRM metadata/version gates from `intel_hdmi.c::intel_hdmi_compute_spd_infoframe()` and `intel_hdmi_compute_drm_infoframe()` (MIT, © 2006-2009 Intel).

`intel_hdmi_infoframe_enable()` in `hdmi.rs` maps all eight source packet-type values to software enable indices (MIT `intel_hdmi.c`, © 2006-2009 Intel).

`crates/ax/tk-intel-display/src/tc_state_machine.rs` translates 95 of the 96 display-12/13-applicable functions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_tc.c` (MIT, Copyright © 2019 Intel); `to_tc_port` is represented by the typed `TcPortState` input, and MTL/XELPDP display-14+ functions are out of scope. `LICENSE-MIT`.

The same module translates display-12/13 TMDS source limits and clock formula plus source/sink BPC predicates from `intel_hdmi.c` (MIT, © 2006-2009 Intel).

The HDMI path additionally translates `hdmi_port_clock_limit()`, `hdmi_port_clock_valid()`, and `intel_hdmi_compute_bpc()` display-12/13 TMDS/BPC selection branches from `intel_hdmi.c` (MIT, © 2006-2009 Intel).

`hdmi.rs` additionally translates `intel_has_hdmi_sink()`, `intel_hdmi_has_audio()`, `intel_hdmi_limited_color_range()`, and the RGB/Y420 branch of `intel_hdmi_sink_format_valid()` (MIT `intel_hdmi.c`, © 2006-2009 Intel).

`hdmi.rs` also translates `intel_hdmi_is_ycbcr420()`, `intel_hdmi_is_cloned()`, `intel_hdmi_compute_has_hdmi_sink()`, and the source scrambling-cap predicate from `intel_hdmi.c` (MIT, © 2006-2009 Intel).

`crates/ax/tk-intel-display/src/dp_aux.rs` translates all 35 function definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_dp_aux.c` (MIT, Copyright © 2020-2021 Intel Corporation); the kernel AUX MMIO/framework adapter remains unconnected. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_dp_link_training_full.rs` translates 66 of 78 definitions in Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_dp_link_training.c` (MIT, Copyright © 2008-2015 Intel Corporation); the twelve unported functions are DRM debugfs wrappers. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_gmbus_full.rs` translates 30 of 39 definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_gmbus.c` (MIT, Copyright © 2006 Dave Airlie and © 2006-2008, 2010 Intel); nine I2C framework wiring/registration methods are excluded. `LICENSE-MIT`.

`kernel/src/drm/intel/regs/aux.rs` declares the display-12/13 DP AUX channel A/B control and data register offsets from `intel_dp_aux_regs.h` (MIT, Copyright © 2023 Intel Corporation). No channel-specific kernel transfer backend is enabled by the declarations.

`crates/ax/tk-intel-display/src/intel_hotplug_full.rs` translates 37 of 44 functions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_hotplug.c` (MIT, Copyright © 2015 Intel); seven debugfs storm-control wrappers are omitted. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_dp_full.rs` translates 284 of 287 functions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_dp.c` (MIT, Copyright © 2008 Intel); omissions are two generic DRM property attachment wrappers and the display-14+-only MTL source-rate helper. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_hotplug_irq_full.rs` translates all 93 function definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_hotplug_irq.c` (MIT, Copyright © 2023 Intel Corporation). `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_ddi_full.rs` translates all 208 function definitions in Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_ddi.c` (MIT, Copyright © 2012 Intel Corporation); DRM object registration and cross-subsystem access are represented by `DdiIo` boundaries. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_hdmi_full.rs` translates 114 of 120 function definitions in Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_hdmi.c` (MIT, Copyright 2006 Dave Airlie and © 2006-2009 Intel); six generic DRM connector/property/modes wrappers are omitted. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/skl_scaler_full.rs` translates all 43 function definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/skl_scaler.c` (MIT, Copyright © 2020 Intel Corporation); DRM atomic state, CASF and DSB/MMIO are explicit hooks. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/skl_watermark_full.rs` translates all 140 function definitions in Linux 7.2.3 `drivers/gpu/drm/i915/display/skl_watermark.c` (MIT, Copyright © 2022 Intel Corporation); DRM atomic objects, PCODE, MMIO and debugfs are trait boundaries. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/skl_universal_plane_full.rs` translates all 112 function definitions in Linux 7.2.3 `drivers/gpu/drm/i915/display/skl_universal_plane.c` (MIT, Copyright © 2020 Intel Corporation); DRM/FB/atomic/IRQ/DSB/MMIO boundaries are traits. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_cursor_full.rs` translates all 39 function definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_cursor.c` (MIT, Copyright © 2020 Intel Corporation); DRM/FB/vblank/atomic/MMIO operations are trait boundaries. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_vblank_full.rs` translates all 30 ctags definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_vblank.c` (MIT, Copyright © 2022-2023 Intel Corporation); both I915/Xe vblank-section alternatives are represented. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_crtc_full.rs` translates all 39 ctags function definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_crtc.c` (MIT, Copyright © 2020 Intel Corporation); DRM/atomic/vblank/QoS operations use explicit hooks. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_color_full.rs` translates all 223 function definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_color.c` (MIT, Copyright © 2016 Intel Corporation); DRM objects, register access and DSB execution remain explicit framework boundaries. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_fb_full.rs` translates all 89 function definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_fb.c` (MIT, Copyright © 2021 Intel Corporation); framebuffer/GEM allocation and lifecycle operations remain explicit hooks. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_atomic_full.rs` translates all 15 function definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_atomic.c` (MIT, Copyright © 2015 Intel Corporation); DRM object allocation, state ownership, HDCP and DP tunnel helpers remain explicit hooks. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_modeset_verify_full.rs` translates all 7 function definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_modeset_verify.c` (MIT, Copyright © 2022 Intel Corporation); DRM object traversal, state readout and diagnostics remain hooks. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_modeset_setup_full.rs` translates all 25 function definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_modeset_setup.c` (MIT, Copyright © 2022 Intel Corporation); DRM state/object enumeration and diagnostics remain hooks. `LICENSE-MIT`.

`crates/ax/tk-intel-display/src/intel_display_modeset_full.rs` translates 223 of 273 ctags function definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_display.c` (MIT, Copyright © 2006-2007 Intel Corporation); 50 target-generation-excluded or DRM/GEM/debug boundary functions are enumerated with reasons in `docs/design/intel-display-modeset-full.md`. `LICENSE-MIT`.

`kernel/src/drm/color_mgmt_full.rs` translates all 26 ctags definitions from Linux 7.2.3 `drivers/gpu/drm/drm_color_mgmt.c` (MIT-style grant, Copyright (c) 2016 Intel Corporation); the full grant is retained in the Rust source and `kernel/LICENSES/LicenseRef-Intel-Color-Mgmt-MIT`.

`kernel/src/drm/atomic_uapi_full.rs` translates all 30 ctags definitions from Linux 7.2.3 `drivers/gpu/drm/drm_atomic_uapi.c` (MIT; Copyright (C) 2014 Red Hat, 2014/2018 Intel, and (c) 2020 The Linux Foundation); the full grant is retained in the source and `kernel/LICENSES/LicenseRef-DRM-Atomic-UAPI-MIT`.

`kernel/src/drm/plane_uapi_full.rs` translates all 38 ctags definitions from Linux 7.2.3 `drivers/gpu/drm/drm_plane.c` (MIT-style grant, Copyright (c) 2016 Intel Corporation); the full grant is retained in source and `kernel/LICENSES/LicenseRef-Intel-Drm-Plane-MIT`.

`kernel/src/drm/connector_uapi_full.rs` translates all 87 ctags definitions plus the unrecognized `drm_get_tv_mode_from_name()` from Linux 7.2.3 `drivers/gpu/drm/drm_connector.c` (MIT-style grant, Copyright (c) 2016 Intel Corporation); the full grant is retained in source and `kernel/LICENSES/LicenseRef-Intel-Drm-Connector-MIT`.

`kernel/src/drm/mode_config_full.rs` translates all 14 ctags function definitions from Linux 7.2.3 `drivers/gpu/drm/drm_mode_config.c` (MIT-style grant, Copyright (c) 2016 Intel Corporation); the full grant is shared with `kernel/LICENSES/LicenseRef-Intel-Drm-Connector-MIT`.

`crates/ax/tk-intel-display/src/intel_audio_dp_full.rs` translates 35/45 ctags functions from Linux 7.2.3 `intel_audio.c` (MIT, Copyright © 2014 Intel Corporation); ten HDMI-only or pre-Display-12 G4x/IBX functions are excluded. The source contains the full MIT grant and `LICENSES/Intel-i915-DP-Audio-MIT.txt` retains it.

`crates/ax/tk-intel-display/src/intel_dp_mst_full.rs` translates all 68 ctags definitions from Linux 7.2.3 `intel_dp_mst.c` (MIT; Copyright © 2008 Intel and 2014 Red Hat); full grant retained in source and `LICENSES/Intel-i915-DP-MST-MIT.txt`.

`crates/ax/tk-intel-display/src/intel_psr_full.rs` translates 155/155 ctags functions from Linux 7.2.3 `intel_psr.c` (MIT, Copyright © 2014 Intel Corporation); the full grant is retained in the Rust source and `LICENSES/Intel-i915-PSR-MIT.txt`.

`crates/ax/tk-intel-display/src/intel_fbc_full.rs` translates 126/134 ctags functions from Linux 7.2.3 `intel_fbc.c` (MIT-style Intel grant; Copyright © 2014 Intel Corporation); eight display-35+ system-cache or DRM debugfs functions are excluded with reasons in `docs/design/intel-fbc-full.md`.

`crates/ax/tk-intel-display/src/intel_pcode_full.rs` translates 14/14 ctags functions from Linux 7.2.3 `intel_pcode.c`, `intel_pcode.h`, and all 93 `intel_pcode_regs.h` definitions (MIT; Copyright © 2013-2021 Intel); the grant is retained in source and `LICENSES/Intel-i915-PCode-MIT.txt`.

`crates/ax/tk-intel-display/src/intel_cdclk_full.rs` translates 152/152 ctags functions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_cdclk.c` (MIT; Copyright © 2006-2017 Intel Corporation); the full grant is retained in source and `LICENSES/Intel-i915-CDCLK-MIT.txt`.

`crates/ax/tk-intel-display/src/intel_dpll_mgr_full.rs` translates 83/175 ctags functions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_dpll_mgr.c` (MIT; Copyright © 2006-2016 Intel Corporation); 92 functions for Gen7/8/9, BXT, IBX, display-14+ and Xe3 are outside the display-12/13 target and are listed in `docs/design/intel-dpll-manager-full.md`.

`kernel/src/drm/intel/connect.rs` applies the extended DPCD receiver-capability selection behavior of Linux 7.2.3 `drivers/gpu/drm/display/drm_dp_helper.c` (`drm_dp_read_dpcd_caps()` / `drm_dp_read_extended_dpcd_caps()`, MIT; Copyright © 2009 Keith Packard); the grant is retained in `kernel/LICENSES/LicenseRef-DRM-DPCD-MIT`.

`crates/ax/tk-intel-display/src/intel_audio_legacy_remainder.rs` translates the remaining 10/45 ctags functions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_audio.c` (MIT; Copyright © 2014 Intel Corporation), completing source coverage alongside `intel_audio_dp_full.rs`; the shared Intel grant remains in `LICENSES/Intel-i915-DP-Audio-MIT.txt`.

`kernel/src/drm/crtc_uapi_full.rs` translates all 26 ctags functions from Linux 7.2.3 `drivers/gpu/drm/drm_crtc.c` (Intel permissive MIT-style grant; Copyright (c) 2006-2008 Intel Corporation, 2007 Dave Airlie, and 2008 Red Hat); the full grant is retained in the source and `kernel/LICENSES/LicenseRef-Intel-Drm-Crtc-MIT`.

`kernel/src/drm/property_uapi_full.rs` translates all 26 ctags functions from Linux 7.2.3 `drivers/gpu/drm/drm_property.c` (Intel permissive MIT-style grant; Copyright (c) 2016 Intel Corporation); the full grant is retained in source and `kernel/LICENSES/LicenseRef-Intel-Drm-Property-MIT`.

`kernel/src/drm/framebuffer_uapi_full.rs` translates all 27 ctags functions from Linux 7.2.3 `drivers/gpu/drm/drm_framebuffer.c` (Intel permissive MIT-style grant; Copyright (c) 2016 Intel Corporation); the full grant is retained in source and `kernel/LICENSES/LicenseRef-Intel-Drm-Framebuffer-MIT`.

## 2026-10-09 excerpt-count reconciliation

The whole-tree scanner was re-run against `/home/ava/Desktop/linux-7.2.3`
using `scripts/ci/scan_linux_excerpts.py` semantics (whitespace-normalized
line hashes, default ASCII-rule filter) at thresholds 40 and 25. The measured
seven-field tuples now pinned in `tests/ci/test_linux_excerpt_baseline.py` are:

| Scope | >=40 | >=25 |
| --- | --- | --- |
| `crates/linux` | `(110,49,20,25,0,18,21)` | `(211,59,22,41,3,20,21)` |
| `kernel/src` | `(86,42,20,142,9,0,0)` | `(159,52,26,317,21,0,0)` |
| `crates/ax` | `(0,0,0,279,127,0,0)` | `(18,15,4,379,223,0,0)` |

The `crates/ax` increase from the prior baseline is attributable to the new
`tk-intel-display` Linux 7.2.3 i915 translations: scanning that crate alone
produced 184 outside-fence matches (39 code) at >=40 and 250 (105 code) at
>=25. These exact-line matches occur in `tk-intel-display/src` translation
files; the crate declares MIT, retains `LICENSE-MIT`, and its `NOTICE` maps
the source modules to MIT i915 display files. This is not the earlier small
oracle-shim delta; the prior totals in this document are historical.

The `kernel/src` increase is a separate whole-scope measurement, not an i915
attribution: current matches include DRM-core UAPI translations and Linux
comments/code outside `drm/intel` (for example syscall, BPF and netlink
paths). The scan establishes line identity against Linux 7.2.3, not a license
or source-file classification. Do not describe this delta as MIT i915. The
kernel-source provenance entries and license texts remain the authority for
those files; this count reconciliation changes only the measured baseline.

`crates/ax/tk-intel-gt/src/uc.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_uc_fw.c`, `__uc_fw_auto_select` platform GuC/HuC filename/version table for TGL/RKL/ADL-S/ADL-P (MIT, Copyright © 2016-2019 Intel Corporation); ADL-N selects ADL-S firmware, while its runtime platform defaults remain ADL-P/N. Metadata only; no binary included.

`crates/ax/tk-intel-gt/src/uc.rs::parse_css`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_uc_fw.c::__check_ccs_header` size-field validation, translated with checked arithmetic (MIT, Copyright © 2016-2019 Intel Corporation); WOPCM and file-size checks retained.

`crates/ax/tk-intel-gt/src/guc_fw.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_guc_fw.c::guc_load_done`, terminal GuC/BootROM status decoding (MIT, Copyright © 2014-2019 Intel Corporation); register access remains through existing `GtIo`.

`crates/ax/tk-intel-gt/src/uc.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_uc_fw.c::uc_unpack_css_version` and `guc_read_css_info`, CSS ABI version extraction and GuC 69/70 compatibility branches (MIT, Copyright © 2016-2019 Intel Corporation).

`crates/ax/tk-intel-gt/src/uc.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_uc.c::uc_expand_default_options`, Gen12 TGL/RKL/ADL-S/ADL-P GuC/HuC defaults (MIT, Copyright © 2016-2019 Intel Corporation); ADL-N follows the default branch and enables both HuC authentication and GuC submission.

`crates/ax/tk-intel-gt/src/guc_fw.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_guc_fw.c::guc_prepare_xfer`, Gen12.0 shim-control and doorbell-enable writes in source order (MIT, Copyright © 2014-2019 Intel Corporation).

`crates/ax/tk-intel-gt/src/guc_fw.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_guc_fw.c::guc_wait_ucode`, readiness polling with three one-second default release attempts (MIT, Copyright © 2014-2019 Intel Corporation); status reads use `GtIo`.

`crates/ax/tk-intel-gt/src/uc.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_uc_fw.c::intel_uc_check_file_version`, selected/wanted major validation and older-minor/patch classification for the non-overridden supported path (MIT, Copyright © 2016-2019 Intel Corporation).

`crates/ax/tk-intel-gt/src/guc_fw.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_uc_fw.c::uc_fw_xfer`, source/destination/size/control programming and completion poll order (MIT, Copyright © 2016-2019 Intel Corporation); ambiguous DMA retirement fails closed.

`crates/ax/tk-intel-gt/src/guc_fw.rs`: Linux 7.2.3 `intel_guc_fw.c::intel_guc_fw_upload`/`guc_xfer_rsa_mmio` and `intel_huc_fw.c::intel_huc_fw_upload`, Gen12.0 RSA scratch, WOPCM destination and HuC ukernel DMA paths (MIT, Copyright © 2014-2019 Intel Corporation); GGTT source residency/forcewake are explicit caller inputs.

`crates/ax/tk-intel-gt/src/guc_fw.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_huc.c::intel_huc_is_authenticated` and `intel_huc_wait_for_auth_complete`, `GEN11_HUC_KERNEL_LOAD_INFO` / `HUC_LOAD_SUCCESSFUL` poll for Gen11+ (MIT, Copyright © 2014-2019 Intel Corporation); GuC authentication is sent through the MMIO HXG transport, while GuC CT remains unimplemented.

`crates/ax/tk-intel-gt/src/guc_fw.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_guc.c::intel_guc_send_mmio` and `intel_guc_auth_huc`, Gen11+ HXG busy/retry/failure handling and HuC-auth action transport (MIT, Copyright © 2014-2019 Intel Corporation); register ownership and forcewake remain with the caller.
`crates/ax/tk-intel-gt/src/guc_fw.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_guc.c::guc_send_reg` and `intel_guc_notify`, four-dword GuC send-register indexing and H2G notification write (MIT, Copyright © 2014-2019 Intel Corporation).

`crates/ax/tk-intel-gt/src/uc.rs`: Linux 7.2.3 `include/drm/intel/pciids.h` TGL/RKL/ADL-S/ADL-P/ADL-N device-ID tables (MIT, Copyright 2013 Intel Corporation), translated to a runtime `Platform` selector.
`crates/ax/tk-intel-gt/src/uc.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_uc_fw.h::intel_uc_fw_status`, the uC firmware phase enum and fetch/init/upload state transitions (MIT, Copyright © 2014-2019 Intel Corporation); unavailable and invalid fetches remain typed loader errors.
`crates/ax/tk-intel-gt/src/guc_fw.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_guc_fw.c::guc_wait_ucode`, BootROM/GuC failure classification and source error mapping for key, signature, exception, save/restore, KLV, and HWCONFIG failures (MIT, Copyright © 2014-2019 Intel Corporation).
`crates/ax/tk-intel-gt/src/guc_config.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_guc.c::{guc_ctl_debug_flags,guc_ctl_feature_flags,guc_ctl_log_params_flags,guc_ctl_ads_flags,guc_ctl_wa_flags,guc_ctl_devid,guc_init_params,intel_guc_write_params}` and `intel_guc_fwif.h`, GuC control block flags and soft-scratch serialization (MIT, Copyright © 2014-2019 Intel Corporation); parameter calculation is implemented, but GuC ADS/log allocations and runtime call-site wiring remain pending.
`crates/ax/tk-intel-gt/src/guc_log.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_guc_log.c` sizing/default-level/section-offset, overflow accounting and debug/crash ring-snapshot helpers plus `intel_guc_log.h`/`intel_guc_fwif.h` state ABI (MIT, Copyright © 2014-2019 Intel Corporation); buffer GGTT allocation, interrupt/workqueue handling, and user relay endpoint are not wired.
`crates/ax/tk-intel-gt/src/guc_ct.rs`: Linux 7.2.3 `intel_guc_ct.c` CTB ring/reset/send/response, receive credit handling, inline TLB completion, FIFO deferred-event dispatch, explicit disable and `intel_guc_send_busy_loop()` helpers plus the complete constant/layout surface of `guc_communication_ctb_abi.h`, `guc_communication_mmio_abi.h`, and `guc_messages_abi.h` (MIT, Copyright © 2014-2021 Intel Corporation; C driver Copyright © 2016-2019 Intel Corporation); CT VMA allocation and self-config/enable registration are wired for the default-submission path; VMA fini/owner teardown, IRQ/tasklet/workqueue dispatch and H2G client callsites remain pending.
`crates/ax/tk-intel-gt/src/guc_fw.rs`: Linux 7.2.3 `intel_guc.c::{__guc_action_self_cfg,__guc_self_cfg,intel_guc_self_cfg32,intel_guc_self_cfg64}` and `guc_actions_abi.h`/`guc_klvs_abi.h`, MMIO self-config KLV request layout and 32/64-bit value handling (MIT, Copyright © 2014-2021 Intel Corporation).
`crates/ax/tk-intel-gt/src/huc.rs`: Linux 7.2.3 `intel_huc.c::{intel_huc_is_authenticated,intel_huc_wait_for_auth_complete,intel_huc_auth,intel_huc_check_status}` Gen11+/Gen12 legacy GuC-auth path (MIT, Copyright © 2016-2019 Intel Corporation); GSC/MEI delayed-load modes are excluded for the TGL/RKL/ADL integrated target.
`crates/ax/tk-intel-gt/src/guc_submission.rs`: Linux 7.2.3 `intel_guc_submission.c` scheduler state/request scheduling and G2H completion plus context-reset/engine-failure payload validation, contiguous multi-LRC/single-LRC ID partitions, v69 descriptor pool layout/reset and v69/v70 descriptor/registration/policy packets, a CTB-backed context state facade, parent-scratch v69/v70 layout, circular multi-LRC work-queue append, and `intel_guc_fwif.h` v69/v70 registration/process/WQ descriptor ABI (MIT, Copyright © 2014-2019 Intel Corporation); kernel CT owner exposes register/submit/receive adapters but no engine context lifecycle or IRQ/tasklet caller is wired; preemption/reset flow remain pending.
`crates/ax/tk-intel-gt/src/guc_ads.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_guc_ads.c` and `intel_guc_fwif.h`, GuC ADS ABI/layout, policy update, source engine-class maps, first default-LRC selection, `guc_mmio_regset_init()` ring/WA/whitelist/MOCS/EU-perf entry packing and steering flags, Gen12 12.55 LRC skip-size, golden-context/capture/WAKLV/private-data sections, full blob reset, and engine-usage offsets (MIT, Copyright © 2014-2019 Intel Corporation); live GT engine/default-state/MCR collection and startup parameter wiring remain pending. `kernel/src/drm/intel/gt/copy.rs` adds an input-driven pinned ADS VMA allocator/reset owner, not yet invoked by GT startup.
`crates/ax/tk-intel-gt/src/guc_capture.rs`: Linux 7.2.3 `intel_guc_capture.c` and `guc_capture_fwif.h` error-capture null header, output-size/3x buffer assessment, ring counters, wrap-safe dword extraction, capture-group/data/register header decoding, unknown-type skipping, dependent-engine node grouping with shared list cloning, preallocated 1536-node cache with bounded register arrays and outlist reuse, context/engine/LRCA node matching and IPEHR/INSTDONE extraction, ADS list/null-header cache, Xe_LP static register offsets/names, slice/subslice steered-register expansion and 12.55+ geometry register gate, base/ext register-list selection, page-aligned ADS capture-list serialization and coredump text formatting adapter (MIT, Copyright © 2021-2022 Intel Corporation); `guc_log::process_capture_log()` snapshots/acks shared state and `copy::handle_guc_capture_notification()` drains it into the cache and sends CT flush-complete; G2H IRQ/workqueue caller, cache destroy lifecycle, ADS registration and coredump hookup remain pending.
`crates/ax/tk-intel-gt/src/guc_log.rs`: Linux 7.2.3 `intel_guc_log.c`/`intel_guc_log.h` log sizing, default log-level policy, overflow/ring snapshot, control/flush action ABI and state-after-success log-level controller (MIT, Copyright © 2014-2019 Intel Corporation); `kernel/src/drm/intel/gt/copy.rs` has a zeroed pinned log-VMA allocator, but GUC_CTL params, relay lifecycle, workqueue and CT action callers remain pending.
`crates/ax/tk-intel-gt/src/wopcm.rs`: Linux 7.2.3 `gt/intel_wopcm.c`, Gen12 WOPCM partition layout, GuC/HuC firmware and reserved-region bounds, BIOS lock-state decoding, `uc_init_wopcm()` register write/readback verification (MIT, Copyright © 2017-2019 Intel Corporation); called before GT firmware DMA for the integrated GT targets; media-GT/deprivileged partition discovery remains unsupported.
`kernel/src/drm/intel/gt/copy.rs`: ADS/log GGTT VMA owner adapters based on `intel_guc_ads.c::intel_guc_ads_create()` and `intel_guc_log.c::intel_guc_log_create()`, address-checked `intel_guc_write_params()` scratch publication, workqueue-side debug-log snapshot/ack and capture notification drain/CT flush-complete adapters (MIT); allocate, bind, initialize and retain buffers once configured. Runtime GT-derived ADS inputs/options, firmware-loader/IRQ/workqueue callers, relay output delivery and teardown remain pending.
`crates/ax/tk-intel-gt/src/guc_fw.rs`: Linux 7.2.3 `intel_guc.c` `intel_guc_suspend()` / `intel_guc_resume()` CLIENT_SOFT_RESET action, ignore-on-failure behavior and GuC-domain sanitize/reset policy (MIT, Copyright © 2014-2021 Intel Corporation); PM/runtime suspend callback and retained CT/ADS/log owner teardown are not wired.
`crates/ax/tk-intel-gt/src/reset.rs`: Linux 7.2.3 `gt/intel_reset.c` `intel_reset_guc()`/`__reset_guc()` Gen12 GuC-only GDRST domain reset, pre-12.70 double-reset policy and settling delay (MIT, Copyright © 2008-2018 Intel Corporation); wired before HuC/GuC firmware transfer; display/global reset domains remain excluded.
`kernel/src/drm/intel/gt/copy.rs`: N305-only GuC boot sequence now sizes/pins ADS and baseline log buffers, publishes GUC_CTL scratch parameters before GuC DMA, then initializes CTB self-config and verifies a control-action HXG response over CTB, using upstream `intel_guc_ads.c`, `intel_guc_log.c`, `intel_guc.c`, and `intel_guc_ct.c` behavior (MIT); runtime ADS MMIO regsets/golden contexts/capture lists, async CT IRQ/event dispatch and non-N305 ADS callers remain pending.
`crates/ax/tk-intel-gt/src/intel_ring.rs`: Linux 7.2.3 `gt/intel_ring.c` ring-space update, pin/unpin/map unwind, reset, GEM VMA fallback, engine ring creation/free, timeline request space wait, wrap/noop emission and begin reservation (MIT, Copyright © 2019 Intel Corporation); GPU object/GGTT/timeline calls are behind a narrow backend and the GuC/execlists call sites remain to be wired.
`kernel/src/drm/intel/gt/copy.rs`: N305 GuC video engine inventory now preserves the physical VDBOX enable mask while compacting logical VCS IDs in upstream `intel_engine_cs.c::setup_logical_ids()` order (MIT); ADL-N VCS0/VCS2 maps to GuC table columns 0/1 -> physical instances 0/2.
`crates/ax/tk-intel-gt/src/intel_ring.rs`: ring mappings now remain owned by the GGTT backend rather than copied into a detached `Vec`; `intel_ring_begin()` returns an offset/length `RingSpan`, with `intel_ring_emit()` writing through the backend mapping to preserve hardware-visible ring contents (Rust adapter for MIT `intel_ring.c` semantics).
`kernel/src/drm/intel/gt/copy.rs`: private prebound BCS/RCS command-ring pages now use `tk-intel-gt::intel_ring::{intel_ring_begin,intel_ring_emit}` to reserve and write the source ring bytes directly through `Ram`/GGTT backend (MIT `intel_ring.c`); the current ELSQ submit path and GuC/execlists production caller remain unchanged.
`crates/ax/tk-intel-gt/src/execlists.rs`: Linux 7.2.3 `gt/intel_execlists_submission.c` `write_desc()` and `execlists_submit_ports()` implement descriptor dword order, clearing every Gen12 ELSQ slot in reverse order, and explicit queue load (MIT, Copyright © 2014 Intel Corporation); wired into the existing private BCS/RCS submit path, while execlists scheduling, CSB completion, preemption, and GuC production submission remain unintegrated.
`crates/ax/tk-intel-gt/src/execlists.rs`: `__gen12_csb_parse()` and `gen12_csb_parse()` translate Gen12 CSB completion/status decoding from `intel_execlists_submission.c`; upstream `GEM_BUG_ON` invariants return `Error::Refused` in the callable adapter. CSB interrupt/tasklet scheduler dispatch has not yet been connected.
`crates/ax/tk-intel-gt/src/execlists.rs`: `process_gen12_csb()` drains the 12-entry Gen11/12 HWS status ring through the source decoder; `copy.rs::consume_gen12_csb()` reads HWS slots after a completed synchronous BCS/RCS job, before reset. This polling caller does not implement the upstream IRQ/tasklet state machine or scheduling actions.
`kernel/src/drm/intel/gt/copy.rs`: `csb_read()` / `wa_csb_read()` mirror the TGL 10us stale-HWSP wait and GEN8/GEN11 MMIO fallback for each status entry, then write the consumed `U64_MAX` sentinel; `gt.rs` admits only the corresponding read-only BCS/RCS MMIO status ranges.
`kernel/src/drm/intel/gt/copy.rs`: GuC submission G2H receive now retains valid non-event response completions for their fence waiters while publishing the updated shared receive head; event-class handling remains limited to context scheduling and deregistration until the async event workers are connected.
`kernel/src/drm/intel/gt/copy.rs`: adds a CT adapter to finish a previously received GuC submission response by fence and return its reserved G2H credits to the shared blob; scheduling/deregister G2H events remain the only event actions currently dispatched.
`kernel/src/drm/intel/gt/copy.rs` and `crates/ax/tk-intel-gt/src/guc_submission.rs`: N305 single-LRC BCS jobs now use Gen12 v70 context registration, normal scheduling policy, context-mode submit, HWS scratch polling, and a GuC deregistration event before transient LRC/ring GGTT bindings are retired; only while GuC remains in MIA reset do user jobs use direct ELSQ. This adapter does not implement general GuC request queues, preemption, async IRQ handling, RCS GuC WAs, or media-engine execution.
`crates/ax/tk-intel-gt/src/guc_ads.rs` and `kernel/src/drm/intel/gt/copy.rs`: pass and reserve page-aligned full LRC image sizes in ADS, while writing `engine_state_size` as full size minus upstream `LRC_SKIP_SIZE` (MIT `intel_guc_ads.c::guc_prep_golden_context` / `guc_init_golden_context`). This keeps each golden-context slot and following ADS region at the upstream offset.
`kernel/src/drm/intel/gt/copy.rs`: feeds retained, hardware-captured BCS and optional RCS default LRC images through `guc_ads::guc_init_golden_contexts()` into the startup ADS; absent render/media defaults remain zeroed while full slots stay reserved, matching `intel_guc_ads.c::guc_init_golden_context` (MIT).
`kernel/src/drm/intel/gt/copy.rs` and `crates/ax/tk-intel-gt/src/guc_ads.rs`: N305 GuC ADS builds Gen12.0 save/restore MMIO regsets for RCS0, BCS0 and fuse-enabled VCS/VECS instances from discovered DSS steering and upstream register/MOCS/WA inputs; `guc_ads.rs::EngineRegsetInput` retains all seven EU performance registers required by `intel_guc_ads.c::guc_mmio_regset_init()` (MIT). Existing render objects can take the synchronous GuC context register/schedule route; media submission callers remain pending.
`crates/ax/tk-intel-gt/src/lrc.rs` and `bcs.rs`: Gen12 XCS LRC and flush/ring generation now parameterize the upstream `gen12_xcs_offsets`, `lrc_update_regs`/`lrc_descriptor`, `gen12_get_aux_inv_reg`/`gen12_emit_aux_table_inv`, and `gen12_emit_flush_xcs` rules for BCS0/VCS0/VCS2/VECS0 (Linux 7.2.3, MIT, Intel copyright lines retained in their source modules). BCS keeps its prior API; video-class callers are not wired yet.
`crates/ax/tk-intel-gt/src/intel_engine_types_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/intel_engine_types.h` MIT owner-record binding; `IntelEngineCs` now consumes this `IntelEngineCs`/execlists/props/WAs/breadcrumb/scheduler/context/timeline type graph, with x86_64 size/offset assertions. Remaining Linux GT services are compatibility-layer bindings, not duplicated records.
`crates/ax/tk-intel-gt/src/intel_gt_types_upstream.rs`: Linux 7.2.3 `gt/intel_gt_types.h` MIT `IntelGt` record and nested engine-map/info/WAs/PM/UC state; now uses owner modules for GSC, UC, RC6, reset, RPS, SSEU, wakeref, WOPCM, LLC, HW config, migrate, and buffer pool where available; ABI assertions guard the x86_64 layout.
`crates/ax/tk-intel-gt/src/intel_context_types_upstream.rs`: Linux 7.2.3 `gt/intel_context_types.h` MIT context, flags, ops and inline predicate records; target config and C-probed ABI offsets are asserted.
`crates/ax/tk-intel-gt/src/i915_request_types_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/i915_request.h` MIT request/fence records and source-config capture-error layout.
`crates/ax/tk-intel-gt/src/i915_scheduler_types_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/i915_scheduler_types.h` MIT scheduler attributes/nodes/dependencies/engine records and callback ABI.
`crates/ax/tk-intel-gt/src/i915_gem_context_types_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gem/i915_gem_context_types.h` MIT by-value context records and target layout assertions.
`crates/ax/tk-intel-gt/src/intel_breadcrumbs_types_upstream.rs`: Linux 7.2.3 `gt/intel_breadcrumbs_types.h` MIT exact breadcrumb fields and callback ABI.
`crates/ax/tk-intel-gt/src/intel_timeline_types_upstream.rs`: Linux 7.2.3 `gt/intel_timeline_types.h` MIT field-order/layout binding; timeline C translation imports this owner type directly.
`crates/ax/tk-intel-gt/src/intel_lrc_types_upstream.rs`: Linux 7.2.3 `gt/intel_lrc.h` MIT source constants, enum values and inline bit-field helpers.
`crates/ax/tk-intel-gt/src/intel_lrc_reg_types_upstream.rs`: Linux 7.2.3 `gt/intel_lrc_reg.h` MIT register offsets and PDP helper macros.
`crates/ax/tk-intel-gt/src/intel_workarounds_types_upstream.rs`: Linux 7.2.3 `gt/intel_workarounds_types.h` MIT register unions, WA record, and bitfield storage/accessors; `intel_workarounds_upstream.rs` uses these header records.
`crates/ax/tk-intel-gt/src/intel_wakeref_types_upstream.rs`: Linux 7.2.3 `intel_wakeref.h` MIT owner records, constants, inline references and out-of-line API bindings; 144-byte config is debug-wakeref-disabled.
`crates/ax/tk-intel-gt/src/intel_sseu_types_upstream.rs`: Linux 7.2.3 `gt/intel_sseu.h` MIT SSEU masks/device-info, source enum/bit constants and inline helper declarations; x86_64 bitfield storage and layouts are asserted.
`crates/ax/tk-intel-gt/src/intel_uc_fw_types_upstream.rs`: Linux 7.2.3 `gt/uc/intel_uc_fw.h` MIT firmware records, status/type enums and firmware state predicates; embeds the canonical 296-byte capture-error VMA resource and asserts its 416-byte outer ABI.
`crates/ax/tk-intel-gt/src/i915_vma_resource_types_upstream.rs`: Linux 7.2.3 `i915_vma_resource.h` MIT page-size, bindinfo and VMA-resource records with capture-error-enabled bitfield storage and fixed target offsets.
`crates/ax/tk-intel-gt/src/intel_guc_types_upstream.rs`: Linux 7.2.3 `gt/uc/intel_guc.h` MIT GuC/state/timestamp/interrupt records, constants and inline firmware-state predicates; includes the owner CT/log/SLPC/firmware/SSEU dependencies.
`crates/ax/tk-intel-gt/src/intel_guc_ct_types_upstream.rs`: Linux 7.2.3 `gt/uc/intel_guc_ct.h` MIT CT buffer/requests/transport records and source action constants under the configured debug-off ABI.
`crates/ax/tk-intel-gt/src/intel_guc_submission_types_upstream.rs`: Linux 7.2.3 `gt/uc/intel_guc_submission.h` MIT submission prototypes and capability predicates with canonical GuC/engine/request owner types.
`crates/ax/tk-intel-gt/src/intel_guc_log_types_upstream.rs`: Linux 7.2.3 `gt/uc/intel_guc_log.h` MIT log state/section records, level constants/macros, and target 224-byte ABI assertions.
`crates/ax/tk-intel-gt/src/intel_guc_slpc_types_upstream.rs`: Linux 7.2.3 `gt/uc/intel_guc_slpc_types.h` MIT SLPC state record and reset timeout constant; target 120-byte ABI.
`crates/ax/tk-intel-gt/src/intel_guc_slpc_upstream.rs`: Linux 7.2.3 `gt/uc/intel_guc_slpc.h` MIT SLPC inline state helpers and C API declarations.
`crates/ax/tk-intel-gt/src/intel_huc_types_upstream.rs`: Linux 7.2.3 `gt/uc/intel_huc.h` MIT HuC records/enums/constants, inline state helpers and C declarations; target layout assertions cover the 608-byte HuC state.
`crates/ax/tk-intel-gt/src/intel_guc_rc_types_upstream.rs`: Linux 7.2.3 `gt/uc/intel_guc_rc.h` MIT RC predicates and C API declarations, delegating submission-used policy to its owner header.
`crates/ax/tk-intel-gt/src/intel_gsc_types_upstream.rs`: Linux 7.2.3 `gt/intel_gsc.h` MIT distinct graphics security controller/interface record and C API declarations.
`crates/ax/tk-intel-gt/src/intel_gsc_uc_types_upstream.rs`: Linux 7.2.3 `gt/uc/intel_gsc_uc.h` MIT GSC firmware/proxy record, action bits, state helpers and API declarations; target state size576.
`crates/ax/tk-intel-gt/src/intel_rc6_types_upstream.rs`: Linux 7.2.3 `gt/intel_rc6_types.h` MIT residency record, enum aliases and target 104-byte ABI.
`crates/ax/tk-intel-gt/src/intel_reset_types_upstream.rs`: Linux 7.2.3 `gt/intel_reset_types.h` MIT reset state/flag masks; embeds the configured 32-byte tree-SRCU ABI.
`crates/ax/tk-intel-gt/src/intel_rps_types_upstream.rs`: Linux 7.2.3 `gt/intel_rps_types.h` MIT RPS/IPS/EI/frequency-capability records and enum values; target 280-byte ABI.
`crates/ax/tk-intel-gt/src/intel_wopcm_types_upstream.rs`: Linux 7.2.3 `gt/intel_wopcm.h` MIT WOPCM record, GuC subrecord, inline accessors and initialization declarations.
`crates/ax/tk-intel-gt/src/intel_hwconfig_types_upstream.rs`: Linux 7.2.3 `gt/intel_hwconfig.h` MIT size/pointer owner record and init/fini declarations.
`crates/ax/tk-intel-gt/src/intel_migrate_types_upstream.rs`: Linux 7.2.3 `gt/intel_migrate_types.h` MIT embedded migrate-context record.
`crates/ax/tk-intel-gt/src/intel_llc_types_upstream.rs`: Linux 7.2.3 `gt/intel_llc_types.h` MIT target-config empty LLC record.
`crates/ax/tk-intel-gt/src/intel_gt_buffer_pool_types_upstream.rs`: Linux 7.2.3 `gt/intel_gt_buffer_pool_types.h` MIT buffer-pool/node records and anonymous union with source-compatible field access paths.
`crates/ax/tk-intel-gt/src/intel_migrate_upstream.rs`: Linux 7.2.3 `gt/intel_migrate.h` MIT C API declarations using the separate `intel_migrate_types.h` owner record; migration implementation and runtime callers remain pending.
`crates/ax/tk-intel-gt/src/linux/srcu.rs`: target-bound Linux 7.2.3 `include/linux/srcutree.h` `srcu_struct` storage for CONFIG_TREE_SRCU=y/CONFIG_LOCKDEP=n; this is an embedded LinuxKPI record, not an SRCU runtime implementation.
`crates/ax/tk-intel-gt/src/intel_uncore_types_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/intel_uncore.h` MIT full uncore/MMIO/forcewake owner records, function tables, raw 64-bit access order, inline flag and wait helpers, and API declarations; replaces the partial prefix in the workarounds C translation.
`crates/ax/tk-intel-gt/src/intel_engine_regs_upstream.rs`: Linux 7.2.3 `gt/intel_engine_regs.h` MIT register-offset constructors, ring/PP/Execlist/SFC constants and parameterized register/field macros.
`crates/ax/tk-intel-gt/src/intel_uc_types_upstream.rs`: Linux 7.2.3 `gt/uc/intel_uc.h` MIT uC ops/state record, all source state-checker wrappers, idle-wait and ops dispatch helpers, with canonical GSC/Guc/HuC/RC/SLPC owners.
`crates/ax/tk-intel-gt/src/intel_gt_api_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/intel_gt.h` MIT declarations, inline GT helpers, source range/step checks, and GT/engine iteration macros. `GT_TRACE` is left at the unavailable `GEM_TRACE` framework boundary.
`crates/ax/tk-intel-gt/src/intel_gt_defines_types_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/intel_gt_defines.h` MIT `I915_MAX_GT` source constant.
`crates/ax/tk-intel-gt/src/i915_vma_types_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/i915_vma_types.h` MIT VMA flag constants and complete source-order `I915Vma` record with target size/offset assertions. The adjacent GTT-view BUILD_BUG_ON helper remains omitted until its owning type header is translated.
`crates/ax/tk-intel-gt/src/i915_vma_api_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/i915_vma.h` MIT declarations, inline state/flag helpers, active/pin/list/sync APIs, and GGTT list iteration; explicit remaining boundaries are documented in the file.
`crates/ax/tk-intel-gt/src/intel_ring_types_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/intel_ring_types.h` MIT ring ABI/constants; the canonical 56-byte `IntelRing` replaces the former context-source duplicate.
`crates/ax/tk-intel-gt/src/i915_request_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/i915_request.c` MIT request/fence source translation; ctags coverage 84/84 function markers.
`crates/ax/tk-intel-gt/src/i915_scheduler_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/i915_scheduler.c` MIT scheduler source translation; ctags coverage 23/23 function markers.
`crates/ax/tk-intel-gt/src/intel_guc_fwif_types_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_guc_fwif.h` MIT ABI constants, structures, enums and inline helpers.
`crates/ax/tk-intel-gt/src/intel_guc_actions_abi_types_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/abi/guc_actions_abi.h` MIT action constants/enums, including TLB invalidation ABI.
`crates/ax/tk-intel-gt/src/intel_gt_mcr_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/intel_gt_mcr.h` MIT MCR declarations and subslice steering iteration semantics.
`crates/ax/tk-intel-gt/src/i915_gem_object_types_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gem/i915_gem_object_types.h` MIT GEM object/MM/VMA/MMO records, owner operations and target-config layout assertions.
`crates/ax/tk-intel-gt/src/i915_gem_shmem_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gem/i915_gem_shmem.c` MIT source translation; ctags coverage 21/21 function markers.
`crates/ax/tk-intel-gt/src/i915_gem_object_header_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gem/i915_gem_object.h` MIT active inline functions, source order (46 configured definitions); consumes the separate page/object API owners and documents unresolved DRM `idr_find`/GEM-free service bindings.
`crates/ax/tk-intel-gt/src/i915_gem_context_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gem/i915_gem_context.c` MIT source translation, registered for compile integration; its framework calls remain dependent on task/signal, DRM-client, tracepoint, mutex, and engine bindings.
`crates/ax/tk-intel-gt/src/intel_ring_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/intel_ring.c` MIT source translation, registered for compile integration; ring allocation/pinning and request/active dependencies remain incomplete.
`crates/ax/tk-intel-gt/src/intel_context_api_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/intel_context.h` MIT active inline/API helper translation for CONFIG_LOCKDEP=n and replay API disabled; 41/41 target-active functions in source order.
`crates/ax/tk-intel-gt/src/linux/average.rs`: Independent fixed-point EWMA expansions for the source-configured `DECLARE_EWMA(runtime, 3, 8)` and `DECLARE_EWMA(_engine_latency, 6, 4)` layouts.
`crates/ax/tk-intel-gt/src/linux/i915_trace.rs`: Typed source-config dispatch for the i915 trace-event header's disabled `CONFIG_DRM_I915_LOW_LEVEL_TRACEPOINTS` branch; target config is checked at compile time.
`crates/ax/tk-intel-gt/src/i915_drm_client_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/i915_drm_client.c` MIT source translation (10/10 C functions plus the two header get/put helpers), registered for feature-build integration.
`crates/ax/tk-intel-gt/src/i915_gem_shrinker_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gem/i915_gem_shrinker.c` MIT translation, 19/19 source-order functions. Canonical object/VMA/GT owner imports are wired; task-specific Linux shrinker/reclaim/notifier/runtime-PM APIs remain explicit unresolved dependencies.
`crates/ax/tk-intel-gt/src/i915_active_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/i915_active.c` MIT source translation; all 67 source functions are present in source order.
`crates/ax/tk-intel-gt/src/i915_sw_fence_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/i915_sw_fence.c` MIT source translation; all 42 configured source functions are present in source order (LOCKDEP, debug objects and DAG checker disabled by target config).
`crates/ax/tk-intel-gt/src/i915_gem_ww_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/i915_gem_ww.c` MIT source translation and 56-byte target `I915GemWwCtx` owner.
`crates/ax/tk-intel-gt/src/i915_gem_object_api_upstream.rs`: MIT i915 GEM object-header API bindings using the canonical GEM object owner and LinuxKPI reference/WW-lock primitives.
`crates/ax/tk-intel-gt/src/intel_ring_upstream.rs`: Linux 7.2.3 `drivers/gpu/drm/i915/gt/intel_ring.c` MIT source translation, integrated against canonical `IntelRing`/VMA/request owners; complete package feature build remains blocked by missing surrounding APIs.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxvar.h` (ISC): AX211 So-F/So GF runtime configuration predicates and associated firmware/PNVM configuration translated in `tk-axdriver-iwx/src/config.rs`; ISC text in that crate's `LICENSES/ISC.txt`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): TLV firmware-header, section and supported-capability parsing adapted in `tk-axdriver-iwx/src/firmware.rs`; ISC text in that crate's `LICENSES/ISC.txt`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): PNVM SKU, hardware-type and runtime-section selection from `iwx_pnvm_parse()` / `iwx_pnvm_handle_section()` adapted in `tk-axdriver-iwx/src/firmware.rs`; ISC text in that crate's `LICENSES/ISC.txt`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): firmware command-version TLVs and UMAC/LMAC debug event-table metadata adapted in `tk-axdriver-iwx/src/firmware.rs`; ISC text in that crate's `LICENSES/ISC.txt`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `sys/dev/pci/pcidevs` (ISC): Intel AX211 PCI vendor/product match translated from `iwx_match()` into `tk-axdriver-iwx/src/config.rs`; ISC text in that crate's `LICENSES/ISC.txt`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263 and `ieee80211_output.c` rev 1.148 (BSD-3-Clause): station-mode Ethernet/802.11 data encapsulation and LLC/SNAP decapsulation translated in `tk-net80211/src/frame.rs`; BSD-3-Clause text in that crate's `LICENSES/BSD-3-Clause.txt`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263 (BSD-3-Clause): RSN/WPA cipher, AKM and information-element parsing translated in `tk-net80211/src/rsn.rs`; BSD-3-Clause text in that crate's `LICENSES/BSD-3-Clause.txt`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxvar.h` (ISC): all 97 ordered `iwx_dev_info_table` rows, reverse lookup predicates, and all referenced `iwx_device_cfg` records translated in `tk-axdriver-iwx/src/config.rs`; Linux-firmware API aliases are mapped as documented in `docs/design/wifi-iwx.md`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): firmware command/notification-version lookup, MIMO2 HT rate classification, packed firmware-version formatting, cipher-scheme validation and separator-based firmware section counts adapted in `tk-axdriver-iwx/src/firmware.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): context-info LMAC/UMAC/paging section counts, allocation, copy, and release lifetime mapped to `tk-axdriver-iwx/src/dma.rs`'s DMA allocator interface.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): monitor fallback allocation, debug destination register operations, and LTR programming translated in `tk-axdriver-iwx/src/dma.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): packed Gen2/Gen3 context-info and PRPH scratch bytes from `iwx_ctxt_info_init()` / `iwx_ctxt_info_gen3_init()`, plus PNVM base/size wiring, translated in `tk-axdriver-iwx/src/context.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): PRPH addressing masks, CSR/HBUS register access, polling, recursive NIC lock, target-memory read/write, and peripheral bit updates translated in `tk-axdriver-iwx/src/registers.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): AX210 RX transfer/completion ring allocation, rearm/reset, TFD/Gen3 byte-count encoding, and TX producer/consumer bookkeeping translated in `tk-axdriver-iwx/src/rings.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): command-group compatibility, wide command headers, split payload TFDs, response storage/status bounds, command ACK lifetime/generation handling, and ordered command-ring publication/kick translated in `tk-axdriver-iwx/src/command.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): MSI-X vector routing, mask snapshots, interrupt disable/ack and phase transitions, RF-kill state, and ordered NIC/firmware-load startup transitions translated in `tk-axdriver-iwx/src/interrupts.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): contiguous PNVM image copying and Gen3 fragmented PNVM address-array staging translated in `tk-axdriver-iwx/src/dma.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): TX antenna, PHY calibration, and DQA enable command payloads translated in `tk-axdriver-iwx/src/init_cmd.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): firmware ALIVE staging/cleanup, read-start-PNVM-post-alive sequence, post-ALIVE ICT/rate-format setup, and Init MVM/NVM access ordering translated in `tk-axdriver-iwx/src/bringup.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): generation-gated NIC config, RX interrupt coalescing and MAC shadow control initialization translated in `tk-axdriver-iwx/src/nic.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): legacy and DQA v3 queue add/remove payloads, ring CB-size encoding, and queue response validation translated in `tk-axdriver-iwx/src/queue.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): hardware-ready retries, generation-aware software reset, APM initialization/shutdown, persistence-bit write-protect check, AX power gating, and initial RF-kill interrupt enable translated in `tk-axdriver-iwx/src/apm.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): generation-specific MPDU antenna-energy offsets and beacon-silence noise averaging translated in `tk-axdriver-iwx/src/rx.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): source-ordered legacy/HT rate table, rate-index conversion, peer/local 11g/HT/VHT masks, and generation-specific management/multicast TX rate flags translated in `tk-axdriver-iwx/src/rate.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): packed Gen2/Gen3 TX command serialization, header-pad offload and payload TFD submission translated in `tk-axdriver-iwx/src/tx.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): FH RX packet validity, length bounds, command header/group decoding, narrow compatibility and 64-byte packet alignment translated in `tk-axdriver-iwx/src/rx_packet.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): CSR/OTP station-address byte ordering, validity predicates and strap-to-OTP fallback translated in `tk-axdriver-iwx/src/mac.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): NVM_GET_INFO request flags, v3/v4 response layouts, SKU/antenna/LAR data and channel profiles translated in `tk-axdriver-iwx/src/nvm.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): 2.4/5 GHz channel mapping, band/SKU filtering, passive/HT/VHT flags and bandwidth extensions translated in `tk-axdriver-iwx/src/channel.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): UMAC scan-abort command and foreground/background scan-state sequencing translated in `tk-axdriver-iwx/src/scan.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): aligned ICT allocation/drain/swizzle and legacy/MSI-X interrupt cause read/ack/re-enable plans translated in `tk-axdriver-iwx/src/intr.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): RX notification cursor drain, buffer completion delivery and 8-entry-aligned RFH/BZ write-pointer acknowledgement translated in `tk-axdriver-iwx/src/notif.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): core firmware RX event classification and direct command response-to-ACK lifetime routing translated in `tk-axdriver-iwx/src/rx_event.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): ALIVE v4-v7 size checks, firmware-good status, debug-table pointers and SKU extraction translated in `tk-axdriver-iwx/src/alive.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): generation-specific RX DMA disable and bounded RFH idle polling translated in `tk-axdriver-iwx/src/nic.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): TX queue enable/disable command completion and qenable/TID state transitions translated in `tk-axdriver-iwx/src/queue.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): RX BA reorder-session state, BAID/ADD_STA payloads, status extraction, timeout decisions, and BAR-release validation translated in `tk-axdriver-iwx/src/ba.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): NVM_GET_INFO request, root of its RF-kill-capable response flow, and v3/v4 NVM parsing adapter translated in `tk-axdriver-iwx/src/nvm.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): Init-MVM extended-configuration and NVM-access-complete host commands translated in `tk-axdriver-iwx/src/init_cmd.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): LTR_CONFIG payload gating and RX_PHY notification state extraction translated in `tk-axdriver-iwx/src/init_cmd.rs` and `tk-axdriver-iwx/src/rx.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): RX buffer replacement, RBD address update, and transfer descriptor repost translated in `tk-axdriver-iwx/src/rings.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): TX response sanity/status/SSN extraction and compressed-BA variable TFD queue progress parsing translated in `tk-axdriver-iwx/src/tx_completion.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): legacy binding-context command/status state and VHT control-position selection translated in `tk-axdriver-iwx/src/binding.rs` and `tk-axdriver-iwx/src/channel.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): PHY_CONTEXT_CMD v3/v4 standard and UHB serialization with bandwidth, control-channel and RX-chain fields translated in `tk-axdriver-iwx/src/phy.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): command waiter timeout/generation behavior and delayed external DMA release on ACK translated in `tk-axdriver-iwx/src/command.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): station drain ADD_STA, TXPATH_FLUSH and fixed response parsing, and ordered station-flush lifecycle translated in `tk-axdriver-iwx/src/station.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `sys/net80211/ieee80211.h` (ISC): U-APSD trigger TID selection, AC bitmap translations and maximum service period mapping translated in `tk-axdriver-iwx/src/power.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): DTIM-dependent power management command serialization, U-APSD timeout, keep-alive and beacon-abort order translated in `tk-axdriver-iwx/src/power.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): beacon filtering defaults, disable behavior and beacon-abort state update translated in `tk-axdriver-iwx/src/power.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): ADD_STA station initialization/update payload fields and status validation translated in `tk-axdriver-iwx/src/station.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): REMOVE_STA command and station removal ordering across drain/flush, queue removal, BA cleanup and DELBA callbacks translated in `tk-axdriver-iwx/src/station.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): generation-specific UMAC scan channel arrays and channel/firmware count caps translated in `tk-axdriver-iwx/src/scan.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `sys/net80211/ieee80211.h` (ISC): bounded scan probe-request header, rates and capability IE segmentation translated in `tk-axdriver-iwx/src/scan_probe.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): packed probe-request segment descriptors and 512-byte frame block serialization translated in `tk-axdriver-iwx/src/scan_probe.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): UMAC scan request v14/v17 serialization, dwell/channel parameters, direct SSID and background async mode translated in `tk-axdriver-iwx/src/scan.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): reduced SCAN_CFG API gate, antenna masks and broadcast station compatibility field translated in `tk-axdriver-iwx/src/scan.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): rate-set index conversion and mandatory-rate CCK/OFDM ACK masks translated in `tk-axdriver-iwx/src/rate.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): PCI attach DMA context, PRPH scratch, ICT, TX queue and RX ring allocation ordering translated in `tk-axdriver-iwx/src/attach.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxvar.h` (ISC): background-scan roam completion, queue flush, RSN key teardown and BSS-switch argument ownership translated in `tk-axdriver-iwx/src/background_scan.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): missed-beacon threshold and directed-probe decision translated in `tk-axdriver-iwx/src/beacon.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): deferred MAC/PHY update callbacks, shutdown guards, reference release and bandwidth-change rate-rescale order translated in `tk-axdriver-iwx/src/context_task.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): ioctl restart, watchdog timeout, media-change and TX scheduling decisions translated in `tk-axdriver-iwx/src/control.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): owned BAR/DMA/ring controller, RX notification and command wait, ALIVE/PNVM firmware sequencing translated in `tk-axdriver-iwx/src/controller.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230, `if_iwxreg.h`, and `if_iwxvar.h` (ISC): WFPM owner access, CRF/CNV PRPH identity and BZ stepping adjustment translated in `tk-axdriver-iwx/src/crf.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): LMAC/UMAC error-table word layouts, pointer validation and SYSASSERT decoding translated in `tk-axdriver-iwx/src/diagnostics.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): deferred key install/remove, MLD SEC_KEY and legacy station-key command payloads translated in `tk-axdriver-iwx/src/keys.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): interface init/stop reset order, task cancellation and multicast-filter selection translated in `tk-axdriver-iwx/src/lifecycle.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): legacy MAC_CONTEXT_CMD and MLD MAC-context wire layouts translated in `tk-axdriver-iwx/src/mac_context.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): MLD LINK_CONFIG_CMD/STA_CONFIG_CMD v1/v2 payloads and ordered peer add/remove translated in `tk-axdriver-iwx/src/mld.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxvar.h` (ISC): zero-initialized driver-private peer extension allocation translated in `tk-axdriver-iwx/src/node.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): suspend, resume, wakeup and low-power command policies translated in `tk-axdriver-iwx/src/pm.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): preinit channel, HT/VHT rate capability, 5-GHz band and MAC-address policy translated in `tk-axdriver-iwx/src/preinit.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): MCC CHUB country-code/source-field decoding translated in `tk-axdriver-iwx/src/regulatory.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): pre-AX multi-packet versus AX210 transfer-buffer ownership and RX ring recycling translated in `tk-axdriver-iwx/src/rx_buffer.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): hardware decrypt status, CCMP PN extraction and same-PN A-MSDU replay checks translated in `tk-axdriver-iwx/src/rx_crypto.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `sys/net80211/ieee80211.h` (ISC): retry duplicate filtering and A-MSDU pseudo-duplicate state translated in `tk-axdriver-iwx/src/rx_duplicate.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): final RX channel/RSSI/noise/rate/timestamp metadata and 802.11 input handoff translated in `tk-axdriver-iwx/src/rx_frame.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): RX_MPDU generation layouts, pad repair, A-MSDU flag fix, replay gate and duplicate dispatch translated in `tk-axdriver-iwx/src/rx_mpdu.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): association session-protection add/remove wire command and state translated in `tk-axdriver-iwx/src/session.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): smart-FIFO scenario, aging watermark and timeout tables translated in `tk-axdriver-iwx/src/spectrum.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): Init-MVM hardware commands, firmware configuration, MCC response and startup action order translated in `tk-axdriver-iwx/src/startup.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): AUTH/DEAUTH/RUN/RUN_STOP software state transitions, key-port gates and error rollback translated in `tk-axdriver-iwx/src/state.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): station add/remove, TX drain/flush, BA teardown and rate-config status commands translated in `tk-axdriver-iwx/src/station.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `if_iwxreg.h` (ISC): statistics clear/response wait and notification state translated in `tk-axdriver-iwx/src/statistics.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): deferred task enqueue/delete reference accounting and shutdown waiter release translated in `tk-axdriver-iwx/src/task.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC): authenticated MFP leave, 500-ms node-unref wait and start-queue gating translated in `tk-axdriver-iwx/src/tx_start.rs`.
- OpenBSD `sys/net80211/ieee80211_rssadapt.c` rev 1.11 and `ieee80211_rssadapt.h` rev 1.5 (BSD-3-Clause): packet-length buckets, RSSI thresholds, exponential averages, success decay, failure updates and legacy rate selection translated in `tk-net80211/src/rssadapt.rs`.
- OpenBSD `sys/net80211/ieee80211_ra.c` rev 1.5, `ieee80211_ra.h` rev 1.2, and the HT rateset table from `ieee80211.c` rev 1.92 (ISC-style and BSD-3-Clause): fixed-point goodput selection, 20/40 MHz ratesets, MCS capability filtering, probing, packet statistics and node reset translated in `tk-net80211/src/ra.rs`; full grant in `tk-net80211/LICENSES/OpenBSD-ISC.txt`.
- OpenBSD `sys/net80211/ieee80211_regdomain.c` rev 1.10 and `ieee80211_regdomain.h` rev 1.9 (ISC): binary search/comparators, country and domain conversions, band-map resolution, and the full 73-name/49-map/116-country tables translated in `tk-net80211/src/regdomain.rs`; grant in `tk-net80211/LICENSES/OpenBSD-ISC.txt`.
- OpenBSD `sys/net80211/ieee80211.c` rev 1.92 (BSD-3-Clause): `ieee80211_mhz2ieee()`, `ieee80211_ieee2mhz()` and bounds-checked `ieee80211_chan2ieee()` translations in `tk-net80211/src/channel.rs`; BSD-3-Clause text in that crate's `LICENSES/BSD-3-Clause.txt`.
- OpenBSD `sys/net80211/ieee80211.c` rev 1.92 and `ieee80211.h` rev 1.137 (BSD-3-Clause): legacy 11a/b/g rate tables, mandatory rate marking, negotiated basic-rate extrema, and PLCP signal/rate mappings translated in `tk-net80211/src/rates.rs`.
- OpenBSD `sys/net80211/ieee80211.c` rev 1.92 and `ieee80211_var.h` rev 1.157 (BSD-3-Clause): channel-set/mode capability population, active mode channel selection and TX AMPDU/QoS gating translated in `tk-net80211/src/channel.rs` as state-returning APIs.
- OpenBSD `sys/net80211/ieee80211.c` rev 1.92 (BSD-3-Clause): supported-rate lookup masking basic bits and source background-scan PHY-mode cycling translated in `tk-net80211/src/channel.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 and `ieee80211_node.h` rev 1.64 (BSD-3-Clause): ESS lookup/compatibility, RSSI/crypto/AP scoring, configured-network scan selection, and 40/80 MHz operating-class channel validators translated in `tk-net80211/src/node.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217, `ieee80211_node.h` rev 1.64, and `ieee80211.h` rev 1.137 (BSD-3-Clause): bounded HT/VHT/HE capability/operation IE parsing, reserved-MCS cleanup, clear routines, and node HT/VHT channel-width selectors translated in `tk-net80211/src/node_caps.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 and `ieee80211.h` rev 1.137 (BSD-3-Clause): `ieee80211_setup_vhtop()` peer/local ext-NSS bandwidth resolution, VHT 80-to-160 widening and unsupported 160-to-80 fallback translated in `tk-net80211/src/node_caps.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): 11g peer inference, Supported/Extended Supported Rates IE installation with bounded extension and delegated rate fix, and fixed/automatic 11a/b/g mode resolution translated in `tk-net80211/src/node_rates.rs`.
- OpenBSD `sys/net80211/ieee80211_node.h` rev 1.64 and `ieee80211.h` rev 1.137 (BSD-3-Clause): peer HT/VHT/HE MCS, SGI and 40/80/160-MHz capability predicates translated in `tk-net80211/src/node_caps.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217, `ieee80211_crypto.h` rev 1.38, `ieee80211_var.h` rev 1.143 and `ieee80211.h` rev 1.137 (BSD-3-Clause): RSN protocol/AKM/cipher/PMKID/MFP selection, negotiated legacy rate reporting, RSSI retrieval and roaming thresholds translated in `tk-net80211/src/node.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217, `ieee80211_node.h` rev 1.64, `ieee80211_crypto.h` rev 1.38 and `ieee80211.h` rev 1.137 (BSD-3-Clause): candidate BSS channel/mode/privacy/rate/SSID/BSSID/CSA/RSN/MFP rejection logic and background-scan failure bookkeeping translated in `tk-net80211/src/node.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): scan candidate enumeration, failure eviction, current-BSS tracking and 5-GHz preference over 2.4-GHz selection translated in `tk-net80211/src/node.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): active/passive scan counters and wraparound channel traversal with passive-only channel filtering translated in `tk-net80211/src/scan.rs`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263 and `ieee80211.h` rev 1.137 (BSD-3-Clause): data/control/QoS/HT-control header shape helpers, source header length and little-endian QoS control extraction translated in `tk-net80211/src/input.rs`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263 and `ieee80211.h` rev 1.137 (BSD-3-Clause): EDCA/WMM QoS update-count parsing, AC parameter field decode and QoS-info extraction translated in `tk-net80211/src/input.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148 (BSD-3-Clause): SSID, Supported Rates and Extended Supported Rates IE encoders translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 and `ieee80211_node.h` rev 1.64 (BSD-3-Clause): bounded cache admission, zeroed node allocation, MAC-keyed lookup, node removal and all-node cleanup translated in `tk-net80211/src/node_table.rs`; Rust borrows replace manual reference increments.
- `tk-axdriver-iwx/src/scan_probe.rs` now consumes `tk-net80211`'s `append_ssid_ie()`, `append_supported_rates_ie()` and `append_extended_rates_ie()` for firmware scan-probe IEs; the wrapper retains OpenBSD `if_iwx.c` rev 1.230 segmented DMA wire construction.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148 and `ieee80211.h` rev 1.137 (BSD-3-Clause): HT capabilities/operation, VHT capabilities and width-dependent HE extension capability IEs translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148, `ieee80211_crypto.h` rev 1.38, `ieee80211_var.h` rev 1.143 and `ieee80211.h` rev 1.137 (BSD-3-Clause): WPA v1/RSN group, pairwise and AKM suite ordering, RSN capabilities, PMKID, PMF/BIP selection and outer IE lengths translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148, `ieee80211.h` rev 1.137 and `ieee80211_var.h` rev 1.143 (BSD-3-Clause): mode-specific EDCA/WMM AC parameter tables, QoS Capability and WMM Info/Parameter encoders, and U-APSD QoS Info mapping translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148 and `ieee80211.h` rev 1.137 (BSD-3-Clause): mode/privacy/preamble Capability Information field, DS Parameter Set, and ERP protection/Barker encoding translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148, `ieee80211_var.h` rev 1.143 and `ieee80211.h` rev 1.137 (BSD-3-Clause): station (Re)Association Request fixed fields and exact SSID/rate/RSN/QoS/WPA/HT/WME/VHT/HE IE ordering translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148 (BSD-3-Clause): open-system Authentication, Deauthentication and Disassociation body field encoders translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148 (BSD-3-Clause): Probe Request SSID/rate/WMM/HT/VHT/HE IE sequence gating from the selected channel and PHY state translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148 (BSD-3-Clause): user-priority to EDCA access-category mapping and station-mode admission-control downgrade translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148 (BSD-3-Clause): Ethernet VLAN PCP/IPv4/IPv6 DSCP classification and per-100ms VI/VO TXOP limiting translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_proto.c` rev 1.176, `ieee80211_var.h` rev 1.143, `ieee80211.h` rev 1.137 and `ieee80211_node.h` rev 1.64 (BSD-3-Clause): rate sorting, local-rate intersection, Basic-rate marking, deletion, fixed-rate validation and hostap rejection translated in `tk-net80211/src/proto.rs`.
- OpenBSD `sys/net80211/ieee80211_proto.c` rev 1.176, `ieee80211_var.h` rev 1.143 and `ieee80211.h` rev 1.137 (BSD-3-Clause): 11g ERP reset and short-slot state/callback predicate translated in `tk-net80211/src/proto.rs`.
- OpenBSD `sys/net80211/ieee80211_proto.c` rev 1.176, `ieee80211_var.h` rev 1.143 and `ieee80211.h` rev 1.137 (BSD-3-Clause): interval-scaled missed-beacon watchdog threshold with minimum one-miss floor translated in `tk-net80211/src/proto.rs`.
- OpenBSD `sys/net80211/ieee80211_proto.c` rev 1.176, `ieee80211_var.h` rev 1.143 and `ieee80211.h` rev 1.137 (BSD-3-Clause): station Open System authentication response sequence/status checks, RSN key/port reset, AP retry and AUTH-to-ASSOC transition translated in `tk-net80211/src/proto.rs`.
- OpenBSD `sys/net80211/ieee80211_proto.c` rev 1.176, `ieee80211_node.h` rev 1.64, `ieee80211.h` rev 1.137 and `ieee80211_crypto.h` rev 1.38 (BSD-3-Clause): HT/VHT/HE PHY/MCS/SGI negotiation, local Basic MCS checks, and TKIP/WEP HT restrictions translated in `tk-net80211/src/proto.rs`.
- OpenBSD `sys/net80211/ieee80211_proto.c` rev 1.176 and `ieee80211_node.c` rev 1.217 (BSD-3-Clause): failed-current-AP aging, optional scan-all-bands AUTO reset, alternative candidate selection and same-AP refusal translated in `tk-net80211/src/proto.rs`.
- OpenBSD `sys/net80211/ieee80211_proto.c` rev 1.176 and `ieee80211_var.h` rev 1.143 (BSD-3-Clause): station INIT/SCAN/AUTH/ASSOC/RUN state transition action order, management frames, cleanup, link-up deferral and beacon timer hooks translated in `tk-net80211/src/proto.rs`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263 (BSD-3-Clause), `ieee80211_crypto_ccmp.c` rev 1.22 and `ieee80211_crypto_tkip.c` rev 1.34 (ISC-style): hardware-decryption receive-counter update, CCMP/TKIP IV replay acceptance (including same-PN A-MSDU subframes), Protected-bit clearing and IV removal translated in `tk-net80211/src/decrypt.rs`.
- OpenBSD `sys/net80211/ieee80211_crypto_ccmp.c` rev 1.22 (ISC) and `ieee80211.h` rev 1.137 (BSD-3-Clause): six software CCMP key setup/delete, nonce/AAD generation, encrypt, packet-number and decrypt/MIC/replay functions translated in `tk-net80211/src/crypto_ccmp.rs`; AES-128 blocks use the existing RustCrypto `aes` 0.8.4 crate.
- OpenBSD `sys/net80211/ieee80211_crypto_bip.c` rev 1.10 (ISC) and `ieee80211.h` rev 1.137 (BSD-3-Clause): four software IGTK setup/delete, MMIE BIP encap/decap and replay/MIC validation functions translated in `tk-net80211/src/crypto_bip.rs`, using AES-CMAC over the RustCrypto AES block primitive.
- OpenBSD `sys/net80211/ieee80211_crypto_wep.c` rev 1.17 (ISC) and `ieee80211.h` rev 1.137 (BSD-3-Clause): WEP set/delete, RC4/CRC32 frame encrypt/decrypt and weak-IV avoidance translated in `tk-net80211/src/crypto_wep.rs`.
- OpenBSD `sys/net80211/ieee80211_crypto_tkip.c` rev 1.34 (ISC), `ieee80211.h` rev 1.137 and `ieee80211_node.h` rev 1.64 (BSD-3-Clause): station TKIP key lifetime, Michael pseudo-header/MIC, key mixing Phase1/Phase2, RC4/ICV frame encrypt/decrypt, TSC/RSC replay and station MIC-countermeasure effects translated in `tk-net80211/src/crypto_tkip.rs`; AP-only peer deauth and timer callbacks are excluded for the iwx STA-only build.
- OpenBSD `sys/net80211/ieee80211_crypto.c` rev 1.81 (ISC) and `ieee80211.h` rev 1.137 (BSD-3-Clause): cipher key lengths, software key setup/delete, WEP/TKIP/CCMP/BIP encrypt/decrypt dispatch and TX/RX pairwise/group/IGTK key selection translated in `tk-net80211/src/crypto.rs`; PMKSA/EAPOL cryptographic handshake remains userspace-owned.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263, `ieee80211_node.c` rev 1.217, `ieee80211.h` rev 1.137 and `ieee80211_var.h` rev 1.157 (BSD-3-Clause): station beacon/probe-response IE parsing, channel validation, scan-node updates, HT/VHT/HE, QoS/RSN, RSSI and current-BSS effects translated in `tk-net80211/src/beacon.rs` with per-node receive/PHY state in `node_table.rs`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263 and `ieee80211_proto.c` rev 1.176 (BSD-3-Clause): authentication management frame bounds/algorithm/sequence/status decoding and station Open System transition effects translated in `tk-net80211/src/auth_rx.rs`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263, `ieee80211_proto.c` rev 1.176, `ieee80211_node.c` rev 1.217 and `ieee80211.h` rev 1.137 (BSD-3-Clause): station Association/Reassociation Response validation, rate fixing, EDCA/WMM/U-APSD update, HT/VHT/HE negotiation, mode selection and RUN transition effects translated in `tk-net80211/src/assoc_rx.rs`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263 and `ieee80211_proto.c` rev 1.176 (BSD-3-Clause): station deauthentication/disassociation reason decoding, background-scan and stay-authentication policy, peer-leave decisions, and state-transition effects translated in `tk-net80211/src/disconnect_rx.rs`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263 and `ieee80211.h` rev 1.137 (BSD-3-Clause): ADDBA request/response and DELBA parsing, agreement accept/refuse transitions, PBAC/no-ACK/delayed-BA policy decisions, and BAR basic/Multi-TID sequence extraction translated in `tk-net80211/src/ba_rx.rs`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263 and `ieee80211.h` rev 1.137 (BSD-3-Clause): station MFP SA Query Request/Response transaction-ID handling and response/timeout effects translated in `tk-net80211/src/sa_query.rs`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263 and `ieee80211.h` rev 1.137 (BSD-3-Clause): BA and SA Query management action category/subtype dispatch translated in `tk-net80211/src/ba_rx.rs`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263 and `ieee80211.h` rev 1.137 (BSD-3-Clause): management subtype dispatcher translated in `tk-net80211/src/mgmt_rx.rs`, routing supported station and management-frame classes to typed handlers.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263 and `ieee80211.h` rev 1.137 (BSD-3-Clause): per-TID BlockAckReq agreement validation, PBAC window rejection, inactivity refresh and sequence-window advance policy translated in `tk-net80211/src/ba_rx.rs`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263, `ieee80211_node.h` rev 1.64 and `ieee80211.h` rev 1.137 (BSD-3-Clause): station `ieee80211_inputm()` sequence/fragment/duplicate/BA/protection/address checks, and `ieee80211_amsdu_decap()` subframe conversion plus `ieee80211_amsdu_decap_validate()` station-destination defense translated in `tk-net80211/src/rx_path.rs` and `frame.rs`.
- OpenBSD `sys/net80211/ieee80211_input.c` rev 1.263 (BSD-3-Clause): A-MSDU destination validation and SNAP/subframe conversion translated in `tk-net80211/src/frame.rs` and consumed by the station RX path in `rx_path.rs`.
- Linux `include/uapi/linux/nl80211.h` 7.2.3 (ISC-style): nl80211 family name/version, GET_WIPHY/GET_INTERFACE command IDs, selector attributes and family multicast-group response layout translated in `kernel/src/file/netlink/nl80211.rs`; full grant in `kernel/LICENSES/ISC.txt`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and `sys/net80211/ieee80211.h` rev 1.137 (ISC/BSD-3-Clause): `iwx_setup_ht_rates()` and `iwx_setup_vht_rates()` NVM antenna/SKU-derived HT/VHT capability and MCS maps carried through `tk-axdriver-net` and encoded as nl80211 band attributes in `kernel/src/file/netlink/nl80211.rs`.
- OpenBSD `sys/net80211/ieee80211.c` rev 1.92 (BSD-3-Clause): `ieee80211_begin_bgscan()` re-entry/state/timer/RSN-port gates and successful driver-callback cache-clear/background flag effects, plus the `ieee80211_bgscan_timeout()` entrypoint, translated in `tk-net80211/src/scan.rs`.
- OpenBSD `sys/net80211/ieee80211.c` rev 1.92 (BSD-3-Clause): management watchdog countdown/rearm, station AUTH/ASSOC failure accounting, auto-join ESS deselection and SCAN timeout transition translated into effects in `tk-net80211/src/proto.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): `ieee80211_reset_scan()` active-channel bitmap restore and ANY-channel sentinel positioning translated in `tk-net80211/src/scan.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): station-mode `ieee80211_end_scan()` selection outcomes, no-candidate rescan, background AP retention/backoff and TX-drained roam effects translated into `end_station_scan()` in `tk-net80211/src/scan.rs`; BSS choice and driver/node cleanup are supplied as caller effects.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): station `ieee80211_node_join_bss()` selected-node copy, ESS-associated failure-history retention, rate/RSN selection, mode and AUTH-trigger choice translated into table update plus `StationBssJoinPlan` in `tk-net80211/src/node.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217, `ieee80211.c` rev 1.92 and `ieee80211_proto.c` rev 1.176 (BSD-3-Clause): station `ieee80211_node_join_bss()` BSS-node transfer, failure-history retention, rate/RSN policy and AUTH transition assembled in `tk-net80211/src/node.rs` from the corresponding translated selectors/state helper.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): station `ieee80211_node_leave_vht()` and `ieee80211_node_leave_he()` capability reset effects translated in `tk-net80211/src/node.rs` via the node-capability clearing helpers.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): `ieee80211_node_leave_rsn()` state/PMK/rekey reset, EAPOL/SA-query timeout cancellation, port/protection clearing and pairwise-key deletion effects translated in `tk-net80211/src/node.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): `ieee80211_node_leave_ht()` clears HT capabilities and returns driver-owned BlockAck/reorder-buffer retirement effects in `tk-net80211/src/node.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): `ieee80211_node_leave_11g()` last-incompatible-peer short-slot/protection/preamble state effects translated in `tk-net80211/src/node.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): station `ieee80211_node_raise_inact()` saturating scan age and `ieee80211_clean_inactive_nodes()` reference-aware expiration translated in `tk-net80211/src/node_table.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): station/multicast `ieee80211_find_txnode()` BSS peer selection translated in `tk-net80211/src/node_table.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 and `ieee80211_node.h` rev 1.64 (BSD-3-Clause): `ieee80211_node_copy()` owned-record/IE replacement and timeout-reset effect translated in `tk-net80211/src/node_table.rs`; station mode omits AP power-save queue initialization.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): `ieee80211_begin_scan()` active/passive scan state, station BSS cleanup/inactivity aging, current-mode reset, scan count reset and next-channel effect plan translated in `tk-net80211/src/scan.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148 (BSD-3-Clause): ADDBA request/response, DELBA and SA Query action body encoders plus 12-bit Tx BA window advancement translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148 (BSD-3-Clause): station-supported BlockAck and SA Query response action dispatch translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_proto.c` rev 1.176 and `ieee80211.h` rev 1.137 (BSD-3-Clause): transmit ADDBA request state, dialog token/window/parameter setup, one-second response timer effect and driver offload result handling translated in `tk-net80211/src/ba_tx.rs`.
- OpenBSD `sys/net80211/ieee80211_proto.c` rev 1.176 (BSD-3-Clause): transmit DELBA send/driver-stop effects, recipient timeout cancellation and reorder-buffer retirement translated in `tk-net80211/src/ba_tx.rs`.
- OpenBSD `sys/net80211/ieee80211_proto.c` rev 1.176 and `ieee80211_node.h` rev 1.64 (BSD-3-Clause): transmit requested/agreed BlockAck timeout transitions, retry interval and receive inactivity timeout cleanup translated in `tk-net80211/src/ba_tx.rs`.
- OpenBSD `sys/net80211/ieee80211_proto.c` rev 1.176 (BSD-3-Clause): `ieee80211_stop_ampdu_tx()` TID traversal and ADDBA-offload/DELBA teardown effects translated in `tk-net80211/src/ba_tx.rs`.
- OpenBSD `sys/net80211/ieee80211_proto.c` rev 1.176 (BSD-3-Clause): `ieee80211_check_wpa_supplicant_failure()` station/IBSS PTK-negotiation failure propagation to the cached BSS node translated in `tk-net80211/src/proto.rs`.
- OpenBSD `sys/net80211/ieee80211_proto.c` rev 1.176 and `ieee80211_node.h` rev 1.64 (BSD-3-Clause): station `ieee80211_keyrun()` run/RSN guard and external supplicant PTKSTART handoff translated in `tk-net80211/src/proto.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148 and `ieee80211_var.h` rev 1.143 (BSD-3-Clause): HT/local AMPDU/station-BSS/RSN eligibility check translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211.c` rev 1.92 (BSD-3-Clause): station media subtype, legacy rate, and MCS conversions translated to typed enums in `tk-net80211/src/media.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148 and `ieee80211.h` rev 1.137 (BSD-3-Clause): compressed Block-Ack request frame serialization translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): duplicate RX node allocation with BSS BSSID/channel inheritance translated in `tk-net80211/src/node_table.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): station-only RX-node admission and BSS/monitor peer lookup translated in `tk-net80211/src/node_table.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148 (BSD-3-Clause): management-frame 802.11 header, sequence and MFP protection logic translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): transmit BA-state reset and retry-timer cancellation effects translated in `tk-net80211/src/ba_tx.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): station background-roam TX-drain and BSS-switch callbacks translated to explicit caller effects in `tk-net80211/src/node.rs`.
- OpenBSD `sys/net80211/ieee80211_node.c` rev 1.217 (BSD-3-Clause): saved security-IE cleanup, BA teardown, reorder-buffer release and HostAP-only queue purge effects translated in `tk-net80211/src/node_table.rs`.
- OpenBSD `sys/net80211/ieee80211_output.c` rev 1.148 (BSD-3-Clause): station management subtype/body selection, node-ref transfer policy and transition-timer effects translated in `tk-net80211/src/output.rs`.
- OpenBSD `sys/net80211/ieee80211_ra_vht.c` rev 1.3, `ieee80211_ra_vht.h` rev 1.1 and VHT rateset table in `ieee80211.c` rev 1.92 (ISC): 20/40/80 MHz MCS/NSS tables, SGI decisions, fixed-point goodput statistics, valid-rate initialization, candidate/probe transitions and selection translated in `tk-net80211/src/ra_vht.rs`; iwx reuses the translated peer-MCS limit helper when building firmware TLC rate masks in `tk-axdriver-iwx/src/rate.rs`; grant in `tk-net80211/LICENSES/OpenBSD-ISC.txt`.

The station VHT rate-adaptation translation adds two ISC license-boilerplate
lines in `crates/ax/tk-net80211/src/ra_vht.rs` that are byte-identical to GPL
Linux license text and therefore match the verbatim-line scanner. They are
license statements, not Linux implementation excerpts. Re-running the pinned
scan yields crates/ax totals at ≥40 `(0,0,0,97,88,0,0)` and ≥25
`(18,15,4,131,118,0,0)`, exactly two more unfenced documentation/comment
matches at both thresholds; fenced lines and Rust-code matches are unchanged.
`tests/ci/test_linux_excerpt_baseline.py` carries the corresponding counters;
there is no new Linux source quotation or change to the seven genuine comment
lines and eight code matches inventoried for this subtree.
- wpa_supplicant 2.11 `src/drivers/driver_nl80211.c` commit `5460547` (BSD): authenticated attribute and command/event behavior was checked for the userspace-SME SAE `AUTHENTICATE` (`AUTH_DATA`/`SAE_DATA`), separate `ASSOCIATE`, and MLME frame events; no wpa_supplicant implementation code was copied. UAPI numbers remain from Linux `include/uapi/linux/nl80211.h` v7.2.3 (ISC) in `kernel/src/file/netlink/nl80211.rs`.
- OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC), Copyright (c) 2014, 2016 genua gmbh, Stefan Sperling, Fixup Software Ltd.: PCI probe/attach profile selection and TheKernel net-device adapter integration in `crates/ax/tk-axdriver/src/iwx.rs`; the ISC text is `crates/ax/tk-axdriver-iwx/LICENSES/ISC.txt`.
- OpenBSD `sys/net80211/` station modules (BSD-3-Clause), including `ieee80211.c` rev 1.92, Copyright (c) 2001 Atsushi Onoe, and module-specific additional copyright holders: `crates/ax/tk-net80211/src/lib.rs` assembles the translated station modules; source files and attributions are recorded in each module header and the license text is `crates/ax/tk-net80211/LICENSES/BSD-3-Clause.txt`.

- FreeBSD `sys/dev/ahci/ahci.h` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): AHCI register encodings, PRD/command descriptor layouts, slot/error enums, and controller/channel state are translated in `crates/ax/tk-axdriver-block/src/ahci/regs.rs`; FreeBSD bus/CAM framework types are not copied. Full license: `crates/ax/tk-axdriver/LICENSES/BSD-2-Clause-FreeBSD-AHCI.txt` and `crates/ax/tk-axdriver-block/LICENSES/BSD-2-Clause-FreeBSD-AHCI.txt`.

- FreeBSD `sys/dev/ahci/ahci.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): `ahci_ch_detval`, `ahci_ctlr_setup`, `ahci_ctlr_reset`, `ahci_attach` capability discovery, channel start/stop/FIS receive, SATA connect/PHY reset, ATA `ahci_setup_fis`, and the single-slot `ahci_execute_transaction` command path are translated/adapted in `crates/ax/tk-axdriver-block/src/ahci/{controller,ata,disk}.rs`; CAM and FreeBSD bus frameworks are not copied. Full license: `crates/ax/tk-axdriver/LICENSES/BSD-2-Clause-FreeBSD-AHCI.txt` and `crates/ax/tk-axdriver-block/LICENSES/BSD-2-Clause-FreeBSD-AHCI.txt`.

- FreeBSD `sys/dev/ahci/ahci_pci.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): PCI class-match/ABAR resource enable, bus-master setup, controller reset, and first ATA disk publication are adapted in `crates/ax/tk-axdriver/src/ahci.rs`; MSI/MSI-X, vendor-ID quirk table, remapped-NVMe, and FreeBSD resource framework paths remain untranslated. Full license: `crates/ax/tk-axdriver/LICENSES/BSD-2-Clause-FreeBSD-AHCI.txt` and `crates/ax/tk-axdriver-block/LICENSES/BSD-2-Clause-FreeBSD-AHCI.txt`.

- FreeBSD `sys/dev/ahci/ahci_pci.c` `ahci_ids[]` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): all 317 non-sentinel PCI ID/revision/name/quirk rows are translated in `crates/ax/tk-axdriver/src/ahci/pci_ids.rs`; full grant in `crates/ax/tk-axdriver/LICENSES/BSD-2-Clause-FreeBSD-AHCI.txt`.

- FreeBSD `sys/dev/ahci/ahci.c` `ahci_process_read_log()` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): the serialized NCQ-error READ LOG EXT recovery path is adapted in `crates/ax/tk-axdriver-block/src/ahci/disk.rs`; one-at-a-time BlockDriverOps completion avoids CAM held-command CCB fanout.

- FreeBSD `sys/dev/sdhci/sdhci.h` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): SDHCI register offsets, capability/interrupt masks, controller quirk and slot flags are translated into `crates/ax/tk-axdriver-block/src/sdhci.rs`; FreeBSD bus/task/CAM types are omitted. Full license: `crates/ax/tk-axdriver-block/LICENSES/BSD-2-Clause-FreeBSD-SDHCI.txt`.

- FreeBSD `sys/dev/sdhci/sdhci.c`, `sys/dev/mmc/mmc.c`, and `sys/dev/mmc/mmcsd.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): the bounded command/PIO engine, SD/eMMC discovery and capacity subset, and single-block `BlockDriverOps` path are adapted in `crates/ax/tk-axdriver-block/src/sdhci.rs`; task/callout/newbus/CAM/disk framework paths, tuning modes, partitions managed by CAM, RPMB, reliable-write and dump paths remain untranslated. Full grants: `crates/ax/tk-axdriver-block/LICENSES/BSD-2-Clause-FreeBSD-SDHCI.txt` and `crates/ax/tk-axdriver-block/LICENSES/BSD-2-Clause-FreeBSD-MMC.txt`.

- FreeBSD `sys/dev/sdhci/sdhci_pci.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): generic PCI class matching, BAR0 mapping and eMMC read-only admission are adapted in `crates/ax/tk-axdriver/src/sdhci.rs`; controller-specific slot/quirk tables, IRQ setup and FreeBSD child bus framework remain untranslated. Full license: `crates/ax/tk-axdriver/LICENSES/BSD-2-Clause-FreeBSD-SDHCI-PCI.txt`.

- FreeBSD `sys/dev/e1000/e1000_regs.h` and `e1000_defines.h` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause): 1,224 numeric register/mask definitions and the queue-register offset functions are translated into `crates/ax/tk-axdriver-net/src/e1000/registers.rs`; the driver functions remain to be ported. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- FreeBSD `sys/dev/e1000/if_em.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): all 193 Intel device IDs in `em_vendor_info_array[]` and the PCI `em_probe()` matching/admission path are translated/adapted in `crates/ax/tk-axdriver/src/e1000.rs`; the PCI BAR mapping is TheKernel-specific. Full license: `crates/ax/tk-axdriver/LICENSES/BSD-2-Clause-FreeBSD-E1000.txt`.

- FreeBSD `sys/dev/e1000/{e1000_82540.c,e1000_nvm.c,em_txrx.c,if_em.c}` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause and BSD-2-Clause): the bounded 82540 reset sequence, RAR0 station-address extraction, RX/TX ring initialization, and single-buffer TX/RX descriptor processing are adapted to the `NetDriverOps` path in `crates/ax/tk-axdriver-net/src/e1000/nic.rs`; iflib/ifnet, interrupt service, PHY/NVM function-pointer families, multi-fragment/TSO, checksum/VLAN offloads, and multi-queue policy are not translated. Full licenses: `crates/ax/tk-axdriver-net/LICENSES/BSD-2-Clause-FreeBSD-E1000.txt` and `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- FreeBSD `sys/dev/e1000/e1000_osdep.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause): the 82579 PCIm2PCI arbiter/tail-write workaround, PCI config/MWI and PCIe capability helpers are translated behind `E1000RegisterIo`/`E1000PciConfig` in `crates/ax/tk-axdriver-net/src/e1000/osdep.rs`. FreeBSD `SYSINIT(enable_pause_delay)` is module-loader glue; TheKernel uses bounded busy-wait delay. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `sys/dev/e1000/e1000_api.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause): all 64 common API entry points, 194 `e1000_set_mac_type()` PCI-ID mappings, and `e1000_setup_init_funcs()` family/initializer ordering are translated into `crates/ax/tk-axdriver-net/src/e1000/api.rs` as typed dispatch over `E1000ApiHardware`; per-generation callbacks are supplied by the following MAC/PHY/NVM source modules. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `sys/dev/e1000/e1000_mac.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause): generic RAR, VLAN-filter, multicast-hash, and multicast-table operations are translated into `crates/ax/tk-axdriver-net/src/e1000/mac.rs` over `E1000RegisterIo`; the remaining MAC callbacks are still being ported. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `sys/dev/e1000/e1000_mac.c` coverage update: all 57 C function definitions now have `upstream:` counterparts in `crates/ax/tk-axdriver-net/src/e1000/mac.rs` (BSD-3-Clause); generation-specific callback installation remains tracked separately. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `sys/dev/e1000/e1000_phy.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause): PHY callback defaults, PHY ID/revision, reset-block, DSP reset, and retry-state helpers are translated in `crates/ax/tk-axdriver-net/src/e1000/phy.rs`; remaining per-generation MDIO and PHY operations are being ported. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 20 of 98 source functions now have direct Rust counterparts, including bounded MDIC and I2C/SFP register operations plus PHY autoneg advertisement; per-generation PHY access paths remain in progress. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 35 of 98 source functions now have direct Rust counterparts, including locked/paged IGP/M88, Kumeran access, sticky-status polling, and bounded autoneg wait. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 37 of 98 source functions now have direct Rust counterparts, including MII copper-autoneg restart and master/slave policy. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 41 of 98 source functions now have direct Rust counterparts, including PHY-type discovery/address scan, the IGP3 init script, and config-done delay. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage correction: the 41-function report omitted already translated MAC helper callbacks from its baseline; the exact ctags marker audit now counts 49 of 98 unique PHY definitions, including M88/IGP/IFE/82577 polarity and M88 cable-length/downshift helpers. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` count audit detail: the earlier 41/98 entry was low by two existing markers; the subsequent six diagnostics translations bring the exact unique marker total to 49/98.

- Intel/FreeBSD `e1000_phy.c` coverage update: 50 of 98 functions are now translated, including four-channel IGP2 AGC cable-length averaging from the upstream table. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 51 of 98 functions are now translated, including I210 and M88 Gen2 cable-length/page handling. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 52 of 98 functions are now translated, including generic forced speed/duplex register configuration. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 56 of 98 functions are now translated, including PHY SW/HW reset and copper power-up/down behavior. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 58 of 98 functions are now translated, including 82577 and M88 copper-link setup register sequences. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 59 of 98 functions are now translated, including M88 Gen2 downshift, commit, and master/slave setup. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 64 of 98 functions are now translated, including IGP copper setup and generic copper auto/forced link establishment. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 65 of 98 functions are now translated, including the IGP forced-speed/duplex path and repeated PHY-link check. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 66 of 98 functions are now translated, including M88 force-link DSP retry and post-reset TX clock restoration. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 67 of 98 functions are now translated, including IFE forced speed/duplex and MDI crossover disable. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` exact marker audit correction: the prior 67 count was over by four; `ctags` name intersection currently finds 65 of 98 unique definitions after D3 LPLU translation. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 68 of 98 functions are now translated, including M88/IGP/IFE PHY-info extraction with link-speed and receiver state. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 73 of 98 functions are now translated, including BM page-address mapping and locked paged BM read/write operations, with wakeup-page access adapted through callbacks. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 76 of 98 functions are now translated, including BM wakeup enable/disable and indexed wakeup register transfers. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` coverage update: 89 of 98 functions are now translated, adding HV page/debug-port access, 82577 forced-speed and cable diagnostics, and 82577 PHY-info extraction. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.

- Intel/FreeBSD `e1000_phy.c` completion update: all 98 of 98 C function definitions have `upstream:` Rust counterparts in `crates/ax/tk-axdriver-net/src/e1000/phy.rs`; BM/HV special accesses are routed through explicit register and wakeup adapters. Full license: `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_nvm.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): generic NVM bit protocols, EERD/EW* operations, PBA image/string helpers, checksum, MAC read and firmware-version decoding in `crates/ax/tk-axdriver-net/src/e1000/nvm.rs`; license `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_manage.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): checksum, management host-interface, packet-filtering, DHCP payload, pass-through and firmware-load routines in `crates/ax/tk-axdriver-net/src/e1000/manage.rs`; license `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_82540.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): 82540/82545/82546 initialization, reset, link workarounds, counters and NVM MAC-address paths in `crates/ax/tk-axdriver-net/src/e1000/chip82540.rs`; license `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_82541.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): 82541/82547 PHY/NVM/MAC initialization, reset, DSP/FFE, link, LED, cable, counters and NVM MAC helpers in `crates/ax/tk-axdriver-net/src/e1000/chip82541.rs`; license `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_82542.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): 82542 register translation, reset, bus info, legacy MAC/NVM, RAR, LED and flow-control helpers in `crates/ax/tk-axdriver-net/src/e1000/chip82542.rs`; license `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_82543.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): 82543/82544 software-MDIO, TBI statistics/compatibility, reset/link/LED/VFTA and MAC/NVM helpers in `crates/ax/tk-axdriver-net/src/e1000/chip82543.rs`; license `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_82571.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): 82571/72/73/74/83 PHY, NVM semaphore/checksum/flash, reset/init, LPLU, serdes, VLAN, LED and LAA helpers in `crates/ax/tk-axdriver-net/src/e1000/chip82571.rs`; license `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_82571.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): 82571/82572/82573/82574/82583 family PHY/NVM/MAC routines in `crates/ax/tk-axdriver-net/src/e1000/chip82571.rs`; includes EEPROM/flash checksum and semaphore paths, reset/init, serdes, VLAN, LED, LAA and counter handling. License `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_82575.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): 82575/76/80/i350/i354/i210/i211 PHY, NVM, PCS/SGMII, SFP/I2C, EEE, VMDQ, VLAN, reset and PCIe completion helpers in `crates/ax/tk-axdriver-net/src/e1000/chip82575.rs`; license `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_80003es2lan.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): 80003ES2LAN PHY/NVM/dual-port semaphores, GG82563 paged access, Kumeran timing and link workarounds, reset/init, cable, MAC and counters in `crates/ax/tk-axdriver-net/src/e1000/chip80003es2lan.rs`; license `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_ich8lan.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): flash/shadow-NVM, ICH/PCH PHY, LED, RAR/MTA, power, reset/init, and link helpers in `crates/ax/tk-axdriver-net/src/e1000/chipich8.rs`; 96 unique source definitions have `upstream:` counterparts (task inventory estimates 99; three extra entries are not unique definitions in this source revision); license `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_i210.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): i210/i211 SW/FW semaphores, EERD/EEWR, iNVM, checksum/flash, LED, PLL and init helpers in `crates/ax/tk-axdriver-net/src/e1000/i210.rs` (22 unique definitions); license `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_mbx.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): generic, PF and VF mailbox synchronization, status, posted-message and VMBMEM transfer helpers in `crates/ax/tk-axdriver-net/src/e1000/mbx.rs` (32 unique definitions); license `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/e1000_vf.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause, Intel copyright): VF no-PHY/NVM defaults, PF mailbox reset, link, unicast/multicast, VLAN, RLPML and promiscuous requests in `crates/ax/tk-axdriver-net/src/e1000/vf.rs` (21 unique definitions); license `crates/ax/tk-axdriver-net/LICENSES/BSD-3-Clause-Intel-E1000.txt`.
- FreeBSD `sys/dev/e1000/if_em.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause; Intel, Nicole Graziano, and Kevin Bowling copyrights): initial queue/AIM, memory-error policy/statistics, MTU, EEE and address validity helpers in `crates/ax/tk-axdriver-net/src/e1000/if_em.rs` (27/164 ctags definitions represented so far); license `crates/ax/tk-axdriver-net/LICENSES/BSD-2-Clause-FreeBSD-E1000.txt`.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: `e1000::if_em` now has 36/164 unique `ctags` markers, including the 82575/76/80 and i210/i350 ECC configuration and counter paths; the 186-function task inventory additionally counts framework methods not exposed by ctags.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: `e1000::if_em` now has 72/164 unique `ctags` markers, adding interrupt/fatal-reset transitions, PCI identity/busmaster fence, hardware counters, PBA/flow-control reset, TX/RX descriptor setup and RSS initialization. iflib/newbus/sysctl callback mapping remains in progress.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: the `if_em` Rust adapter is at 100/164 unique `ctags` definitions, adding TX/RX/RSS initialization, VLAN shadow/filter, promisc/multicast and VF counter/update paths. Remaining hardware and framework definitions still need translation or a per-function framework omission reason.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: `e1000::if_em` reaches 103/164 unique definitions, adding VF counter epochs, stop/flush/reset, legacy/advanced TX/RX descriptor and RSS initialization, DMA-coalescing and SmartSpeed helpers.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: `e1000::if_em` now has 107/164 unique `ctags` markers, including 82574/82575 interrupt-vector routing, Ethernet media mode/status and flow-control mode validation; device wake, some lifecycle callbacks, PCI power and remaining pure framework registrations are not translated yet.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: 115/164 unique `ctags` functions now have Rust markers, including 82574 NVM MSI-X update, IGB queue-vector interrupt enable/disable, WoL wake-link power policy, PCH sleep ULP/EEE setup, LED, and cached firmware-version helpers.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: 116/164 `ctags` functions now have Rust counterparts, including the admin status transition policy, reset/media/PHY-hang handling, statistics timing, 82571 LAA restore and legacy SmartSpeed; PCI binding uses shared MAC type to choose advanced queue mode.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: 117/164 ctags functions now have Rust markers; `em_if_timer` preserves queue-zero-only stats scheduling and deferred admin processing.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: 118/164 ctags functions now have Rust counterparts; `em_get_wakeup` translates generation/function-specific NVM APME lookup, PCH WUC PHY wake, PCI PME/board restrictions, quad-port policy and magic-packet default.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: 119/164 ctags functions now have Rust counterparts; `em_enable_wakeup` covers multicast wake filters, RCTL/CTRL/CTRL_EXT programming, PHY-vs-MAC wake selection, failure rollback, PME and PCI bus-master shutdown through platform adapters.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: 121/164 ctags functions now have Rust counterparts; `em_enable_phy_wakeup` and `em_disable_phy_wakeup` preserve BM-page RAR/MTA/RCTL/filter setup, PHY lock, wake-enable restore, sticky WUS clearing and armed-state transitions.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: 122/164 ctags functions now have Rust counterparts; `em_if_init` retains PF/VF reset and bus-master gates, RAR and link reset ordering, TX/RX/VLAN/IOV setup, interrupts, EEE/ECC and reset-cause completion.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: 123/164 ctags functions now have Rust counterparts; `em_setup_interface` preserves single-queue send-queue policy and copper/fiber/IFE/VF media option advertisement.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: 126/164 ctags functions now have Rust counterparts; suspend/shutdown/resume preserve VF retry stops, wake setup, ownership release, PCH resume workaround, PHY/MAC wake-status clearing and PME cleanup.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: 132/164 ctags functions now have Rust counterparts; added DMAC/EEE policy, interrupt-delay tick and TXD IDE behavior, TSO flag mask register RMW, and TX/RX ring sysctl accessors.
- FreeBSD `sys/dev/e1000/if_em.c` coverage update: 133/164 ctags functions now have Rust counterparts; `em_sysctl_interrupt_rate_handler` selects the generation/vector register and applies the legacy/IGB reciprocal conversion.
- FreeBSD `sys/dev/e1000/em_txrx.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): added `e1000::txrx` source-marker counterparts for `em_receive_checksum` and `em_determine_rsstype`; FreeBSD author copyrights are retained in the Rust module and license is `tk-axdriver-net/LICENSES/BSD-2-Clause-FreeBSD-E1000.txt`.
- FreeBSD `sys/dev/e1000/em_txrx.c` coverage update: 9/15 unique ctags definitions have counterparts; translated checksum-context and TSO context setup, TX encapsulation/sentinel split, tail flush/AIM publication and report-status credit walk in `e1000::txrx`.
- FreeBSD `sys/dev/e1000/em_txrx.c` coverage update: 14/15 ctags definitions have Rust counterparts; legacy/advanced RX descriptor refill, availability, packet get/error drop, checksum/VLAN/RSS metadata, and tail flush are translated in `e1000::txrx`. `em_dump_rs` is diagnostic-only; task inventory's 17 count exceeds the 15 unique function definitions in this source revision.
- FreeBSD `sys/dev/e1000/igb_txrx.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): all 12 unique ctags function definitions translated in `e1000::txrx`, including advanced TSO/checksum/VLAN context/data descriptors, completion, RX, SCTP/RSS, and VF VLAN policy; license `tk-axdriver-net/LICENSES/BSD-2-Clause-FreeBSD-E1000.txt`.
- FreeBSD `sys/dev/igc/igc_api.c` and `sys/dev/igc/igc_i225.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause): translated the shared I225 function-table initialization and all `igc_api.c` operation-table dispatch entrypoints in `tk-axdriver-net/src/igc/{api,i225}.rs`; retained Intel and Rubicon Communications copyrights; license `tk-axdriver-net/LICENSES/BSD-3-Clause-FreeBSD-IGC.txt`.
- FreeBSD `sys/dev/igc/igc_base.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause): all 7 functions translated in `tk-axdriver-net/src/igc/base.rs`, including PHY semaphore selection, base initialization, receive FIFO flush, and I225/I226 predicates; license `tk-axdriver-net/LICENSES/BSD-3-Clause-FreeBSD-IGC.txt`.
- FreeBSD `sys/dev/igc/igc_nvm.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause): all 22 source functions translated in `tk-axdriver-net/src/igc/nvm.rs`, including SPI bit shifts, EERD/EEWR polling, NVM grant/release, checksum, PBA/MAC and firmware-version helpers; license `tk-axdriver-net/LICENSES/BSD-3-Clause-FreeBSD-IGC.txt`.
- FreeBSD `sys/dev/igc/igc_mac.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause): all 29 source functions translated in `tk-axdriver-net/src/igc/mac.rs`, including RAR/VFTA/MTA, alternate MAC, link, flow control, counters, NVM semaphore and PCI master handling; license `tk-axdriver-net/LICENSES/BSD-3-Clause-FreeBSD-IGC.txt`.
- FreeBSD `sys/dev/igc/igc_phy.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause): all 26 source functions translated in `tk-axdriver-net/src/igc/phy.rs`, including MDIC/GPY/XMDIO, autonegotiation, link, LPLU, PHY reset and power management; license `tk-axdriver-net/LICENSES/BSD-3-Clause-FreeBSD-IGC.txt`.
- FreeBSD `sys/dev/igc/igc_txrx.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): 11/12 ctags functions translated in `tk-axdriver-net/src/igc/txrx.rs` (advanced TSO/context descriptors, RS credits, RX descriptors, checksums and RSS); `igc_dump_rs` is a debug-only descriptor dump and is omitted. Copyright Matthew Macy and Rubicon Communications retained; license `tk-axdriver-net/LICENSES/BSD-2-Clause-FreeBSD-IGC.txt`.
- FreeBSD `sys/dev/igc/if_igc.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): first 18/91 ctags functions translated in `tk-axdriver-net/src/igc/if_igc.rs` (AIM deltas/rate, restart policy, I225 IPG, filters, multicast, timer and address checks); retained Intel, Nicole Graziano and Rubicon copyrights; license `tk-axdriver-net/LICENSES/BSD-2-Clause-FreeBSD-IGC-IF.txt`.
- FreeBSD `sys/dev/igc/if_igc.c` coverage update: `igc_reset`, RSS map programming, and TX/RX unit setup are now translated in `igc/if_igc.rs`; per-file coverage 22/91 ctags definitions.
- FreeBSD `sys/dev/igc/if_igc.c` coverage update: translated `igc_if_init/stop/suspend/shutdown/resume`, MTU, admin link transition, and fatal parity recovery helpers; coverage is now 31/91 ctags definitions.
- FreeBSD `sys/dev/igc/if_igc.c` coverage update: added ICR handling, MSI-X/legacy masks, fatal-error capture/admin transition, queue interrupt re-enable, IVAR routing, and initial EITR programming; coverage is now 43/91 ctags functions.
- FreeBSD `sys/dev/igc/if_igc.c` coverage update: translated PCI identity/L1.2 and busmaster controls, shared firmware ownership, iflib counter selection, queue cap, and MSI-X setup stub; coverage is now 51/91 ctags definitions.
- FreeBSD `sys/dev/igc/if_igc.c` coverage update: `igc_update_ecc_stats` and `igc_update_stats_counters` now read/accumulate clear-on-read counters, preserve low-before-high 64-bit reads, update pause interval state, and acknowledge corrected ECC status; coverage is 63/91.
- FreeBSD `sys/dev/igc/if_igc.c` coverage update: translated configuration of WOL capability, suspend filters/PME and firmware-version formatting, plus `if_attach_pre/post/detach` sequencing; current marker recount 67/91 ctags functions.
- FreeBSD `sys/dev/igc/if_igc.c` coverage update: `if_igc.rs` now also translates flow-control, DMAC/EEE sysctl policy, TSO flag-mask register updates, EITR reporting and register-read behavior; current ctags marker count 74/91, with only debug/sysctl registration, PCI/iflib allocation callbacks and interface media registration omitted as framework-only (reasons in `progress-S.md`).
- Linux excerpt scanner overlap inventory (2026-10-09): `crates/ax` reports 25 additional normalized code-line matches at the 25-character threshold (three also match at 40 characters) in FreeBSD-derived e1000/igc and SDHCI translations. These Intel register/descriptor expressions are independently present in the cited FreeBSD BSD-2/3-Clause sources; they were translated from those FreeBSD files, not copied from Linux. The `crates/ax` aggregate baselines in `tests/ci/test_linux_excerpt_baseline.py` include these newly visible common expressions; the source-specific FreeBSD license/provenance entries above remain authoritative.
- FreeBSD `sys/dev/igc/igc_regs.h` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause): corrected live queue-0 descriptor register offsets in `tk-axdriver-net/src/igc/regs.rs` and added the RXCSUM access required by translated `if_igc.c` queue initialization; license `tk-axdriver-net/LICENSES/BSD-3-Clause-FreeBSD-IGC.txt`.
- FreeBSD `sys/dev/igc/igc_phy.h`, `igc_regs.h` and `igc_i225.c` at `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause): named the I225 PHPM, MANC and SW/FW semaphore MMIO registers in `igc/regs.rs` for the live translated generic PHY reset and I225 semaphore path; license `tk-axdriver-net/LICENSES/BSD-3-Clause-FreeBSD-IGC.txt`.
- FreeBSD `sys/dev/igc/if_igc.c` `igc_enable_pci_busmaster()` at commit `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause): the live IGC PCI probe now uses this translated command-register read/enable/reread gate with `PciRoot`; source license is present in `tk-axdriver-net/LICENSES/BSD-2-Clause-FreeBSD-IGC-IF.txt`.
- FreeBSD `sys/dev/mmc/mmc.c` (`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e`, BSD-2-Clause): `mmc_calculate_clock`, `mmc_switch_to_hs200`, `mmc_switch_to_hs400`, `mmc_retune`, and the single-card `mmc_discover_cards` path are now named as their source functions in `tk-axdriver-block/src/sdhci.rs`; the timing planner intersects card type, host version/capabilities, bus width, and clock before sequencing VCCQ/tuning/timing.
- FreeBSD `sys/dev/ahci/ahci.c` (`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e`, BSD-2-Clause): the BlockDriverOps error/timeout path is now explicitly named `ahci_reset()` and unifies DMA stop proof, best-effort CLO, PHY reset, and engine restart; CAM queue freeze/requeue and request-sense handling remain outside the block interface.
- FreeBSD `sys/dev/sdhci/sdhci.c` (`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e`, BSD-2-Clause): runtime card polling/add path in `tk-axdriver/src/sdhci.rs` is split into source-named `sdhci_card_task`, `sdhci_card_poll`, `sdhci_handle_card_present`, and `sdhci_handle_card_present_locked`; the lock spans only one slot's presence/reprobe transition rather than the previous whole-slot-vector scan.
- FreeBSD `sys/dev/ahci/ahci.c` (`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e`, BSD-2-Clause): the PCI interrupt acknowledge path now has a per-port `ahci_ch_intr()` adapter that validates the BAR slot, latches PxIS status, and W1C-acknowledges it before the BlockCompletion sampler runs; controller-wide `ahci_intr()` dispatch remains the MSI-X/MSI/INTx owner.
- FreeBSD `sys/dev/ahci/ahci.c` (`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e`, BSD-2-Clause): the NCQ/transport error recovery routine is now named `ahci_issue_recovery()` and reads the error log before invoking `ahci_reset()`; request retirement remains typed block completion rather than CAM held-CCB retry.
- FreeBSD `sys/dev/ahci/ahci.c` (`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e`, BSD-2-Clause): the READ LOG EXT NQ/tag decoder is now named `ahci_process_read_log()` and has regression coverage for NQ/tag masks; it supplies the recovery decision in `ahci_issue_recovery()`.
- FreeBSD `sys/dev/ahci/ahci.c` (`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e`, BSD-2-Clause): block request timeout handling is explicitly named `ahci_timeout()`; the reset/quiescence result determines whether requests fail normally or return `Quarantined`, while FreeBSD callout scheduling remains absent.
- FreeBSD `sys/dev/ahci/ahci.c` (`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e`, BSD-2-Clause): typed physical request retirement is factored into `ahci_done()` and port-reset batch retirement into `ahci_end_transaction()`; completion identity/status/byte count are preserved while replacing CAM CCB completion queues.
- FreeBSD `sys/dev/sdhci/sdhci.c` (`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e`, BSD-2-Clause): `sdhci_retune()` is translated as a pending-retune latch consumed by request-context `mmc_retune()`; this preserves timer/interrupt semantics without executing MMC commands from interrupt context.
- FreeBSD `sys/dev/ahci/ahci.c` (`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e`, BSD-2-Clause): `ahci_process_timeout()` now handles per-request poll expiry by resetting the port once and retiring the affected typed physical-SG batch as device errors or quarantined requests according to proven DMA stop.
- FreeBSD `sys/dev/ahci/ahci_pci.c` (`c2b7fe4a9e94a0edba9dd2772874928b565c4f9e`, BSD-2-Clause): PCI bind now keeps exact `ahci_probe()`/`ahci_pci_attach()` markers, delegates revision-qualified device ID matching to `ahci_ata_probe()`, and maps shared HBA reset to `ahci_pci_ctlr_reset()`; PCI detach and PM callbacks remain unavailable.
| FreeBSD `sys/dev/ichiic/ig4_iic.c`, `ig4_pci.c`, `ig4_acpi.c`, `ig4_reg.h`, `ig4_var.h` | `crates/ax/tk-i2c/src/lib.rs` | FreeBSD 2026-10-08 snapshot | BSD-3-Clause (`ig4_iic.c`, `ig4_pci.c`, `ig4_reg.h`, `ig4_var.h`); BSD-2-Clause (`ig4_acpi.c`) | Copyright (c) 2014 The DragonFly Project; Copyright (c) 2016 Oleksandr Tymoshenko <gonzo@FreeBSD.org> | Controller registers, bounded transfer core, ACPI bus address type, and PCI IDs translated; platform bus attachment and i2c-dev ABI remain outstanding. |
| FreeBSD `sys/dev/ichiic/ig4_iic.c`, `ig4_pci.c`, `ig4_acpi.c`, `ig4_reg.h`, `ig4_var.h`; ACPI I2cSerialBusV2 AML resource format | `crates/ax/tk-i2c/src/{ig4.rs,pci.rs,acpi.rs,reg.rs}`, `crates/ax/tk-acpica/src/resources.rs`, `crates/ax/tk-axdriver/src/i2c.rs`, `kernel/src/acpi/i2c.rs`, `kernel/src/pseudofs/dev/i2c.rs` | FreeBSD 2026-10-08 snapshot | BSD-3-Clause (ig4_iic/pci/reg/var); BSD-2-Clause (ig4_acpi); ACPI resource parser is TheKernel-owned | Copyright (c) 2014 The DragonFly Project; Copyright (c) 2016 Oleksandr Tymoshenko <gonzo@FreeBSD.org> | Function-level ig4 transfer/configuration/suspend/ISR/dump paths and full 140-row PCI device table translated; default PCI binding and MSI/poll fallback attached; ACPICA I2cSerialBusV2 child enumeration and Linux i2c-dev `/dev/i2c-N` ioctls wired. |
| FreeBSD `sys/dev/iicbus/iichid.c`, `sys/dev/hid/hid.c`, `hidbus.c`, `hmt.c` | `crates/ax/tk-i2c-hid/src/lib.rs` | FreeBSD 2026-10-08 snapshot | BSD-2-Clause | Copyright (c) 2018-2020 Marc Priggemeyer and Vladimir Kondratyev; Copyright (c) 1998 The NetBSD Foundation, Inc.; Copyright (c) 2001 Lennart Augustsson | Descriptor framing, command/power/reset path and input packet framing translated; transport attachment, shared report parser integration and multitouch/evdev mapping remain outstanding. |
| FreeBSD `sys/x86/iommu/intel_dmar.h`, DMAR portions of `intel_drv.c` | `crates/ax/tk-vtd/src/lib.rs` | FreeBSD 2026-10-08 snapshot | BSD-2-Clause | Copyright (c) 2013-2015 The FreeBSD Foundation; developed by Konstantin Belousov under Foundation sponsorship | DMAR DRHD/RMRR parsing, requester unit selection for direct endpoint scopes, and fail-closed DMA facade contract translated; VT-d hardware initialization, page tables, QI, faults, interrupt remapping, and driver integration remain outstanding. |
| FreeBSD `sys/netgraph/bluetooth/drivers/ubt/ng_ubt.c` | `crates/ax/tk-bt-hci/src/lib.rs` | FreeBSD 2026-10-08 snapshot | BSD-2-Clause | Copyright (c) 2001-2009 Maksim Yevmenkin | HCI command/ACL/event packet validation, endpoint routing contract, channel ownership model and no-device ioctl result translated; USB binding, firmware updater, AF_BLUETOOTH socket plumbing and control/monitor ioctl semantics remain outstanding. |
| FreeBSD `sys/dev/iicbus/iichid.c`, `sys/dev/hid/{hid.c,hidbus.c,hmt.c}`; HID-over-I2C 1.0 protocol | `crates/ax/tk-i2c-hid/src/lib.rs`, `crates/ax/tk-axdriver/src/i2c_hid.rs`, `kernel/src/acpi/i2c.rs` | FreeBSD 2026-10-08 snapshot | BSD-2-Clause | Copyright (c) 2018-2019 Marc Priggemeyer; Copyright (c) 2019-2020 Vladimir Kondratyev; Copyright (c) 1998 The NetBSD Foundation, Inc.; Copyright (c) 2014-2020 Vladimir Kondratyev; HID parser contributed by Lennart Augustsson | HID descriptor/report/register, reset/power, report I/O, ACPI _DSM and evdev input binding translated; newbus/quirks, GPIO interrupt/sampling controls, and specialized hmt quirks omitted. |
| FreeBSD `sys/x86/iommu/intel_{dmar.h,drv.c,ctx.c,idpgtbl.c,qi.c,fault.c}`; Intel VT-d Architecture Specification | `crates/ax/tk-vtd/src/{lib.rs,pgtbl.rs,iova.rs}`, `kernel/src/acpi/vtd.rs`, `crates/ax/tk-axdriver/src/{virtio.rs,nvme.rs}` | FreeBSD 2026-10-08 snapshot; VT-d page-table/MMIO programming is TheKernel-owned | BSD-2-Clause for FreeBSD-derived DMAR/requester/DMA behavior; original page-table/IOMMU setup uses the specification | Copyright (c) 2013-2015 The FreeBSD Foundation; developed by Konstantin Belousov under Foundation sponsorship | DMAR/RMRR/scopes, requester selection, QI/global invalidation, faults, second-level mapping facade and VirtIO/NVMe DMA seams implemented; per-requester domain isolation and interrupt remapping remain incomplete. |
| FreeBSD `sys/dev/iicbus/iichid.c`, `sys/dev/hid/{hid.c,hidbus.c,hmt.c}` | `crates/ax/tk-axdriver/src/{hidbus.rs,hmt.rs,i2c_hid.rs}`, `crates/ax/tk-axdriver/src/usb/{hid_report.rs,hid_usage.rs}`, `crates/ax/tk-i2c-hid/src/lib.rs`, `kernel/src/acpi/i2c.rs` | FreeBSD 2026-10-08 snapshot; generic report parser reused from TheKernel | BSD-2-Clause for adapter behavior translated from FreeBSD; report parser is TheKernel-owned | Copyright (c) 2018-2019 Marc Priggemeyer; Copyright (c) 2019-2020 Vladimir Kondratyev; Copyright (c) 1998 The NetBSD Foundation, Inc.; Copyright (c) 2014-2020 Vladimir Kondratyev | Corrected I2C HID command semantics and lifecycle, ACPI ELAN0000 exception, report and adaptive-sampling flow, hidbus report/metadata access, hmt slot/X/Y validation, and ABS_MT evdev mappings; GPIO IRQ forwarding and device-specific hmt quirks remain unavailable/untranslated. |
| FreeBSD `sys/dev/hid/hid.c`, `hidbus.c`, `hmt.c`; HID 1.11 report grammar | `crates/ax/tk-axdriver/src/usb/hid_report.rs`, `crates/ax/tk-axdriver/src/hidbus.rs`, `crates/ax/tk-axdriver/src/hmt.rs`, `crates/ax/tk-axdriver/src/i2c_hid.rs` | FreeBSD 2026-10-08 snapshot; generic report parser is TheKernel-owned and shared with USB | BSD-2-Clause behavior adapters; report parser is original TheKernel code | Copyright (c) 1998 The NetBSD Foundation, Inc.; Copyright (c) 2001 Lennart Augustsson; Copyright (c) 2014-2020 Vladimir Kondratyev | HID input/output/feature report-size metadata, Contact-ID/TIP/Confidence parsing, persistent Type-B slots, empty-sample release, hidbus metadata and report operations are implemented; feature-report-driven touchpad mode/slot-count quirks, runtime sysctl tuning, timestamp/orientation transforms, and GPIO IRQ forwarding remain incomplete. |
| FreeBSD `sys/dev/hid/hid.c` (`hid_locate`, `hid_get_data`, `hid_get_udata`, `hid_put_udata`); FreeBSD `sys/dev/hid/hmt.c` Contact Count Maximum path | `crates/ax/tk-axdriver/src/usb/hid_report.rs`, `crates/ax/tk-axdriver/src/hmt.rs`, `crates/ax/tk-axdriver/src/i2c_hid.rs` | FreeBSD 2026-10-08 snapshot; protocol parsing remains TheKernel-owned | BSD-2-Clause behavior mappings; parser code is original TheKernel implementation | Copyright (c) 1998 The NetBSD Foundation, Inc.; Copyright (c) 2001 Lennart Augustsson; Copyright (c) 2014-2020 Vladimir Kondratyev | Added bounded report usage locations and signed/unsigned bitfield get/set, required variable Contact Count Maximum feature discovery, optional feature-report read, descriptor logical-maximum fallback, and slot cap. |
| FreeBSD `sys/dev/hid/hmt.c` open/close, `sys/dev/iicbus/iichid.c` power-state and Contact Count Maximum paths | `crates/ax/tk-axdriver-input/src/lib.rs`, `crates/ax/tk-axdriver/src/structs/static.rs`, `crates/ax/tk-axdriver/src/i2c_hid.rs`, `kernel/src/pseudofs/dev/event.rs` | FreeBSD 2026-10-08 snapshot; evdev client lifecycle is TheKernel-owned | BSD-2-Clause behavior mappings | Copyright (c) 2014-2020 Vladimir Kondratyev; Copyright (c) 2018-2019 Marc Priggemeyer | First/last evdev client now controls I2C-HID SET_POWER; descriptor Contact Count Maximum feature data determines bounded Type-B slot count; touch geometry derives ABS_MT major/minor/orientation. |
| FreeBSD `sys/dev/iicbus/iichid.c` `iichid_ioctl`/HID methods, FreeBSD `sys/dev/hid/hid.c` bitfield helpers, FreeBSD `sys/dev/hid/hmt.c` evdev open/close | `crates/ax/tk-i2c-hid/src/lib.rs`, `crates/ax/tk-axdriver/src/{i2c_hid.rs,usb/hid_report.rs}`, `crates/ax/tk-axdriver-input/src/lib.rs`, `kernel/src/pseudofs/dev/event.rs` | FreeBSD 2026-10-08 snapshot; evdev client callback path is TheKernel-owned | BSD-2-Clause behavior translations; no newbus/evdev code imported | Copyright (c) 2018-2019 Marc Priggemeyer; Copyright (c) 1998 The NetBSD Foundation, Inc.; Copyright (c) 2001 Lennart Augustsson; Copyright (c) 2014-2020 Vladimir Kondratyev | Added I2CRDWR bridge, first/last-client open/close power transitions, Type-B major/minor/orientation updates, and suspend/resume power bookkeeping using the shared report bitfield interface. |
| FreeBSD `sys/dev/hid/hid.c` `hid_item_resolution()` | `crates/ax/tk-axdriver/src/usb/hid_report.rs`, `crates/ax/tk-axdriver/src/i2c_hid.rs` | FreeBSD 2026-10-08 snapshot; HID unit math translated into the shared parser | BSD-2-Clause behavior | Copyright (c) 1998 The NetBSD Foundation, Inc.; Copyright (c) 2001 Lennart Augustsson | Implemented HID centimeters/inches/radians/degrees resolution scaling with bounded arithmetic; hmt evdev ABS axes now report computed unit resolution where descriptor physical ranges are usable. |
| FreeBSD `usr.sbin/bluetooth/iwmbtfw/iwmbt_fw.c` and `iwmbt_fw.h` | BSD-2-Clause | `crates/ax/tk-bt-hci/src/iwmbt_fw.rs` | Initial translation of `iwmbt_get_fwname`, `iwmbt_get_fwname_tlv`, `iwmbt_parse_tlv`; kernel firmware I/O is routed through the planned rootfs firmware service rather than copying userspace file APIs. |
| FreeBSD `sys/netgraph/bluetooth/drivers/ubt/ng_ubt.c` | BSD-2-Clause | `crates/ax/tk-axdriver/src/usb/bluetooth.rs` | Initial xHCI HCI transport binding for control commands, interrupt events and bulk ACL, mapped to CrabUSB; Netgraph node lifecycle is not copied. |
| FreeBSD `usr.sbin/bluetooth/iwmbtfw/main.c` | BSD-2-Clause | `crates/ax/tk-bt-hci/src/iwmbt_fw.rs` | Initial translation of `iwmbt_is_supported()` USB VID/PID table; 8087:0033 CNVi family is represented. |
| FreeBSD `usr.sbin/bluetooth/iwmbtfw/iwmbt_hw.c` | BSD-2-Clause | `crates/ax/tk-bt-hci/src/iwmbt_fw.rs` | Initial translation of the bounded command/event record parser used by `iwmbt_patch_fwfile()`; endpoint execution and matching are still pending. |
| FreeBSD `usr.sbin/bluetooth/iwmbtfw/iwmbt_hw.c` | BSD-2-Clause | `crates/ax/tk-bt-hci/src/lib.rs` | Initial translation of Intel HCI command-complete matching and fixed/TLV version query response paths; firmware upload sequence remains pending. |
| FreeBSD `usr.sbin/bluetooth/iwmbtfw/main.c` `iwmbt_identify`/`handle_9260`; `iwmbt_hw.c` version, secure-header, firmware/DDR/reset and event-mask paths | BSD-2-Clause | `crates/ax/tk-bt-hci/src/{lib.rs,iwmbt_fw.rs}`, `crates/ax/tk-axdriver/src/usb/bluetooth.rs` | N305-class 9260-family firmware selection and rootfs-ready SFI/DDC upload path; legacy 7260/8260 handlers remain partial. |
| FreeBSD `usr.sbin/bluetooth/iwmbtfw/main.c` `handle_7260`/`handle_8260`/`handle_9260`; `iwmbt_fw.c` fallback naming | BSD-2-Clause | `crates/ax/tk-axdriver/src/usb/bluetooth.rs`, `crates/ax/tk-bt-hci/src/iwmbt_fw.rs` | Rootfs-ready firmware request and family dispatch for BSEQ/SFI/DDC workflows; CLI and USB enumeration are replaced by the in-kernel USB class adapter. |
| FreeBSD `sys/dev/hid/hmt.c` `hmt_set_input_mode`; `sys/dev/hid/hconf.c` `hconf_set_feature_control` | BSD-2-Clause | `crates/ax/tk-axdriver/src/{hmt.rs,i2c_hid.rs}` | Locates Feature Input Mode and writes value 3 while preserving same-report Surface/Button switch defaults for touchpad attach. |
| FreeBSD `sys/dev/hid/hmt.c` Button Type feature report in `hmt_attach()` | BSD-2-Clause | `crates/ax/tk-axdriver/src/{hmt.rs,i2c_hid.rs}` | Reads `HUD_BUTTON_TYPE` when it is in a distinct feature report and advertises `INPUT_PROP_BUTTONPAD` for value zero; THQA certificate feature handling remains omitted. |
| FreeBSD `sys/dev/hid/hmt.c` serial/hybrid Contact Count batching in `hmt_intr()` | BSD-2-Clause | `crates/ax/tk-axdriver/src/usb/hid_report.rs`, `crates/ax/tk-axdriver/src/i2c_hid.rs` | Uses Contact Count to defer SYN and processes only the remaining per-report contact subset; timestamp updates remain incomplete. |
| FreeBSD `sys/dev/hid/hmt.c` same-report Button Type/Contact Count feature reuse and PTP button mapping | BSD-2-Clause | `crates/ax/tk-axdriver/src/{i2c_hid.rs,usb/hid_usage.rs}` | Reads one shared feature report once; maps integrated and external-primary buttons to BTN_LEFT, external button 3 to BTN_RIGHT, while full button quirks remain incomplete. |
| FreeBSD `sys/dev/hid/hmt.c` Contact Count batches across report IDs | BSD-2-Clause | `crates/ax/tk-axdriver/src/usb/hid_report.rs` | Restricts contact collection count to its input Report ID and allows other Report IDs to decode and synchronize without corrupting pending contact batches. |
| USB HID Digitizers Contact Count, Confidence, Width, Height, and Contact ID usage mapping | HID usage semantics, TheKernel parser implementation | `crates/ax/tk-axdriver/src/usb/hid_usage.rs` | Keeps contact-only usages out of pressure/blob/tracking event capabilities when outside a finger collection; Contact Count no longer aliases ABS_MT_PRESSURE. |
| Linux Bluetooth UAPI `hci_dev_req` / `HCIGETDEVLIST` record layout (structure semantics only; no GPL implementation code) | Linux UAPI specification | `kernel/src/file/bluetooth.rs` | Device list entries now include aligned `dev_opt` flags with HCI_UP matching the attached adapter state. |
| Linux Bluetooth UAPI `hci_dev_stats` fields (structure semantics only; no GPL implementation code) | Linux UAPI specification | `crates/ax/tk-bt-hci/src/lib.rs`, `crates/ax/tk-axdriver/src/usb/bluetooth.rs`, `kernel/src/file/bluetooth.rs` | HCI command/event/ACL packet and byte counters are reflected through HCIGETDEVINFO; errors and SCO remain zero until their transport paths exist. |
| Bluetooth Core standard Read BD_ADDR, Read Local Supported Features, Read Buffer Size commands | Bluetooth Core specification | `crates/ax/tk-bt-hci/src/lib.rs`, `crates/ax/tk-axdriver/src/usb/bluetooth.rs`, `kernel/src/file/bluetooth.rs` | On HCI device up, caches controller address, feature bitmap, and ACL/SCO buffer limits for HCIGETDEVINFO; failure leaves the last/default cache unchanged. |
| FreeBSD `sys/dev/bluetooth/iwmbtfw/iwmbt_hw.c` `iwmbt_patch_fwfile()` expected events | BSD-2-Clause | `crates/ax/tk-bt-hci/src/lib.rs` | Validates both HCI event code and expected payload, bounds returned transfer length, and accounts successful command/event transfers in device statistics. |
| Bluetooth HCI monitor packet framing | Bluetooth Core and Linux HCI monitor UAPI semantics | `crates/ax/tk-bt-hci/src/lib.rs`, `crates/ax/tk-axdriver/src/usb/bluetooth.rs`, `kernel/src/file/bluetooth.rs` | Variable monitor records now retain a complete maximum-size 1028-byte ACL frame rather than truncating to the former 266-byte capture buffer. |
| Linux `include/uapi/linux/i2c.h` (`GPL-2.0-or-later WITH Linux-syscall-note`) SMBus capability bits | Linux I2C UAPI only; no `drivers/i2c` or hwmon implementation copied | `crates/ax/tk-i2c/src/i2cdev.rs` | `/dev/i2c-N` functionality mask uses the defined SMBus read-word capability; one normalized code-line scan match is an incidental match to the same bitwise term used by Linux drivers. |
| FreeBSD `sys/x86/iommu/intel_reg.h` (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/reg.rs` | Complete register-bit, descriptor and entry definitions: 240/240 non-guard macros and all 3 upstream 128-bit root/context/IRTE entry types; QI descriptor is represented as an additional two-word Rust type. Hardware translations not yet enabled by this constants-only commit. |
| FreeBSD `sys/x86/iommu/intel_dmar.h` data model/capability macros (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/dmar.rs` | All 3 upstream persistent structures (`dmar_domain`, `dmar_ctx`, `dmar_unit`) have Rust storage models. PCI/VM/taskqueue/lock handles are represented by TheKernel manager-owned metadata/resources; pointer-container and lock assertion macros map to native ownership rather than being copied. |
| FreeBSD `sys/x86/iommu/intel_utils.c` (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/utils.rs` | 25/26 functions ported: page geometry, AGAW/SAGAW selection, flush callbacks, GCMD/GSTS command waits, write-buffer/protected-region handling, IRTA/IR enablement, barriers and timeout accessors. Only the sysctl handler is framework-only; register writes/completion conditions have fake-I/O unit tests. |
| FreeBSD `sys/x86/iommu/intel_qi.c` (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/qi.rs` | 19/19 functions mapped: QI enable/disable, ring head/tail/space/descriptor encoding, sequence wrap/wait, global/page/IEC invalidation, interrupt/task hooks, initialization/teardown and interrupt masks. Coherent DMA queue allocation, taskqueues, lock ownership and waiter notifications use `QiIo` adapters. |
| FreeBSD `sys/x86/iommu/intel_idpgtbl.c` (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/{idpgtbl.rs,pgtbl.rs}`, `kernel/src/acpi/vtd.rs` | 14/14 functions mapped: identity table cache/ref lifecycle, second-level table walk and map/unmap bridges, domain allocation/free, access bits, and register PSI/domain IOTLB flush fallback. VM/sf_buf locks map to exclusive SecondLevel/PageMemory ownership; freeing table pages is deferred to quiesced domain destruction. Current hardware adapter admits 4-level/2 MiB identity tables only. |
| FreeBSD `sys/x86/iommu/intel_ctx.c` (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/context.rs`, `crates/ax/tk-vtd/src/dmar.rs`, `kernel/src/acpi/vtd.rs` | 24/24 functions have mapping entry points: per-bus root/context page setup, CTX1/CTX2 population/flush policy, domain/context reference lifecycle, requester acquisition/move/free and deferred unload; PCI/ACPI discovery, IOMMU GAS/RMRR resource objects and taskqueue are adapters. Kernel builds one shared DMA domain and populates each bus context page; per-device isolation remains absent. |
| FreeBSD `sys/x86/iommu/intel_fault.c` (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/fault.rs` | 9/9 functions mapped: fault ring advance/clear, interrupt drain and W1C semantics, deferred source-ID reporting/context fault cache, fault-log init/fini, and FECTL masking. Spin locks/taskqueues/console/device lookup map to FaultIo callbacks. |
| FreeBSD `sys/x86/iommu/intel_quirks.c` (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/quirks.rs` | 7/7 functions and all 6 northbridge/1 CPU quirk records mapped: 5400 protected-memory, 5500 interrupt-remapping revision, and E5 AM=9 behavior. PCI northbridge and CPUID facts are inputs to the matcher. |
| FreeBSD `sys/x86/iommu/intel_drv.c` (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/{lib.rs,driver.rs}`, `kernel/src/acpi/vtd.rs` | 35/40 functions mapped: ACPI structure walk/find/count/RHSA, DRHD/RMRR scopes, PCI path matching, non-PCI UID lookup, unit/attach/lifecycle/RMRR hooks. Five DDB-only `db_dmar_print_domain`/`dmar_print_one`/`db_dmar_print`/`db_show_all_dmars` debugger routines are framework diagnostics and omitted. |
| FreeBSD `sys/dev/iommu/iommu_gas.c` (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/gas.rs`, `crates/ax/tk-vtd/src/iova.rs` | 31/33 functions mapped: guard-page/alignment/boundary first-fit, placeholders, range/RMRR reserve/clip/remove, deferred unmap, entry accounting and MSI translation. Two DDB-only GAS inspection functions are omitted; RB-tree augmentation maps to an ordered vector scan while preserving address-ordered behavior. `IovaAllocator` now delegates to GAS and uses guard pages. |
| FreeBSD `sys/dev/iommu/iommu_gas.c` (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/gas.rs`, `crates/ax/tk-vtd/src/iova.rs` | 31/33 functions mapped: guard-page/alignment/boundary first-fit, placeholders, range/RMRR reserve/clip/remove, deferred unmap, entry accounting and MSI translation. Two DDB-only GAS inspection functions are omitted; intrusive augmented RB-tree operations map to an address-ordered Vec with linear gap scans. |
| FreeBSD `sys/dev/iommu/busdma_iommu.c` (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/busdma.rs` | 2/34 functions adapted: segment-constrained map loading, all-or-nothing rollback, and map unload through TheKernel's DMA facade; VM-page discovery, busdma tag/callback/KMSAN/taskqueue framework wrappers are not copied. |
| FreeBSD `sys/x86/iommu/intel_intrmap.c` (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/intrmap.rs` | 10/10 function entry points modeled: IRTE first-fit allocation, MSI and IOAPIC remap encoding, requester-tagged entry writes, free/invalidation, and IR table init/fini. VMEM, `device_t`/HPET/PCI resolution, physical table allocation, and APIC reprogramming are native adapters; kernel vector-path integration is not active. |
| FreeBSD `sys/x86/iommu/iommu_utils.c` (BSD-2-Clause; FreeBSD `c2b7fe4`) | `crates/ax/tk-vtd/src/iommu_utils.rs`, `crates/ax/tk-vtd/src/qi.rs` | 8/44 mapped: pgtbl PTE offsets/index/count/page size, DMA-tag address constraints, and QI generation/wait helpers. VM/sf_buf, x86 vtable, bus topology, IRQ/MSI framework, sysctl and DDB registration/output remain native platform concerns. |
| FreeBSD `sys/dev/iommu/busdma_iommu.c` `iommu_bus_dmamap_load_ma()` / `_load_phys()` | `crates/ax/tk-vtd/src/busdma.rs` | Scatter-page/offset and contiguous-extent loaders added to the TheKernel `DmaMap` facade, including noncontiguous physical pages and segment constraints. |
| FreeBSD `sys/x86/iommu/intel_utils.c` / `intel_qi.c` hardware command flow | `kernel/src/acpi/vtd.rs`, `crates/ax/tk-vtd/src/{utils.rs,qi.rs}` | Kernel setup now routes shutdown/root-pointer/register flush/QIE/TE ordering and global context+IOTLB QI sequence waits through the translated helpers; QI interrupt delivery remains masked and completion uses the memory-write sequence path. |
| FreeBSD `sys/dev/iommu/busdma_iommu.c` `iommu_bus_dmamap_load_something()` / `_load_something1()` / `_load_buffer()` | `crates/ax/tk-vtd/src/busdma.rs` | Split transactional load commit from segment mapper and added virtual-buffer page extraction callback; source coverage now 6/34 with VM/pmap lookup represented by TheKernel caller hooks. |
| FreeBSD `sys/dev/hid/hid.c` `hid_report_size()` / `hid_report_size_max()` | `crates/ax/tk-axdriver/src/usb/hid_report.rs` | Per-ID byte length includes the report-ID byte; max-size returns the largest report length while selecting the first nonzero report ID, matching the shared HID parser metadata API. |
| FreeBSD `sys/dev/hid/hid.c` `hid_get_report_descr()` / `sys/dev/hid/hidbus.c` `hidbus_get_rdesc()` | `crates/ax/tk-axdriver/src/hidbus.rs` | Descriptor accessor reports required length/validates destination capacity and copies the immutable ACPI/I2C-HID report descriptor. |
| FreeBSD `sys/dev/hid/hidbus.c` `hidbus_locate()` | `crates/ax/tk-axdriver/src/{hidbus.rs,usb/hid_report.rs}`, `hmt.rs`, `i2c_hid.rs` | Top-level collection indexes are retained with parsed usages and hmt feature lookups are restricted to the selected touch collection; a composite keyboard+touchpad regression covers index separation. |
| FreeBSD `sys/dev/hid/hidbus.c` `hidbus_get_report/set_report/read/write/set_idle/set_protocol()` | `crates/ax/tk-axdriver/src/hidbus.rs`, `i2c_hid.rs` | Generic HID-bus dispatch helpers now delegate to the I2C-HID protocol client for report/control operations; bus softc, child device dispatch and newbus registration are replaced by direct TheKernel methods. |
| FreeBSD `sys/dev/hid/hidbus.c` `hidbus_is_collection()` / `sys/dev/hid/hmt.c` `hmt_hid_parse()` | `crates/ax/tk-axdriver/src/{hidbus.rs,hmt.rs,usb/hid_report.rs}` | Parser records collection usages/TLC index; hmt selects the matching touch collection and uses its collection-scoped required-feature/report locations. |
| FreeBSD `sys/dev/hid/hmt.c` Contact Count Maximum feature path | `crates/ax/tk-axdriver/src/i2c_hid.rs` | Feature GET_REPORT now uses the exact selected Report ID byte length while retaining existing transfer-length validation. |
- OpenBSD `sys/dev/acpi/pchgpio.c` rev 1.19 (ISC), Copyright (c) 2020 Mark Kettenis and James Hastings; translated PCH GPIO group/device tables and pad/register algorithms into `kernel/src/acpi/pchgpio.rs` (license: `kernel/LICENSES/ISC.txt`).

| Linux `drivers/i2c/busses/` DesignWare controller register/bit definitions (the original comments do not pin an exact source file or revision) | `crates/ax/tk-i2c/src/reg.rs` | The `snarfed from linux` comments identify Linux as the source for individual bit/register values; the file contains no Linux driver functions, control-flow, or copied structure layout. Constants are independently named and used by TheKernel's I2C implementation. |
| Linux `drivers/hid/hid-input.c` (GPL-2.0-or-later) usage-to-input behavior | `crates/ax/tk-axdriver/src/usb/hid_usage.rs` | Linux is a behavior reference for selected HID usage-to-evdev mappings only. No Linux function, data structure, or table layout is copied; TheKernel's match-based mapping implementation is original and the HID usage identifiers are protocol facts. |

## 2026-10-09 G1/main merged excerpt baseline

Measured after merging `cb70f258` with the G1 display changes, using the same
scanner and Linux 7.2.3 tree. `crates/linux` and `kernel/src` retain the tuples
in the earlier reconciliation. `crates/ax` is now `(0,0,0,284,130,0,0)` at >=40
and `(18,15,4,407,249,0,0)` at >=25. The combined inventory includes the
BSD/ISC-source register/control matches documented by main as well as the MIT
i915 matches; hash identity alone does not classify their license or source.

## G2 closeout / merged-main excerpt measurement (2026-10-09)

After merging `origin/main` (`cb70f258`) with the opt-in GT source translations,
`scan_linux_excerpts.py` against the local Linux 7.2.3 tree measures the following
current whole-scope totals. These supersede earlier historical whole-scope
counts above; both branches' provenance and license records are retained.

| Scope | Threshold | Fenced lines / blocks / files | Outside fences / code | Marked blocks / markers |
|---|---|---|---|---|
| `crates/ax` | 40 | 0 / 0 / 0 | 408 / 399 | 0 / 0 |
| `crates/ax` | 25 | 18 / 15 / 4 | 1283 / 1268 | 0 / 0 |

The `crates/linux` and `kernel/src` scan totals are unchanged from the CI
baseline. Matching Rust expressions in the MIT i915 translations are counted,
not exempted; a scanner match is a measurement, not a license classification.
The feature remains default-off; compilation is not runtime acceptance.

G2 follow-up: `i915_mm_upstream.rs` translates Linux 7.2.3 MIT `i915_mm.c`
`sgt_pfn`, `remap_sg`, `remap_pfn`, `remap_io_mapping`, `remap_io_sg`; copyright
© 2014 Intel Corporation, full grant in `tk-intel-gt/LICENSE-MIT`. Native
`linux/{mm_native,shmem}.rs` are original LinuxKPI implementations over axmm,
axtask, the native allocator and axhal physical-address translation, not copied
Linux GPL MM/fs implementations. The C layout probe confirms the VMA manager
lock at offset 0, size 248, and CONFIG_TRANSPARENT_HUGEPAGE=n (no huge_mnt field).

G2 follow-up: Linux 7.2.3 MIT `gem/i915_gem_phys.c` all nine functions are in
`i915_gem_phys_upstream.rs` (copyright © 2014-2016 Intel Corporation);
`i915_gem_gtt.c` page prepare/finish and `i915_utils.c` VT-d predicate have
source-order owners. `intel_memory_region_upstream.rs` contains the MIT
initialization/memtest/type/name dependency functions from
`intel_memory_region.c` (copyright © 2019 Intel Corporation). Full MIT grants
are retained in the crate. The original LinuxKPI DMA adapter allocates native
DMA pages and uses requester-scoped tk-vtd mapping/retirement; unknown device
ownership refuses mapping, and no feature-enabled runtime path is installed.

### G2 LinuxKPI follow-up (2026-10-09)

The GPL Linux core implementations were not copied. Original LinuxKPI owners
map anonymous shmem/folio/file I/O to the native allocator and page registry,
MM/VMA/usercopy to `axmm::AddrSpace`, task context to axtask's preemption count,
DMA to axalloc and requester-scoped tk-vtd, hard-IRQ synchronization to axhal's
IRQ-boundary active counters, and reservation/fence completion to native locks,
reference counts and workqueue callbacks. No-swap writeback keeps pages dirty;
there is no kswapd and unsupported tracepoints emit no events. MM/device owner
registration remains explicit and is not installed in the product.

Additional Linux 7.2.3 MIT translations (grants retained in the crate):

| Source | Rust owner / translated scope |
|---|---|
| `gem/i915_gem_clflush.c`, `i915_sw_fence_work.c` | `i915_gem_clflush_upstream.rs`: all five clflush and eight fence-work C functions plus two header helpers; Linux-core DMA-fence/reservation APIs are original adapters |
| `gt/intel_engine_pm.c` | `intel_engine_pm_upstream.rs`: all ten C functions; breadcrumbs park/unpark header helpers in their existing owner |
| `gt/intel_reset.c` | `intel_reset_hw_upstream.rs`: Gen6/Gen8+ hardware-domain reset dependencies, GuC reset and GSC workarounds; selector explicitly limited to the admitted Gen12 target, not an implementation of Gen2–5 reset |
| `i915_cmd_parser.c` | `i915_cmd_parser_upstream.rs`: all 23 C functions and two command header helpers, including complete Gen7/Haswell/Gen9 command/register tables |
| `gt/uc/intel_guc_capture.c` | `intel_guc_capture_upstream.rs`: linked output-node/cache lifecycle, extraction, log processing and engine matching dependencies, not the entire file or runtime hookup |
| `i915_gpu_error.c` | `i915_gpu_error_upstream.rs`: needed capture/store/reset/disable entry points; coredump storage is a native GT-only adapter retaining real GuC nodes, not Linux's full display/VM/compression/debugfs snapshot |
| `i915_irq.c` | `linux/irq.rs`: `intel_synchronize_hardirq` entry mapped to the registered device's native vector |

The additional files retain Intel Corporation copyright notices (2008–2022 as
applicable). The measured `crates/ax` totals now supersede the merge snapshot:
threshold 40: `(0, 0, 0, 414, 405, 0, 0)`; threshold 25:
`(18, 15, 4, 1318, 1303, 0, 0)`, in the CI baseline's seven-column order.
Other scopes are unchanged. The feature remains default-off; compilation and
host tests do not constitute native hardware or runtime integration acceptance.

`intel_gt_requests_upstream.rs` is a complete source-order translation of all 15
function definitions in Linux 7.2.3 `drivers/gpu/drm/i915/gt/intel_gt_requests.c`
(MIT, Copyright © 2019 Intel Corporation). Its LinuxKPI additions are the
source-semantic fence wait dispatcher and relative jiffies rounding helper.

### GT preparation source-order translations

| Source | Rust owner / translated scope |
|---|---|
| `gt/intel_gt_buffer_pool.c` | `intel_gt_buffer_pool_upstream.rs`: all 11 functions; RCU reclamation, delayed work, intrusive-list and GEM object behavior retain source ordering. Its `__list_del_many()` helper follows `i915_list_util.h` (MIT, Copyright © 2025 Intel Corporation). |
| `gt/intel_gt_clock_utils.c` | `intel_gt_clock_utils_upstream.rs`: all 16 functions including Gen4–Gen11 frequency selection and interval conversions; `i9xx_fsb_freq()` is the companion `i915_freq.c` dependency. |
| `gt/intel_sseu.c` | `intel_sseu_upstream.rs`: all 26 C functions; the existing `intel_sseu_get_hsw_subslices()` definition remains in `intel_sseu_types_upstream.rs` to avoid duplicate ownership. |

`linux/seq_file.rs` implements the Linux 7.2.3 `seq_file` buffer-prefix,
`seq_printf`/`seq_write` count and overflow behavior as original LinuxKPI, using
the crate's typed C-format formatter. GPL `fs/seq_file.c` function bodies were
not copied.

`i915_freq_upstream.rs` translates the three MIT functions from Linux 7.2.3
`drivers/gpu/drm/i915/i915_freq.c` (Copyright © 2025 Intel Corporation), which
are required by the Gen4 clock-frequency branch in `intel_gt_clock_utils.c`.
`linux/firmware.rs` supplies a fail-closed `request_firmware_nowarn` adapter
backed by `tk-axdriver-base` rootfs firmware reads; request size is capped at
8 MiB, and ownership is released through the matching adapter.

`intel_wopcm_upstream.rs` translates all 10 functions from Linux 7.2.3
`drivers/gpu/drm/i915/gt/intel_wopcm.c` (MIT, Copyright © 2017-2019 Intel
Corporation), including Gen9 layout restrictions, locked-register verification,
and GuC/HuC capacity checks.

`intel_context_sseu_upstream.rs` translates all 3 definitions in Linux 7.2.3
`drivers/gpu/drm/i915/gt/intel_context_sseu.c` (MIT, Copyright © 2019 Intel
Corporation); it uses the source header's kernel-context request helper.

`intel_tlb_upstream.rs` translates all 6 definitions in Linux 7.2.3
`drivers/gpu/drm/i915/gt/intel_tlb.c` (MIT, Copyright © 2023 Intel Corporation);
its diagnostic path applies a monotonic five-second error rate limit.

`intel_gt_pm_irq_upstream.rs` translates all 8 functions in Linux 7.2.3
`drivers/gpu/drm/i915/gt/intel_gt_pm_irq.c` (MIT, Copyright © 2019 Intel
Corporation), retaining mask/update, repeated reset writes, and posting-read order.

`intel_gt_mcr_impl_upstream.rs` translates all 21 functions from Linux 7.2.3
`drivers/gpu/drm/i915/gt/intel_gt_mcr.c` (MIT, Copyright © 2022 Intel
Corporation); common steering and multicast behavior follows the source.

`linux/forcewake.rs` implements the Linux 7.2.3 i915 forcewake reference-count,
domain-selection, callback, and release-register behavior based on the MIT
`drivers/gpu/drm/i915/intel_uncore.c` implementation (Copyright © 2013 Intel
Corporation).

`intel_huc_fw_upstream.rs` translates all 6 functions from Linux 7.2.3
`drivers/gpu/drm/i915/gt/uc/intel_huc_fw.c` (MIT, Copyright © 2014-2019 Intel
Corporation), preserving firmware state transitions and HECI/PXP message layout.

`intel_guc_hwconfig_upstream.rs` translates all 7 functions from Linux 7.2.3
`drivers/gpu/drm/i915/gt/uc/intel_guc_hwconfig.c` (MIT, Copyright © 2022 Intel
Corporation), retaining KLV parsing, temporary VMA lifecycle, and failure order.

`intel_uc_upstream.rs` translates all 36 functions from Linux 7.2.3
`drivers/gpu/drm/i915/gt/uc/intel_uc.c` (MIT, Copyright © 2016-2019 Intel
Corporation), preserving uC policy selection, lifecycle, rollback, and suspend/resume ordering.

`intel_renderstate_upstream.rs` translates all 5 functions from Linux 7.2.3
`drivers/gpu/drm/i915/gt/intel_renderstate.c` (MIT, Copyright © 2014 Intel
Corporation). Gen6-Gen9 immutable render-state table data is not included because
this target is Gen12, where the upstream selector returns null.

`intel_gt_irq_upstream.rs` translates all 21 definitions in Linux 7.2.3
`drivers/gpu/drm/i915/gt/intel_gt_irq.c` (MIT, Copyright © 2019 Intel
Corporation); display interrupt dispatch remains a kernel-provided callback boundary.

`intel_guc_upstream.rs` translates all 38 functions from Linux 7.2.3
`drivers/gpu/drm/i915/gt/uc/intel_guc.c` (MIT, Copyright © 2014-2019 Intel
Corporation), including GuC parameter construction, PCI revision handling, MMIO/CT helpers, and suspend/auth lifecycle.

`intel_gt_pm_upstream.rs` translates all 20 functions from Linux 7.2.3
`drivers/gpu/drm/i915/gt/intel_gt_pm.c` (MIT, Copyright © 2019 Intel
Corporation); runtime-PM, display-power, RC6/RPS, request, and system-PM services remain explicit owner boundaries.

`gen8_ppgtt_upstream.rs` translates all 30 definitions from Linux 7.2.3
`drivers/gpu/drm/i915/gt/gen8_ppgtt.c` (MIT, Copyright © 2020 Intel
Corporation), retaining source-order page-table allocation, insertion, and
cleanup. The target build disables GVT and the i915 PPGTT selftests; no
GVT-specific locking behavior is claimed.

`shmem_utils_upstream.rs` translates all 8 definitions from Linux 7.2.3
`drivers/gpu/drm/i915/gt/shmem_utils.c` (MIT, Copyright © 2020 Intel
Corporation). `linux/vm.rs` supplies its vmap/vfree dependencies through axmm
and fails closed until the kernel owner installs an acknowledged global TLB
shootdown callback; it is not a runtime-ready vmap path by itself.

`i915_gem_busy_upstream.rs` translates all 6 definitions from Linux 7.2.3
`drivers/gpu/drm/i915/gem/i915_gem_busy.c` (MIT, Copyright © 2014-2016 Intel
Corporation), including reservation restart handling and the engine-class uABI
busy-bit encoding.

`i915_gem_internal_upstream.rs` translates all 5 definitions from Linux 7.2.3
`drivers/gpu/drm/i915/gem/i915_gem_internal.c` (MIT, Copyright © 2014-2016
Intel Corporation), preserving page-allocation fallback and SG/object cleanup.

`i915_gem_wait_upstream.rs` translates all 12 definitions from Linux 7.2.3
`drivers/gpu/drm/i915/gem/i915_gem_wait.c` (MIT, Copyright © 2016 Intel
Corporation), including request prioritization and reservation wait order.

`i915_gem_create_upstream.rs` translates all 13 definitions from Linux 7.2.3
`drivers/gpu/drm/i915/gem/i915_gem_create.c` (MIT, Copyright © 2020 Intel
Corporation), preserving placement selection, user-extension validation, and
GEM handle publication/error order.

`i915_gem_throttle_upstream.rs` translates the sole definition in Linux 7.2.3
`drivers/gpu/drm/i915/gem/i915_gem_throttle.c` (MIT, Copyright © 2014-2016
Intel Corporation); context/engine/timeline locks and wait ordering follow the
source path.

`i915_gem_pm_upstream.rs` translates all 9 definitions from Linux 7.2.3
`drivers/gpu/drm/i915/gem/i915_gem_pm.c` (MIT, Copyright © 2019 Intel
Corporation), preserving suspend/freeze/resume order and TTM calls behind the
upstream local-memory-region type checks.

`i915_gem_dmabuf_upstream.rs` translates all 12 actual C functions in Linux
7.2.3 `drivers/gpu/drm/i915/gem/i915_gem_dmabuf.c` (MIT, Copyright 2012 Red
Hat Inc); ctags reports one additional `I915_SELFTEST_DECLARE` macro pseudo-tag.
The `dma_buf` ABI views and Linux PRIME/DMA mappings keep
their source layout; generic DMA-BUF, DMA map, and VMA services remain external
LinuxKPI/kernel ownership boundaries.

`i915_gem_object_frontbuffer_upstream.rs` translates all 12 functions from
Linux 7.2.3 `drivers/gpu/drm/i915/gem/i915_gem_object_frontbuffer.c` (MIT,
Copyright © 2025 Intel Corporation). The frontbuffer interface preserves the
display-owned init/fini/flush/invalidate calls as external owner boundaries.

`i915_getparam_upstream.rs` translates the sole definition in Linux 7.2.3
`drivers/gpu/drm/i915/i915_getparam.c` (MIT; its source header has no
copyright line), preserving the complete 59-parameter switch and checked
userspace result write.

`i915_gem_stolen_upstream.rs` translates all 44 definitions from Linux 7.2.3
`drivers/gpu/drm/i915/gem/i915_gem_stolen.c` (MIT, Copyright © 2008-2012 Intel
Corporation), preserving platform-specific stolen-memory discovery, reserved
region handling, and GEM object lifetime branches. `linux/iomapping.rs` supplies
the configured x86 WC mapping with PAT1 readiness, reserved-range admission,
and acknowledged TLB retirement; it fails closed until the kernel owner installs
the shared-map shootdown callback.

`i915_query_upstream.rs` translates all 16 definitions from Linux 7.2.3
`drivers/gpu/drm/i915/i915_query.c` (MIT, Copyright © 2018 Intel Corporation),
including topology, engine, memory-region, HWConfig, GuC submission-version,
and PERF query paths. PERF ownership calls remain real service dependencies;
the separate `i915_perf.c` translation is deferred to the end of the task.

`i915_gem_evict_upstream.rs` translates all 9 actual definitions in Linux
7.2.3 `drivers/gpu/drm/i915/i915_gem_evict.c` (MIT, Copyright © 2008-2010
Intel Corporation). Ctags reports the same number of entries but one is an
`I915_SELFTEST_DECLARE` data pseudo-tag and it misses the actual `dying_vma()`;
all nine function bodies are translated and only function bodies carry markers.

`i915_vma_upstream.rs` translates all 74 source definitions in Linux 7.2.3
`drivers/gpu/drm/i915/i915_vma.c` (MIT, Copyright © 2016 Intel Corporation).
There are 73 unique function names because the mutually exclusive
`vma_print_allocator()` preprocessor variants are both retained and marked in
their original source order.

`intel_guc_rc_upstream.rs` translates all 7 definitions from Linux 7.2.3
`drivers/gpu/drm/i915/gt/uc/intel_guc_rc.c` (MIT, Copyright © 2021 Intel
Corporation), using the existing GuC CT action transport and preserving RC
support/selection checks.

`intel_guc_upstream.rs` translates all 38 functions from Linux 7.2.3
`drivers/gpu/drm/i915/gt/uc/intel_guc.c` (MIT, Copyright © 2014-2019 Intel
Corporation), including GuC parameter construction, PCI revision handling, MMIO/CT helpers, and suspend/auth lifecycle.

`intel_gt_pm_upstream.rs` translates all 20 functions from Linux 7.2.3
`drivers/gpu/drm/i915/gt/intel_gt_pm.c` (MIT, Copyright © 2019 Intel
Corporation); runtime-PM, display-power, RC6/RPS, request, and system-PM services remain explicit owner boundaries.

### GP GT/GEM/power coverage provenance (Linux 7.2.3)

The following MIT source files were completed for the default-off upstream-gt
preparation. Copyright notices are retained in each Rust owner; GPL DRM/MM
framework bodies are not copied. The i915_irq.c owner additionally retains the
complete Tungsten Graphics permission and warranty text.

| Upstream source | Rust owner | Scope / source copyright |
|---|---|---|
| drivers/gpu/drm/i915/gt/intel_gt.c | intel_gt_upstream.rs | 42/42 functions; MIT, Copyright © 2019 Intel Corporation |
| drivers/gpu/drm/i915/gt/intel_ggtt.c | intel_ggtt_upstream.rs | 74/74 functions; MIT, Copyright © 2020 Intel Corporation |
| drivers/gpu/drm/i915/gt/intel_gtt.c | intel_gtt_upstream.rs | 33/33 functions; MIT, Copyright © 2020 Intel Corporation |
| drivers/gpu/drm/i915/gt/intel_ppgtt.c | intel_ppgtt_upstream.rs | 18/18 functions; MIT, Copyright © 2020 Intel Corporation |
| drivers/gpu/drm/i915/gt/intel_mocs.c | intel_mocs_upstream.rs | 15/15 functions; MIT, Copyright © 2015 Intel Corporation |
| drivers/gpu/drm/i915/gt/uc/intel_uc_fw.c | intel_uc_fw_upstream.rs | 38/38 functions; MIT, Copyright © 2016-2019 Intel Corporation |
| drivers/gpu/drm/i915/gt/uc/intel_guc_ct.c | intel_guc_ct_upstream.rs | 44/44 functions; MIT, Copyright © 2016-2019 Intel Corporation |
| drivers/gpu/drm/i915/gt/uc/intel_guc_ads.c | intel_guc_ads_upstream.rs | 40/40 functions; MIT, Copyright © 2014-2019 Intel Corporation |
| drivers/gpu/drm/i915/gt/uc/intel_huc.c | intel_huc_upstream.rs | 29/29 functions; MIT, Copyright © 2016-2019 Intel Corporation |
| drivers/gpu/drm/i915/gem/i915_gem_execbuffer.c | i915_gem_execbuffer_upstream.rs | 90/90 functions; MIT, Copyright © 2008, 2010 Intel Corporation |
| drivers/gpu/drm/i915/gt/intel_rc6.c | intel_rc6_upstream.rs | 30/30 functions; MIT, Copyright © 2019 Intel Corporation |
| drivers/gpu/drm/i915/gt/intel_rps.c | intel_rps_upstream.rs | 134/134 functions; MIT, Copyright © 2019 Intel Corporation |
| drivers/gpu/drm/i915/gt/intel_reset.c | intel_reset_upstream.rs | 67/67 functions; MIT, Copyright © 2008-2018 Intel Corporation |
| drivers/gpu/drm/i915/i915_irq.c | i915_irq_upstream.rs | 13/55 selected Gen11+/DG1 GT/top-level functions per task scope; MIT permission text retained; Copyright © 2003 Tungsten Graphics, Inc. |

The intel_gt.c body and source-location markers are present in the feature owner.

### Whole-crates/ax Linux excerpt re-scan (2026-10-10)

After registering the GT/GEM/power source-order translations, the scanner's
whole-scope crates/ax totals are threshold >=40 `(0,0,0,763,589,0,0)` and
threshold >=25 `(18,15,4,1988,1808,0,0)` in the test's seven-column order.
The corresponding tk-intel-gt NOTICE and CI scan baseline were reconciled in
the same change; MIT i915 text remains counted rather than exempted.
`crates/ax/tk-intel-gt/src/intel_guc_log_upstream.rs`: partial port of Linux 7.2.3 `drivers/gpu/drm/i915/gt/uc/intel_guc_log.c` (MIT, Copyright © 2014-2019 Intel Corporation): section sizing, log vma create/destroy, default level, init_early and flush-event dispatch; relay/debugfs channel omitted (fail-closed flush work).
`crates/ax/tk-intel-gt/src/gt_header_inline_upstream.rs`: C-ABI wrappers for i915 header `static inline` helpers (`i915_request.h`, `gt/intel_engine_pm.h`, `i915_gpu_error.h`, `gt/intel_workarounds.h`, `gt/uc/intel_guc_ct.h`, `gt/uc/intel_guc.h`), MIT, Copyright © 2019 Intel Corporation, bodies per header text.
