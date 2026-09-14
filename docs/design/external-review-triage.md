# Triage of the external review of `dev`

Status: done.  Every verdict below was reached by reading the named code and by
running the repository's own gates; the report itself ran no tests (its
subagents failed to start, and it says so in its own summary).  Nothing here
has been near the N305 -- like the rest of the display work, the evidence is
QEMU.

## What was reviewed

An external agent read `dev` at `3a96bed8` and returned eight items, no high
findings, risk 3/5, and a suggested six-batch merge order for `dev` -> `main`.
`dev` has moved on since: the tip this document ships with is a different tree,
and the merge order is advice for whoever does that merge, not something this
work depends on.

The report's items are listed below in its own order, by subject.  Three named
a real defect, four did not, and one was a suggestion.  Checking one of the
seven turned up a defect the report did not find, which is the largest thing on
this page.

## The defect the report did not find

While checking the report's question about the exit-status ring (whether a
wrapped ring is observable), `tools/stack_frames.py` found that
`exit_status_trace_dump` reserved a **364 KiB stack frame** against a
`TASK_STACK_SIZE` of 256 KiB, and that the frame came from copying the 182 KiB
`[ExitStatusRecord; 512]` ring by value twice:

```rust
let (records, sequence) = { let trace = EXIT_STATUS_TRACE.lock(); (trace.records, trace.sequence) };
```

Task stacks are `alloc::alloc` with no guard page (`TaskStack::try_alloc`), and
`/proc/sys/kernel/exit-status` is mode `0o444`: any process could ask, and the
stack probe would walk below the stack and zero a page inside whatever the
allocator had put there.  It did not fault.

Fixed on `fix/exit-status-dump-frame`: the ring is copied into a reserved heap
buffer before the lock is taken, a `const _` assertion beside the ring bounds it
against the task stack, `the_dump_renders_inside_a_stack_far_smaller_than_the_ring`
fails if a by-value copy returns (it aborts with a stack overflow when the old
body is restored), `tools/stack_frames.py` checks every frame in the built image
against `task-stack-size`, and the guest case `exit-status-trace-read` reads a
wrapped ring through `/proc` on every `verify --tier daily`.

The report's own question has a concrete answer: a wrapped ring is legible,
because the header carries the window it rendered -- `EXITSTATUS_TRACE_BEGIN
seq=1664 first=1153 entries=512` was read out of a guest whose ring had wrapped.

## Items that named a real defect

- **The `modeset` accessor's unreachable `expect`.**  `set_mode` matched
  `ModeChoice::Refused` and then called `choice.mode().expect("a non-refusal
  names a mode")`.  The expect was unreachable, but it put the reason in a
  comment that every reader had to re-derive.  `ModeChoice::into_mode` now
  returns `Result<Mode, ModeRefusal>` and one exhaustive match is what proves
  it.  Fixed on `fix/unreachable-panics`.
  The report's description of the mechanism was wrong -- a new `ModeChoice`
  variant already fails to compile in the accessor's match, so nothing could
  reach the expect that way -- but the refactor is still the better shape.
- **`klog::Filter::render`'s `from_utf8(..).unwrap()`.**  `parse` admits only
  ASCII alphanumerics and four punctuation bytes, so the unwrap was also
  unreachable today; unlike the `expect` above it is on a path a process
  reaches, and the kernel's single panic handler ends in `system_off()`.  It now
  prints `[invalid filter prefix]`, as `record` twenty lines below already did.
  Fixed on `fix/unreachable-panics`.
- **`scripts/ci/n305-capture-image.sh --out`.**  The script removes and rewrites
  `$OUT`, and prints a closing hint pairing it with a device:
  `sudo dd if=$OUT of=/dev/sdX`.  A transposed argument was destructive in a way
  nothing downstream could detect.  `--out` under `/dev` and an existing
  non-regular `--out` are now refused before anything is downloaded or written.
  Fixed on `fix/n305-capture-out-guard`.

## Items that were not defects

- **The `igc` register-name lookups.**  `regs::named` is a scan of a static
  table, and the `regs::named("IGC_..").expect(..)` calls in `bringup.rs` sit
  inside `bring_up`, `start_link` and the address readers, all of which the host
  tests call: a name that left the table fails a test, not a boot.  The
  doc comment on `named` now says so.
- **`fb.rs`'s `get_u32`/`get_u64`.**  The offsets are `offset_of!` values into
  the `#[repr(C)]` structure whose image is being read, and the callers pass
  exactly `size_of::<Struct>()` bytes, so the slicing could not go out of
  bounds.  The accessors now take the image length and the offset as const
  generics, which makes an offset outside the image a build failure
  (`E0080`) rather than a panic.  That hardening is on `fix/review-followups`.
- **`Gtt::from_mapped`.**  It is the test-side twin of `MappedArray::map_bar`;
  production maps the device's aperture, and nothing outside `gtt.rs`'s tests
  called it.  It is now `#[cfg(test)]`, so that stays true by construction.
  Also on `fix/review-followups`.
- **The DMT and VIC tables.**  The suggestion was to fuzz them.  There is
  nothing to fuzz: `dmt::by_code`, `dmt::by_std_id`, `dmt::by_size` and
  `vic::by_vic` are `iter().find(..)` over static tables and return `Option`,
  so every byte and every timing id already has an answer with no panic path.
  The tables' contents are checked against the standard by
  `published_rows_match_the_standard`, `published_formats_match_the_standard`
  and their neighbours.

## What this cost, and what it did not touch

The three fixes and the hardening are four branches off `dev`, each in its own
worktree: `fix/exit-status-dump-frame`, `fix/unreachable-panics`,
`fix/n305-capture-out-guard` and `fix/review-followups`.  They are merged into
`dev` and never into `main`, and the merged tip is gated once more with
`verify --tier daily` before it is handed over.  The report's eight items did
not include the frame bound, the guest read path, or anything under
`kernel/src/drm/intel/` other than the accessor above, and no item here changes
a decision recorded in the stage-2 report.
