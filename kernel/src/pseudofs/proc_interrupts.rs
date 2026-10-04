//! Linux 7.2.3 kernel/irq/proc.c and fs/proc/softirqs.c output format facts.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::fmt::Write;

use axhal::irq::statistics::{VECTOR_COUNT, snapshot_cpu, snapshot_ipi_reasons};

use super::{DirMapping, SimpleFile, SimpleFs};

const LOCAL_TIMER: usize = 0xf0;
const APIC_SPURIOUS: usize = 0xf1;
const APIC_ERROR: usize = 0xf2;
const SOFTIRQ_NAMES: [&str; 10] = [
    "HI", "TIMER", "NET_TX", "NET_RX", "BLOCK", "IRQ_POLL", "TASKLET", "SCHED", "HRTIMER", "RCU",
];

fn header(cpus: usize, leading: usize) -> String {
    let mut text = " ".repeat(leading);
    for cpu in 0..cpus {
        let _ = write!(text, "CPU{cpu:<8}");
    }
    text.push('\n');
    text
}

fn row(text: &mut String, label: &str, counts: impl Iterator<Item = u64>, description: &str) {
    let _ = write!(text, "{label:>3}:");
    for count in counts {
        let _ = write!(text, " {count:>10}");
    }
    if !description.is_empty() {
        let _ = write!(text, "  {description}");
    }
    text.push('\n');
}

fn render_interrupts(vectors: &[[u64; VECTOR_COUNT]], reasons: &[[u64; 8]], ipi: usize) -> String {
    let mut text = header(vectors.len(), 11);
    for vector in 0..VECTOR_COUNT {
        if [LOCAL_TIMER, APIC_SPURIOUS, APIC_ERROR, ipi].contains(&vector)
            || vectors.iter().all(|cpu| cpu[vector] == 0)
        {
            continue;
        }
        row(
            &mut text,
            &alloc::format!("{vector}"),
            vectors.iter().map(|cpu| cpu[vector]),
            "x86-vector",
        );
    }
    for (label, vector, description) in [
        ("LOC", LOCAL_TIMER, "Local timer interrupts"),
        ("SPU", APIC_SPURIOUS, "Spurious interrupts"),
        ("IPI", ipi, "IPI broker interrupts"),
    ] {
        row(
            &mut text,
            label,
            vectors
                .iter()
                .map(|cpu| cpu.get(vector).copied().unwrap_or(0)),
            description,
        );
    }
    for (label, reason, description) in [
        ("TLB", 0, "TLB maintenance dispatches"),
        ("RES", 1, "Rescheduling dispatches"),
        ("CAL", 2, "Function call dispatches"),
    ] {
        row(
            &mut text,
            label,
            reasons.iter().map(|cpu| cpu[reason]),
            description,
        );
    }
    let errors = vectors.iter().map(|cpu| cpu[APIC_ERROR]).sum::<u64>();
    let _ = writeln!(text, "ERR: {errors:>10}");
    text
}

fn render_softirqs(cpus: usize) -> String {
    let mut text = header(cpus, 20);
    // There is no Linux softirq execution subsystem here. Network polling,
    // timers and deferred work run in hard IRQ or task context, not softirqs.
    // Zero describes the absent softirq events, not absent network/timer work.
    for name in SOFTIRQ_NAMES {
        let _ = write!(text, "{name:>12}:");
        for _ in 0..cpus {
            let _ = write!(text, " {:>10}", 0);
        }
        text.push('\n');
    }
    text
}

pub(super) fn register(root: &mut DirMapping, fs: &Arc<SimpleFs>) {
    root.add(
        "interrupts",
        SimpleFile::new_regular(fs.clone(), || {
            let cpus = axhal::cpu_num();
            let vectors: Vec<_> = (0..cpus).map(snapshot_cpu).collect();
            let reasons: Vec<_> = (0..cpus).map(snapshot_ipi_reasons).collect();
            Ok(render_interrupts(
                &vectors,
                &reasons,
                axconfig::devices::IPI_IRQ,
            ))
        }),
    );
    root.add(
        "softirqs",
        SimpleFile::new_regular(fs.clone(), || Ok(render_softirqs(axhal::cpu_num()))),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn irq_rows_keep_cpu_columns_and_raw_ipi_counts_separate_from_reasons() {
        let mut vectors = [[0; VECTOR_COUNT]; 2];
        vectors[0][0x80] = 7;
        vectors[1][LOCAL_TIMER] = 123;
        vectors[1][0xf3] = 2;
        let text = render_interrupts(&vectors, &[[0; 8], [2, 1, 0, 0, 0, 0, 0, 0]], 0xf3);
        assert_eq!(
            text.lines()
                .next()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["CPU0", "CPU1"]
        );
        assert!(text.contains("128:          7          0  x86-vector\n"));
        assert!(text.contains("LOC:          0        123"));
        assert!(text.contains("IPI:          0          2"));
        assert!(text.contains("RES:          0          1"));
        assert!(text.contains("TLB:          0          2"));
        assert!(!text.contains("243:"));
    }

    #[test]
    fn absent_softirq_subsystem_has_ten_linux_named_rows() {
        let text = render_softirqs(2);
        let rows: Vec<_> = text.lines().skip(1).collect();
        assert_eq!(rows.len(), 10);
        for (name, line) in SOFTIRQ_NAMES.iter().zip(rows) {
            assert_eq!(line.split_once(':').unwrap().0.trim(), *name);
            assert_eq!(
                line.split_once(':')
                    .unwrap()
                    .1
                    .split_whitespace()
                    .collect::<Vec<_>>(),
                ["0", "0"]
            );
        }
    }
}
