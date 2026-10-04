//! Observational stall diagnostics, not a reset/recovery algorithm. Native
//! RTL8168 intermittent stop has not been reproduced by this diagnostic code.
use crate::DevResult;

pub(crate) fn stage<T>(name: &'static str, result: DevResult<T>) -> DevResult<T> {
    if let Err(error) = &result {
        log::warn!(
            "\x013RTL8168_STEP_FAILED stage={name} error={error:?}; no firmware/PHY success \
             claimed"
        );
    }
    result
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub tx: u64,
    pub tx_reaped: u64,
    pub rx: u64,
    pub rx_bad: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    TxNoProgress,
    RxQuiet,
}
impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TxNoProgress => "tx-no-progress",
            Self::RxQuiet => "rx-quiet-not-proof-of-stall",
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub command: u32,
    pub intr_status: u32,
    pub intr_mask: u32,
    pub phy_status: u32,
    pub bmcr: Option<u16>,
    pub bmsr: Option<u16>,
    pub tx_head: usize,
    pub tx_tail: usize,
    pub tx_used: usize,
    pub rx_head: usize,
    pub tx_head_status: u32,
    pub rx_head_status: u32,
    pub rx_loans: usize,
    pub counters: Counters,
    pub firmware_stage: &'static str,
}
#[derive(Default)]
pub(crate) struct Monitor {
    counters: Counters,
    last_tx: u64,
    last_rx: u64,
    tx_report: Option<u64>,
    rx_report: Option<u64>,
    tx_reports: u8,
    rx_reports: u8,
    initialized: bool,
    previous_owned: bool,
}
impl Monitor {
    pub(crate) fn observe(
        &mut self,
        now: u64,
        counters: Counters,
        tx_owned: bool,
    ) -> Option<Reason> {
        if !self.initialized {
            self.initialized = true;
            self.last_tx = now;
            self.last_rx = now;
            self.counters = counters;
            self.previous_owned = tx_owned;
            return None;
        }
        if tx_owned && !self.previous_owned {
            self.last_tx = now;
        }
        let completion = !tx_owned && self.previous_owned;
        self.previous_owned = tx_owned;
        if completion
            || counters.tx_reaped != self.counters.tx_reaped
            || (!tx_owned && counters.tx != self.counters.tx)
        {
            self.last_tx = now;
            self.tx_report = None;
            self.tx_reports = 0;
        }
        if counters.rx != self.counters.rx {
            self.last_rx = now;
            self.rx_report = None;
            self.rx_reports = 0;
        }
        self.counters = counters;
        // No timestamp clock or no traffic cannot prove a stopped queue.
        if tx_owned
            && now.saturating_sub(self.last_tx) >= 5000
            && self.tx_reports < 3
            && self
                .tx_report
                .is_none_or(|last| now.saturating_sub(last) >= 30000)
        {
            self.tx_report = Some(now);
            self.tx_reports += 1;
            return Some(Reason::TxNoProgress);
        }
        if counters.rx != 0
            && now.saturating_sub(self.last_rx) >= 30000
            && self.rx_reports < 3
            && self
                .rx_report
                .is_none_or(|last| now.saturating_sub(last) >= 30000)
        {
            self.rx_report = Some(now);
            self.rx_reports += 1;
            return Some(Reason::RxQuiet);
        }
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn late_first_tx_gets_a_full_age_and_reports_stop_after_three() {
        let mut m = Monitor::default();
        let c = Counters::default();
        assert_eq!(m.observe(0, c, false), None);
        let c = Counters { tx: 1, ..c };
        assert_eq!(m.observe(100000, c, true), None);
        assert_eq!(m.observe(104999, c, true), None);
        for now in [105000, 135000, 165000] {
            assert_eq!(m.observe(now, c, true), Some(Reason::TxNoProgress));
        }
        assert_eq!(m.observe(195000, c, true), None);
        assert_eq!(m.observe(195001, c, false), None); // descriptor completion re-arms
        assert_eq!(m.observe(200000, c, true), None);
        assert_eq!(m.observe(205000, c, true), Some(Reason::TxNoProgress));
    }
    #[test]
    fn outstanding_tx_and_post_traffic_rx_quiet_are_rate_limited_not_guessed() {
        let mut m = Monitor::default();
        let c = Counters::default();
        assert_eq!(m.observe(0, c, false), None);
        assert_eq!(m.observe(100000, c, false), None);
        let c = Counters { tx: 1, rx: 1, ..c };
        assert_eq!(m.observe(100001, c, true), None);
        assert_eq!(m.observe(105001, c, true), Some(Reason::TxNoProgress));
        assert_eq!(m.observe(105002, c, true), None);
        assert_eq!(m.observe(130001, c, false), Some(Reason::RxQuiet));
        assert_eq!(m.observe(130002, c, false), None);
        let c = Counters {
            rx: 2,
            tx_reaped: 1,
            ..c
        };
        assert_eq!(m.observe(130003, c, false), None);
        assert_eq!(m.observe(130004, c, false), None);
        // Clock reversal is saturating, never an underflow-driven warning.
        assert_eq!(m.observe(1, c, false), None);
    }
}
