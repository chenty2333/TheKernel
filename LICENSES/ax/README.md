# License texts

AX components retain the license expression in their own Cargo.toml.
This directory supplies Apache-2.0, GPL-3.0-or-later, and MulanPSL-2.0 texts
used by those expressions. Corresponding copies live in each applicable
crate's LICENSES/ directory so packages do not depend on root files: all 30
crates in `crates/ax` whose `license` is `GPL-3.0-or-later OR Apache-2.0 OR
MulanPSL-2.0` ship all three texts locally.

Other component licenses (MIT, GPL-2.0, and 0BSD) remain in the respective
crate directories with their original notices, with two known gaps:
`crates/ax/tk-lwext4-rust` ships `LICENSE.GPLv2` only, so the BSD-3-Clause part
of its vendored C has no text here, and `crates/ax/tk-axfs-ng-vfs` declares
`MIT OR Apache-2.0` while shipping only the Apache-2.0 text. Both are recorded
in `docs/licensing.md`.

Every component in `crates/ax` that offers an Apache-2.0 option includes that
complete text locally. Outside `crates/ax`, two workspace members declare
`Apache-2.0` with no license text at all — `crates/process-adapter` and
`crates/readiness-adapter` — as `docs/upstream-provenance.md` notes.
