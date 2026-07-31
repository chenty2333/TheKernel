//! CPU topology.

use ax_plat::cpu::CpuTopologyIf;

use crate::config::MAX_CPU_NUM;

struct CpuTopologyIfImpl;

#[impl_plat_interface]
impl CpuTopologyIf for CpuTopologyIfImpl {
    /// Maps an MPIDR value to the dense logical index. BCM2837 exposes the
    /// cores as `0..4` in the low MPIDR bits.
    fn resolve_cpu_index(hardware_id: usize) -> Option<usize> {
        (hardware_id < MAX_CPU_NUM).then_some(hardware_id)
    }
}
