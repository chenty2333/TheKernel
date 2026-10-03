#!/usr/bin/env python3
"""Compare QEMU WAV samples to tests/guest/hda-smoke.c, not a boot marker."""
import argparse
from pathlib import Path
import struct
import wave


def expected_payload() -> bytes:
    return b"".join(struct.pack("<hh", (i * 97) % 20001 - 10000,
                                10000 - (i * 97) % 20001) for i in range(8192))


def check(path: Path) -> int:
    with wave.open(str(path), "rb") as recording:
        if (recording.getnchannels(), recording.getsampwidth(), recording.getframerate()) != (2, 2, 48000):
            raise ValueError("expected stereo S16LE 48000-Hz WAV")
        samples = recording.readframes(recording.getnframes())
    payload = expected_payload()
    offset = samples.find(payload)
    if offset < 0 or offset % 4:
        raise ValueError("complete waveform is missing or altered")
    if any(samples[:offset]) or any(samples[offset + len(payload):]):
        raise ValueError("unexpected non-silent samples outside the one complete waveform")
    return len(payload) // 4


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("wav", type=Path)
    args = parser.parse_args()
    try:
        frames = check(args.wav)
    except (OSError, ValueError, wave.Error) as error:
        parser.exit(1, f"HDA_WAVEFORM_FAIL: {error}\n")
    print(f"HDA_WAVEFORM_OK frames={frames} bytes={frames * 4}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
