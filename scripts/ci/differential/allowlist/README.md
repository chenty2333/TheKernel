# Differential allowlists

Empty by default. An allowlist documents a known host-kernel divergence for
one differential case; it is the ONLY way a runner may downgrade a missing or
failed marker, and every application is recorded in the run's `receipt.json`
under `allowlist_applied`. Do not add entries without a linked justification.

## Schema

`<case>.json` is a JSON array of entries:

```json
[
  {
    "marker": "THEKERNEL_PACKET_SEND_FLAGS_OK",
    "kernel_range": ">=6.1 <6.9",
    "reason": "MSG_CONFIRM acceptance changed in torvalds/linux@<commit>; see issue #NNN"
  }
]
```

- `marker`: the exact manifest line being waived.
- `kernel_range`: space-separated clauses that must ALL match the reference
  kernel's `uname -r` numeric prefix. Each clause is
  `(>=|<=|==|>|<)?MAJOR[.MINOR[.PATCH]]`; a bare version means `==`.
  Examples: `>=6.1 <6.9`, `<5.15`, `6.6`.
- `reason`: why the divergence is acceptable, with a reference (upstream
  commit, TheKernel issue) a reviewer can follow.

An entry waives its marker only when the range matches; unmatched entries are
inert. Malformed allowlists are hard runner errors, never silent skips.
