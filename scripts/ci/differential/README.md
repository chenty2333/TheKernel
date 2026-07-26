# Linux-ABI differential-testing framework

This directory hosts the shared machinery for TheKernel's Linux-ABI
differential cases. A case is a portable C smoke program whose observable
behavior is first pinned against a host Linux kernel (the reference oracle)
and later replayed inside the TheKernel guest. The host runners here produce
auditable evidence: a bounded log, a `result.txt` verdict, and a
`receipt.json` conforming to `thekernel-differential-receipt-v0`.

Contents:

- `lib.sh` — shared bash library used by the host runners
  (`scripts/ci/seccomp-host-differential.sh`,
  `scripts/ci/packet-host-differential.sh`,
  `scripts/ci/futex-host-differential.sh`,
  `scripts/ci/epoll-host-differential.sh`, and
  `scripts/ci/signal-order-host-differential.sh`).
- `manifests/<case>.markers` — the exact marker set a passing run must emit.
- `allowlist/` — documented host-divergence waivers, empty by default.
- `validate-receipt.py` — schema validator for produced receipts.

## Authoring a case

### 1. Write the smoke program

- Location: `tests/guest/tools/<case>-smoke.c`. One file, C11, compiles with
  `cc -static -O2 -Wall -Wextra -Werror`, using only libc and Linux UAPI
  headers.
- The SAME binary must be meaningful on host Linux and in the TheKernel
  guest. No host-only or guest-only `#ifdef`s.
- Every check must assert a SPECIFIC Linux-observable behavior — return
  value, errno, ordering, `/proc` content — never merely "the call
  succeeded".

Output protocol (matches the existing smokes):

- Progress and boundary markers go to stdout as exact lines:
  `THEKERNEL_<CASE>_<CHECK>_OK` or
  `THEKERNEL_<CASE>_<CHECK>_BOUNDARY key=value ...`
- Failures go to stderr as
  `THEKERNEL_<CASE>_FAIL <stage> actual=<n> expected=<n> errno=<n> (<msg>)`
  followed by `exit(EXIT_FAILURE)`.
- The final stdout line of a passing run is `THEKERNEL_<CASE>_OK`, exit 0.

### 2. Declare the marker manifest

Add `manifests/<case>.markers`: one exact marker line per line, the full set
a passing run must emit. Runners verify each line with `grep -Fqx` (or the
exactly-once mode for cases that forbid duplicates). Blank lines and lines
starting with `#` are ignored.

### 3. Write the host runner

Model `scripts/ci/<case>-host-differential.sh` on
`scripts/ci/seccomp-host-differential.sh`: `set -euo pipefail`, a
`--workdir` option, artifacts under `.state/ci/<case>-host-differential/`.
Source the library and let it do the mechanical work:

```bash
. "$SCRIPT_DIR/differential/lib.sh"

WORKDIR=$(differential_resolve_workdir "$REPO_ROOT" "$WORKDIR")
differential_build_smoke "$BINARY" "$REPO_ROOT/tests/guest/tools/<case>-smoke.c"
status=0
differential_run_bounded "$LOG" 60s 5s -- "$BINARY" || status=$?
missing=$(differential_missing_markers "$LOG" "$MANIFEST" || true)
[ -z "$missing" ] || missing=$(printf '%s\n' "$missing" \
    | differential_apply_allowlist "$ALLOWLIST" "$(uname -r)" "$APPLIED")
differential_write_receipt "$RECEIPT" <case> "$REPO_ROOT" \
    "$expected" "$matched" "$APPLIED" pass
```

The library functions are mechanism-only: they never print runner-branded
diagnostics, so the runner keeps exact control over its user-visible
messages, `result.txt` contents, and exit codes. See `lib.sh` for the full
per-function documentation.

### 4. Receipt

Every executed run writes `<workdir>/receipt.json`:

```json
{
  "schema": "thekernel-differential-receipt-v0",
  "case": "<case>",
  "git_rev": "<rev-parse HEAD>",
  "reference": {
    "kind": "host-linux",
    "kernel_release": "<uname -r>",
    "kernel_version_line": "<first line of /proc/version>"
  },
  "toolchain": {"cc": "<cc --version | head -1>"},
  "markers_expected": 8,
  "markers_matched": 8,
  "allowlist_applied": [],
  "result": "pass"
}
```

Portable runners refuse a dirty checkout, freeze their C source and marker
manifest directly from `git_rev`, execute that frozen source, and revalidate
the repository revision and cleanliness immediately before publishing the
receipt. Thus `git_rev` identifies the exact input bytes rather than merely
describing whichever commit happened to be checked out near the run.

`epoll` has an explicit guest capability boundary. Host Linux is checked
against `epoll.markers`, including real `EPOLLEXCLUSIVE` wake selection. The
current TheKernel fd-core contract lacks the cross-epoll/source selector needed
to implement that rule, so guest replay invokes the same binary with
`--thekernel` and checks `epoll-guest.markers`: `EPOLL_CTL_ADD` and
`EPOLL_CTL_MOD` must reject `EPOLLEXCLUSIVE` with `EINVAL`, and the guest
console records `THEKERNEL_EPOLL_EXCLUSIVE_UNSUPPORTED_OK`. Every other epoll
marker is shared and remains strict. This bounded capability branch must be
removed when the lower-layer selector exists; it is not a Linux conformance
PASS.

A `pass` receipt must account for every expected marker as either matched or
explicitly allowlisted. Validate with:

```sh
python3 scripts/ci/differential/validate-receipt.py \
    --receipt <workdir>/receipt.json --case <case> \
    --manifest scripts/ci/differential/manifests/<case>.markers
```

### 5. Allowlist (documented divergence, never silent)

A runner may downgrade a missing or failed marker ONLY through an entry in
`allowlist/<case>.json` whose kernel range matches the reference kernel, and
every application is recorded in the receipt's `allowlist_applied`. The
default is no allowlist file at all. See `allowlist/README.md` for the
schema.

## Verification expectations

Runners must verify their own work end to end: compile the smoke, run it on
the host, verify the manifest, and emit the receipt. If the environment
blocks execution (inherited seccomp profile, missing user namespaces), the
runner must fail loudly or take an explicitly requested compile-only skip —
never fabricate a pass. Compile-only skips do not produce a receipt.
