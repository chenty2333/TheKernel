use std::{env, fs, path::{Path, PathBuf}};
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    let mut entries: Vec<_> = fs::read_dir(from).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for source in entries {
        let dest = to.join(source.file_name().unwrap());
        if source.is_dir() { copy_tree(&source, &dest); } else { fs::copy(source, dest).unwrap(); }
    }
}
fn main() {
    println!("cargo:rerun-if-changed=vendor");
    println!("cargo:rerun-if-changed=c");
    let include = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("include");
    copy_tree(Path::new("vendor/include"), &include);
    let header = include.join("platform/acenv.h");
    let original = fs::read_to_string(&header).unwrap();
    let needle = "#if defined(_LINUX) || defined(__linux__)";
    assert_eq!(original.matches(needle).count(), 1);
    fs::write(header, original.replacen(needle,
        "#if defined(__THEKERNEL__)\n#include \"acthekernel.h\"\n#elif defined(_LINUX) || defined(__linux__)", 1)).unwrap();
    fs::copy("c/acthekernel.h", include.join("platform/acthekernel.h")).unwrap();
    let mut build = cc::Build::new();
    build.include(&include).define("__THEKERNEL__", None)
        .flag("-U__linux__").flag("-ffreestanding").flag("-fno-builtin")
        .flag("-fno-stack-protector").flag("-mno-red-zone")
        .flag("-mno-sse").flag("-mno-sse2").flag("-mno-mmx")
        .flag("-std=gnu11").warnings(false);
    // cc doesn't have a compiler spec for Rust's bare-metal triple. The host
    // x86_64 C compiler emits the same SysV ABI with all hosted features off.
    if env::var("CARGO_CFG_TARGET_OS").unwrap() == "none" {
        build.compiler(env::var("CC").unwrap_or_else(|_| "cc".into()));
    }
    for component in ["dispatcher", "events", "executer", "hardware", "namespace", "parser", "resources", "tables", "utilities"] {
        let mut sources: Vec<_> = fs::read_dir(format!("vendor/components/{component}"))
            .unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "c")).collect();
        sources.retain(|p| !matches!(p.file_stem().unwrap().to_str().unwrap(), "rsdump" | "rsdumpinfo"));
        sources.sort();
        build.files(sources);
    }
    build.file("c/abi.c").file("c/bridge.c").compile("tk_acpica");
}
