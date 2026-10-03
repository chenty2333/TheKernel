"""Safe append-only GRUB arguments and CLI/spec wiring."""
import unittest
from tools.kernel_cmdline import append_kernel_cmdline
from tools import thekernel as product


class KernelCmdlineTests(unittest.TestCase):
    def test_only_the_boot_line_changes_and_existing_arguments_survive(self):
        source = 'set timeout=0\nmenuentry "kernel" {\n    multiboot2 /TheKernel.elf quiet\n    boot\n}\n'
        result = append_kernel_cmdline(source, ' loglevel=7\twatchdog.timeout=60 ')
        self.assertEqual(result, source.replace(' quiet\n', ' quiet loglevel=7 watchdog.timeout=60\n'))
        self.assertEqual(append_kernel_cmdline(source, ''), source)

    def test_rejects_grub_injection_and_ambiguous_boot_templates(self):
        for text in ['quiet\nreboot', 'quiet\r', 'x;reboot', '${x}', '"x"', 'x #', '\x00', 'x{y}', 'x\\y']:
            with self.assertRaises(ValueError):
                append_kernel_cmdline('multiboot2 /TheKernel.elf\n', text)
        for template in ['', 'linux /kernel\n', 'multiboot2 /a\nmultiboot2 /b\n']:
            with self.assertRaises(ValueError):
                append_kernel_cmdline(template, 'loglevel=7')

    def test_run_and_fbcon_accept_the_explicit_append_option(self):
        parser = product.build_parser()
        for command in [['run'], ['test', '--suite', 'fbcon']]:
            args = parser.parse_args(command + ['--kernel-cmdline', 'loglevel=7'])
            self.assertEqual(args.kernel_cmdline, 'loglevel=7')
        self.assertIsNone(parser.parse_args(['run']).kernel_cmdline)
