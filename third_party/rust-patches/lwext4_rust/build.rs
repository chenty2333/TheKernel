#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

struct CrossToolchain {
    cc: String,
    cxx: String,
    ar: String,
    as_: String,
    objcopy: String,
    objdump: String,
    size: String,
}

fn main() {
    let c_path = PathBuf::from("c/lwext4")
        .canonicalize()
        .expect("cannot canonicalize path");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    assert_eq!(arch, "x86_64", "lwext4_rust supports x86_64 targets only");
    let toolchain = discover_cross_toolchain(&arch, &out_dir);
    let lwext4_lib = &format!("lwext4-{arch}");
    {
        let status = Command::new("make")
            .args([
                "musl-generic",
                "-C",
                c_path.to_str().expect("invalid path of lwext4"),
            ])
            .env("CC", &toolchain.cc)
            .env("CXX", &toolchain.cxx)
            .env("AR", &toolchain.ar)
            .env("AS", &toolchain.as_)
            .env("OBJCOPY", &toolchain.objcopy)
            .env("OBJDUMP", &toolchain.objdump)
            .env("SIZE", &toolchain.size)
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
        let output = Command::new(&toolchain.cc)
            .args(["-print-sysroot"])
            .output()
            .expect("failed to execute process: gcc -print-sysroot");

        let sysroot = core::str::from_utf8(&output.stdout).unwrap();
        let sysroot = sysroot.trim_end();

        generates_bindings_to_rust(arch.as_str(), &toolchain.cc, sysroot, &out_dir);
    }

    println!("cargo:rustc-link-lib=static={lwext4_lib}");
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rerun-if-changed=c/wrapper.h");
    println!("cargo:rerun-if-changed={}/src", c_path.to_str().unwrap());
}

fn find_in_path(cmd: &str) -> Option<String> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(cmd);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn find_tool(candidates: &[String]) -> Option<String> {
    for candidate in candidates {
        if let Some(found) = find_in_path(candidate) {
            return Some(found);
        }
    }
    None
}

