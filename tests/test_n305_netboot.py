import functools
import http.client
import http.server
import io
import os
import socket
import subprocess
from pathlib import Path
import tarfile
import tempfile
import threading
import unittest

from tools.n305_netboot import DEFAULT_GFXMODE, CaptureHandler, generate_session_scripts, grub_config

ROOT = Path(__file__).resolve().parents[1]

class NetbootTests(unittest.TestCase):
    def test_userspace_probe_log_relay_uses_udp_without_kernel_log_access(self):
        from tests.support import test_tmpdir
        with test_tmpdir() as directory:
            relay = Path(directory) / 'netconsole'
            subprocess.run(['cc', '-O2', '-Wall', '-Wextra', '-Werror',
                            str(ROOT / 'tests/guest/tools/netconsole.c'), '-o', str(relay)], check=True)
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as receiver:
                receiver.bind(('127.0.0.1', 0))
                receiver.settimeout(1)
                payload = b'N305_GPU_SMOKE state=NOT_RUN\n' + b'x' * 1500 + b'\n'
                subprocess.run([str(relay), '127.0.0.1', str(receiver.getsockname()[1]), '--stdin'],
                               input=payload, check=True, timeout=3)
                received = b''
                while len(received) < len(payload):
                    packet = receiver.recv(2048)
                    self.assertLessEqual(len(packet), 1199)
                    received += packet
                self.assertEqual(received, payload)
            invalid = subprocess.run([str(relay), '127.0.0.1', '0', '--stdin'], input=b'', timeout=3)
            self.assertEqual(invalid.returncode, 2)

    def test_opt_in_mesa_probe_reports_skip_exit_and_deadline_without_stalling_shell(self):
        from tests.support import test_tmpdir
        source = (ROOT / "tests/guest/shell-init.sh").read_text()
        probe = source.split('# The existing Mesa client', 1)[1].split('# Emit readiness', 1)[0]
        # Execute the real optional block, substituting only external resources
        # and the wall-clock budget. No GPU/rendering success is simulated here.
        probe = '# The existing Mesa client' + probe
        with test_tmpdir() as directory:
            root = Path(directory)
            client, log = root / "client", root / "log"
            block = probe.replace('client=/usr/local/bin/intel-mesa-smoke', f'client={client}')
            block = block.replace('node=/dev/dri/renderD128', 'node=/dev/null')
            block = block.replace('mkdir -p /run', ':')
            block = block.replace('/run/n305-gpu-smoke.log', str(log)).replace('sleep 45 &', 'sleep 0.1 &')
            def run(enabled):
                subprocess.run(['sh', '-eu', '-c', f'gpu_smoke={enabled}\nnetconsole=""\n{block}\nwait'],
                               check=True, timeout=3)
            run(0)
            self.assertFalse(log.exists())
            run(1)
            self.assertIn('state=NOT_RUN', log.read_text())
            client.write_text('#!/bin/sh\nexit 7\n')
            client.chmod(0o755)
            run(1)
            self.assertIn('state=EXIT status=7', log.read_text())
            client.write_text('#!/bin/sh\nexec sleep 5\n')
            run(1)
            self.assertIn('state=DEADLINE', log.read_text())
            self.assertIn('state=EXIT status=137', log.read_text())

    def test_n305_has_no_compile_time_slirp_address_or_default_gateway(self):
        from tools import thekernel as product
        parser=product.build_parser()
        for platform,expected in [("n305",("","")),("q35-uefi",("10.0.2.15","10.0.2.2"))]:
            artifacts=product.artifacts_for(parser.parse_args(["build","--platform",platform]))
            env=product.command_env(artifacts)
            self.assertEqual((env["AX_IP"],env["AX_GW"]),expected)

    def test_lease_hook_flushes_scoped_defaults_before_replacing_source_address(self):
        from tests.support import test_tmpdir
        with test_tmpdir() as directory:
            directory=Path(directory);log=directory/"calls";stub=directory/"ip"
            stub.write_text("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CALL_LOG\"\n")
            stub.chmod(0o755)
            for router in ["","192.168.10.1 192.168.10.2"]:
                log.write_text("")
                result=subprocess.run(["sh",str(ROOT/"scripts/ci/n305-dhcp.script"),"bound"],
                    env={**os.environ,"PATH":str(directory)+":"+os.environ["PATH"],"CALL_LOG":str(log),
                         "interface":"eth0","ip":"192.168.10.15","subnet":"255.255.255.0","router":router},
                    text=True,capture_output=True)
                self.assertEqual(result.returncode,0,result.stderr)
                calls=log.read_text().splitlines()
                self.assertEqual(calls[0],"-4 route flush dev eth0")
                self.assertTrue(calls[1].startswith("-4 addr flush"))
                defaults=[c for c in calls if "route replace default" in c]
                self.assertEqual(len(defaults),int(bool(router)))
                if router:self.assertIn("via 192.168.10.1 dev eth0 src 192.168.10.15",defaults[0])

    def test_lease_hook_is_a_rootfs_cache_input(self):
        from tools.product_state import ROOTFS_INPUT_FILES
        self.assertIn("scripts/ci/n305-dhcp.script", ROOTFS_INPUT_FILES)

    def test_grub_uses_explicit_mode_not_keep_and_preserves_diagnostics(self):
        self.assertEqual(DEFAULT_GFXMODE, "3840x2160x32,auto")
        text = grub_config("kernel", "192.168.10.1", 8080, "debug", DEFAULT_GFXMODE)
        self.assertIn("quiet loglevel=debug", text)
        self.assertNotIn("gfxpayload=keep", text)
        self.assertIn("set gfxmode=3840x2160x32,auto\n", text)
        self.assertIn("set gfxpayload=3840x2160x32,auto\n", text)
        # GRUB's multiboot2 command derives gfxpayload from the ELF header,
        # replacing an earlier assignment with auto for a 0x0 request.
        payload = None
        for line in text.splitlines():
            if line.startswith("multiboot2 "):
                payload = "auto"
            elif line.startswith("set gfxpayload="):
                payload = line.split("=", 1)[1]
            elif line == "boot":
                self.assertEqual(payload, DEFAULT_GFXMODE)
        self.assertIn("module2", text)
        capture = grub_config("capture", "192.168.10.1", 8080, "info", "auto")
        self.assertIn("apkovl=http://192.168.10.1:8080/", capture)
        self.assertIn("alpine_repo=http://192.168.10.1:8080/apks/main", capture)

    def test_prepare_appends_diagnostic_tokens_without_starting_host_services(self):
        from types import SimpleNamespace
        from unittest.mock import patch
        from tools.n305_netboot import prepare
        from tools.product_state import state_root
        scratch = state_root() / "test-tmp"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            root = Path(directory)
            kernel, rootfs = root / "kernel", root / "rootfs"
            kernel.write_bytes(b"fake ELF")
            rootfs.write_bytes(b"fake disk")
            args = SimpleNamespace(interface="eth1", address="192.168.10.1", port=8080,
                gfxmode="auto", out=root / "session", mode="kernel", kernel=kernel,
                rootfs=rootfs, loglevel="info", kernel_cmdline="tty.input_trace=1")
            def fake_grub(command, **kwargs):
                Path(command[command.index("-o") + 1]).write_bytes(b"fake GRUB")
            with patch("tools.n305_netboot.shutil.which", return_value="/tool/grub"), \
                 patch("tools.n305_netboot.subprocess.run", side_effect=fake_grub) as command:
                prepare(args)
                self.assertEqual(command.call_count, 1)
            text = (args.out / "grub.cfg").read_text()
            self.assertIn("quiet loglevel=info n305.net=dhcp", text)
            self.assertIn("tty.input_trace=1", text)
            self.assertNotIn("loglevel=info tty.input_trace=1 n305.net", text)

    def test_dnsmasq_follows_address_changes_across_link_bounces(self):
        # bind-interfaces answered every DISCOVER with "has no address" after
        # the N305 bounced the link on 2026-10-04.
        source = (ROOT / "tools/n305_netboot.py").read_text(encoding="utf-8")
        self.assertIn("\nbind-dynamic\n", source)
        self.assertNotIn("\nbind-interfaces\n", source)

    def test_generated_session_has_rollback_for_success_and_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            generate_session_scripts(root, "eth1", "192.168.10.1")
            text = (root / "start.sh").read_text()
            self.assertIn("trap cleanup EXIT", text)
            self.assertIn("trap 'exit 129' HUP", text)
            # The DUT's OS resets its NIC; after the link bounce the session
            # profile, not the user's DHCP profile, must come back.
            self.assertIn("connection.autoconnect yes connection.autoconnect-priority 999", text)
            self.assertIn('nmcli connection delete "$PROFILE"', text)
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
                                entry.mode = 0 if kind == tarfile.DIRTYPE else 0o6777
                                entry.uid = entry.gid = 12345
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
                    self.assertEqual(send("/upload/test-token", [("n305-20261003T000000Z/link", tarfile.LNKTYPE)]), 400)
                    self.assertEqual(send("/upload/test-token", [("/n305-20261003T000000Z/capture-status.txt", tarfile.REGTYPE)]), 400)
                    self.assertEqual(send("/upload/test-token", valid + valid), 400)
                    self.assertFalse((root / "received/n305-20261003T000000Z").exists())
                    self.assertEqual(send("/upload/test-token", [
                        ("n305-20261003T000000Z", tarfile.DIRTYPE), *valid,
                        ("n305-20261003T000000Z/logs/boot.txt", tarfile.REGTYPE),
                    ]), 201)
                    self.assertEqual(send("/upload/test-token", valid), 400)
                    saved = root / "received/n305-20261003T000000Z/capture-status.txt"
                    self.assertEqual(saved.read_text(), "OK\n")
                    self.assertEqual(saved.stat().st_uid, os.getuid())
                    self.assertEqual(saved.stat().st_mode & 0o7111, 0)
                    self.assertEqual((saved.parent / "logs/boot.txt").read_text(), "OK\n")
                finally:
                    server.shutdown(); thread.join(timeout=5)
                    self.assertFalse(thread.is_alive())
