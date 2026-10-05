// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Compile unmodified local upstream functions as independent test oracles.
//! Generated input is temporary, below the configured /home state, not archived.
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub fn reference() -> PathBuf {
    PathBuf::from(
        std::env::var_os("THEKERNEL_LINUX_REFERENCE")
            .expect("set THEKERNEL_LINUX_REFERENCE to Linux 7.2.3"),
    )
}
pub fn function(source: &str, signature: &str) -> String {
    let start = source.find(signature).expect("upstream signature changed");
    let body = start + source[start..].find('{').unwrap();
    let mut depth = 0;
    for (i, c) in source[body..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return source[start..=body + i].to_owned();
                }
            }
            _ => {}
        }
    }
    panic!("unterminated upstream function")
}
/// Pull definitions, including continuations, rather than retype tested masks.
pub fn defines(source: &str, names: &[&str]) -> String {
    let mut output = String::new();
    let mut lines = source.lines();
    let mut count = 0;
    while let Some(line) = lines.next() {
        let Some(directive) = line.trim_start().strip_prefix('#') else {
            continue;
        };
        let Some(definition) = directive.trim_start().strip_prefix("define") else {
            continue;
        };
        let name = definition
            .trim_start()
            .split([' ', '\t', '('])
            .next()
            .unwrap();
        if names.contains(&name) {
            count += 1;
            output.push_str(line);
            output.push('\n');
            let mut current = line;
            while current.trim_end().ends_with('\\') {
                current = lines.next().expect("macro continuation");
                output.push_str(current);
                output.push('\n');
            }
        }
    }
    assert_eq!(count, names.len(), "upstream macro missing or duplicated");
    output
}
pub struct Oracle {
    pub executable: PathBuf,
    directory: PathBuf,
}
impl Drop for Oracle {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}
pub fn compile(code: &str, name: &str) -> Oracle {
    let state = PathBuf::from(
        std::env::var_os("THEKERNEL_STATE_DIR").expect("set THEKERNEL_STATE_DIR under /home"),
    );
    assert!(
        state.is_absolute() && state.starts_with("/home"),
        "no RAM-backed temp oracle build"
    );
    let directory = state
        .join("test-tmp")
        .join(format!("i915-{name}-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let executable = directory.join("oracle");
    let oracle = Oracle {
        executable,
        directory,
    };
    let source = oracle.directory.join("oracle.c");
    // Preserve the MIT grant and source copyrights even in temporary copies.
    let license = include_str!("../../LICENSE-MIT");
    fs::write(&source, format!("/*\n{license}\n*/\n{code}")).unwrap();
    let result = Command::new("gcc")
        .args(["-std=c11", "-O2", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(&oracle.executable)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "oracle compilation failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    oracle
}
pub fn read(root: &Path, name: &str) -> String {
    fs::read_to_string(root.join("drivers/gpu/drm/i915").join(name)).unwrap()
}
