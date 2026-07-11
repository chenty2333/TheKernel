use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};
use std::sync::{Mutex, Once};

use axerrno::AxError;

use crate::{WaitQueue, api as axtask, current};

static INIT: Once = Once::new();
static DEFERRED_INIT: Once = Once::new();
static SERIAL: Mutex<()> = Mutex::new(());
static DEFERRED_CALLS: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_REENTER: AtomicBool = AtomicBool::new(false);

fn test_deferred_dispatcher() {
    assert!(axtask::can_block_current());
    DEFERRED_CALLS.fetch_add(1, Ordering::Release);
    if DEFERRED_REENTER.swap(false, Ordering::AcqRel) {
        axtask::run_deferred_work();
    }
}

fn other_deferred_dispatcher() {}

#[test]
fn deferred_work_runs_at_yield_and_suppresses_same_task_recursion() {
    let _lock = SERIAL.lock();
    INIT.call_once(axtask::init_scheduler);
    DEFERRED_INIT.call_once(|| {
        assert!(axtask::set_deferred_work_dispatcher(
            test_deferred_dispatcher
        ));
    });
    assert!(!axtask::set_deferred_work_dispatcher(
        other_deferred_dispatcher
    ));

    DEFERRED_CALLS.store(0, Ordering::Release);
    DEFERRED_REENTER.store(true, Ordering::Release);
    axtask::run_deferred_work();
    assert_eq!(DEFERRED_CALLS.load(Ordering::Acquire), 1);

    DEFERRED_CALLS.store(0, Ordering::Release);
    axtask::yield_now();
    assert!(DEFERRED_CALLS.load(Ordering::Acquire) >= 2);
}

#[test]
fn test_sched_fifo() {
    let _lock = SERIAL.lock();
    INIT.call_once(axtask::init_scheduler);

    const NUM_TASKS: usize = 10;
    static FINISHED_TASKS: AtomicUsize = AtomicUsize::new(0);

    for i in 0..NUM_TASKS {
        axtask::spawn_raw(
            move || {
                println!("sched-fifo: Hello, task {}! ({})", i, current().id_name());
                axtask::yield_now();
                let order = FINISHED_TASKS.fetch_add(1, Ordering::Release);
                assert_eq!(order, i); // FIFO scheduler
            },
            format!("T{i}"),
            0x1000,
        );
    }

    while FINISHED_TASKS.load(Ordering::Acquire) < NUM_TASKS {
        axtask::yield_now();
    }
}

#[test]
fn test_fp_state_switch() {
    let _lock = SERIAL.lock();
    INIT.call_once(axtask::init_scheduler);

    const NUM_TASKS: usize = 5;
    const FLOATS: [f64; NUM_TASKS] = [
        std::f64::consts::PI,
        std::f64::consts::E,
        -std::f64::consts::SQRT_2,
        0.0,
        0.618033988749895,
    ];
    static FINISHED_TASKS: AtomicUsize = AtomicUsize::new(0);

    for (i, float) in FLOATS.iter().enumerate() {
        axtask::spawn(move || {
            let mut value = float + i as f64;
            axtask::yield_now();
            value -= i as f64;

            println!("fp_state_switch: Float {i} = {value}");
            assert!((value - float).abs() < 1e-9);
            FINISHED_TASKS.fetch_add(1, Ordering::Release);
        });
    }
    while FINISHED_TASKS.load(Ordering::Acquire) < NUM_TASKS {
        axtask::yield_now();
    }
}

#[test]
fn test_wait_queue() {
    let _lock = SERIAL.lock();
    INIT.call_once(axtask::init_scheduler);

    const NUM_TASKS: usize = 10;

    static WQ1: WaitQueue = WaitQueue::new();
    static WQ2: WaitQueue = WaitQueue::new();
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    for _ in 0..NUM_TASKS {
        axtask::spawn(move || {
            COUNTER.fetch_add(1, Ordering::Release);
            println!("wait_queue: task {:?} started", current().id());
            WQ1.notify_one(true); // WQ1.wait_until()
            WQ2.wait();

            COUNTER.fetch_sub(1, Ordering::Release);
            println!("wait_queue: task {:?} finished", current().id());
            WQ1.notify_one(true); // WQ1.wait_until()
        });
    }

    println!("task {:?} is waiting for tasks to start...", current().id());
    WQ1.wait_until(|| COUNTER.load(Ordering::Acquire) == NUM_TASKS);
    axtask::yield_now();
    assert_eq!(COUNTER.load(Ordering::Acquire), NUM_TASKS);
    WQ2.notify_all(true); // WQ2.wait()

    println!(
        "task {:?} is waiting for tasks to finish...",
        current().id()
    );
    WQ1.wait_until(|| COUNTER.load(Ordering::Acquire) == 0);
    assert_eq!(COUNTER.load(Ordering::Acquire), 0);
}

