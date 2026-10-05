//! Measured PCI BAR prefix, with unobserved trailing resources left unknown.
use alloc::{format, string::String};

use axdriver::{pci::Address, pci_resources::Resource};
use axfs_ng_vfs::VfsResult;

fn render(resources: &[Resource]) -> String {
    let mut out = String::new();
    for resource in resources {
        out.push_str(&format!(
            "0x{:016x} 0x{:016x} 0x{:016x}\n",
            resource.start, resource.end, resource.flags
        ));
    }
    out
}

pub(super) fn snapshot(address: Address) -> VfsResult<String> {
    let resources =
        axdriver::pci_resources::prefix(address).map_err(|error| match error {
            axdriver::prelude::DevError::NoMemory => axfs_ng_vfs::VfsError::NoMemory,
            _ => axfs_ng_vfs::VfsError::Io,
        })?;
    Ok(render(&resources))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_prefix_has_three_fixed_width_hex_columns_and_no_guessed_suffix() {
        assert_eq!(
            render(&[
                Resource {
                    start: 0x6060,
                    end: 0x607f,
                    flags: 0x40101
                },
                Resource::default()
            ]),
            "0x0000000000006060 0x000000000000607f 0x0000000000040101\n0x0000000000000000 \
             0x0000000000000000 0x0000000000000000\n"
        );
        assert_eq!(render(&[]), "");
    }
}
