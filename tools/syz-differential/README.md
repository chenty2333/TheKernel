# syz-differential: syzlang-driven differential-case generation (prototype)

This prototype generates differential C smoke cases from syzkaller-style
syscall descriptions (syzlang) plus a small hand-written semantics annotation.

```
descriptions/<name>.txt   syzlang-subset description (what the ABI looks like)
        |
   syzlang_parser.py ---> typed AST (resources, flag sets, structs, syscalls)
        |
semantics/<name>.json     hand-written annotation (what behavior to assert)
        |
    generate.py ---------> generated/<name>-gen-smoke.c
```

Run end to end:

```sh
bash tools/syz-differential/test_prototype.sh
```

This parses both descriptions, verifies parser rejection paths, regenerates
both C cases, checks annotation/description drift, compiles with
`cc -static -O2 -Wall -Wextra -Werror` (falling back to dynamic linking when a
static libc is unavailable), and runs the cases on the host kernel. Generation
is deterministic: no timestamps or randomness; output depends only on the two
input files.

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

The description defines the ABI shape; the annotation defines concrete calls
and expected Linux-visible results. Each check can save syscall results, use
declared locals, assert return values, errno, and values, or set a local field
before a call. The existing `eventfd.json` and `timerfd.json` are the concise
format references.

Argument forms are integer literals, `NAME|NAME` flag expressions validated
against the declared flag set, `@var` values saved by an earlier call in the
same check, and `&local` addresses for pointer or buffer parameters (`0` is
NULL).

Calls are emitted as `syscall(SYS_<basename>, ...)` — `read$eventfd` invokes
`SYS_read` — so the same generated source is meaningful on host Linux and in
the TheKernel guest with no libc-wrapper divergence.

Cross-validation the generator enforces (drift between annotation and
description is a generation error, exercised by `test_prototype.sh`):
undeclared syscall names, wrong arity, flag names outside the declared set,
pointer args for non-pointer params, references to never-saved values,
undeclared locals, duplicate check identifiers, and malformed errno names.

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

Intended import path for upstream `sys/linux/*.txt`: vendor files unmodified,
extend the parser incrementally while retaining hard errors for unsupported
constructs, replace compiler-side constant resolution with a
`syz-extract`-style table, and grow annotations only where behavior cannot be
derived. Integrating generated cases into a test suite is separate work from
this prototype.
