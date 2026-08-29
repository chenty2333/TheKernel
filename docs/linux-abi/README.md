# Linux x86_64 ABI evidence matrix

This directory is the reviewable syscall/UAPI coverage ledger for native x86_64.
Its Linux baseline is **v6.12.103**, commit
`25c09b42358e73e1476e517b296edb6344f2e4bd`.  It uses only `common` and
`64` rows from the checked-in `syscall_64.tbl` snapshot; x32 is deliberately
excluded.  The snapshot SHA-256 is `980ce3115028c71c5618e7864d262017bde8103bcfe7b413147a14fd312c92ac`.

`syscall-matrix.json` has one row for every one of the 375 selected Linux
syscalls.  Facts (`nr`, `name`, `entry`) are generated from the snapshot.
Every semantic field is an explicit review decision: unknown is the conservative
default and is not evidence of implementation. Linux entry symbols and a
dispatcher match alone must never be recorded as coverage.

Matrix schema v2 records the native Linux route, dispatch classification,
disposition, handler, UAPI family, implementation root, contract IDs, four
evidence lanes, gap IDs, review-evidence IDs, and review state. The route and
static metadata are checked against `static-inventory.json`. Evidence IDs must first be added to
`evidence-catalog.json`. Catalog v2 has `static-audit`, `host-unit`,
`host-linux-differential`, and `guest-KTAP` lanes; every record names a real
repository-relative source and SHA-256 and is `pass` or `not-applicable`
(which requires a reason). Differential evidence declares its command and
markers, while KTAP evidence declares its case name, plan size, and markers.
Every row is now a completed static review: the two eventfd entries and
`creat` are implemented, the 17 Linux `sys_ni_syscall` entries are
explicit-ENOSYS through their own dispatcher arm, and the remaining 355 rows
are conservatively partial. The Gate 0 matrix therefore reports
`reviewed=375`, `resolved=20`, `implemented=3`, `explicit-enosys=17`,
`partial=355`, and `unknown=0`. `gap-catalog.json` is authoritative for partial
gaps: every referenced
ID must exist, declare that it applies to `partial`, and provide a concrete
description. Partial rows always carry the unresolved dynamic contract/evidence
gap, plus applicable dispatcher fallback/feature and UAPI-family gaps. A
TheKernel unsupported fallback is never treated as Linux native NI evidence.

The validator computes `reviewed` and `resolved` totals. A reviewed row has a
non-unknown disposition, no placeholder handler/family/root metadata, and
non-empty review evidence. A partial row needs a gap; implemented and
explicit-ENOSYS rows name none. The 17 `sys_ni_syscall` native entries are
exactly the explicit-ENOSYS rows. A row is resolved only when it is reviewed,
implemented or explicit-ENOSYS, matches static inventory, and has reviewed
syscall-matching contracts, static-audit review evidence, and passing evidence
in all four matrix lanes. Contract v2 cells explicitly list their syscall
members, subject, review state, required lanes, and evidence.

Three additional manifests keep later closure work finite and invalidatable:

- `closure-cohorts-v1.json` freezes the 17 native-NI, 271 Phase 2, and 87
  Phase 3 rows, syscall-table order, and six permitted alias groups.
- `uapi-surfaces-v1.json` and `exposure-inventory-v1.json` bind syscall-owned
  flags, commands, structures, state edges, and exposed object classes to
  source hashes. A changed exposure hash identifies the rows that must reopen.
- `conditional-syscalls-v1.json` fixes the independent 162-member Linux
  `COND_SYSCALL` intersection. All 162 positive-witness records remain
  explicitly unresolved until both product routing and a feature/hardware
  profile are proved.

## Four-phase closure gates

The executable first-phase gate is `python3 tools/abi_matrix.py phase1-gate`:
all 375 rows must be reviewed and `unknown` must be zero.  This gate is already
satisfied; it establishes the real implementation map without claiming that a
static dispatch arm is behaviorally complete.

Phase 2 closes low-complexity partial rows one vertical at a time, beginning
with high-use filesystem, task/process, memory, signal, and network calls.  A
vertical is closed only after its contract and all four evidence lanes pass;
`creat(2)` is the first such implementation slice closed through that process.
Phase 3 applies the same rule to mount/namespaces, AIO/io_uring,
perf, security, and administrative interfaces.  Regeneration must not convert
a partial row merely because a dispatcher arm exists.

