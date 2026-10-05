#!/usr/bin/env python3
"""Q35 NIC without MSI/MSI-X: actual TCP bytes and routed INTx delivery."""
from pathlib import Path
import argparse
import os
import socket
import sys
import tempfile
import threading

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from tools import thekernel as product
from tools.product_state import state_root


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-build", action="store_true")
    args = parser.parse_args()
    os.environ["THEKERNEL_TOOLCHAIN"] = "acpica"
    build = product.build_parser().parse_args(["run", "--profile", "shell", "--toolchain", "acpica"])
    artifacts = product.artifacts_for(build)
    if not args.no_build:
        product.build_cmd(build)
    directory = Path(tempfile.mkdtemp(prefix="acpica-intx-", dir=state_root() / "runs"))
    seen = [0]
    failures = []
    stop = threading.Event()
    with socket.socket() as server:
        server.bind(("127.0.0.1", 0))
        server.listen()
        server.settimeout(0.5)
        port = server.getsockname()[1]

        def echo() -> None:
            try:
                while not stop.is_set():
                    try:
                        connection, _ = server.accept()
                        break
                    except socket.timeout:
                        continue
                else:
                    return
                with connection:
                    connection.settimeout(10)
                    while data := connection.recv(4096):
                        connection.sendall(data)
                        seen[0] += len(data)
            except Exception as error:
                if not stop.is_set():
                    failures.append(str(error))

        worker = threading.Thread(target=echo)
        worker.start()
        commands = directory / "commands"
        commands.write_text(
            "/opt/thekernel-tests/bin/thekernel-acpi-smoke require-tools && /bin/busybox echo THEKERNEL_ACPI_INSPECTION_COMPLETE\n"
            "/bin/busybox dmesg | /bin/busybox grep 'acpica:'\n"
            f"/opt/thekernel-tests/bin/thekernel-acpi-intx-smoke {port} 54 && /bin/busybox echo THEKERNEL_INTX_COMPLETE\n"
            "/bin/busybox poweroff -f\n"
        )
        try:
            result = product.run_product(artifacts, product.RunSpec(
                accel="kvm", timeout=120, workdir=directory, interactive=False,
                input_after_marker="THEKERNEL_SHELL_READY", stop_after_marker=None,
                commands=commands, extra_block=None, run_cpus=4,
                kernel_cmdline=None, qemu_extra_args=("-global", "virtio-net-pci.vectors=0"),
            ))
        finally:
            stop.set()
            worker.join(timeout=11)
    console = (directory / "console.log").read_text()
    kernel = (directory / "kernel.log").read_text()
    markers = ("THEKERNEL_ACPI_USER_TOOLS_OK", "THEKERNEL_ACPI_INSPECTION_COMPLETE", "THEKERNEL_ACPI_NIC_INTX_ONLY", "THEKERNEL_ACPI_INTX_IO_OK", "THEKERNEL_INTX_COMPLETE")
    if result or failures or worker.is_alive() or seen[0] != 65536 or any(m not in console for m in markers):
        raise RuntimeError(f"INTx I/O failed: exit={result} bytes={seen[0]} server={failures}; {directory}")
    if "INTx 00:06.0 pin=1 GSI=22 low=false" not in kernel + console:
        raise RuntimeError("missing actual ACPI route for tested NIC")
    if "THEKERNEL_ACPICA_S5_PREPARED" not in console:
        raise RuntimeError("native AML S5 missing")
    print("ACPICA_INTX_QEMU_PASS", directory)


if __name__ == "__main__":
    main()
