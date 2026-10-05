//! One real block-counter source for diskstats and block stat attributes.
use alloc::{string::String, sync::Arc};
use core::fmt::Write;

use axdriver::block_statistics::Snapshot;
use axfs_ng_vfs::VfsResult;

use super::{DirMapping, SimpleFile, SimpleFs};

fn fields(snapshot: Snapshot) -> String {
    let mut out = String::new();
    for (index, value) in snapshot.fields.into_iter().enumerate() {
        let _ = write!(out, "{}{value}", if index == 0 { "" } else { " " });
    }
    out.push('\n');
    out
}

fn required(snapshot: Option<Snapshot>) -> VfsResult<String> {
    snapshot
        .map(fields)
        .ok_or(axfs_ng_vfs::VfsError::OperationNotSupported)
}

pub(super) fn augment_device(dir: &mut DirMapping, fs: &Arc<SimpleFs>, name: &str) {
    let name = String::from(name);
    dir.add(
        "stat",
        SimpleFile::new_regular(fs.clone(), move || {
            required(axfs::block_device_statistics(&name))
        }),
    );
}

pub(super) fn augment_loop(dir: &mut DirMapping, fs: &Arc<SimpleFs>, number: u32) {
    dir.add(
        "stat",
        SimpleFile::new_regular(fs.clone(), move || {
            required(super::dev::r#loop::io_statistics(number))
        }),
    );
}

fn diskstats() -> VfsResult<String> {
    let mut out = String::new();
    for entry in axfs::block_inventory() {
        let Some(id) = super::block_inventory::device_id(&entry.name) else {
            continue;
        };
        let row = required(axfs::block_device_statistics(&entry.name))?;
        let _ = write!(
            out,
            "{:4} {:7} {} {row}",
            id.major(),
            id.minor(),
            entry.name
        );
    }
    for number in 0..super::dev::r#loop::LOOP_COUNT as u32 {
        let row = required(super::dev::r#loop::io_statistics(number))?;
        let _ = write!(out, "   7 {number:7} loop{number} {row}");
    }
    Ok(out)
}

pub(super) fn register_proc(root: &mut DirMapping, fs: &Arc<SimpleFs>) {
    root.add("diskstats", SimpleFile::new_regular(fs.clone(), diskstats));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stat_has_all_linux_decimal_fields_without_a_header_or_units() {
        let fields = fields(Snapshot {
            fields: core::array::from_fn(|index| index as u64),
        });
        assert_eq!(fields, "0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16\n");
        assert!(required(None).is_err());
    }
}
