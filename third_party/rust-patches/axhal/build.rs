use std::{io::Result, path::Path};

fn main() {
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
            r#"debug_abbrev : { . += SIZEOF(.debug_abbrev); }
    debug_addr : { . += SIZEOF(.debug_addr); }
    debug_aranges : { . += SIZEOF(.debug_aranges); }
    debug_info : { . += SIZEOF(.debug_info); }
    debug_line : { . += SIZEOF(.debug_line); }
    debug_line_str : { . += SIZEOF(.debug_line_str); }
    debug_ranges : { . += SIZEOF(.debug_ranges); }
    debug_rnglists : { . += SIZEOF(.debug_rnglists); }
    debug_str : { . += SIZEOF(.debug_str); }
    debug_str_offsets : { . += SIZEOF(.debug_str_offsets); }"#
        } else {
            ""
        },
    );

    // target/<target_triple>/<mode>/build/axhal-xxxx/out
    let out_dir = std::env::var("OUT_DIR").unwrap();
    // target/<target_triple>/<mode>/linker_xxxx.lds
    let out_path = Path::new(&out_dir).join("../../..").join(fname);
    std::fs::write(out_path, ld_content)?;
    Ok(())
}
