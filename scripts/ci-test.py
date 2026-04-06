#!/usr/bin/env python3

import argparse
import pathlib
import subprocess
import sys


def canonical_arch(value: str) -> str:
    if value in {"rv", "riscv64"}:
        return "rv"
    if value in {"la", "loongarch64"}:
        return "la"
    raise argparse.ArgumentTypeError(f"unsupported arch: {value}")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Run the official pre-2025 evaluator image locally."
    )
    parser.add_argument("arch", type=canonical_arch)
    parser.add_argument("extra", nargs=argparse.REMAINDER)
    args = parser.parse_args()

    script = pathlib.Path(__file__).resolve().with_name("oscomp.sh")
    cmd = [str(script), "run", "--arch", args.arch, *args.extra]
    return subprocess.run(cmd, check=False).returncode


if __name__ == "__main__":
    sys.exit(main())
