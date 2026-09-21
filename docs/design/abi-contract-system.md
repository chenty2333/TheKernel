# The ABI contract system: the ledger, the gates, and what "verified" may mean

`config/linux-contracts.toml` is the only machine-checked statement this
repository makes about what TheKernel claims regarding the Linux 7.2.3 syscall
interface: 385 syscall cells, 368 contract records, 9 465 hand-maintained lines.
This document is the specification of that ledger and of the three things that
enforce it — `scripts/ci/linux_abi_gate.py`,
`scripts/ci/check_abi_contracts.py` and the KVM differential behind
`tools/thekernel.py test --suite abi` — plus the four shrink-only baselines that
keep the ledger from quietly re-widening the claims it has already narrowed.

It is written against the tree as it stands; every statement carries the
`path:line` it was read from.

## 1. Files and their roles

| File | Role |
| --- | --- |
| `config/linux-abi.toml` | the pinned release (`repository`, `tag`, `table`) and the independent routing inventory (`config/linux-abi.toml:5-17`), with terminal counts at `:19-22` |
| `config/linux-contracts.toml` | the ledger: `[[contract]]` semantics, `[[cell]]` claims, `[progress]`, `[ratchet]` |
| `scripts/ci/linux_abi_gate.py` | the static enforcer: materializes the release, cross-checks dispatch routing, validates the ledger, pins the shape of all four baselines and (under `--final`) applies three of them |
| `scripts/ci/check_abi_contracts.py` | the other static half: the differential registry vs its guest C sources, both directions, plus ledger→runner program bindings and the fourth baseline |
| `tools/qemu_runner/abi_differential.py` | the runtime authority: what asserts exist, what each guest must print, and the comparison |

The ledger's top level is closed: the gate rejects the file unless its key set
is exactly `{"schema", "linux_manifest", "progress", "ratchet", "contract",
"cell"}` and `schema == 3` (`scripts/ci/linux_abi_gate.py:877-878`), and it must
name the pinned manifest
(`scripts/ci/linux_abi_gate.py:879-880`, against
`config/linux-contracts.toml:6-7`).

## 2. The contract record: what a syscall is specified to do

A `[[contract]]` binds exactly ten keys
(`scripts/ci/linux_abi_gate.py:271-274`): `id`, `flags`, `structs`,
`multiplexer_commands`, `provider_ioctls`, `errno_order`, `usercopy`, `state`,
`concurrency`, `teardown`. The nine after `id` are *graph fields*: each must be a
non-empty list of non-empty strings
(`scripts/ci/linux_abi_gate.py:762-764`), each item must start with that
field's grammar prefix — `flag:`, `struct:`, `mux:`, `ioctl:`, `errno:`,
`usercopy:`, `state:`, `concurrency:`, `teardown:`
(`scripts/ci/linux_abi_gate.py:759,767-770`) — and must carry content after the
prefix. `["explicit-none"]` is legal only alone
(`scripts/ci/linux_abi_gate.py:768-769`). The vocabulary of non-answers is
banned outright, by regex, over every item
(`scripts/ci/linux_abi_gate.py:765-767`: `contract-defined`,
`Linux syscall-specific`, `todo`, `tbd`, `unknown`, `generic`,
`handler-defined`).

The gate calls that prefix map `GRAPH_PREFIX`
(`scripts/ci/linux_abi_gate.py:759`), and the marker cross-check in
`scripts/ci/check_abi_contracts.py:126-166` already treats a contract and its
guest program as two views of one symbol set. Nothing today parses the prefixes
into an actual graph, so the shape is a promise to a later consumer rather than
a used mechanism.

`config/linux-contracts.toml:622-631` (`linux-landlock_create_ruleset`) is the
reference shape for a fully specified record: the flag item names both ABI 10
create flags and the conditions each is legal under, the struct item names the
48-byte six-`u64` `landlock_ruleset_attr` *and* the minimum accepted size, the
usercopy item names the zero-fill/non-zero-extension rule, and the errno item
is an ordered sentence over that record's own symbols.

## 3. The cell record: what this kernel claims

A `[[cell]]` binds exactly nine fields
(`scripts/ci/linux_abi_gate.py:275`): `number`, `name`, `status`, `contract`,
`handler`, `conditional`, `tests`, `validation_gaps`, `limitations`.

