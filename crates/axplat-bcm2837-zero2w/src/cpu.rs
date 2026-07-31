//! CPU topology.

use ax_plat::cpu::CpuTopologyIf;

struct CpuTopologyIfImpl;

#[impl_plat_interface]
impl CpuTopologyIf for CpuTopologyIfImpl {
    /// Maps an MPIDR value to the dense logical index. The platform boots a
    /// single CPU core (CPU 0) for now.
    fn resolve_cpu_index(hardware_id: usize) -> Option<usize> {
        (hardware_id == 0).then_some(0)
    }
}
