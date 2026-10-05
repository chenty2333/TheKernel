#!/usr/bin/env python3
"""Authored AML policy fixtures through the formal Q35/OVMF product runner.

No hardware tables or host-forced VM termination count as acceptance.
"""
from pathlib import Path
import argparse
import os
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from tools import thekernel as product
from tools.product_state import state_root


def ordered(text: str, *markers: str) -> None:
    at = -1
    for marker in markers:
        at = text.find(marker, at + 1)
        if at < 0:
            raise RuntimeError("missing ordered shutdown marker: " + marker)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--case", choices=("thermal",), default="thermal")
    args = parser.parse_args()
    os.environ["THEKERNEL_TOOLCHAIN"] = "acpica"
    run_args = product.build_parser().parse_args(["run", "--profile", "shell", "--toolchain", "acpica"])
    artifacts = product.artifacts_for(run_args)
    if not args.no_build:
        product.build_cmd(run_args)
    directory = Path(tempfile.mkdtemp(prefix="acpica-" + args.case + "-", dir=state_root() / "runs"))
    commands = directory / "commands"
    commands.write_text("\n".join([
        "/bin/busybox cat /sys/class/thermal/thermal_zone0/type",
        "/bin/busybox cat /sys/class/thermal/thermal_zone0/temp",
        "/bin/busybox cat /sys/class/thermal/thermal_zone0/trip_point_0_temp",
        "/bin/busybox cat /sys/class/thermal/thermal_zone0/trip_point_0_type",
        "/bin/busybox cat /sys/class/thermal/thermal_zone0/trip_point_1_temp",
        "/bin/busybox cat /sys/class/thermal/thermal_zone0/trip_point_1_type",
        "/bin/busybox echo THEKERNEL_THERMAL_SYSFS_READ",
        "/bin/busybox sleep 60", "",
    ]))
    result = product.run_product(artifacts, product.RunSpec(
        accel="kvm", timeout=120, workdir=directory, interactive=False,
        input_after_marker="THEKERNEL_SHELL_READY", stop_after_marker=None,
        commands=commands, extra_block=None, run_cpus=4,
        kernel_cmdline="acpi=acpica",
        qemu_extra_args=("-acpitable", "file=" + str(ROOT / "tests/guest/acpi/thermal.aml")),
    ))
    if result:
        raise RuntimeError(f"guest failed ({result}); see {directory}")
    console = (directory / "console.log").read_text()
    kernel = (directory / "kernel.log").read_text()
    lines = console.splitlines()
    for value in ("acpitz", "26800", "36800", "critical", "31800", "passive"):
        if value not in lines:
            raise RuntimeError(f"missing actual sysfs value {value!r}; see {directory}")
    ordered(console, "THEKERNEL_THERMAL_SYSFS_READ", "THEKERNEL_ACPI_BUTTON_EVENT", "THEKERNEL_ACPICA_S5_PREPARED")
    ordered(kernel, "thermal critical trip reached", "acpi-power: filesystems flushed; entering S5", "acpica: entering S5 after AML preparation")
    print("ACPICA_THERMAL_QEMU_PASS", directory, flush=True)


if __name__ == "__main__":
    main()
