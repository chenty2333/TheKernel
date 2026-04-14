use alloc::{collections::BTreeMap, sync::Arc};
use core::ops::Deref;
use core::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};

use crate::{BaseScheduler, EnqueueReason};

pub const RR_TIMESLICE_TICKS: usize = 5;
pub const RT_PRIORITY_MIN: u8 = 1;
pub const RT_PRIORITY_MAX: u8 = 99;
const FAIR_PREEMPT_GRANULARITY_TICKS: isize = 2;

/// Runtime scheduling class for CFS tasks.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CfsTaskClass {
    Normal      = 0,
    Batch       = 1,
    Idle        = 2,
    RoundRobin  = 3,
    Fifo        = 4,
}

/// Runtime scheduling parameters for a CFS task.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct CfsTaskParams {
    pub class: CfsTaskClass,
    pub nice: i8,
    pub rt_priority: u8,
    pub reset_on_fork: bool,
}

impl Default for CfsTaskParams {
    fn default() -> Self {
        Self {
            class: CfsTaskClass::Normal,
            nice: 0,
            rt_priority: 0,
            reset_on_fork: false,
        }
    }
}

impl CfsTaskParams {
    fn canonicalize(mut self) -> Self {
        match self.class {
            CfsTaskClass::Idle => {
                self.nice = NICE_RANGE_POS as i8;
                self.rt_priority = 0;
            }
            CfsTaskClass::Normal | CfsTaskClass::Batch => {
                self.rt_priority = 0;
            }
            CfsTaskClass::RoundRobin | CfsTaskClass::Fifo => {
                self.nice = 0;
            }
        }
        self
    }
}

/// task for CFS
pub struct CFSTask<T> {
    inner: T,
    init_vruntime: AtomicIsize,
    delta: AtomicIsize,
    seeded_vruntime: AtomicBool,
    nice: AtomicIsize,
    class: AtomicU8,
    rt_priority: AtomicU8,
    reset_on_fork: AtomicBool,
    rr_time_slice: AtomicIsize,
    id: AtomicIsize,
}

// https://elixir.bootlin.com/linux/latest/source/include/linux/sched/prio.h

const NICE_RANGE_POS: usize = 19; // MAX_NICE in Linux
const NICE_RANGE_NEG: usize = 20; // -MIN_NICE in Linux, the range of nice is [MIN_NICE, MAX_NICE]
// https://elixir.bootlin.com/linux/latest/source/kernel/sched/core.c

const NICE2WEIGHT_POS: [isize; NICE_RANGE_POS + 1] = [
    1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
];
const NICE2WEIGHT_NEG: [isize; NICE_RANGE_NEG + 1] = [
    1024, 1277, 1586, 1991, 2501, 3121, 3906, 4904, 6100, 7620, 9548, 11916, 14949, 18705, 23254,
    29154, 36291, 46273, 56483, 71755, 88761,
];

