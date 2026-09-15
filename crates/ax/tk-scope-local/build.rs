use std::path::Path;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        let ld_script_path = Path::new(std::env!("CARGO_MANIFEST_DIR")).join("percpu.x");
        println!("cargo::rerun-if-changed={}", ld_script_path.display());
        println!("cargo::rustc-link-arg-tests=-no-pie");
        println!("cargo::rustc-link-arg-tests=-T{}", ld_script_path.display());
    }
}