- `number`/`name` must exist in the pinned release's syscall table and agree
  with each other, and no number may appear twice
  (`scripts/ci/linux_abi_gate.py:870-871`; table path
  `config/linux-abi.toml:8`).
- `status` ∈ {`implemented`, `explicit-enosys`, `partial`}
  (`scripts/ci/linux_abi_gate.py:872`).
- `handler` is `path.rs:fn_name`. It must be one of the dispatch-call bindings
  the gate has reviewed (`scripts/ci/linux_abi_gate.py:293-684`), the actual
  `kernel/src/syscall/dispatch.rs` arm for that syscall must call it
  (`scripts/ci/linux_abi_gate.py:977-980`), and `conditional` must equal the
  arm's `cfg(feature = ...)` profile exactly
  (`scripts/ci/linux_abi_gate.py:981-985`).
- `tests` entries are `path:symbol`; the symbol must be a real function in that
  file and, for `.rs`, must carry `#[test]`
  (`scripts/ci/linux_abi_gate.py:918-931`). Only `.rs` and `.c` are accepted
  (`scripts/ci/linux_abi_gate.py:932-933`).
- `[progress]` (`config/linux-contracts.toml:9-16`) is recomputed from the cells
  and compared
  (`scripts/ci/linux_abi_gate.py:945-956`).
  Editing the counts cannot make anything pass; changing a status forces the
  counts to be re-derived in the same edit.

## 4. What makes a cell "verified" — and what does not

The three statuses are not a quality ranking; they are statements about which
of two independent fields is allowed to carry content.

`limitations` and `validation_gaps` each must be either `["explicit-none"]` or
real prose (`scripts/ci/linux_abi_gate.py:879-884`), and the two statuses pin
them in opposite directions:

- `implemented` may not carry `limitations`
  (`scripts/ci/linux_abi_gate.py:885-886`). A known behavioural divergence from
  Linux therefore *must* be `partial`, and `partial` must say what it is
  (`scripts/ci/linux_abi_gate.py:887-888`). This is why cells 9 (`mmap`) and 204
  (`sched_getaffinity`) are `partial` (`config/linux-contracts.toml:797,7145`)
  rather than `implemented` with an explanatory note.
- `validation_gaps` is about *evidence*, not behaviour. A cell with no `tests`
  must declare a gap (`scripts/ci/linux_abi_gate.py:908-909`).

The load-bearing case is `implemented` **plus**
`validation_gaps = ["explicit-none"]` — 213 of the 299 `implemented` cells today.
That pair claims "the contract holds and current guest behaviour proves it", so
the gate demands the claim be backed by an actually registered differential
case: it imports the runtime registry, looks the syscall number up in
`SYSCALL_CASES`, and requires the program to be registered, the case to be in
`CONTRACTS`, and the cell to bind
`tests/guest/portable/<program>-differential.c:main`
(`scripts/ci/linux_abi_gate.py:936-941`, registries at
`tools/qemu_runner/abi_differential.py:25,337,378,402,444`).

Everything weaker is *static declaration with runtime debt*, and the gate says
so out loud:

```
linux-abi static declarations: implemented-with-gaps=86; runtime execution is not established by this gate
```

(`scripts/ci/linux_abi_gate.py:1000-1002`.) Those 86 cells, plus the 27 with an
empty `tests` array, are 100 distinct cells — but `--final` today tolerates 208
enumerated ones, because the remaining 108 are `implemented` cells whose
contract record still carries the §6 ordering placeholder (§7).

`explicit-enosys` is verified differently and more strictly: the cell's handler
must be `kernel/src/syscall/dispatch.rs:sys_ni_syscall`, its name must appear in
the parsed NI arms, and its dispatch expression must literally be
`sys_ni_syscall()` (`scripts/ci/linux_abi_gate.py:971-976`). On top of that,
`config/linux-abi.toml:10-17` is a second, hand-maintained, independent
statement of which numbers route to ENOSYS, and the two must agree
(`scripts/ci/linux_abi_gate.py:994-997`) — an agreement checked against the real
dispatch source by the `inventory` mode
(`scripts/ci/linux_abi_gate.py:263-268`).

