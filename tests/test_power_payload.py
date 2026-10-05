import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

class PowerPayloadTests(unittest.TestCase):
    def test_signed_alpine_closure_and_license_rows(self):
        rows = [line.split("\t") for line in (ROOT / "config/guest-power-apk-pins.tsv").read_text().splitlines() if line and not line.startswith("#")]
        self.assertEqual(len(rows), 9)
        for filename, checksum, url, license, origin in rows:
            self.assertEqual(len(checksum), 64)
            self.assertTrue(url.startswith("https://dl-cdn.alpinelinux.org/alpine/v3.24/"))
            self.assertTrue(url.endswith(filename))
            self.assertTrue(license and origin)
        script = (ROOT / "scripts/build-power-payload.sh").read_text()
        self.assertIn('"${APK[@]}" verify "${PACKAGES[@]}"', script)
        self.assertNotIn("sudo", script)
        self.assertIn("POWER-MANIFEST", script)
