//! Active regular-file swap rows. Resolve/escape paths after registry unlock.
use alloc::{format, vec::Vec};

use axfs_ng_vfs::VfsResult;

const HEADER: &[u8] = b"Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n";
fn escaped_path(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for &byte in bytes {
        let escape: Option<&[u8]> = match byte {
            b' ' => Some(b"\\040"),
            b'\t' => Some(b"\\011"),
            b'\n' => Some(b"\\012"),
            b'\\' => Some(b"\\134"),
            _ => None,
        };
        if let Some(escape) = escape {
            out.extend_from_slice(escape);
        } else {
            out.push(byte);
        }
    }
    out
}
fn row(path: &[u8], total: usize, used: usize, priority: i16) -> Vec<u8> {
    let mut out = path.to_vec();
    let spacing = if path.len() < 40 { 40 - path.len() } else { 1 };
    out.resize(out.len() + spacing, b' ');
    let tail = format!(
        "file\t\t{total}\t{}{used}\t{}{priority}\n",
        if total < 10_000_000 { "\t" } else { "" },
        if used < 10_000_000 { "\t" } else { "" }
    );
    out.extend_from_slice(tail.as_bytes());
    out
}
pub(super) fn swaps() -> VfsResult<Vec<u8>> {
    let entries = crate::mm::swap_devices()?;
    let mut out = HEADER.to_vec();
    for entry in entries {
        let path = entry.location.absolute_path()?;
        let escaped = escaped_path(path.as_bytes());
        out.extend_from_slice(&row(
            &escaped,
            entry.total_bytes / 1024,
            entry.used_bytes / 1024,
            entry.priority,
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn swap_rows_preserve_linux_columns_units_and_raw_path_separators() {
        let path = escaped_path(b"/swap space\tline\n\\file");
        assert_eq!(path, b"/swap\\040space\\011line\\012\\134file");
        let text = row(&path, 2044, 64, -1);
        let text = core::str::from_utf8(&text).unwrap();
        let fields: alloc::vec::Vec<_> = text.split_whitespace().collect();
        assert_eq!(
            fields,
            [
                core::str::from_utf8(&path).unwrap(),
                "file",
                "2044",
                "64",
                "-1"
            ]
        );
        let raw = escaped_path(b"/swap\xff");
        assert_eq!(raw, b"/swap\xff");
        let long = [b'x'; 45];
        assert_eq!(&row(&long, 1, 0, -1)[45..51], b" file\t");
    }
}