fn env_tool(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

fn discover_env_toolchain() -> Option<CrossToolchain> {
    let cc = env_tool("CC")?;
    let ar = env_tool("AR")?;
    let as_ = env_tool("AS").unwrap_or_else(|| cc.clone());
    let objcopy = env_tool("OBJCOPY")?;
    let objdump = env_tool("OBJDUMP")?;
    let size = env_tool("SIZE")?;
    let cxx = env_tool("CXX").unwrap_or_else(|| cc.clone());

    Some(CrossToolchain {
        cc,
        cxx,
        ar,
        as_,
        objcopy,
        objdump,
        size,
    })
}

fn find_tool_by_names(candidates: &[&str]) -> Option<String> {
    find_tool(
        &candidates
            .iter()
            .map(|candidate| candidate.to_string())
            .collect::<Vec<_>>(),
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn write_tool_wrapper(wrapper_dir: &Path, name: &str, tool: &str, args: &[&str]) -> Option<String> {
    fs::create_dir_all(wrapper_dir).ok()?;
    let path = wrapper_dir.join(name);
    let mut script = String::from("#!/bin/sh\nexec ");
    script.push_str(&shell_quote(tool));
    for arg in args {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    script.push_str(" \"$@\"\n");
    fs::write(&path, script).ok()?;

    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&path).ok()?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).ok()?;
    }

    Some(path.to_string_lossy().into_owned())
}

fn write_compat_header(include_dir: &Path, name: &str, content: &str) -> Option<()> {
    fs::write(include_dir.join(name), content).ok()
}

fn write_freestanding_headers(include_dir: &Path) -> Option<()> {
    fs::create_dir_all(include_dir).ok()?;
    write_compat_header(
        include_dir,
        "inttypes.h",
        r#"#ifndef LWEXT4_FREESTANDING_INTTYPES_H
#define LWEXT4_FREESTANDING_INTTYPES_H
#include <stdint.h>
#define PRId8 "d"
#define PRIi8 "i"
#define PRIo8 "o"
#define PRIu8 "u"
#define PRIx8 "x"
#define PRIX8 "X"
#define PRId16 "d"
#define PRIi16 "i"
#define PRIo16 "o"
#define PRIu16 "u"
#define PRIx16 "x"
#define PRIX16 "X"
#define PRId32 "d"
#define PRIi32 "i"
#define PRIo32 "o"
#define PRIu32 "u"
#define PRIx32 "x"
#define PRIX32 "X"
#define PRId64 "lld"
#define PRIi64 "lli"
#define PRIo64 "llo"
#define PRIu64 "llu"
#define PRIx64 "llx"
#define PRIX64 "llX"
#endif
"#,
    )?;
    write_compat_header(
        include_dir,
        "string.h",
        r#"#ifndef LWEXT4_FREESTANDING_STRING_H
#define LWEXT4_FREESTANDING_STRING_H
#include <stddef.h>
void *memcpy(void *restrict dest, const void *restrict src, size_t n);
void *memmove(void *dest, const void *src, size_t n);
void *memset(void *s, int c, size_t n);
int memcmp(const void *s1, const void *s2, size_t n);
size_t strlen(const char *s);
int strcmp(const char *s1, const char *s2);
int strncmp(const char *s1, const char *s2, size_t n);
char *strcpy(char *restrict dest, const char *restrict src);
char *strncpy(char *restrict dest, const char *restrict src, size_t n);
#endif
"#,
    )?;
    write_compat_header(
        include_dir,
        "stdlib.h",
        r#"#ifndef LWEXT4_FREESTANDING_STDLIB_H
#define LWEXT4_FREESTANDING_STDLIB_H
#include <stddef.h>
#ifndef NULL
#define NULL ((void *)0)
#endif
typedef int (*__compar_fn_t)(const void *, const void *);
void qsort(void *base, size_t nel, size_t width, __compar_fn_t compar);
void *malloc(size_t size);
void *calloc(size_t nmemb, size_t size);
void *realloc(void *ptr, size_t size);
void free(void *ptr);
#endif
"#,
    )?;
    write_compat_header(
        include_dir,
        "errno.h",
        r#"#ifndef LWEXT4_FREESTANDING_ERRNO_H
#define LWEXT4_FREESTANDING_ERRNO_H
#define EPERM 1
#define ENOENT 2
#define EIO 5
#define ENXIO 6
#define E2BIG 7
#define ENOMEM 12
#define EACCES 13
#define EFAULT 14
#define EEXIST 17
#define ENODEV 19
#define ENOTDIR 20
#define EISDIR 21
#define EINVAL 22
#define EFBIG 27
#define ENOSPC 28
#define EROFS 30
#define EMLINK 31
#define ERANGE 34
#define ENOTEMPTY 39
#define ENODATA 61
#define ENOTSUP 95
#endif
"#,
    )?;
    write_compat_header(
        include_dir,
        "assert.h",
        r#"#ifndef LWEXT4_FREESTANDING_ASSERT_H
#define LWEXT4_FREESTANDING_ASSERT_H
#define assert(expr) ((void)(expr))
#endif
"#,
    )?;
    write_compat_header(
        include_dir,
        "stdio.h",
        r#"#ifndef LWEXT4_FREESTANDING_STDIO_H
#define LWEXT4_FREESTANDING_STDIO_H
typedef struct FILE FILE;
extern FILE *stdout;
int printf(const char *restrict format, ...);
int fflush(FILE *stream);
#endif
"#,
    )
}

fn discover_llvm_freestanding_toolchain(arch: &str, out_dir: &Path) -> Option<CrossToolchain> {
    if arch != "x86_64" {
        return None;
    }

    let clang = find_tool_by_names(&["clang"])?;
    let clangxx = find_tool_by_names(&["clang++"]).unwrap_or_else(|| clang.clone());
    let ar = find_tool_by_names(&["llvm-ar", "rust-ar"])?;
    let objcopy = find_tool_by_names(&["llvm-objcopy", "rust-objcopy"])?;
    let objdump = find_tool_by_names(&["llvm-objdump", "rust-objdump"])?;
    let size = find_tool_by_names(&["llvm-size", "rust-size"])?;
    let resource_include =
        PathBuf::from(command_stdout(&clang, &["-print-resource-dir"])?).join("include");

    let target = env::var("TARGET").unwrap_or_else(|_| format!("{arch}-unknown-none"));
    let clang_target = target.strip_suffix("-softfloat").unwrap_or(&target);
    let wrapper_dir = out_dir.join(format!("llvm-{arch}-toolchain"));
    let include_dir = wrapper_dir.join("include");
    write_freestanding_headers(&include_dir)?;
    let common_args = vec![
        format!("--target={clang_target}"),
        "-ffreestanding".to_string(),
        "-nostdinc".to_string(),
        format!("-I{}", include_dir.display()),
        format!("-isystem{}", resource_include.display()),
    ];
    let common_arg_refs = common_args.iter().map(String::as_str).collect::<Vec<_>>();

    let cc = write_tool_wrapper(
        &wrapper_dir,
        &format!("{arch}-clang-cc"),
        &clang,
        &common_arg_refs,
    )?;
    let cxx = write_tool_wrapper(
        &wrapper_dir,
        &format!("{arch}-clang-cxx"),
        &clangxx,
        &common_arg_refs,
    )?;
    let as_ = write_tool_wrapper(
        &wrapper_dir,
        &format!("{arch}-clang-as"),
        &clang,
        &common_arg_refs,
    )?;

    println!(
        "cargo:warning=lwext4_rust falling back to LLVM {arch} toolchain wrappers in {}",
        wrapper_dir.display(),
    );

    Some(CrossToolchain {
        cc,
        cxx,
        ar,
        as_,
        objcopy,
        objdump,
        size,
    })
}

fn discover_cross_toolchain(arch: &str, out_dir: &Path) -> CrossToolchain {
    if let Some(toolchain) = discover_env_toolchain() {
        return toolchain;
    }

    for family in [format!("{arch}-linux-musl"), format!("{arch}-linux-gnu")] {
        let cc = find_tool(&[format!("{family}-gcc"), format!("{family}-cc")]);
        let ar = find_tool(&[format!("{family}-ar")]);
        let as_ = find_tool(&[format!("{family}-as")]);
        let objcopy = find_tool(&[format!("{family}-objcopy")]);
        let objdump = find_tool(&[format!("{family}-objdump")]);
        let size = find_tool(&[format!("{family}-size")]);

        if let (Some(cc), Some(ar), Some(as_), Some(objcopy), Some(objdump), Some(size)) =
            (cc, ar, as_, objcopy, objdump, size)
        {
            let cxx = find_tool(&[format!("{family}-g++"), format!("{family}-c++")])
                .unwrap_or_else(|| cc.clone());
            if family.ends_with("-linux-gnu") {
                println!(
                    "cargo:warning=lwext4_rust falling back to GNU cross toolchain family {family}"
                );
            }
            let toolchain = CrossToolchain {
                cc,
                cxx,
                ar,
                as_,
                objcopy,
                objdump,
                size,
            };
            return toolchain;
        }
    }

    if let Some(toolchain) = discover_llvm_freestanding_toolchain(arch, out_dir) {
        return toolchain;
    }

    panic!("no usable cross toolchain found for architecture {arch}");
}

fn command_stdout(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string(),
    )
}