impl<T> CFSTask<T> {
    /// new with default values
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            init_vruntime: AtomicIsize::new(0_isize),
            delta: AtomicIsize::new(0_isize),
            seeded_vruntime: AtomicBool::new(false),
            nice: AtomicIsize::new(0_isize),
            class: AtomicU8::new(CfsTaskClass::Normal as u8),
            rt_priority: AtomicU8::new(0),
            reset_on_fork: AtomicBool::new(false),
            rr_time_slice: AtomicIsize::new(RR_TIMESLICE_TICKS as isize),
            id: AtomicIsize::new(0_isize),
        }
    }

    fn class(&self) -> CfsTaskClass {
        match self.class.load(Ordering::Acquire) {
            0 => CfsTaskClass::Normal,
            1 => CfsTaskClass::Batch,
            2 => CfsTaskClass::Idle,
            3 => CfsTaskClass::RoundRobin,
            _ => CfsTaskClass::Fifo,
        }
    }

    fn is_rt(&self) -> bool {
        matches!(self.class(), CfsTaskClass::RoundRobin | CfsTaskClass::Fifo)
    }

    /// Consumes the scheduler wrapper and returns the inner task.
    pub fn into_inner(self) -> T {
        self.inner
    }

    fn effective_nice(&self) -> isize {
        match self.class() {
            CfsTaskClass::Idle => NICE_RANGE_POS as isize,
            CfsTaskClass::Normal | CfsTaskClass::Batch => self.nice.load(Ordering::Acquire),
            CfsTaskClass::RoundRobin | CfsTaskClass::Fifo => 0,
        }
    }

    fn get_weight(&self) -> isize {
        let nice = self.effective_nice();
        if nice >= 0 {
            NICE2WEIGHT_POS[nice as usize]
        } else {
            NICE2WEIGHT_NEG[(-nice) as usize]
        }
    }

    fn get_id(&self) -> isize {
        self.id.load(Ordering::Acquire)
    }

    fn get_vruntime(&self) -> isize {
        if self.get_weight() == 1024 {
            self.init_vruntime.load(Ordering::Acquire) + self.delta.load(Ordering::Acquire)
        } else {
            self.init_vruntime.load(Ordering::Acquire)
                + self.delta.load(Ordering::Acquire) * 1024 / self.get_weight()
        }
    }

    fn rebase_vruntime(&self, v: isize) {
        self.init_vruntime.store(v, Ordering::Release);
        self.delta.store(0, Ordering::Release);
    }

    fn take_seeded_vruntime(&self) -> bool {
        self.seeded_vruntime.swap(false, Ordering::AcqRel)
    }

    fn rt_priority(&self) -> u8 {
        self.rt_priority.load(Ordering::Acquire)
    }

    fn rr_time_slice(&self) -> isize {
        self.rr_time_slice.load(Ordering::Acquire)
    }

    fn reset_rr_time_slice(&self) {
        self.rr_time_slice
            .store(RR_TIMESLICE_TICKS as isize, Ordering::Release);
    }

    fn task_tick_rr(&self) -> isize {
        self.rr_time_slice.fetch_sub(1, Ordering::Release)
    }

    fn set_sched_params(
        &self,
        class: CfsTaskClass,
        nice: isize,
        rt_priority: u8,
        reset_on_fork: bool,
    ) {
        let current_vruntime = self.get_vruntime();
        self.rebase_vruntime(current_vruntime);
        self.nice.store(nice, Ordering::Release);
        self.class.store(class as u8, Ordering::Release);
        self.rt_priority.store(rt_priority, Ordering::Release);
        self.reset_on_fork.store(reset_on_fork, Ordering::Release);
        if matches!(class, CfsTaskClass::RoundRobin) {
            self.reset_rr_time_slice();
        } else {
            self.rr_time_slice.store(0, Ordering::Release);
        }
    }

    fn set_id(&self, id: isize) {
        self.id.store(id, Ordering::Release);
    }

    fn task_tick(&self) {
        self.delta.fetch_add(1, Ordering::Release);
    }

    /// Returns a reference to the inner task struct.
    pub const fn inner(&self) -> &T {
        &self.inner
    }

    /// Returns the current scheduling parameters.
    pub fn sched_params(&self) -> CfsTaskParams {
        CfsTaskParams {
            class: self.class(),
            nice: self.effective_nice() as i8,
            rt_priority: if self.is_rt() { self.rt_priority() } else { 0 },
            reset_on_fork: self.reset_on_fork.load(Ordering::Acquire),
        }
    }

    /// Applies the given scheduling parameters to the task.
    pub fn configure(&self, params: CfsTaskParams) -> bool {
        let params = params.canonicalize();
        match params.class {
            CfsTaskClass::RoundRobin | CfsTaskClass::Fifo => {
                if !(RT_PRIORITY_MIN..=RT_PRIORITY_MAX).contains(&params.rt_priority) {
                    return false;
                }
            }
            CfsTaskClass::Normal | CfsTaskClass::Batch | CfsTaskClass::Idle => {
                if !(-20..=19).contains(&(params.nice as isize)) {
                    return false;
                }
            }
        }
        self.set_sched_params(
            params.class,
            params.nice as isize,
            params.rt_priority,
            params.reset_on_fork,
        );
        true
    }

    /// Seeds a freshly forked fair task just behind its parent so fork bursts
    /// do not unfairly jump ahead of the running parent.
    pub fn inherit_fair_vruntime_from(&self, parent: &Self) {
        if self.is_rt() || parent.is_rt() {
            return;
        }
        self.rebase_vruntime(
            parent
                .get_vruntime()
                .saturating_add(FAIR_PREEMPT_GRANULARITY_TICKS),
        );
        self.seeded_vruntime.store(true, Ordering::Release);
    }
}

impl<T> Deref for CFSTask<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// A simple [Completely Fair Scheduler][1] (CFS).
///
/// [1]: https://en.wikipedia.org/wiki/Completely_Fair_Scheduler
pub struct CFScheduler<T> {
    ready_fair_queue: BTreeMap<(isize, isize), Arc<CFSTask<T>>>, // (vruntime, taskid)
    ready_rt_queue: BTreeMap<(u8, isize), Arc<CFSTask<T>>>, // (priority key, queue seq)
    min_vruntime: Option<isize>,
    id_pool: AtomicIsize,
    rt_front_seq: isize,
    rt_back_seq: isize,
}

