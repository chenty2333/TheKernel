"""Blocking firmware service prerequisites and initial driver probe boundary."""
from pathlib import Path
import unittest
ROOT = Path(__file__).resolve().parents[1]
class PlatformServicePhaseTests(unittest.TestCase):
    def test_bsp_runtime_precedes_firmware_and_pci_before_ap_startup(self):
        source = (ROOT / 'crates/ax/tk-axruntime/src/lib.rs').read_text()
        body = source[source.index('pub fn rust_main('):source.index('pub fn reconcile_pci_input_hotplug(')]
        markers = ['axtask::init_scheduler()', 'init_interrupt();', 'ctor_bare::call_ctors();',
                   'axhal::asm::enable_irqs();', 'PlatformServices::before_pci_probe',
                   'init_device_subsystems();', 'self::mp::start_secondary_cpus(cpu_id)', 'unsafe { main() }']
        positions = [body.index(marker) for marker in markers]
        self.assertEqual(positions, sorted(positions))
        self.assertEqual(body.count('init_interrupt();'), 1)
        self.assertEqual(body.count('ctor_bare::call_ctors();'), 1)
    def test_interpreter_has_one_preprobe_owner_not_late_user_boot(self):
        entry = (ROOT / 'kernel/src/entry.rs').read_text()
        self.assertNotIn('crate::acpi::init();', entry)
        acpi = (ROOT / 'kernel/src/acpi/mod.rs').read_text()
        self.assertIn('impl axruntime::PlatformServices', acpi)
        self.assertIn('INIT_TRIED.swap', acpi)
