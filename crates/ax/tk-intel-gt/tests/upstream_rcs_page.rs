// SPDX-License-Identifier: MIT
// Copyright2026 TheKernel contributors. See ../LICENSE-MIT.
use std::process::Command;
#[test]
#[ignore = "requires source-only IGT reference and GCC/Python; run explicitly"]
fn fixed_kernel_render_page_matches_compiled_igt_with_explicit_gen120_uc_fields() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/igt_page.py");
    let out = Command::new("python3").arg(script).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let mut lines = text.lines();
    let count: usize = lines.next().unwrap().parse().unwrap();
    let mut original: Vec<u32> = lines.map(|w| u32::from_str_radix(w, 16).unwrap()).collect();
    assert_eq!(original.len(), 1024);
    assert!(count < 2048);
    // The only intentional source adaptation: Gen12022-word SBA and explicit
    // UC6 MOCS fields (Mesa26.1.2 gen110/gen120.xml). Shader/state bytes stay
    // identical, no fabricated shader or copied GPU result.
    let first = original.iter().position(|v| *v == 0x61010010).unwrap();
    let mut sba = original[first..first + 19].to_vec();
    sba.extend([0, 0, 0]);
    sba[0] = 0x61010014;
    for offset in [1, 4, 6, 8, 10, 16, 19] {
        sba[offset] |= 6 << 4;
    }
    sba[3] |= 6 << 16;
    let commands = count / 4;
    let mut new = sba;
    new.extend_from_slice(&original[first + 19..commands]);
    original[first..commands + 3].copy_from_slice(&new);
    let vertex = original.iter().position(|v| *v == 0x78080003).unwrap();
    original[vertex + 1] |= 6 << 16;
    assert_eq!(tk_intel_gt::rcs_page::COMMAND_BYTES as usize, count + 12);
    for (i, (a, b)) in original.iter().zip(tk_intel_gt::rcs_page::PAGE).enumerate() {
        assert_eq!(*a, b, "page word{i}");
    }
}
