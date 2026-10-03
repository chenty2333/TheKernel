import unittest
import wave
from pathlib import Path
from tests.support import test_tmpdir
from tools.check_hda_waveform import check, expected_payload


class HdaWaveformTests(unittest.TestCase):
    def recording(self, path, data):
        with wave.open(str(path), "wb") as output:
            output.setparams((2, 2, 48000, 0, "NONE", "not compressed"))
            output.writeframes(data)

    def test_one_exact_waveform_with_silence(self):
        with test_tmpdir() as directory:
            path = Path(directory) / "audio.wav"
            self.recording(path, b"\0" * 256 + expected_payload() + b"\0" * 512)
            self.assertEqual(check(path), 8192)

    def test_changed_missing_and_duplicated_samples_fail(self):
        payload = expected_payload()
        for data in (payload[4:], b"\0\0\0\0" + payload[4:], payload * 2):
            with self.subTest(length=len(data)), test_tmpdir() as directory:
                path = Path(directory) / "audio.wav"
                self.recording(path, data)
                with self.assertRaises(ValueError):
                    check(path)
