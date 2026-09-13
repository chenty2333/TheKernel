//! The table lists every register the submodules declare, and lists each once.
//!
//! [`table`](super::table)'s slice literals are hand-maintained: the adjacent
//! submodules hold the declarations and the table names them, because a
//! `&[Register]` cannot be built from "every `const` in a module" without a
//! macro, and a macro would have hidden the doc comments that carry each
//! register's citation.
//!
//! That leaves one failure mode worth a test.  A new register is declared,
//! tested on its own, and never added to the table -- so the properties the
//! table exists to check (one address, one owner, inside a forcewake-free band,
//! inside the mapped window) silently stop covering it.  This test reads the
//! declarations and the table and refuses to let the two disagree.
//!
//! It parses the *source* rather than inspecting a running program on purpose:
//! the set that has to agree is a source-level set, and a missing entry is
//! reported by name rather than as a mysterious gap in coverage.  The sources
//! are pulled in with `include_str!` rather than read from disk, so the test
//! depends on the same revision of the file the compiler saw and on nothing
//! about the working directory it is run from.

use alloc::{collections::BTreeSet, format, string::String, vec::Vec};

/// One `pub(crate) const NAME: Register = ...` declaration, as parsed.
struct Declared {
    name: String,
    offset: u32,
}

/// The submodule sources, and the table slice each one's declarations belong
/// to.
const GROUPS: &[(&str, &str)] = &[
    (include_str!("../ddi.rs"), "DDI"),
    (include_str!("../dpll.rs"), "DPLL"),
    (include_str!("../interrupt.rs"), "INTERRUPT"),
    (include_str!("../pipe.rs"), "PIPE"),
    (include_str!("../port.rs"), "PORT"),
];

/// The table itself.
const TABLE: &str = include_str!("mod.rs");

/// How many `pub(crate) const` items `source` declares.
fn consts(source: &str) -> usize {
    source
        .lines()
        .filter(|line| line.starts_with("pub(crate) const "))
        .count()
}

/// Every register declared in `source`, in declaration order.
///
/// The parse is deliberately narrow: it accepts the shape the constructors
/// enforce, `Register::<kind>("NAME", 0xOFFSET, Meaning::..., ...)`, split over
/// the one or two lines rustfmt produces.  A declaration written in a shape
/// this does not recognise is not silently skipped -- the callers compare the
/// parse against [`consts`], so a new shape shows up as a count mismatch rather
/// than as a register that quietly escaped the check.
fn declarations(source: &str) -> Vec<Declared> {
    let mut out = Vec::new();
    let mut lines = source.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.trim().strip_prefix("pub(crate) const ") else {
            continue;
        };
        let Some((name, tail)) = rest.split_once(':') else {
            continue;
        };
        let mut body = String::from(tail);
        while !body.contains("Register::") && lines.peek().is_some() {
            body.push(' ');
            body.push_str(lines.next().unwrap().trim());
        }
        while !body.contains(';') && lines.peek().is_some() {
            body.push(' ');
            body.push_str(lines.next().unwrap().trim());
        }
        let call = body
            .split_once("Register::")
            .unwrap_or_else(|| panic!("{name}: no Register constructor"))
            .1;
        let offset = call
            .split_once(", 0x")
            .unwrap_or_else(|| panic!("{name}: no offset"))
            .1;
        let offset = offset.split(',').next().unwrap().trim();
        let offset = u32::from_str_radix(&offset.replace('_', ""), 16)
            .unwrap_or_else(|error| panic!("{name}: offset {offset:#?}: {error}"));
        out.push(Declared {
            name: String::from(name.trim()),
            offset,
        });
    }
    out
}

/// Every `NAME` in the table slice named `slice`.
fn table_entries(source: &str, slice: &str) -> Vec<String> {
    let header = format!("pub(crate) const {slice}: &[Register] = &[");
    let start = source
        .find(&header)
        .unwrap_or_else(|| panic!("the table has no {slice} slice"))
        + header.len();
    let end = start
        + source[start..]
            .find(']')
            .unwrap_or_else(|| panic!("the {slice} slice is never closed"));
    source[start..end]
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split_once("::")
                .unwrap_or_else(|| panic!("{slice}: {line:#?} is not a qualified path"))
                .1
                .trim_end_matches(',')
        })
        .map(String::from)
        .collect()
}

#[test]
fn the_table_lists_every_declared_register_and_names_it_once() {
    for (source, slice) in GROUPS {
        let declared = declarations(source);
        let total = consts(source);
        assert!(
            total > 0,
            "the {slice} submodule declares no register, which means the parse found none"
        );
        // A shape the parser does not understand would show up here.  The
        // submodules' only `const` items are registers, so the two counts are
        // the same number.
        assert_eq!(
            total,
            declared.len(),
            "the {slice} submodule declares {total} constants but only {} were parsed",
            declared.len()
        );

        let listed = table_entries(TABLE, slice);
        let declared_names: Vec<&str> = declared.iter().map(|item| item.name.as_str()).collect();
        let listed_names: Vec<&str> = listed.iter().map(String::as_str).collect();
        assert_eq!(
            listed_names, declared_names,
            "the {slice} slice of the table does not match its submodule: the two must name the \
             same registers in the same order, nothing missing and nothing twice"
        );
    }
}

#[test]
fn no_register_is_listed_in_two_slices() {
    // The duplicate-address check in `super::tests` walks the same tables, so
    // this is narrower on purpose: it names the failure as "the table lists
    // this name twice", which is what a register moved between submodules and
    // left behind looks like.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (_, slice) in GROUPS {
        for name in table_entries(TABLE, slice) {
            assert!(seen.insert(name.clone()), "{name} is listed twice");
        }
    }
}

#[test]
fn every_table_offset_is_dword_aligned_and_distinct() {
    // The same properties `Register::declare` asserts at compile time, restated
    // over the parsed numbers: a declaration that somehow bypassed the
    // constructor, or a literal the constructor could not have checked, is
    // reported with the group and the name.
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    for (source, slice) in GROUPS {
        for item in declarations(source) {
            assert_eq!(
                item.offset % 4,
                0,
                "{slice}: {} is not dword aligned",
                item.name
            );
            assert!(
                seen.insert(item.offset),
                "{slice}: {} reuses {:#x}, which another register already claims",
                item.name,
                item.offset
            );
        }
    }
}
