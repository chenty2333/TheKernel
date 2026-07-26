# syz-differential: syzlang-driven differential-case generation (prototype)

Prototype (workstream C of the Linux-ABI differential-testing framework)
proving that contract-conforming differential C smoke cases can be *generated*
from syzkaller-style syscall descriptions (syzlang) plus a small hand-written
semantics annotation, instead of being hand-written C.

```
descriptions/<name>.txt   syzlang-subset description (what the ABI looks like)
        |
   syzlang_parser.py ---> typed AST (resources, flag sets, structs, syscalls)
        |
semantics/<name>.json     hand-written annotation (what behavior to assert)
        |
    generate.py ---------> generated/<name>-gen-smoke.c   (contract v0 case)
                           generated/<name>-gen-smoke.markers (manifest)
```

Run end to end:

```sh
bash tools/syz-differential/test_prototype.sh
```

This parses both descriptions, verifies the parser rejects out-of-subset
syntax, generates both C cases, checks generation is byte-identical on
regeneration, verifies the generator rejects annotation/description drift,
compiles with `cc -static -O2 -Wall -Wextra -Werror` (falling back to a
dynamic link with an explicit warning on hosts without a static libc, matching
`scripts/ci/seccomp-host-differential.sh`), runs the binaries on the host
kernel, and verifies every manifest marker with `grep -Fqx`.

Generated cases follow the differential-case contract v0: per-check
`THEKERNEL_<NAME>GEN_<CHECK>_OK` markers on stdout, a final
`THEKERNEL_<NAME>GEN_OK`, and on failure a single
`THEKERNEL_<NAME>GEN_FAIL <stage> actual=<n> expected=<n> errno=<n> (<msg>)`
line on stderr followed by `exit(EXIT_FAILURE)`. Generation is deterministic:
no timestamps, no randomness, output depends only on the two input files.

## Supported syzlang subset (grammar)

Real syzlang syntax, restricted. One construct per line except struct blocks.
`#` starts a comment. Anything outside this grammar is a hard parse error
with file/line/reason — the parser never silently mis-parses.

```
file      := line*
line      := include | resource | flagset | struct | syscall | comment | blank
include   := "include" "<" path ">"                      # recorded as metadata
resource  := "resource" name "[" "fd" "]"                # fd-backed only
flagset   := name "=" IDENT ("," IDENT)*                 # C constant names
struct    := name "{" NEWLINE (fieldname type NEWLINE)* "}"
syscall   := name ["$" variant] "(" [param ("," param)*] ")" [rettype]
param     := name type
type      := "int8" | "int16" | "int32" | "int64" | "intptr"
           | inttype "[" const ":" const "]"             # value range
           | "fd" | resource-name
           | "flags[" flagset-name ["," inttype] "]"
           | "const[" int ["," inttype] "]"
           | "ptr[" ("in"|"out"|"inout") "," (inttype|struct-name) "]"
           | "buffer[" ("in"|"out"|"inout") "]"
           | "len[" param-name ["," inttype] "]"
           | struct-name
rettype   := inttype | resource-name | "fd"
```

Whole-description validation: `flags[x]` must name a declared flag set,
`len[x]` must name a sibling parameter, struct references must resolve,
duplicate names are rejected. `syzlang_parser.py <file>...` dumps the AST as
JSON.

## Semantics annotation format (`semantics/<name>.json`)

The description says what the ABI looks like; the annotation says which
concrete sequences to run and what Linux must observably do. Schema
`syz-differential-semantics-v0`:

```jsonc
{
  "schema": "syz-differential-semantics-v0",
  "name": "eventfd",              // output basename; prefix must be NAMEGEN
  "marker_prefix": "EVENTFDGEN",
  "c_includes": ["sys/eventfd.h"],  // appended after the base include set
  "checks": [
    {
      "marker": "COUNTER_INITVAL",   // -> THEKERNEL_EVENTFDGEN_COUNTER_INITVAL_OK
      "locals": {"rval": "u64"},     // u64 | i64 | int | itimerspec | timespec
      "steps": [
        // call step: syscall must be declared in the description, arity is
        // checked, named flags are checked against the arg's flag set.
        {"call": "eventfd2", "args": ["3", "EFD_NONBLOCK"], "save": "efd",
         "expect": {"ret": ">=0"}},
        {"call": "read$eventfd", "args": ["@efd", "&rval", "8"],
         "expect": {"ret": "8"}},
        // value assertion on a local after a call
        {"expect": {"var": "rval", "cmp": "==", "value": 3}},
        // errno assertion (requires ret == "-1")
        {"call": "read$eventfd", "args": ["@efd", "&rval", "8"],
         "expect": {"ret": "-1", "errno": "EAGAIN"}},
        // set step: write a field of a declared local before a call
        // {"set": "its.it_value.tv_nsec", "value": 20000000}
        {"call": "close$eventfd", "args": ["@efd"], "expect": {"ret": "0"}}
      ]
    }
  ]
}
```

