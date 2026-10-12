use std::{io::Result, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=linker.lds.S");
    println!("cargo:rerun-if-env-changed=DWARF");
    println!("cargo:rerun-if-env-changed=AX_LINKER_SCRIPT_OUTPUT");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    assert_eq!(arch, "x86_64", "axhal supports x86_64 targets only");
    let platform = axconfig::PLATFORM;

    if platform != "dummy" {
        gen_linker_script(platform).unwrap();
    }
}

fn gen_linker_script(platform: &str) -> Result<()> {
    let fname = format!("linker_{platform}.lds");
    let output_arch = "i386:x86-64";
    let ld_content = std::fs::read_to_string("linker.lds.S")?;
    let ld_content = ld_content.replace("%ARCH%", output_arch);
    let ld_content = ld_content.replace(
        "%KERNEL_BASE%",
        &format!("{:#x}", axconfig::plat::KERNEL_BASE_VADDR),
    );
    let ld_content = ld_content.replace("%CPU_NUM%", &format!("{}", axconfig::plat::MAX_CPU_NUM));
    let ld_content = ld_content.replace(
        "%DWARF%",
        if std::env::var("DWARF").is_ok_and(|v| v == "y") {
            r#"debug_abbrev : { __start_debug_abbrev = .; BYTE(0); . = __start_debug_abbrev + MAX(SIZEOF(.debug_abbrev), 1); __stop_debug_abbrev = __start_debug_abbrev + SIZEOF(.debug_abbrev); }
    debug_addr : { __start_debug_addr = .; BYTE(0); . = __start_debug_addr + MAX(SIZEOF(.debug_addr), 1); __stop_debug_addr = __start_debug_addr + SIZEOF(.debug_addr); }
    debug_aranges : { __start_debug_aranges = .; BYTE(0); . = __start_debug_aranges + MAX(SIZEOF(.debug_aranges), 1); __stop_debug_aranges = __start_debug_aranges + SIZEOF(.debug_aranges); }
    debug_info : { __start_debug_info = .; BYTE(0); . = __start_debug_info + MAX(SIZEOF(.debug_info), 1); __stop_debug_info = __start_debug_info + SIZEOF(.debug_info); }
    debug_line : { __start_debug_line = .; BYTE(0); . = __start_debug_line + MAX(SIZEOF(.debug_line), 1); __stop_debug_line = __start_debug_line + SIZEOF(.debug_line); }
    debug_line_str : { __start_debug_line_str = .; BYTE(0); . = __start_debug_line_str + MAX(SIZEOF(.debug_line_str), 1); __stop_debug_line_str = __start_debug_line_str + SIZEOF(.debug_line_str); }
    debug_ranges : { __start_debug_ranges = .; BYTE(0); . = __start_debug_ranges + MAX(SIZEOF(.debug_ranges), 1); __stop_debug_ranges = __start_debug_ranges + SIZEOF(.debug_ranges); }
    debug_rnglists : { __start_debug_rnglists = .; BYTE(0); . = __start_debug_rnglists + MAX(SIZEOF(.debug_rnglists), 1); __stop_debug_rnglists = __start_debug_rnglists + SIZEOF(.debug_rnglists); }
    debug_str : { __start_debug_str = .; BYTE(0); . = __start_debug_str + MAX(SIZEOF(.debug_str), 1); __stop_debug_str = __start_debug_str + SIZEOF(.debug_str); }
    debug_str_offsets : { __start_debug_str_offsets = .; BYTE(0); . = __start_debug_str_offsets + MAX(SIZEOF(.debug_str_offsets), 1); __stop_debug_str_offsets = __start_debug_str_offsets + SIZEOF(.debug_str_offsets); }"#
        } else {
            ""
        },
    );

    let out_path = match std::env::var_os("AX_LINKER_SCRIPT_OUTPUT") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join(fname),
    };
    std::fs::write(out_path, ld_content)?;
    Ok(())
}
