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
    let elf_bytes = include_bytes!("ld-linux-x86-64.so.2");
    let aligned_elf_bytes = elf_bytes.to_vec();
    let headers = parse_headers(&aligned_elf_bytes);
    let interp_base = 0x1000;
    let elf_parser = ELFParser::new(&headers, interp_base).unwrap();
    let base_addr = elf_parser.base();
    assert_eq!(base_addr, interp_base);

    let segments = headers
        .ph
        .iter()
        .filter(|header| header.get_type() == Ok(Type::Load))
        .map(|header| header.virtual_addr as usize + elf_parser.base())
        .collect::<Vec<_>>();
    assert_eq!(segments.len(), 4);
    assert_eq!(segments[0], 0x1000);
}
