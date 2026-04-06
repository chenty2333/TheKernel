use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let c_path = PathBuf::from("c/lwext4")
        .canonicalize()
        .expect("cannot canonicalize path");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let lwext4_lib = &format!("lwext4-{arch}");
    {
        let status = Command::new("make")
            .args([
                "musl-generic",
                "-C",
                c_path.to_str().expect("invalid path of lwext4"),
            ])
            .arg(format!("ARCH={arch}"))
            .arg(format!(
                "ULIBC={}",
                if env::var("CARGO_FEATURE_STD").is_ok() {
                    "OFF"
                } else {
                    "ON"
                }
            ))
            .arg(format!("OUT_DIR={}", out_dir.display()))
            .status()
            .expect("failed to execute process: make lwext4");
        assert!(status.success());
    }
    {
        let cc = &format!("{arch}-linux-musl-gcc");
        let output = Command::new(cc)
            .args(["-print-sysroot"])
            .output()
            .expect("failed to execute process: gcc -print-sysroot");

        let sysroot = core::str::from_utf8(&output.stdout).unwrap();
        let sysroot = sysroot.trim_end();
        let sysroot_inc = &format!("-I{sysroot}/include/");

        generates_bindings_to_rust(arch.as_str(), cc.as_str(), sysroot_inc, &out_dir);
    }

    println!("cargo:rustc-link-lib=static={lwext4_lib}");
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rerun-if-changed=c/wrapper.h");
    println!("cargo:rerun-if-changed={}/src", c_path.to_str().unwrap());
}

fn command_stdout(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
}

fn generates_bindings_to_rust(arch: &str, cc: &str, mpath: &str, out_dir: &Path) {
    let target = env::var("TARGET").unwrap();
    if target.ends_with("-softfloat") {
        // Clang does not recognize the `-softfloat` suffix
        unsafe { env::set_var("TARGET", target.replace("-softfloat", "")) };
    }

    let mut builder = bindgen::Builder::default()
        .use_core()
        .wrap_unsafe_ops(true)
        // The input header we would like to generate bindings for.
        .header("c/wrapper.h")
        .clang_arg("-I./c/lwext4/include")
        .clang_arg(format!(
            "-I{}/build_musl-generic/include/",
            out_dir.display()
        ))
        .layout_tests(false)
        // Tell cargo to invalidate the built crate whenever any of the included header files changed.
        .parse_callbacks(Box::new(CustomCargoCallbacks))
        .clang_arg(mpath);

    if arch == "loongarch64" {
        if let Some(triple) = command_stdout(cc, &["-dumpmachine"]) {
            builder = builder.clang_arg(format!("--target={triple}"));

            if let Some(gcc_include) = command_stdout(cc, &["-print-file-name=include"]) {
                builder = builder.clang_arg(format!("-isystem{gcc_include}"));

                let gcc_include_path = PathBuf::from(&gcc_include);
                if let Some(toolchain_root) = gcc_include_path.ancestors().nth(5) {
                    let sys_include = toolchain_root.join(&triple).join("sys-include");
                    if sys_include.is_dir() {
                        builder =
                            builder.clang_arg(format!("-isystem{}", sys_include.display()));
                    }
                }
            }
        }
    }

    let bindings = builder.generate().expect("Unable to generate bindings");

    // Restore the original target environment variable
    unsafe { env::set_var("TARGET", target) };

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}

#[derive(Debug)]
struct CustomCargoCallbacks;
impl bindgen::callbacks::ParseCallbacks for CustomCargoCallbacks {
    fn header_file(&self, filename: &str) {
        add_include(filename);
    }

    fn include_file(&self, filename: &str) {
        add_include(filename);
    }

    fn read_env_var(&self, key: &str) {
        println!("cargo:rerun-if-env-changed={key}");
    }
}

fn add_include(filename: &str) {
    if !Path::new(filename).ends_with("ext4_config.h") {
        println!("cargo:rerun-if-changed={filename}");
    }
}