## 5. The `Linux <path>:<line>` citation convention

A contract record cites the pinned release as

```
Linux <path-relative-to-release-root>:<line>
Linux <path>:<start>-<end>
```

recognised by `scripts/ci/linux_abi_gate.py:281`
(`Linux [A-Za-z0-9_./*-]+:[0-9]+(?:-[0-9]+)?`), and the convention is annotated
in the ledger header comment at `scripts/ci/linux_abi_gate.py:276-280`.
Examples in the ledger:
`config/linux-contracts.toml:623` (`Linux include/uapi/linux/landlock.h:93-96,
security/landlock/syscalls.c:219-233`), `:627`, `:629`, `:646-652`, `:669-675`.
Cell-level `limitations` use the same form
(`config/linux-contracts.toml:803`, `:7151`).

Why the release root and not the materialized tree: the gate's sparse checkout
contains only `arch/x86/entry/syscalls/syscall_64.tbl`
(`scripts/ci/linux_abi_gate.py:1019-1021`, `config/linux-abi.toml:8`), so
`Linux mm/mmap.c:1333` is a pointer into release v7.2.3
(`config/linux-abi.toml:6-7`) that a reviewer resolves against their own copy —
for instance against the full oracle tree the runtime path builds
(`scripts/build-linux-oracle.sh:144`). **The gate checks the citation's form,
not that the named lines say what the record claims they say.** That is a
deliberate, stated limit of this design; the reviewer is part of the mechanism.

The convention is enforced as a ratchet, because 365 of 368 records did not
meet it when it was introduced. `ratchet.uncited_contracts`
(`config/linux-contracts.toml:8690-9057`) enumerates every contract id with no
citation, and `record_ratchet()` fails on an uncited record outside that list
(`scripts/ci/linux_abi_gate.py:826-828`). The list may only shrink: once a
record is cited it should be deleted from it, and the gate prints the retirement
candidates rather than failing
(`scripts/ci/linux_abi_gate.py:832-842`). Today the three landlock records are
the only cited ones, and the baseline is exactly at reality:

```
linux-abi citation ratchet: 365 of 368 contracts remain (baseline 365, may not grow)
```

## 6. `ratchet.unreviewed_errno_order`: the placeholder that admits itself

177 contract records carried the identical sentence

```
errno:entry-specific flag, descriptor, pathname and provider admission; known ordering deviations are recorded in limitations
```

It is not a specification — it names no errno, no gate, and no order for the
entry it is attached to — and its escape clause is void for the interesting
cases: it defers to `limitations`, and an `implemented` cell must leave
`limitations` empty (`scripts/ci/linux_abi_gate.py:911-912`). For the gap-free
`implemented` claims, the sentence defers to nothing.

The fix is to make the sentence honest about itself rather than to invent
content. Every such record now carries

```
errno:unreviewed - no per-syscall Linux ordering comparison recorded against the pinned release
```

(`scripts/ci/linux_abi_gate.py:288`, `UNREVIEWED_ERRNO_ORDER`), and the ids that
still carry it are enumerated by `ratchet.unreviewed_errno_order`
(`config/linux-contracts.toml:9058-9236`), which may only shrink like the
citation list (`scripts/ci/linux_abi_gate.py:829-831`). A new contract that
copies the placeholder without joining the baseline fails, so the placeholder
cannot be the default for anything written from now on.

The two lists are then tied together so they cannot be gamed independently: a
record may not leave `uncited_contracts` while keeping the placeholder
(`scripts/ci/linux_abi_gate.py:844-846`). Citing a record is therefore not a
cosmetic find-and-replace — it obliges whoever cites it to also state the errno
ordering that was actually compared. And because an `implemented` cell is a
claim that the syscall matches Linux, `--final` refuses one whose record still
carries the placeholder (`:855-859`), which is why the not-final cell list went
from 100 to 208 when this landed. That inflation is the point: the ledger now
counts 108 cells whose `implemented` status rests on an ordering nobody
verified, instead of letting them read as final.

Writing 177 real ordering reviews was out of scope for this change, and the
alternative — a batch rewrite of 177 `errno_order` fields — was rejected: it
would have required inventing 177 ordering sentences without reading 177 entry
paths, replacing one dishonest form with 177 confident ones.