#[test]
fn test_task_join() {
    let _lock = SERIAL.lock();
    INIT.call_once(axtask::init_scheduler);

    const NUM_TASKS: usize = 10;
    let mut tasks = Vec::with_capacity(NUM_TASKS);

    for i in 0..NUM_TASKS {
        tasks.push(axtask::spawn_raw(
            move || {
                println!("task_join: task {}! ({})", i, current().id_name());
                axtask::yield_now();
                axtask::exit(i as _);
            },
            format!("T{i}"),
            0x1000,
        ));
    }

    for (i, task) in tasks.into_iter().enumerate() {
        assert_eq!(task.join(), i as _);
    }
}

#[test]
fn task_join_closes_exit_before_waker_registration_race() {
    let task = crate::TaskInner::new_init("join-race".into());
    let mut context = Context::from_waker(Waker::noop());

    // Force exactly the lost-wake ordering: the first state check observes a
    // live task, then exit publishes and wakes before AtomicWaker::register.
    // No later wake exists, so only the post-registration state check can make
    // this poll complete.
    assert_ne!(task.state(), crate::TaskState::Exited);
    task.notify_exit(73);
    let result = task.register_join_waiter_and_recheck_for_test(&mut context);

    assert_eq!(result, Poll::Ready(73));
}

#[test]
fn reclaim_driver_yields_until_exited_queue_drains() {
    let mut reclaim_calls = 0;
    let mut yield_calls = 0;

    axtask::drive_reclaim_until_clear(
        8,
        || {
            reclaim_calls += 1;
            reclaim_calls < 4
        },
        || {
            yield_calls += 1;
        },
    );

    assert_eq!(reclaim_calls, 4);
    assert_eq!(yield_calls, 3);
}

#[test]
fn reclaim_driver_is_bounded_when_scheduler_refs_remain() {
    let mut reclaim_calls = 0;
    let mut yield_calls = 0;

    axtask::drive_reclaim_until_clear(
        8,
        || {
            reclaim_calls += 1;
            true
        },
        || {
            yield_calls += 1;
        },
    );

    assert_eq!(reclaim_calls, 9);
    assert_eq!(yield_calls, 8);
}

#[test]
fn fallible_task_construction_rejects_invalid_stack_sizes() {
    assert!(crate::TaskInner::try_new(|| {}, String::new(), 0).is_err());
    assert!(crate::TaskInner::try_new(|| {}, String::new(), usize::MAX).is_err());
}

#[test]
fn current_affinity_admission_failure_does_not_publish() {
    let mut published = false;
    let result = axtask::admit_affinity_then_publish(
        true,
        || Err::<usize, AxError>(AxError::NoMemory),
        |_| published = true,
    );

    assert_eq!(result, Err(AxError::NoMemory));
    assert!(!published);
}

#[test]
fn remote_running_affinity_admission_failure_does_not_publish() {
    let mut published_mask = None;
    let requested_mask = 0b10usize;
    let result = axtask::admit_affinity_then_publish(
        true,
        || Err::<usize, AxError>(AxError::NoMemory),
        |_| published_mask = Some(requested_mask),
    );

    assert_eq!(result, Err(AxError::NoMemory));
    assert_eq!(published_mask, None);
}

#[test]
fn later_affinity_publication_returns_replaced_helper_for_out_of_lock_drop() {
    struct Helper<'a>(&'a AtomicUsize, usize);

    impl Drop for Helper<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }

    let drops = AtomicUsize::new(0);
    let mut pending = Some(Helper(&drops, 1));
    let displaced = axtask::admit_affinity_then_publish(
        true,
        || Ok::<_, ()>(Helper(&drops, 2)),
        |replacement| core::mem::replace(&mut pending, replacement),
    )
    .unwrap();

    assert_eq!(drops.load(Ordering::Acquire), 0);
    assert_eq!(displaced.as_ref().map(|helper| helper.1), Some(1));
    assert_eq!(pending.as_ref().map(|helper| helper.1), Some(2));
    drop(displaced);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    drop(pending);
    assert_eq!(drops.load(Ordering::Acquire), 2);
}