Argument forms: integer literal (always allowed — this is how negative tests
pass bad flags/fds/lengths), `NAME|NAME` flag expressions (validated against
the parameter's flag set from the description), `@var` (a value saved from an
earlier call in the same check — resource threading), `&local` (address of a
declared local, only for `ptr`/`buffer` parameters; `0` means NULL).

Calls are emitted as `syscall(SYS_<basename>, ...)` — `read$eventfd` invokes
`SYS_read` — so the same generated source is meaningful on host Linux and in
the TheKernel guest with no libc-wrapper divergence.

Cross-validation the generator enforces (drift between annotation and
description is a generation error, exercised by `test_prototype.sh`):
undeclared syscall names, wrong arity, flag names outside the declared set,
pointer args for non-pointer params, references to never-saved values,
undeclared locals, duplicate markers, malformed errno/marker names.

## Current cases

- `eventfd` (8 checks): create; EINVAL on bad flags; `EFD_CLOEXEC` visible via
  `F_GETFD`; initval read + drain-to-EAGAIN; EAGAIN on empty nonblocking read;
  write accumulation (5+2 reads back 7); `EFD_SEMAPHORE` decrement-by-one
  semantics then EAGAIN; EINVAL on `read` with count < 8.
- `timerfd` (11 checks): create on MONOTONIC/REALTIME/BOOTTIME; EINVAL on bad
  clockid and bad create flags; settime EINVAL on bad flags and on
  `tv_nsec >= 1e9`; EBADF on bad fd; gettime on a disarmed timer reads zero;
  armed settime/gettime round-trip (remaining time sane, interval zero);
  blocking read returns 8 bytes with expiration count exactly 1.

## Honest gap to real syzkaller descriptions

This is a prototype; `sys/linux/*.txt` upstream files will NOT parse today.
What full support additionally needs:

- **Grammar coverage**: `string`/`stringnoz`, `array[T, n]`, `union`s,
  `optional`/attribute annotations (`packed`, `align[]`, `size[]`),
  `vma`/`proc`/`csum`/`text` types, templated types (`type` aliases,
  `bytesize`), `define` constants, `incdir`, per-arch const files
  (`*.txt.const` with `arches`), `$`-variant inheritance, ioctl `_IOR`-style
  const expressions. Our parser deliberately hard-errors on all of these.
- **Const resolution**: syzkaller extracts numeric values for `EFD_CLOEXEC`
  etc. from kernel headers per-arch (`syz-extract`). We instead emit the C
  identifiers and let the compiler resolve them via `c_includes` — fine for
  libc/UAPI-visible constants, wrong for description-local `define`s or
  arch-divergent values.
- **Value generation**: syzkaller *fuzzes* — it derives interesting values
  (boundary ints, ranges, checksummed blobs) from the types. Here every
  concrete value comes from the hand-written annotation; the description only
  validates it. Auto-deriving negative cases (each flag bit outside the set →
  expect EINVAL; each `int32[lo:hi]` boundary) is the natural next step and
  needs no grammar extension.
- **Resource threading**: we thread resources only via `@save` within one
  check, and only fd-backed resources. Real descriptions have non-fd resources
  and cross-call inference of producer/consumer relations.
- **Semantics oracle**: syzlang describes *shape*, not *behavior*. The
  expected return/errno/value assertions can never come from the description
  alone; the annotation layer stays even with a full parser. Scaling means
  standard annotation libraries per subsystem, or deriving expectations from
  a reference-kernel recording run (record host behavior, replay as
  assertions), plus an allowlist for legitimately nondeterministic returns.
- **Struct field mapping**: description struct fields (`sec`, `nsec`) are not
  yet mapped to C field names (`tv_sec`, `tv_nsec`); `set`/`expect` paths use
  C names, validated only at the root variable. Deep validation should map
  description structs to generated C struct definitions instead of relying on
  libc's.
- **Deterministic timing**: blocking reads on armed timers are deterministic
  in count but not duration; a full harness wants per-case timeouts.

Intended import path for upstream `sys/linux/*.txt`: (1) vendor the files
unmodified, (2) extend the parser milestone-by-milestone (strings/arrays →
unions/attributes → templates/defines), keeping the hard-error property so
unsupported constructs are visible rather than mis-parsed, (3) replace
compiler-side const resolution with a `syz-extract`-style const table,
(4) grow the annotation schema only where behavior cannot be derived, and
(5) at integration time, hook `generated/*.c` into the standard
`scripts/ci/<subsys>-host-differential.sh` runner + receipt flow from the
shared contract (kept out of this prototype to avoid touching CI wiring).