impl<T> CFScheduler<T> {
    /// Creates a new empty [`CFScheduler`].
    pub const fn new() -> Self {
        Self {
            ready_fair_queue: BTreeMap::new(),
            ready_rt_queue: BTreeMap::new(),
            min_vruntime: None,
            id_pool: AtomicIsize::new(0_isize),
            rt_front_seq: 0,
            rt_back_seq: 0,
        }
    }

    /// get the name of scheduler
    pub fn scheduler_name() -> &'static str {
        "Completely Fair"
    }

    fn queue_floor(&self) -> isize {
        self.min_vruntime.unwrap_or(0)
    }

    fn next_task_id(&self) -> isize {
        self.id_pool.fetch_add(1, Ordering::Release)
    }

    fn min_ready_vruntime(&self) -> Option<isize> {
        self.ready_fair_queue
            .first_key_value()
            .map(|((vruntime, _), _)| *vruntime)
    }

    fn refresh_min_vruntime(&mut self, current_vruntime: Option<isize>) {
        let candidate = match (current_vruntime, self.min_ready_vruntime()) {
            (Some(current), Some(ready)) => Some(current.min(ready)),
            (Some(current), None) => Some(current),
            (None, Some(ready)) => Some(ready),
            (None, None) => None,
        };

        self.min_vruntime = match (self.min_vruntime, candidate) {
            (_, None) => None,
            (Some(old), Some(new)) => Some(old.max(new)),
            (None, Some(new)) => Some(new),
        };
    }

    fn insert_task(&mut self, task: Arc<CFSTask<T>>) {
        if task.is_rt() {
            self.insert_rt_task(task, false);
        } else {
            self.insert_fair_task(task);
        }
    }

    fn insert_fair_task(&mut self, task: Arc<CFSTask<T>>) {
        let taskid = self.next_task_id();
        let vruntime = task.get_vruntime();
        task.set_id(taskid);
        self.ready_fair_queue.insert((vruntime, taskid), task);
        self.refresh_min_vruntime(None);
    }

    fn wakeup_floor(&self, task: &CFSTask<T>) -> isize {
        let floor = self.queue_floor();
        match task.class() {
            CfsTaskClass::Normal => floor,
            CfsTaskClass::Batch => floor.saturating_add(1),
            CfsTaskClass::Idle => floor.saturating_add(2),
            CfsTaskClass::RoundRobin | CfsTaskClass::Fifo => floor,
        }
    }

    fn rt_priority_key(priority: u8) -> u8 {
        RT_PRIORITY_MAX - priority
    }

    fn next_rt_seq(&mut self, front: bool) -> isize {
        if front {
            self.rt_front_seq -= 1;
            self.rt_front_seq
        } else {
            let seq = self.rt_back_seq;
            self.rt_back_seq += 1;
            seq
        }
    }

    fn insert_rt_task(&mut self, task: Arc<CFSTask<T>>, front: bool) {
        let seq = self.next_rt_seq(front);
        let key = (Self::rt_priority_key(task.rt_priority()), seq);
        task.set_id(seq);
        self.ready_rt_queue.insert(key, task);
    }

    fn has_ready_rt_with_higher_priority(&self, current_priority: u8) -> bool {
        self.ready_rt_queue
            .first_key_value()
            .map(|((key, _), _)| RT_PRIORITY_MAX - *key > current_priority)
            .unwrap_or(false)
    }

    fn has_ready_rt_with_same_priority(&self, current_priority: u8) -> bool {
        let key = Self::rt_priority_key(current_priority);
        self.ready_rt_queue
            .range((key, isize::MIN)..=(key, isize::MAX))
            .next()
            .is_some()
    }

    /// Updates runtime scheduling parameters for a task.
    pub fn set_task_params(&mut self, task: &Arc<CFSTask<T>>, params: CfsTaskParams) -> bool {
        task.configure(params)
    }
}

impl<T> BaseScheduler for CFScheduler<T> {
    type SchedItem = Arc<CFSTask<T>>;

    fn init(&mut self) {}

    fn add_task(&mut self, task: Self::SchedItem) {
        if task.is_rt() {
            if matches!(task.class(), CfsTaskClass::RoundRobin) {
                task.reset_rr_time_slice();
            }
            self.insert_rt_task(task, false);
        } else {
            let vruntime = if task.take_seeded_vruntime() {
                task.get_vruntime().max(self.queue_floor())
            } else {
                self.queue_floor()
            };
            task.rebase_vruntime(vruntime);
            self.insert_task(task);
        }
    }

