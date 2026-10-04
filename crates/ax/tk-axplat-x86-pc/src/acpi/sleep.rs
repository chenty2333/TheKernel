//! Bounded static AML subset, not an AML interpreter. Unknown executable terms
//! invalidate their term list; buffers, methods and field bodies are opaque.
//! Encoding facts: ACPI 6.6 sections 5.2.9, 16 and 20. Original implementation.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FixedPower {
    pub event_a: u16,
    pub event_b: u16,
    pub event_half: u16,
    pub control_a: u16,
    pub control_b: u16,
    pub sci: u16,
    pub fixed_button: bool,
    pub smi: u16,
    pub enable: u8,
    pub dsdt: u64,
}
fn word(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn dword(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn qword(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}
fn port(b: &[u8], legacy: usize, gas: usize, size: u16, optional: bool) -> Option<u16> {
    let extended = qword(b, gas + 4).unwrap_or(0);
    let address = if extended != 0 {
        if *b.get(gas)? != 1 || *b.get(gas + 2)? != 0 || *b.get(gas + 1)? < (size * 8) as u8 {
            return None;
        }
        extended
    } else {
        u64::from(dword(b, legacy)?)
    };
    let p = u16::try_from(address).ok()?;
    if p == 0 {
        return optional.then_some(0);
    }
    p.checked_add(size - 1)?;
    Some(p)
}
pub(crate) fn parse_fadt(b: &[u8]) -> Option<FixedPower> {
    if b.get(..4)? != b"FACP" || b.len() < 116 {
        return None;
    }
    let flags = dword(b, 112)?;
    if flags & (1 << 20) != 0 {
        return None;
    } // HW-reduced needs another interface.
    let length = *b.get(88)?;
    if !matches!(length, 4 | 8) || *b.get(89)? != 2 {
        return None;
    }
    let dsdt = match qword(b, 140).unwrap_or(0) {
        0 => u64::from(dword(b, 40)?),
        n => n,
    };
    Some(FixedPower {
        event_a: port(b, 56, 148, length.into(), false)?,
        event_b: port(b, 60, 160, length.into(), true)?,
        event_half: u16::from(length / 2),
        control_a: port(b, 64, 172, 2, false)?,
        control_b: port(b, 68, 184, 2, true)?,
        sci: word(b, 46)?,
        fixed_button: flags & (1 << 4) == 0,
        smi: u16::try_from(dword(b, 48)?).ok()?,
        enable: b[52],
        dsdt,
    })
}

// Length includes its own encoding but not the opcode. All slices stay within
// the containing term list; reserved lead bits must be zero in multibyte form.
fn package(b: &[u8], at: usize) -> Option<(usize, usize)> {
    let lead = *b.get(at)?;
    let extra = usize::from(lead >> 6);
    if extra > 0 && lead & 0x30 != 0 {
        return None;
    }
    let mut length = usize::from(lead & if extra == 0 { 0x3f } else { 0x0f });
    for i in 0..extra {
        length |= usize::from(*b.get(at + 1 + i)?) << (4 + 8 * i);
    }
    let body = at.checked_add(1 + extra)?;
    let end = at.checked_add(length)?;
    (body <= end && end <= b.len()).then_some((body, end))
}
// The scanner only needs namespace depth and the terminal segment. It still
// validates every nameseg, so operand bytes cannot masquerade as a NameString.
fn name(b: &[u8], mut at: usize, mut depth: usize) -> Option<(usize, usize, [u8; 4])> {
    if b.get(at) == Some(&b'\\') {
        depth = 0;
        at += 1;
    }
    while b.get(at) == Some(&b'^') {
        depth = depth.checked_sub(1)?;
        at += 1;
    }
    let count = match *b.get(at)? {
        0 => {
            return Some((at + 1, depth, [0; 4]));
        }
        0x2e => {
            at += 1;
            2
        }
        0x2f => {
            at += 1;
            let n = usize::from(*b.get(at)?);
            at += 1;
            if n == 0 {
                return None;
            }
            n
        }
        _ => 1,
    };
    let mut last = [0; 4];
    for _ in 0..count {
        last.copy_from_slice(b.get(at..at + 4)?);
        if !(last[0] == b'_' || last[0].is_ascii_uppercase())
            || !last
                .iter()
                .all(|c| *c == b'_' || c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            return None;
        }
        at += 4;
    }
    Some((at, depth.checked_add(count)?, last))
}
fn integer(b: &[u8], at: usize) -> Option<(u64, usize)> {
    match *b.get(at)? {
        0 => Some((0, at + 1)),
        1 => Some((1, at + 1)),
        0xff => Some((u64::MAX, at + 1)),
        0x0a => Some((u64::from(*b.get(at + 1)?), at + 2)),
        0x0b => Some((u64::from(word(b, at + 1)?), at + 3)),
        0x0c => Some((u64::from(dword(b, at + 1)?), at + 5)),
        0x0e => Some((qword(b, at + 1)?, at + 9)),
        _ => None,
    }
}
fn data_end(b: &[u8], at: usize, depth: usize) -> Option<usize> {
    if let Some((_, end)) = integer(b, at) {
        return Some(end);
    }
    match *b.get(at)? {
        0x0d => Some(at + 2 + b.get(at + 1..)?.iter().position(|v| *v == 0)?),
        0x11..=0x13 => Some(package(b, at + 1)?.1),
        _ => Some(name(b, at, depth)?.0),
    }
}
// Skip a bounded arithmetic operand in OperationRegion declarations without
// evaluating it. Unsupported expressions invalidate the enclosing root list.
fn operand_end(b: &[u8], at: usize, depth: usize, nesting: usize) -> Option<usize> {
    if nesting > 16 {
        return None;
    }
    match *b.get(at)? {
        0x72 | 0x74 | 0x77 | 0x79 | 0x7a | 0x7b | 0x7d | 0x7f => {
            let a = operand_end(b, at + 1, depth, nesting + 1)?;
            let c = operand_end(b, a, depth, nesting + 1)?;
            // Target is NullName or a validated namespace name; local/argument
            // objects belong inside methods, which this scanner never enters.
            Some(name(b, c, depth)?.0)
        }
        _ => data_end(b, at, depth),
    }
}
fn s5_package(b: &[u8], at: usize) -> Option<[u8; 2]> {
    if b.get(at) != Some(&0x12) {
        return None;
    }
    let (body, end) = package(b, at + 1)?;
    let part = b.get(..end)?;
    if *part.get(body)? < 2 {
        return None;
    }
    let (a, next) = integer(part, body + 1)?;
    let (z, mut next) = integer(part, next)?;
    for _ in 2..part[body] {
        next = data_end(part, next, 0)?;
    }
    if next != end {
        return None;
    }
    (a <= 7 && z <= 7).then_some([a as u8, z as u8])
}
fn walk(b: &[u8], depth: usize, nesting: usize, found: &mut Option<[u8; 2]>) -> Option<()> {
    if nesting > 16 {
        return None;
    }
    let mut at = 0;
    while at < b.len() {
        match b[at] {
            0x08 => {
                let (value, name_depth, last) = name(b, at + 1, depth)?;
                if name_depth == 1 && last == *b"_S5_" {
                    let types = s5_package(b, value)?;
                    if found.is_some() {
                        return None;
                    } // duplicate declarations are ambiguous.
                    *found = Some(types);
                }
                at = data_end(b, value, depth)?;
            }
            0x06 => {
                let next = name(b, at + 1, depth)?.0;
                at = name(b, next, depth)?.0;
            }
            0x15 => {
                at = name(b, at + 1, depth)?.0.checked_add(2)?;
                if at > b.len() {
                    return None;
                }
            }
            0x10 => {
                let (body, end) = package(b, at + 1)?;
                let (terms, child_depth, _) = name(&b[..end], body, depth)?;
                if child_depth == 0 {
                    walk(&b[terms..end], child_depth, nesting + 1, found)?;
                }
                at = end;
            }
            0x14 | 0xa0..=0xa2 => {
                // Methods/conditional execution are not evaluated.
                at = package(b, at + 1)?.1;
            }
            0x5b => match *b.get(at + 1)? {
                0x80 => {
                    // OperationRegion(name, space, offset, length)
                    let next = name(b, at + 2, depth)?.0.checked_add(1)?;
                    let next = operand_end(b, next, depth, 0)?;
                    at = operand_end(b, next, depth, 0)?;
                }
                0x01 => {
                    at = name(b, at + 2, depth)?.0.checked_add(1)?;
                    if at > b.len() {
                        return None;
                    }
                }
                0x02 => {
                    at = name(b, at + 2, depth)?.0;
                }
                0x81 | 0x86 | 0x87 => {
                    at = package(b, at + 2)?.1;
                }
                0x82..=0x85 => {
                    let kind = b[at + 1];
                    let (body, end) = package(b, at + 2)?;
                    let (terms, child_depth, _) = name(&b[..end], body, depth)?;
                    let terms = terms.checked_add(match kind {
                        0x83 => 6,
                        0x84 => 3,
                        _ => 0,
                    })?;
                    // _S5 must be at namespace root; don't interpret device terms.
                    if child_depth == 0 {
                        walk(b.get(terms..end)?, child_depth, nesting + 1, found)?;
                    }
                    at = end;
                }
                _ => return None,
            },
            0xa3 => at += 1, // Noop
            _ => return None,
        }
    }
    Some(())
}
/// Accept only a constant root Name(_S5, Package(...)), never a raw byte match.
pub(crate) fn sleep_types(aml: &[u8]) -> Option<[u8; 2]> {
    let mut found = None;
    walk(aml, 0, 0, &mut found)?;
    found
}
#[cfg(test)]
mod tests {
    use super::*;
    const S5: &[u8] = b"\x08\\_S5_\x12\x06\x03\x0a\x05\x01\x00";
    #[test]
    fn constant_root_and_opaque_objects() {
        assert_eq!(sleep_types(S5), Some([5, 1]));
        let mut aml = std::vec![0x08, b'B', b'U', b'F', b'F', 0x11, 0x10, 0x0a, 13];
        aml.extend_from_slice(S5);
        assert_eq!(sleep_types(&aml), None);
        let mut method = std::vec![0x14, 0x13, b'T', b'E', b'S', b'T', 0];
        method.extend_from_slice(S5);
        assert_eq!(sleep_types(&method), None);
        let mut duplicate = S5.to_vec();
        duplicate.extend_from_slice(S5);
        assert_eq!(sleep_types(&duplicate), None);
        let mut scoped = std::vec![0x10, 0x10, b'\\', 0];
        scoped.extend_from_slice(&S5[0..]);
        // Correct the containing package size, which includes the two-byte root name.
        scoped[1] = (scoped.len() - 1) as u8;
        assert_eq!(sleep_types(&scoped), Some([5, 1]));
    }
    #[test]
    fn skips_operation_region_expression_without_evaluating_or_scanning_it() {
        let mut aml = std::vec![
            0x5b, 0x80, b'R', b'E', b'G', b'N', 0, 0x72, b'B', b'A', b'S', b'E', 0x0b, 0, 0x50, 0,
            0x0b, 0, 0x10
        ];
        aml.extend_from_slice(S5);
        assert_eq!(sleep_types(&aml), Some([5, 1]));
        // Unknown executable root opcodes do not permit byte-resynchronization.
        aml.insert(0, 0x70);
        assert_eq!(sleep_types(&aml), None);
        let mut nested = std::vec![0x10, 0x12, b'_', b'S', b'B', b'_'];
        nested.extend_from_slice(S5);
        nested[1] = (nested.len() - 1) as u8;
        assert_eq!(sleep_types(&nested), None);
    }
    #[test]
    fn malformed_is_bounded_and_never_guess() {
        for end in 0..S5.len() {
            assert_eq!(sleep_types(&S5[..end]), None);
        }
        let mut invalid = S5.to_vec();
        invalid[6] = 0x41;
        assert_eq!(sleep_types(&invalid), None);
        invalid = S5.to_vec();
        invalid[10] = 8;
        assert_eq!(sleep_types(&invalid), None);
        for len in 0..256 {
            let bytes = std::vec![0xff; len];
            assert_eq!(sleep_types(&bytes), None);
        }
    }
    #[test]
    fn fixed_fadt_and_gas_validation() {
        let mut b = [0u8; 244];
        b[..4].copy_from_slice(b"FACP");
        b[56..60].copy_from_slice(&0x1800u32.to_le_bytes());
        b[64..68].copy_from_slice(&0x1804u32.to_le_bytes());
        b[46] = 9;
        b[88] = 4;
        b[89] = 2;
        let p = parse_fadt(&b).unwrap();
        assert!(p.fixed_button);
        assert_eq!((p.event_a, p.control_a, p.sci), (0x1800, 0x1804, 9));
        b[112] = 16;
        assert!(!parse_fadt(&b).unwrap().fixed_button);
        b[114] = 16;
        assert!(parse_fadt(&b).is_none());
        b[114] = 0;
        b[176..184].copy_from_slice(&0x1804u64.to_le_bytes());
        assert!(parse_fadt(&b).is_none());
        b[172] = 1;
        b[173] = 16;
        assert_eq!(parse_fadt(&b).unwrap().control_a, 0x1804);
    }
}
