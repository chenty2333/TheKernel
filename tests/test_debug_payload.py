"""Pinned real-tool staging and transcript matching regressions."""
from pathlib import Path
import subprocess
import unittest

from tests.support import repo_root, test_tmpdir
from tools import product_state

ROOT = repo_root()


class DebugPayloadTests(unittest.TestCase):
    def test_signed_package_closure_is_pinned_and_fingerprinted(self):
        rows = [line.split("\t") for line in
                (ROOT / "config/guest-debug-apk-pins.tsv").read_text().splitlines()
                if line and not line.startswith("#")]
        self.assertEqual(len(rows), 40)
        self.assertEqual(len({row[0] for row in rows}), len(rows))
        for name, checksum, url, license_name, origin in rows:
            self.assertRegex(checksum, r"^[0-9a-f]{64}$")
            self.assertEqual(url, "https://dl-cdn.alpinelinux.org/alpine/v3.24/main/x86_64/" + name)
            self.assertTrue(license_name)
            self.assertTrue(origin)
        self.assertIn("gdb-16.3-r4.apk", {row[0] for row in rows})
        self.assertIn("strace-6.19-r1.apk", {row[0] for row in rows})
        self.assertIn("config/guest-debug-apk-pins.tsv", product_state.ROOTFS_INPUT_FILES)
        self.assertIn("scripts/build-debug-payload.sh", product_state.ROOTFS_INPUT_FILES)
        self.assertIn("tests/guest/debugger/*", product_state.ROOTFS_INPUT_GLOBS)
        self.assertEqual(product_state.rootfs_image_bytes("debug"), 224 * 1024 * 1024)

    def test_strace_column_alignment_does_not_change_token_checks(self):
        source = ROOT / "tests/guest/tools/debugger-smoke.c"
        with test_tmpdir() as directory:
            probe = Path(directory) / "probe.c"
            probe.write_text(f'''#define main guest_smoke_main
#include "{source}"
#undef main
int main(void) {{
    strcpy(output, "write(3, \\\"trace-data\\\", 10)              = 10\\n"
                   "lseek(3, 0, SEEK_SET)\\t = 0\\r\\n");
    normalize_spacing();
    return !strstr(output, "\\\"trace-data\\\", 10) = 10") ||
           !strstr(output, "SEEK_SET) = 0");
}}
''')
            executable = Path(directory) / "probe"
            subprocess.run(["gcc", "-std=c11", "-Wall", "-Wextra", "-Werror",
                            str(probe), "-o", str(executable)], check=True,
                           capture_output=True, text=True)
            subprocess.run([str(executable)], check=True)
