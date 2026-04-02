use alloc::{collections::BTreeMap, sync::Arc};
use core::ops::Deref;
use core::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};

use crate::{BaseScheduler, EnqueueReason};

/// Runtime scheduling class for CFS tasks.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CfsTaskClass {
    Normal = 0,
    Batch  = 1,
    Idle   = 2,
}

/// Runtime scheduling parameters for a CFS task.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct CfsTaskParams {
    pub class: CfsTaskClass,
    pub nice: i8,
    pub reset_on_fork: bool,
}

impl Default for CfsTaskParams {
    fn default() -> Self {
        Self {
            class: CfsTaskClass::Normal,
            nice: 0,
            reset_on_fork: false,
        }
    }
}

/// task for CFS
pub struct CFSTask<T> {
    inner: T,
    init_vruntime: AtomicIsize,
    delta: AtomicIsize,
    nice: AtomicIsize,
    class: AtomicU8,
    reset_on_fork: AtomicBool,
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
            nice: AtomicIsize::new(0_isize),
            class: AtomicU8::new(CfsTaskClass::Normal as u8),
            reset_on_fork: AtomicBool::new(false),
            id: AtomicIsize::new(0_isize),
        }
    }

    fn class(&self) -> CfsTaskClass {
        match self.class.load(Ordering::Acquire) {
            0 => CfsTaskClass::Normal,
            1 => CfsTaskClass::Batch,
            _ => CfsTaskClass::Idle,
        }
    }

    fn effective_nice(&self) -> isize {
        match self.class() {
            CfsTaskClass::Idle => NICE_RANGE_POS as isize,
            CfsTaskClass::Normal | CfsTaskClass::Batch => self.nice.load(Ordering::Acquire),
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

    fn set_sched_params(&self, class: CfsTaskClass, nice: isize, reset_on_fork: bool) {
        let current_vruntime = self.get_vruntime();
        self.rebase_vruntime(current_vruntime);
        self.nice.store(nice, Ordering::Release);
        self.class.store(class as u8, Ordering::Release);
        self.reset_on_fork.store(reset_on_fork, Ordering::Release);
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
            nice: self.nice.load(Ordering::Acquire) as i8,
            reset_on_fork: self.reset_on_fork.load(Ordering::Acquire),
        }
    }

    /// Applies the given scheduling parameters to the task.
    pub fn configure(&self, params: CfsTaskParams) -> bool {
        if !(-20..=19).contains(&(params.nice as isize)) {
            return false;
        }
        self.set_sched_params(params.class, params.nice as isize, params.reset_on_fork);
        true
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
    ready_queue: BTreeMap<(isize, isize), Arc<CFSTask<T>>>, // (vruntime, taskid)
    min_vruntime: Option<isize>,
    id_pool: AtomicIsize,
}

impl<T> CFScheduler<T> {
    /// Creates a new empty [`CFScheduler`].
    pub const fn new() -> Self {
        Self {
            ready_queue: BTreeMap::new(),
            min_vruntime: None,
            id_pool: AtomicIsize::new(0_isize),
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
        self.ready_queue.first_key_value().map(|((vruntime, _), _)| *vruntime)
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
        let taskid = self.next_task_id();
        let vruntime = task.get_vruntime();
        task.set_id(taskid);
        self.ready_queue.insert((vruntime, taskid), task);
        self.refresh_min_vruntime(None);
    }

    fn wakeup_floor(&self, task: &CFSTask<T>) -> isize {
        let floor = self.queue_floor();
        match task.class() {
            CfsTaskClass::Normal => floor,
            CfsTaskClass::Batch => floor.saturating_add(1),
            CfsTaskClass::Idle => floor.saturating_add(2),
        }
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
        task.rebase_vruntime(self.queue_floor());
        self.insert_task(task);
    }

    fn remove_task(&mut self, task: &Self::SchedItem) -> Option<Self::SchedItem> {
        let removed = self
            .ready_queue
            .remove_entry(&(task.clone().get_vruntime(), task.clone().get_id()))
            .map(|(_, task)| task);
        self.refresh_min_vruntime(None);
        removed
    }

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        let next = self.ready_queue.pop_first().map(|(_, task)| task);
        if let Some(task) = &next {
            self.refresh_min_vruntime(Some(task.get_vruntime()));
        } else {
            self.refresh_min_vruntime(None);
        }
        next
    }

    fn put_prev_task(&mut self, prev: Self::SchedItem, _preempt: bool) {
        self.insert_task(prev);
    }

    fn enqueue_task(&mut self, task: Self::SchedItem, reason: EnqueueReason) {
        match reason {
            EnqueueReason::New => self.add_task(task),
            EnqueueReason::Wakeup => {
                let floor = self.wakeup_floor(&task);
                let vruntime = task.get_vruntime().max(floor);
                task.rebase_vruntime(vruntime);
                self.insert_task(task);
            }
            EnqueueReason::Yield | EnqueueReason::Preempt => self.put_prev_task(task, false),
        }
    }

    fn task_tick(&mut self, current: &Self::SchedItem) -> bool {
        current.task_tick();
        let current_vruntime = current.get_vruntime();
        self.refresh_min_vruntime(Some(current_vruntime));

        match self.min_ready_vruntime() {
            Some(ready_min) => current_vruntime > ready_min,
            None => false,
        }
    }

    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> bool {
        if !(-20..=19).contains(&prio) {
            return false;
        }
        self.set_task_params(
            task,
            CfsTaskParams {
                class: task.class(),
                nice: prio as i8,
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