The executable terminal gate is `python3 tools/abi_matrix.py final-gate`.  For
this pinned Linux table its exact target is `reviewed=resolved=375`,
`implemented=358`, `partial=unknown=0`, and `explicit-enosys=17`.  The 17 are
Linux's own native `sys_ni_syscall` slots and therefore count as resolved ABI
coverage; requiring `implemented=375` and `explicit-enosys=0` would disagree
with the selected Linux ABI rather than improve compatibility.

## Fixed Linux oracles

`oracle-configs.json` binds the product and feature-witness q35 kernels to the
pinned Linux tarball, a checked-in minimal product defconfig seed, the expanded
config hashes, a fixed Kbuild user/host/timestamp/version identity, and the
resulting bzImage hashes.  Put the recorded tarball at
`.state/linux-6.12.103/linux-6.12.103.tar.xz`, then run the materializer inside
the repository development image:

```bash
./scripts/dev-shell.sh -- \
  python3 scripts/ci/materialize_linux_oracles.py
python3 scripts/ci/verify_linux_oracles.py --require-materialized
python3 scripts/ci/materialize_linux_uapi.py
python3 tools/abi_uapi.py --require-materialized
```

The default materialization command accepts either an empty output state or a
complete hash-matching prior state, rebuilds both kernels in separate staging
directories, and refuses to publish hash drift.  Use `--update-manifest` only
when deliberately accepting newly audited config and artifact hashes.  The
checked-in development image includes `bc`, `bison`, `flex`, and the libelf
headers required by the Linux build.

`uapi-headers.json` separately binds the 1,000-file x86_64 userspace-header
tree produced by Linux `headers_install`. Formal rootfs builds set
`THEKERNEL_ABI_UAPI_INCLUDE=.state/linux-6.12.103/uapi/include`; only the
portable ABI cases receive that include path. The tree digest is installed in
the rootfs and published beside the exact case binaries. An ordinary rootfs
build leaves both metadata files as `unbound`, which the formal runner rejects.

## Runtime differential receipts

`abi-cases.json` declares raw-syscall binaries, required markers, targets, and
pinned Linux oracle identities. `tools/abi_runner.py` boots the same rootfs and
exact case binaries under the pinned Linux product kernel and TheKernel,
rejects FAIL/SKIP/panic/timeout/non-clean shutdown, and atomically publishes a
run-group only when both targets pass. Each receipt captures the runtime commit
and tree of all three clean checkouts, closure-input hashes, binary, rootfs,
kernel, QEMU launch receipt, topology, command line, transcript, exit status,
shutdown state, and the pinned UAPI tree and rootfs/binary metadata hashes. The
manifest contains no self-referential TheKernel commit. Before the differential
run, the runner also requires the successful clean-source system-test launch
receipt that produced the exact TheKernel kernel, ESP, and rootfs. Case resource
profiles determine QEMU CPU, memory, and total timeout; incompatible profiles
cannot be combined in one run group.

The checked-in `evidence/eventfd-systemtest.txt` is a regression transcript for
the matrix validator, not a formal source receipt. Gate 0 acceptance additionally
requires CI's runtime `tools/abi_runner.py` step and its generated clean-source
run group. A changed current commit therefore invalidates runtime evidence
without creating a self-referential checked-in manifest.

The q35 platform currently binds Debian bookworm's OVMF ECAM layout. Implicit
firmware selection accepts only the pinned OVMF code and vars hashes and fails
before launch on an incompatible host image. Formal runs use the repository
development image; explicit paths and environment overrides must match the
same hashes and their bytes appear in the QEMU receipt.

## Maintenance

1. Update the checked-in Linux table only when intentionally moving the fixed
   baseline, and update every baseline field and its SHA-256 together.
2. Run `python3 tools/abi_matrix.py regenerate` to refresh factual rows while
   preserving review fields by syscall name.
3. Review dispositions, handlers, UAPI families, implementation roots, gaps,
   contracts, and review evidence in the generated matrix. Do not infer them
   from syscall-dispatch branches.
4. Add evidence records to the catalog before referencing them, then run
   `python3 tools/abi_cases.py`, `python3 tools/abi_cohorts.py`,
   `python3 tools/abi_conditions.py --validate`,
   `python3 tools/abi_surfaces.py validate`,
   `python3 tools/abi_matrix.py validate`, and the applicable phase gate.

The validator rejects source/matrix baseline drift, reviewed inventory
disagreement, duplicates, missing syscall rows, invalid enum values,
ENOSYS-set drift, unknown or inapplicable gap IDs, invalid review/gap combinations, cross-bound contracts,
source hash drift, generic KTAP plans, and evidence references absent from the
catalog.