fn command_succeeds(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn add_existing_include_dir(builder: bindgen::Builder, path: PathBuf) -> bindgen::Builder {
    if path.is_dir() {
        builder.clang_arg(format!("-isystem{}", path.display()))
    } else {
        builder
    }
}

fn generates_bindings_to_rust(arch: &str, cc: &str, sysroot: &str, out_dir: &Path) {
    let target = env::var("TARGET").unwrap();
    let llvm_wrapper_dir = out_dir.join(format!("llvm-{arch}-toolchain"));
    let using_llvm_wrapper = arch == "x86_64" && Path::new(cc).starts_with(&llvm_wrapper_dir);
    let bindgen_target = if target.ends_with("-softfloat") {
        target.replace("-softfloat", "")
    } else {
        target.clone()
    };
    if target.ends_with("-softfloat") {
        // Clang does not recognize the `-softfloat` suffix
        unsafe { env::set_var("TARGET", &bindgen_target) };
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
        .parse_callbacks(Box::new(CustomCargoCallbacks));

    if let Some(triple) = command_stdout(cc, &["-dumpmachine"]) {
        if command_succeeds(
            "clang",
            &[&format!("--target={triple}"), "-dM", "-E", "-x", "c", "-"],
        ) {
            builder = builder.clang_arg(format!("--target={triple}"));
            if !sysroot.is_empty() {
                builder = builder.clang_arg(format!("--sysroot={sysroot}"));
            }
        }

        if using_llvm_wrapper {
            let include_dir = llvm_wrapper_dir.join("include");
            if let Some(resource_dir) = command_stdout(cc, &["-print-resource-dir"]) {
                builder = builder
                    .clang_arg("-ffreestanding")
                    .clang_arg("-nostdinc")
                    .clang_arg(format!("-I{}", include_dir.display()))
                    .clang_arg(format!("-isystem{}/include", resource_dir));
            }
        } else if let Some(gcc_include) = command_stdout(cc, &["-print-file-name=include"]) {
            builder = builder.clang_arg(format!("-isystem{gcc_include}"));

            let gcc_include_path = PathBuf::from(&gcc_include);
            if let Some(toolchain_root) = gcc_include_path.ancestors().nth(5) {
                builder = add_existing_include_dir(
                    builder,
                    toolchain_root.join(&triple).join("sys-include"),
                );
            }
        }

        if !sysroot.is_empty() {
            let sysroot_path = PathBuf::from(sysroot);
            builder = add_existing_include_dir(builder, sysroot_path.join("include"));
            builder = add_existing_include_dir(builder, sysroot_path.join("usr").join("include"));
            builder = add_existing_include_dir(
                builder,
                sysroot_path.join("usr").join("include").join(&triple),
            );
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
