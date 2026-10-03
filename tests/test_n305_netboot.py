import functools
import http.client
import http.server
import io
import os
import subprocess
from pathlib import Path
import tarfile
import tempfile
import threading
import unittest

from tools.n305_netboot import CaptureHandler, generate_session_scripts, grub_config


class NetbootTests(unittest.TestCase):
    def test_lease_hook_is_a_rootfs_cache_input(self):
        from tools.product_state import ROOTFS_INPUT_FILES
        self.assertIn("scripts/ci/n305-dhcp.script", ROOTFS_INPUT_FILES)

    def test_grub_uses_explicit_mode_not_keep_and_preserves_diagnostics(self):
        text = grub_config("kernel", "192.168.10.1", 8080, "debug", "1920x1080x32,auto")
        self.assertIn("quiet loglevel=debug", text)
        self.assertNotIn("gfxpayload=keep", text)
        self.assertIn("module2", text)
        capture = grub_config("capture", "192.168.10.1", 8080, "info", "auto")
        self.assertIn("apkovl=http://192.168.10.1:8080/", capture)
        self.assertIn("alpine_repo=http://192.168.10.1:8080/apks/main", capture)

    def test_generated_session_has_rollback_for_success_and_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            generate_session_scripts(root, "eth1", "192.168.10.1")
            text = (root / "start.sh").read_text()
            self.assertIn("trap cleanup EXIT", text)
            self.assertIn("connection.autoconnect no", text)
            self.assertIn('connection up uuid "$OLD"', text)
            self.assertNotIn("systemctl", text)
            self.assertNotIn("connection modify", text)
            self.assertIn('wait "$HTTP"', text)
            self.assertIn('/proc/$PID/cmdline', (root / "stop.sh").read_text())

    def test_failed_foreground_child_restores_connection_and_runtime_zone(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); bin_dir = root / "bin"; bin_dir.mkdir()
            log = root / "calls"
            commands = {
                "id": "echo 0",
                "nmcli": 'echo "nmcli $*" >> "$CALLS"\ncase "$*" in\n *GENERAL.TYPE*) echo ethernet;;\n *GENERAL.CON-UUID*) echo old-uuid;;\n *connection.uuid*) echo new-uuid;;\n *"up uuid new-uuid"*) echo trusted > "$ZONE";;\nesac',
                "firewall-cmd": 'echo "firewall $*" >> "$CALLS"\ncase "$*" in\n --state) echo running;;\n --get-zone-of-interface=*) cat "$ZONE";;\n --zone=*) echo restored > "$ZONE";;\nesac',
                "python3": "exec sleep 5",
                "dnsmasq": "exit 1",
            }
            for name, body in commands.items():
                path = bin_dir / name
                path.write_text("#!/bin/sh\n" + body + "\n"); path.chmod(0o755)
            zone = root / "zone"; zone.write_text("FedoraWorkstation\n")
            generate_session_scripts(root, "eth1", "192.168.10.1")
            result = subprocess.run(["bash", str(root / "start.sh")], timeout=10,
                env={**os.environ, "PATH": str(bin_dir) + ":" + os.environ["PATH"],
                     "CALLS": str(log), "ZONE": str(zone)}, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            calls = log.read_text()
            self.assertIn("nmcli connection up uuid old-uuid", calls)
            self.assertIn("firewall --zone=FedoraWorkstation --change-interface=eth1", calls)
            self.assertFalse((root / "session.pid").exists())

    def test_upload_validates_token_archive_paths_and_duplicates(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); (root / "http").mkdir()
            factory = functools.partial(CaptureHandler, root=root, token="test-token")
            with http.server.HTTPServer(("127.0.0.1", 0), factory) as server:
                thread = threading.Thread(target=server.serve_forever); thread.start()
                try:
                    def send(path, members):
                        data = io.BytesIO()
                        with tarfile.open(fileobj=data, mode="w:gz") as archive:
                            for name, kind in members:
                                entry = tarfile.TarInfo(name); entry.type = kind
                                entry.size = 3 if kind == tarfile.REGTYPE else 0
                                archive.addfile(entry, io.BytesIO(b"OK\n") if entry.size else None)
                        connection = http.client.HTTPConnection(*server.server_address, timeout=5)
                        connection.request("POST", path, data.getvalue())
                        reply = connection.getresponse(); status = reply.status
                        reply.read(); connection.close(); return status
                    valid = [("n305-20261003T000000Z/capture-status.txt", tarfile.REGTYPE)]
                    self.assertEqual(send("/upload/wrong", valid), 403)
                    self.assertEqual(send("/upload/test-token", [("../escape", tarfile.REGTYPE)]), 400)
                    self.assertEqual(send("/upload/test-token", [("n305-20261003T000000Z/link", tarfile.SYMTYPE)]), 400)
                    self.assertEqual(send("/upload/test-token", valid), 201)
                    self.assertEqual(send("/upload/test-token", valid), 400)
                    self.assertEqual((root / "received/n305-20261003T000000Z/capture-status.txt").read_text(), "OK\n")
                finally:
                    server.shutdown(); thread.join(timeout=5)
                    self.assertFalse(thread.is_alive())
