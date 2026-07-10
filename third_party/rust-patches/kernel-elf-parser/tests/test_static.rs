use kernel_elf_parser::{ELFHeaders, ELFHeadersBuilder, ELFParser};
use xmas_elf::program::Type;

fn parse_headers(bytes: &[u8]) -> ELFHeaders<'_> {
    let builder = ELFHeadersBuilder::new(bytes).expect("parse ELF header");
    let range = builder.ph_range().expect("valid program header range");
    builder
        .build(&bytes[range.start as usize..range.end as usize])
        .expect("parse program headers")
}

#[test]
fn test_elf_parser() {
    // A simple elf file compiled by the x86_64-linux-musl-gcc.
    let elf_bytes = include_bytes!("elf_static");
    let aligned_elf_bytes = elf_bytes.to_vec();
    let headers = parse_headers(&aligned_elf_bytes);

    let interp_base = 0x1000;
    let elf_parser = ELFParser::new(&headers, interp_base).unwrap();
    let base_addr = elf_parser.base();
    assert_eq!(base_addr, 0);

    let segments = headers
        .ph
        .iter()
        .filter(|header| header.get_type() == Ok(Type::Load))
        .map(|header| header.virtual_addr as usize + elf_parser.base())
        .collect::<Vec<_>>();
    assert_eq!(segments.len(), 4);
    let mut last_start = 0;
    for &segment in &segments {
        // start vaddr should be sorted
        assert!(segment > last_start);
        last_start = segment;
    }
    assert_eq!(segments[0], 0x400000);

    test_ustack(&elf_parser);
}

fn test_ustack(elf_parser: &ELFParser) {
    let auxv = elf_parser.aux_vector(0x1000, None).collect::<Vec<_>>();
    // let phent = auxv.get(&AT_PHENT).unwrap();
    // assert_eq!(*phent, 56);
    auxv.iter().for_each(|entry| {
        if entry.get_type() == kernel_elf_parser::AuxType::PHENT {
            assert_eq!(entry.value(), 56);
        }
    });

    let args: Vec<String> = vec!["arg1".to_string(), "arg2".to_string(), "arg3".to_string()];
    let envs: Vec<String> = vec!["LOG=file".to_string()];

    // The highest address of the user stack.
    let ustack_end = 0x4000_0000;

    let stack_data =
        kernel_elf_parser::app_stack_region(&args, &envs, &auxv, "/elf_static", ustack_end);
    // The first 8 bytes of the stack is the number of arguments.
    assert_eq!(stack_data[0..8], [3, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn malformed_program_header_entry_size_is_rejected() {
    let mut bytes = include_bytes!("elf_static").to_vec();
    bytes[54..56].copy_from_slice(&0u16.to_le_bytes());
    let builder = ELFHeadersBuilder::new(&bytes).expect("parse ELF header");
    assert!(builder.ph_range().is_none());
}

#[test]
fn parser_rejects_an_unmapped_program_header_table() {
    let bytes = include_bytes!("elf_static").to_vec();
    let mut headers = parse_headers(&bytes);
    headers.ph.clear();
    assert!(ELFParser::new(&headers, 0).is_err());
}