## 7. `ratchet.final_static_allowlist`

`--final` is the strength at which the ledger is checked against its own
promises. `final_static()` (`scripts/ci/linux_abi_gate.py:849-872`) recomputes
the debt set — every `implemented` cell with a declared gap or no test, plus
every cell with an empty `tests` array, plus every `implemented` cell resting on
a record that still carries the §6 placeholder (`:852-859`, identity
`"<number>:<name>"` from `:804-805`) — and fails if any of it is outside
`ratchet.final_static_allowlist` (`config/linux-contracts.toml:9237-9446`, 208
cells sorted by number), or if any syscall in the pinned table has no cell at
all (`scripts/ci/linux_abi_gate.py:862-867`):

```
final ABI static prerequisites incomplete: unknown=<n>, outside the final allowlist=[...]
```

The `unknown` term is why the allowlist cannot be used as a dumping ground:
adding a cell is never a way to pass `--final`, since an unreviewed number
fails and a reviewed one must carry a real status. As with the citation list,
cleaning a cell is rewarded by a shrink hint instead of a failure
(`scripts/ci/linux_abi_gate.py:868-872`).

All four baselines live in the registry rather than in separate files, and the
gate requires the table to declare exactly those four keys, each a
duplicate-free string array
(`scripts/ci/linux_abi_gate.py:292,779-804`) — so an allowlist entry cannot be
removed by deleting the table. The gate applies the three it can compute from
the ledger; `unbound_programs` needs the runner registry, so
`scripts/ci/check_abi_contracts.py` enforces that one (§8).

## 8. Static versus runtime: who checks what

Two verification stages, both in the default (daily) plan:

```python
Stage("linux-abi", "static", (sys.executable, "scripts/ci/linux_abi_gate.py", "all", "--final"), 900),
Stage("abi-contracts", "static", (sys.executable, "scripts/ci/check_abi_contracts.py"), 120),
```

(`tools/verification.py:72,81`; `--final` is rejected for any other mode,
`scripts/ci/linux_abi_gate.py:1034-1035`.) `all` runs `materialize`, `inventory`
and `schema` (`scripts/ci/linux_abi_gate.py:1037-1039`).

The runtime half is *not* in the daily plan: `test --suite abi` is KVM-only
(`tools/thekernel.py:1921-1922`) and appears only as the `abi-kvm` stage of the
hardware tier (`tools/verification.py:44-45`). On that path the gate runs first,
**with** `--final` (`tools/thekernel.py:1739-1740`), so the manual route and the
scheduled static stage now check the same strength, and then
`abi_test_cmd` builds the rootfs and kernel, resolves the oracle, and boots both
guests (`tools/thekernel.py:1920-1973`).

In CI this means every pull request runs the two static stages — `ci.yml`
computes `VERIFY_TIER` as `daily`/`full`
(`.github/workflows/ci.yml:42`) and runs `verify --tier "$VERIFY_TIER"`
(`.github/workflows/ci.yml:60`) — while the KVM comparison runs in the
`hardware` job, which is gated on a runner probe
(`.github/workflows/ci.yml:141`, `scripts/ci/check_hardware_runner.py`) and is
skipped unless that probe found an online idle KVM runner
(`.github/workflows/ci.yml:146`), then calls `verify --tier hardware`
(`.github/workflows/ci.yml:158`).

The probe is triggered by the nightly schedule as well as by an explicit
`tier=hardware` dispatch (`.github/workflows/ci.yml:121`), because the schedule
is otherwise the only unattended run the repository gets and its `full` tier has
no guest-ABI stage at all — without that trigger the two-guest differential would
only ever have run when somebody dispatched it by hand. The two triggers differ
in how they treat a missing runner: a dispatch asked for KVM evidence, so an
unusable runner pool fails the run, while the schedule takes it
opportunistically and records a skip (`THEKERNEL_HARDWARE_OPTIONAL`,
`.github/workflows/ci.yml:140`, `scripts/ci/check_hardware_runner.py:9-13,39-44`).