    fn remove_task(&mut self, task: &Self::SchedItem) -> Option<Self::SchedItem> {
        if task.is_rt() {
            self.ready_rt_queue
                .remove_entry(&(Self::rt_priority_key(task.rt_priority()), task.get_id()))
                .map(|(_, task)| task)
        } else {
            let removed = self
                .ready_fair_queue
                .remove_entry(&(task.clone().get_vruntime(), task.clone().get_id()))
                .map(|(_, task)| task);
            self.refresh_min_vruntime(None);
            removed
        }
    }

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        if let Some((_, task)) = self.ready_rt_queue.pop_first() {
            return Some(task);
        }
        let next = self.ready_fair_queue.pop_first().map(|(_, task)| task);
        match &next {
            Some(task) => self.refresh_min_vruntime(Some(task.get_vruntime())),
            None => self.refresh_min_vruntime(None),
        }
        next
    }

    fn put_prev_task(&mut self, prev: Self::SchedItem, preempt: bool) {
        match prev.class() {
            CfsTaskClass::Fifo => self.insert_rt_task(prev, preempt),
            CfsTaskClass::RoundRobin => {
                if preempt && prev.rr_time_slice() > 0 {
                    self.insert_rt_task(prev, true);
                } else {
                    prev.reset_rr_time_slice();
                    self.insert_rt_task(prev, false);
                }
            }
            CfsTaskClass::Normal | CfsTaskClass::Batch | CfsTaskClass::Idle => {
                self.insert_fair_task(prev)
            }
        }
    }

    fn enqueue_task(&mut self, task: Self::SchedItem, reason: EnqueueReason) {
        if task.is_rt() {
            match reason {
                EnqueueReason::New | EnqueueReason::Wakeup => {
                    if matches!(task.class(), CfsTaskClass::RoundRobin) {
                        task.reset_rr_time_slice();
                    }
                    self.insert_rt_task(task, false);
                }
                EnqueueReason::Yield => self.put_prev_task(task, false),
                EnqueueReason::Preempt => self.put_prev_task(task, true),
            }
            return;
        }

        match reason {
            EnqueueReason::New => self.add_task(task),
            EnqueueReason::Wakeup => {
                let floor = self.wakeup_floor(&task);
                let vruntime = task.get_vruntime().max(floor);
                task.rebase_vruntime(vruntime);
                self.insert_fair_task(task);
            }
            EnqueueReason::Yield => {
                // A cooperative yield should put a fair task behind peers that
                // are already ready, otherwise fork storms keep rescheduling
                // the yielding parent and freshly forked children never reach
                // their first blocking syscall.
                let floor = self
                    .min_ready_vruntime()
                    .unwrap_or_else(|| self.queue_floor())
                    .saturating_add(FAIR_PREEMPT_GRANULARITY_TICKS);
                let vruntime = task.get_vruntime().max(floor);
                task.rebase_vruntime(vruntime);
                self.insert_fair_task(task);
            }
            EnqueueReason::Preempt => self.put_prev_task(task, false),
        }
    }

    fn task_tick(&mut self, current: &Self::SchedItem) -> bool {
        if current.is_rt() {
            let current_priority = current.rt_priority();
            if self.has_ready_rt_with_higher_priority(current_priority) {
                return true;
            }

            return match current.class() {
                CfsTaskClass::Fifo => false,
                CfsTaskClass::RoundRobin => {
                    let old_slice = current.task_tick_rr();
                    if old_slice <= 1 {
                        current.reset_rr_time_slice();
                        self.has_ready_rt_with_same_priority(current_priority)
                    } else {
                        false
                    }
                }
                CfsTaskClass::Normal | CfsTaskClass::Batch | CfsTaskClass::Idle => false,
            };
        }

        if !self.ready_rt_queue.is_empty() {
            return true;
        }

        current.task_tick();
        let current_vruntime = current.get_vruntime();
        self.refresh_min_vruntime(Some(current_vruntime));

        match self.min_ready_vruntime() {
            // Keep the current fair task running for a small vruntime window
            // after a wakeup burst so it can complete short follow-up work
            // instead of being displaced immediately by freshly woken peers.
            Some(ready_min) => {
                current_vruntime > ready_min.saturating_add(FAIR_PREEMPT_GRANULARITY_TICKS)
            }
            None => false,
        }
    }

    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> bool {
        if task.is_rt() {
            return false;
        }
        if !(-20..=19).contains(&prio) {
            return false;
        }
        self.set_task_params(
            task,
            CfsTaskParams {
                class: task.class(),
                nice: prio as i8,
                rt_priority: 0,
                reset_on_fork: task.reset_on_fork.load(Ordering::Acquire),
            },
        )
    }
}

impl<T> Default for CFScheduler<T> {
    fn default() -> Self {
        Self::new()
    }
}
