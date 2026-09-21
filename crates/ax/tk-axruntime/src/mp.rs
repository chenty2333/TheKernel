// Copyright 2025 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use core::sync::atomic::{AtomicUsize, Ordering};

use axconfig::{TASK_STACK_SIZE, plat::MAX_CPU_NUM};
use axhal::mem::{VirtAddr, virt_to_phys};

#[unsafe(link_section = ".bss.stack")]
static mut SECONDARY_BOOT_STACK: [[u8; TASK_STACK_SIZE]; MAX_CPU_NUM - 1] =
    [[0; TASK_STACK_SIZE]; MAX_CPU_NUM - 1];

static ENTERED_CPUS: AtomicUsize = AtomicUsize::new(1);

/// How long the boot CPU stays quiet about an application processor that has
/// not reported in, and how often it says so afterwards.
///
/// Nothing here gives up on a CPU, because the kernel has no reduced-fleet
/// mode: `axhal::cpu_num()` is fixed by the discovered topology and every CPU
/// in it is expected at the init rendezvous, so abandoning one would leave the
/// remaining CPUs waiting for a peer that never arrives.  What a stuck CPU must
/// not be is silent -- on real hardware a frozen boot with no output is
/// indistinguishable from a dead console.  Linux bounds the *delivery* of the
/// STARTUP IPIs (`apic_mem_wait_icr_idle_timeout`,
/// arch/x86/kernel/apic/ipi.c:116-127) and then waits for the CPU's hotplug
/// thread unconditionally (kernel/cpu.c:269-273).
const ENTRY_REPORT_INTERVAL: u64 = 10 * axhal::time::NANOS_PER_SEC;

#[allow(clippy::absurd_extreme_comparisons)]
pub fn start_secondary_cpus(primary_cpu_id: usize) {
    let mut logic_cpu_id = 0;
    let cpu_num = axhal::cpu_num();
    let started_at = axhal::time::monotonic_time_nanos();
    for i in 0..cpu_num {
        if i != primary_cpu_id && logic_cpu_id < cpu_num - 1 {
            let stack_top = virt_to_phys(VirtAddr::from(unsafe {
                SECONDARY_BOOT_STACK[logic_cpu_id].as_ptr_range().end as usize
            }));

            debug!("starting CPU {i}...");
            axhal::power::cpu_boot(i, stack_top.as_usize());
            logic_cpu_id += 1;

            let waited_at = axhal::time::monotonic_time_nanos();
            let mut next_report = waited_at + ENTRY_REPORT_INTERVAL;
            while ENTERED_CPUS.load(Ordering::Acquire) <= logic_cpu_id {
                core::hint::spin_loop();
                let now = axhal::time::monotonic_time_nanos();
                if now >= next_report {
                    error!(
                        "CPU {i} has not reported in {}s after its STARTUP IPIs ({} of {cpu_num} CPUs up)",
                        (now - waited_at) / axhal::time::NANOS_PER_SEC,
                        ENTERED_CPUS.load(Ordering::Acquire),
                    );
                    next_report = now + ENTRY_REPORT_INTERVAL;
                }
            }
        }
    }

    // Linux ends the bring-up with the same summary for the same reason
    // (`impress_friends`, arch/x86/kernel/smpboot.c:785-801): the count and the
    // time it took are the two facts that say whether this machine's MP
    // initialization is healthy.
    if cpu_num > 1 {
        let elapsed = axhal::time::monotonic_time_nanos() - started_at;
        info!(
            "Total of {cpu_num} processors activated in {}ms.",
            elapsed / axhal::time::NANOS_PER_MILLIS
        );
    }
}

/// The main entry point of the ArceOS runtime for secondary cores.
///
/// It is called from the bootstrapping code in the specific platform crate.
#[axplat::secondary_main]
pub fn rust_main_secondary(cpu_id: usize) -> ! {
    axhal::percpu::init_secondary(cpu_id);
    axhal::init_early_secondary(cpu_id);

    ENTERED_CPUS.fetch_add(1, Ordering::Release);
    info!("Secondary CPU {cpu_id} started.");

    #[cfg(feature = "paging")]
    axmm::init_memory_management_secondary();

    axhal::init_later_secondary(cpu_id);

    #[cfg(feature = "multitask")]
    if let Err(error) = axtask::init_scheduler_secondary() {
        error!("Secondary task scheduler initialization failed: {error:?}");
        axhal::power::system_off();
    }

    info!("Secondary CPU {cpu_id:x} init OK.");
    super::INITED_CPUS.fetch_add(1, Ordering::Release);

    while !super::is_init_ok() {
        core::hint::spin_loop();
    }

    #[cfg(feature = "irq")]
    {
        // Legacy one-shot LAPIC initialization leaves the initial count at
        // zero, so every secondary must arm its own periodic chain before it
        // can enter the idle wait. Otherwise remotely placed work can remain
        // stranded forever because that CPU has no interrupt to leave idle.
        super::rearm_timer(axhal::time::monotonic_time_nanos());
        axhal::asm::enable_irqs();
    }

    #[cfg(all(feature = "tls", not(feature = "multitask")))]
    super::init_tls();

    #[cfg(feature = "multitask")]
    axtask::run_idle();
    #[cfg(not(feature = "multitask"))]
    loop {
        axhal::asm::wait_for_irqs();
    }
}
