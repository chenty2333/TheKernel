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

Each row records a dispatch classification (`unknown`, `dispatch-arm`, `alias`,
`feature`, or `fallback`), disposition (`implemented`, `partial`,
`explicit-enosys`, or `unknown`), handler, UAPI family, contract IDs, three
evidence lanes, and review state. Evidence IDs must first be added to
`evidence-catalog.json`; empty lanes reserve the row for future vertical
tests.

## Maintenance

1. Update the checked-in Linux table only when intentionally moving the fixed
   baseline, and update every baseline field and its SHA-256 together.
2. Run `python3 tools/abi_matrix.py regenerate` to refresh factual rows while
   preserving review fields by syscall name.
3. Review dispositions, handlers, UAPI families, contracts, and evidence in the
   generated matrix. Do not infer them from syscall-dispatch branches.
4. Add evidence records to the catalog before referencing them, then run
   `python3 tools/abi_matrix.py validate`.

The validator rejects source/matrix baseline drift, duplicates, missing syscall
rows, invalid enum values, unknown contract IDs, and evidence references absent
from the catalog.