`check_abi_contracts.py` covers the drift the gate cannot see, because the gate
only demands a registered case from a cell that *claims* verified behaviour
(`scripts/ci/linux_abi_gate.py:936-941`). This script checks all of it
(`scripts/ci/check_abi_contracts.py:1-25`):

- every registry marker, case anchor and completion marker for a program must
  exist in that program's C source (`:172-208`), so a renamed guest assertion
  fails at the static tier instead of after a full oracle build and two boots;
- every marker the source emits through the assert protocol must be claimed by
  the registry (`:130-169`), so coverage the ledger does not account for cannot
  be hidden;
- every `-differential.c` bound by any `[[cell]]` must be a program the runner
  registers (`:211-241`, pattern at `:53`), i.e. no ledger runtime promise may
  point at a program `--suite abi` would never boot;
- and in the other direction, every registered program must be bound by a cell
  or named by `ratchet.unbound_programs` (`:242-261`), the fourth shrink-only
  baseline. Four programs are named there today — `console-integrity`,
  `tty-job-control`, `tty-termios`, `unix-write-credentials` — because no
  `[[contract]]` record yet describes console line atomicity, the tty ioctl
  vocabulary, or `SCM_CREDENTIALS`/`SO_PEERCRED`, so their evidence is real but
  attributed to nothing. A fifth registered program fails unless it is either
  bound or admitted (`--skip-ledger` at `:276-280`).

What the runtime then asserts is exact, not fuzzy. Each guest's serial log is
parsed into a `Counter` (`tools/qemu_runner/abi_differential.py:718`) and
compared against `expected_records(programs) + [COMPLETE_MARKER]`
(`tools/qemu_runner/abi_differential.py:705-715,755-757`): every
`THEKERNEL_ABI_CASE`/`ASSERT`/`RESULT` line, in the registry's own
assertion set, with no duplicates and nothing extra, each inside its own
watchdog interval (`tools/qemu_runner/abi_differential.py:732-750`), each
record inside its active case
(`tools/qemu_runner/abi_differential.py:759-772`).
Finally the two `Counter`s must be equal
(`tools/qemu_runner/abi_differential.py:862-863`) — the differential is against
the expected set *and* between the two guests. A case that passes on TheKernel
and fails on Linux cannot be encoded, which is why a divergence such as cell 204
has to be written down as a `limitations` string instead.

## 9. The Linux 7.2.3 oracle, and how it is pinned

Two different "sources of Linux" exist and must not be confused.

The **gate** materializes a sparse git checkout purely to read the syscall
table: `~/.cache/thekernel-targets/linux-v7.2.3`
(`scripts/ci/linux_abi_gate.py:24`), cloned `--depth=1 --filter=blob:none
--no-checkout` at the tag from `config/linux-abi.toml:6-7`, sparse-checked-out
to `arch/x86/entry/syscalls/syscall_64.tbl` only
(`scripts/ci/linux_abi_gate.py:1017-1022`). It refuses a tmpfs destination
(`:1016`), and requires a clean tree whose `HEAD` is exactly the tag's commit and
whose `origin` is the pinned repository (`:1023-1025`).

The **runtime** builds a bootable reference kernel:
`scripts/build-linux-oracle.sh` downloads `linux-7.2.3.tar.xz` from
`cdn.kernel.org` (`:8-10`) into
`${THEKERNEL_STATE_DIR:-$HOME/.cache/thekernel-targets}/linux-7.2.3-oracle`
(`:44`), under an exclusive `flock` (`:104-113`), verifies the archive against a
pinned `TARBALL_SHA256` (`:21`, checked at `:132`), verifies
`VERSION`/`PATCHLEVEL`/`SUBLEVEL` from the extracted `Makefile` (`:165-167`),
configures `config/linux/7.2.3-q35-graphics.config` (`:22`) over `defconfig`,
asserts the required `CONFIG_` settings before and after `olddefconfig`
(`:72-89`, `:96`, `:175`), refuses a `kernelrelease` other than `7.2.3`
(`:176-177`), and prints only the `bzImage` path on stdout
(`:178-180`) — which is how `abi_test_cmd` captures it
(`tools/thekernel.py:1938-1945`). The ESP for the reference boot is built with
`scripts/build-x86-uefi-esp.sh --mode linux` and
`config/x86_64/grub-linux-shell.cfg` (`tools/thekernel.py:1953-1958`).

