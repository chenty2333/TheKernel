//! Emits the extension linker script `axplat.x` into OUT_DIR. axruntime's
//! linker script starts with `INCLUDE "axplat.x"`, and the runtime registers
//! our OUT_DIR on the linker search path, so the platform provides all
//! image-level symbols (_skernel/_ekernel, boot stack, bss bounds, per-CPU
//! template sections) without touching axruntime.

use std::{
    env, fs,
    io::{Error, ErrorKind, Result},
    path::PathBuf,
};

const LINKER_SCRIPT_NAME: &str = "axplat.x";
const DEFAULT_CPU_CAPACITY: usize = 1;

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rerun-if-env-changed=SMP");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ld = include_str!("link.ld");
    println!("cargo:rustc-link-search={}", out_dir.display());
    let ld_content = ld.replace("{{SMP}}", &format!("{}", cpu_capacity()?));
    fs::write(out_dir.join(LINKER_SCRIPT_NAME), ld_content)
}

fn cpu_capacity() -> Result<usize> {
    match env::var("SMP") {
        Ok(value) => parse_usize(&value)
            .map_err(|err| invalid_data(format!("failed to parse SMP value `{value}`: {err}"))),
        Err(_) => Ok(DEFAULT_CPU_CAPACITY),
    }
}

fn parse_usize(value: &str) -> Result<usize> {
    let value = value.replace('_', "");
    if let Some(hex) = value.strip_prefix("0x") {
        usize::from_str_radix(hex, 16)
    } else {
        value.parse()
    }
    .map_err(|err| invalid_data(format!("invalid SMP value `{value}`: {err}")))
}

fn invalid_data(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidData, message.into())
}
