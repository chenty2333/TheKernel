//! Retain the HPET address before firmware tables leave the direct map.
//! Layout facts: Linux include/acpi/actbl1.h:2121-2128 (ACPI HPET 1.0a).
use core::sync::atomic::{AtomicUsize, Ordering};

use super::{RootWalk, find_in_root, root_tables};
use crate::cpu::{physical_bytes, read_u64, table_length_and_bytes};

static BASE: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn hpet_base() -> Option<usize> {
    let base = BASE.load(Ordering::Acquire);
    (base != 0).then_some(base)
}

fn parse_base(table: &[u8]) -> Option<usize> {
    if table.len() < 56 || &table[..4] != b"HPET" || table[40] != 0 || table[42] != 0 {
        return None;
    }
    let base = usize::try_from(read_u64(table, 44)?).ok()?;
    if base == 0 || base & 0xfff != 0 {
        return None;
    }
    base.checked_add(4096)?;
    Some(base)
}

pub(super) fn init_early() {
    let rsdp = crate::boot_info::get()
        .rsdp()
        .map(|r| r.bytes().as_slice())
        .or_else(|| {
            let address = crate::cpu::find_rsdp()?;
            // SAFETY: early boot still maps firmware memory, as in MCFG discovery.
            unsafe { physical_bytes(address, 36) }
        });
    let Some(rsdp) = rsdp else {
        return;
    };
    let roots = root_tables(rsdp);
    for (address, width) in [
        (if roots.revision >= 2 { roots.xsdt } else { 0 }, 8),
        (roots.rsdt, 4),
    ] {
        if address == 0 {
            continue;
        }
        if let RootWalk::Found(address) = find_in_root(address, width, b"HPET")
            && let Some((_, table)) = table_length_and_bytes(address)
            && let Some(base) = parse_base(table)
        {
            BASE.store(base, Ordering::Release);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_relocated_system_memory_hpet_and_rejects_bad_gas() {
        let mut table = [0u8; 56];
        table[..4].copy_from_slice(b"HPET");
        table[44..52].copy_from_slice(&0xfed1_0000u64.to_le_bytes());
        assert_eq!(parse_base(&table), Some(0xfed1_0000));
        table[40] = 1;
        assert_eq!(parse_base(&table), None);
        table[40] = 0;
        table[42] = 8;
        assert_eq!(parse_base(&table), None);
        table[42] = 0;
        table[44] = 1;
        assert_eq!(parse_base(&table), None);
        assert_eq!(parse_base(&table[..55]), None);
    }
}
