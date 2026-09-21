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
(`mm/userfaultfd.c:3659-3661`) and `net/src/lib.rs:332` (`net/socket.c:1947`)
were both re-read against the reference tree and are right — `-EINVAL` before
`vma_can_userfault()`, and `move_addr_to_kernel()` inside `__sys_bind()`. The
third, `kernel/src/syscall/fs/io_uring.rs:773`, is the callee-body case named
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
estimate: **89 verbatim Linux lines in 43 fenced blocks across 19 files at
≥ 40**, **166 lines / 53 blocks / 25 files at ≥ 25**. By subtree, where `files`
counts any category and so includes comment-only files:

| Subtree | ≥ 40 fenced lines / blocks / files | ≥ 25 fenced lines / blocks / files | ≥ 40 outside fences: doc / `//` / `code` |
| --- | --- | --- | --- |
| `syscall/` | 65 / 31 / 29 | 116 / 37 / 34 | 7 / 108 / 8 |
| `task/` | 9 / 5 / 4 | 16 / 5 / 5 | 0 / 1 / 0 |
| `mm/` | 9 / 3 / 2 | 14 / 4 / 3 | 0 / 0 / 0 |
| `mounts.rs` | 5 / 3 / 1 | 13 / 3 / 1 | 0 / 3 / 0 |
| `file/` | 0 / 0 / 2 | 3 / 1 / 4 | 0 / 5 / 0 |
| `drm/` | 1 / 1 / 1 | 2 / 2 / 2 | 0 / 0 / 0 |
| `time.rs` | 0 / 0 / 0 | 2 / 1 / 1 | 0 / 0 / 0 |
| `bpf/` | 0 / 0 / 0 | 0 / 0 / 1 | 0 / 0 / 2 |
| **`kernel/src` total** | **89 / 43 / 19** | **166 / 53 / 25** | 132 lines, of which 8 `code` |

Three things about that table need stating plainly. **Not one** of the 43 blocks
(53 at ≥ 25) carries an `Excerpt:` marker, while `crates/linux` puts 18 of 49 in
that form: kernel-side citation is uniformly weaker. The ≥ 40 outside-fence count
is **132 with the ASCII-rule filter and 234 without it**, so 102 of the raw
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
