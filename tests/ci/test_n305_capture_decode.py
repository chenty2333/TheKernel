"""Run the same POSIX byte decoder shipped in Alpine, with no hardware probes."""
from pathlib import Path
import struct
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / 'scripts/ci/n305-capture-acpi.sh'


def table(signature, body):
    data = bytearray(struct.pack('<4sIBB6s8sI4sI', signature, 36 + len(body), 1, 0,
                                b'TKTEST', b'DECODE  ', 1, b'TEST', 1) + body)
    data[9] = (-sum(data)) & 255
    return data


class CaptureDecoderTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def run_helper(self, mode, *paths):
        return subprocess.run(['sh', str(HELPER), mode, *map(str, paths)],
                              capture_output=True, text=True, timeout=10)

    def test_mcfg_formats_each_byte_without_signed_or_float_address_conversion(self):
        data = table(b'MCFG', bytes(8) + struct.pack('<QHBBI', 0xc0000000, 0, 0, 255, 0)
                     + struct.pack('<QHBBI', 0xfedcba9876500000, 7, 32, 63, 0))
        path = self.root / 'MCFG'; path.write_bytes(data)
        result = self.run_helper('mcfg', path)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('ecam_base=0xc0000000 segment=0 bus=00-ff', result.stdout)
        self.assertIn('ecam_base=0xfedcba9876500000 segment=7 bus=20-3f', result.stdout)
        self.assertNotIn('0x80000000', result.stdout)

    def test_truncated_length_wrong_signature_and_reversed_bus_fail(self):
        valid = table(b'MCFG', bytes(8) + struct.pack('<QHBBI', 0xc0000000, 0, 0, 255, 0))
        cases = [b'', valid[:35], valid[:-1], table(b'FACP', bytes(24)),
                 table(b'MCFG', bytes(8) + struct.pack('<QHBBI', 0xc0000000, 0, 255, 0, 0))]
        path = self.root / 'bad'
        for data in cases:
            path.write_bytes(data)
            self.assertEqual(self.run_helper('mcfg', path).returncode, 2)

    def test_facs_has_no_checksum_and_actual_sdt_errors_remain_visible(self):
        facs = b'FACS' + struct.pack('<I', 64) + b'\xff' * 56
        (self.root / 'FACS').write_bytes(facs)
        dsdt = table(b'DSDT', b'\x08_S5_'); dsdt[-1] ^= 1
        (self.root / 'DSDT').write_bytes(dsdt)
        (self.root / 'MCFG').write_bytes(table(b'MCFG', bytes(8)))
        result = self.run_helper('audit', self.root)
        self.assertEqual(result.returncode, 0)
        rows = result.stdout.splitlines()
        self.assertTrue(any(row.startswith('n/a') and 'FACS has no checksum' in row for row in rows))
        self.assertTrue(any(row.startswith('BAD') and 'DSDT' in row for row in rows))
        self.assertTrue(any(row.startswith('ok') and 'MCFG' in row for row in rows))

    def test_fadt_power_button_flags_and_ports_are_explicit(self):
        path = self.root / 'FACP'
        for flags, kind in [(0x3c6e5, 'fixed-pm1'), (0x80000010, 'control-method')]:
            body = bytearray(80)
            struct.pack_into('<H', body, 46 - 36, 9)
            struct.pack_into('<I', body, 56 - 36, 0x1800)
            struct.pack_into('<I', body, 64 - 36, 0x1804)
            struct.pack_into('<I', body, 112 - 36, flags)
            path.write_bytes(table(b'FACP', body))
            result = self.run_helper('fadt', path)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(f'flags=0x{flags:08x}', result.stdout)
            self.assertIn('button_kind=' + kind, result.stdout)
            self.assertIn('SCI=9', result.stdout)
            self.assertIn('PM1a_EVT_BLK=0x00001800 PM1a_CNT_BLK=0x00001804', result.stdout)
        path.write_bytes(table(b'FACP', bytes(79)))
        self.assertEqual(self.run_helper('fadt', path).returncode, 2)

    def test_missing_retained_log_uses_kernel_iomem_or_reports_unavailable(self):
        log, iomem = self.root / 'dmesg', self.root / 'iomem'
        log.write_text('late kernel log has no early PCI messages\n')
        iomem.write_text('c0000000-cfffffff : PCI ECAM 0000 [bus 00-ff]\n')
        result = self.run_helper('kernel-ecam', log, iomem)
        self.assertEqual(result.returncode, 0)
        self.assertIn('c0000000-cfffffff', result.stdout)
        self.assertIn('source: kernel /proc/iomem', result.stdout)
        iomem.write_text('ordinary RAM\n')
        result = self.run_helper('kernel-ecam', log, iomem)
        self.assertEqual(result.returncode, 3)
        self.assertIn('UNAVAILABLE:', result.stdout)
        log.write_text('PCI: MMCONFIG ready\n')
        self.assertEqual(self.run_helper('kernel-ecam', log, iomem).returncode, 0)
        log.unlink()
        self.assertEqual(self.run_helper('kernel-ecam', log, iomem).returncode, 2)

    def test_power_button_enumeration_distinguishes_absent_directory_and_no_button(self):
        bus = self.root / 'acpi'; bus.mkdir()
        other = bus / 'ACPI000E:00'; other.mkdir(); (other / 'hid').write_text('ACPI000E\n')
        result = self.run_helper('power-devices', bus)
        self.assertEqual(result.returncode, 0)
        self.assertIn('enumerated_count=0', result.stdout)
        button = bus / 'PNP0C0C:00'; button.mkdir()
        (button / 'hid').write_text('PNP0C0C\n'); (button / 'path').write_text('_SB.PWRB\n')
        result = self.run_helper('power-devices', bus)
        self.assertEqual(result.returncode, 0)
        self.assertIn('PNP0C0C:00 hid=PNP0C0C', result.stdout)
        self.assertIn('path: _SB.PWRB', result.stdout)
        self.assertIn('enumerated_count=1', result.stdout)
        self.assertEqual(self.run_helper('power-devices', self.root / 'missing').returncode, 3)

    def test_codec_enumeration_keeps_all_cards_and_skips_unrelated_nodes(self):
        asound = self.root / 'asound'; asound.mkdir()
        self.assertEqual(self.run_helper('codec-paths', asound).returncode, 3)
        expected = []
        for card, index in [('card0', '0'), ('card1', '2')]:
            directory = asound / card; directory.mkdir()
            path = directory / ('codec#' + index); path.write_text('Codec: fixture\n')
            (directory / 'pcm0p').mkdir(); expected.append(str(path))
        result = self.run_helper('codec-paths', asound)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout.splitlines(), expected)
        self.assertEqual(self.run_helper('codec-paths', self.root / 'missing').returncode, 3)

    def test_both_payload_builders_deploy_the_helper_and_codec_capture_is_present(self):
        self.assertIn('n305-capture-acpi.sh', (ROOT / 'tools/n305_netboot.py').read_text())
        image = (ROOT / 'scripts/ci/n305-capture-image.sh').read_text()
        self.assertIn('$WORK/apkovl/etc/local.d/n305-capture-acpi.sh', image)
        self.assertIn('::/payload/n305-capture-acpi.sh', image)
        payload = (ROOT / 'scripts/ci/n305-capture-payload.sh').read_text()
        self.assertIn('cap audio/load-hda.txt modprobe snd_hda_intel', payload)
        self.assertIn('/proc/asound/card*/codec#*', payload)
        self.assertIn('power-devices /sys/bus/acpi/devices', payload)