`--linux-kernel PATH` reuses an already-built `bzImage`
(`tools/thekernel.py:2105`) and is mandatory with `--no-build`
(`tools/thekernel.py:1935-1937`). Both guests run the same generated command
script against a copy of the product shell rootfs
(`tools/qemu_runner/abi_differential.py:802-838`), into which every
`tests/guest/portable/*.c` is compiled automatically
(`scripts/build-rootfs.sh:339-345`) — there is no per-program registration in
the rootfs.

Only 10 of the 40 differential programs are additionally registered in the
TheKernel `guest` system suite (`tests/guest/system-init.c`), so the portable
programs' primary home is the two-guest comparison, not the KTAP suite.

## 10. Adding or changing a contract, end to end

1. Find the number in the pinned table and check its routing state in
   `config/linux-abi.toml:10-17`.
2. Write the `[[contract]]` with all nine fields, real grammar prefixes, and a
   `Linux <path>:<line>` citation for each claim
   (`config/linux-contracts.toml:622-631` is the model). A *new* uncited record
   fails immediately (`scripts/ci/linux_abi_gate.py:826-828`).
3. Write the `[[cell]]`. For `implemented`, the contract id must be
   `linux-<syscall name>` and may not be shared with another `implemented` cell
   (`scripts/ci/linux_abi_gate.py:900-901`).
4. Make the claim honest: `partial` + a concrete `limitations` sentence for any
   divergence; `implemented` only where there is none.
5. For a *verified* claim, register the differential case in the four runtime
   maps — `CONTRACTS` (`tools/qemu_runner/abi_differential.py:25`),
   `PROGRAM_CASES` (`:337`), `PROGRAM_SUCCESS` (`:402`) and `SYSCALL_CASES`
   (`:444`) — write the guest program at
   `tests/guest/portable/<program>-differential.c`, and bind
   `…-differential.c:main` in the cell.
6. Make sure the dispatch arm exists, its `cfg` matches `conditional`, and its
   handler symbol is dynamically validated against `dispatch.rs`
   (`scripts/ci/linux_abi_gate.py`).
7. Re-derive `[progress]` (`config/linux-contracts.toml:9-16`) if a status
   changed (`scripts/ci/linux_abi_gate.py:945-956`).
8. Shrink the baselines you have earned: delete a cited contract id from
   `ratchet.uncited_contracts`, a reviewed ordering sentence's id from
   `ratchet.unreviewed_errno_order`, a cleaned `number:name` cell from
   `ratchet.final_static_allowlist`, and a program you have bound to a cell from
   `ratchet.unbound_programs`. Adding to any of them is a review decision, and
   the gate will reject one that is merely convenient.
9. `python3 scripts/ci/linux_abi_gate.py all --final`,
   `python3 scripts/ci/check_abi_contracts.py`, and if the claim is runtime,
   `python3 tools/thekernel.py test --suite abi --smp 4 --accel kvm`.

## 11. Known limits, stated rather than implied

- The citation ratchet verifies form, not content (§5): a wrong-but-well-formed
  `Linux path:line` passes the gate. Only 3 of 368 records are cited today.
- The three ledger ratchets are enforced wherever the gate is invoked with
  `--final` — the `verify` tiers (`tools/verification.py:72`) and the manual
  `test --suite abi` path (`tools/thekernel.py:1740`) alike. Running
  `linux_abi_gate.py all` by hand without the flag checks none of them. The
  fourth baseline is enforced by `check_abi_contracts.py` instead, and 4 of the
  40 registered differential programs still sit in it (§8): they assert
  behavior no `[[contract]]` record describes.
- `--final` is a *static* prerequisite: passing it establishes that every
  remaining gap is enumerated and shrink-only, never that a guest ran anything
  (`scripts/ci/linux_abi_gate.py:1002,1005`).
- 100 of the 208 allowlisted cells (`:852-859`) carry runtime debt that only a
  KVM boot can retire (`tools/verification.py:44-45`); the other 108 rest on one
  of the 177 records that still carry the §6 placeholder, and 365 of 368 records
  are still uncited.
