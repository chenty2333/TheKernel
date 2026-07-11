#!/usr/bin/env python3
"""One-shot host peer for the non-loopback QEMU network gate."""

from __future__ import annotations

import argparse
import socket
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--nonce", required=True)
    parser.add_argument("--port-file", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=600.0)
    args = parser.parse_args()

    expected = f"THEKERNEL_NETWORK_PROBE {args.nonce}\n".encode()
    reply = f"THEKERNEL_NETWORK_REPLY {args.nonce}\n".encode()

    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(args.timeout)
        args.port_file.write_text(f"{listener.getsockname()[1]}\n", encoding="ascii")
        print(f"network-peer: listening on 127.0.0.1:{listener.getsockname()[1]}", flush=True)

        connection, address = listener.accept()
        with connection:
            connection.settimeout(30.0)
            request = bytearray()
            while len(request) <= 4096 and not request.endswith(b"\n"):
                chunk = connection.recv(4096 - len(request) + 1)
                if not chunk:
                    break
                request.extend(chunk)
            if bytes(request) != expected:
                print(
                    f"network-peer: invalid request from {address}: {bytes(request)!r}",
                    flush=True,
                )
                return 1
            connection.sendall(reply)
            connection.shutdown(socket.SHUT_WR)

    print("network-peer: validated guest request", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
