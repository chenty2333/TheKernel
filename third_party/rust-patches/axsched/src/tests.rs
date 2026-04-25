macro_rules! def_test_sched {
    ($name:ident, $scheduler:ty, $task:ty) => {
        mod $name {
            use alloc::sync::Arc;

            use crate::*;

            #[test]
            fn test_sched() {
                const NUM_TASKS: usize = 11;

                let mut scheduler = <$scheduler>::new();
                for i in 0..NUM_TASKS {
                    scheduler.add_task(Arc::new(<$task>::new(i)));
                }

                for i in 0..NUM_TASKS * 10 - 1 {
                    let next = scheduler.pick_next_task().unwrap();
                    assert_eq!(*next.inner(), i % NUM_TASKS);
                    // pass a tick to ensure the order of tasks
                    scheduler.task_tick(&next);
                    scheduler.put_prev_task(next, false);
                }

                let mut n = 0;
                while scheduler.pick_next_task().is_some() {
                    n += 1;
                }
                assert_eq!(n, NUM_TASKS);
            }

            #[test]
            fn bench_yield() {
                const NUM_TASKS: usize = 1_000_000;
                const COUNT: usize = NUM_TASKS * 3;

                let mut scheduler = <$scheduler>::new();
                for i in 0..NUM_TASKS {
                    scheduler.add_task(Arc::new(<$task>::new(i)));
                }

                let t0 = std::time::Instant::now();
                for _ in 0..COUNT {
                    let next = scheduler.pick_next_task().unwrap();
                    scheduler.put_prev_task(next, false);
                }
                let t1 = std::time::Instant::now();
                println!(
                    "  {}: task yield speed: {:?}/task",
                    stringify!($scheduler),
                    (t1 - t0) / (COUNT as u32)
                );
            }

            #[test]
            fn bench_remove() {
                const NUM_TASKS: usize = 10_000;

                let mut scheduler = <$scheduler>::new();
                let mut tasks = Vec::new();
                for i in 0..NUM_TASKS {
                    let t = Arc::new(<$task>::new(i));
                    tasks.push(t.clone());
                    scheduler.add_task(t);
                }

                let t0 = std::time::Instant::now();
                for i in (0..NUM_TASKS).rev() {
                    let t = scheduler.remove_task(&tasks[i]).unwrap();
                    assert_eq!(*t.inner(), i);
                }
                let t1 = std::time::Instant::now();
                println!(
                    "  {}: task remove speed: {:?}/task",
                    stringify!($scheduler),
                    (t1 - t0) / (NUM_TASKS as u32)
                );
            }
        }
    };
}

def_test_sched!(fifo, FifoScheduler::<usize>, FifoTask::<usize>);
def_test_sched!(rr, RRScheduler::<usize, 5>, RRTask::<usize, 5>);
def_test_sched!(cfs, CFScheduler::<usize>, CFSTask::<usize>);

mod cfs_rt {
    use alloc::sync::Arc;

    use crate::*;

    #[test]
    fn rt_tasks_preempt_fair_tasks() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt = Arc::new(CFSTask::new(2));
        assert!(rt.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 10,
            reset_on_fork: false,
        }));
        scheduler.add_task(fair.clone());
        scheduler.add_task(rt.clone());

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 2);
        scheduler.put_prev_task(first, false);

        let second = scheduler.pick_next_task().unwrap();
        assert_eq!(*second.inner(), 2);
    }

    #[test]
    fn ready_rt_task_preempts_running_fair_task() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt = Arc::new(CFSTask::new(2));
        assert!(rt.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 50,
            reset_on_fork: false,
        }));

        scheduler.add_task(fair.clone());
        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 1);
        scheduler.enqueue_task(rt, EnqueueReason::Wakeup);

        assert!(scheduler.task_tick(&running));
    }

    #[test]
    fn higher_rt_priority_runs_first() {
        let mut scheduler = CFScheduler::<usize>::new();
        let low = Arc::new(CFSTask::new(1));
        let high = Arc::new(CFSTask::new(2));
        assert!(low.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 10,
            reset_on_fork: false,
        }));
        assert!(high.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 20,
            reset_on_fork: false,
        }));
        scheduler.add_task(low);
        scheduler.add_task(high);

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 2);
    }

    #[test]
    fn rr_rotates_between_equal_priority_tasks() {
        let mut scheduler = CFScheduler::<usize>::new();
        let a = Arc::new(CFSTask::new(1));
        let b = Arc::new(CFSTask::new(2));
        for task in [&a, &b] {
            assert!(task.configure(CfsTaskParams {
                class: CfsTaskClass::RoundRobin,
                nice: 0,
                rt_priority: 42,
                reset_on_fork: false,
            }));
            scheduler.add_task(task.clone());
        }

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 1);
        for tick in 0..RR_TIMESLICE_TICKS {
            assert_eq!(scheduler.task_tick(&first), tick + 1 == RR_TIMESLICE_TICKS);
        }
        scheduler.put_prev_task(first, false);

        let second = scheduler.pick_next_task().unwrap();
        assert_eq!(*second.inner(), 2);
    }

    #[test]
    fn rr_timer_preemption_rotates_between_equal_priority_tasks() {
        let mut scheduler = CFScheduler::<usize>::new();
        let a = Arc::new(CFSTask::new(1));
        let b = Arc::new(CFSTask::new(2));
        for task in [&a, &b] {
            assert!(task.configure(CfsTaskParams {
                class: CfsTaskClass::RoundRobin,
                nice: 0,
                rt_priority: 42,
                reset_on_fork: false,
            }));
            scheduler.add_task(task.clone());
        }

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 1);
        for tick in 0..RR_TIMESLICE_TICKS {
            assert_eq!(scheduler.task_tick(&first), tick + 1 == RR_TIMESLICE_TICKS);
        }
        scheduler.put_prev_task(first, true);

        let second = scheduler.pick_next_task().unwrap();
        assert_eq!(
            *second.inner(),
            2,
            "timer-driven RR preemption must rotate an expired task",
        );
    }

    #[test]
    fn fifo_same_priority_peers_get_bounded_progress() {
        let mut scheduler = CFScheduler::<usize>::new();
        let a = Arc::new(CFSTask::new(1));
        let b = Arc::new(CFSTask::new(2));
        for task in [&a, &b] {
            assert!(task.configure(CfsTaskParams {
                class: CfsTaskClass::Fifo,
                nice: 0,
                rt_priority: 99,
                reset_on_fork: false,
            }));
            scheduler.add_task(task.clone());
        }

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 1);
        for tick in 0..RR_TIMESLICE_TICKS {
            assert_eq!(scheduler.task_tick(&first), tick + 1 == RR_TIMESLICE_TICKS);
        }
        scheduler.put_prev_task(first, true);

        let second = scheduler.pick_next_task().unwrap();
        assert_eq!(
            *second.inner(),
            2,
            "same-priority FIFO peers need a starvation guard on single-core test VMs",
        );
    }

    #[test]
    fn fifo_periodic_rt_yields_to_fair_control_task() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt = Arc::new(CFSTask::new(2));
        assert!(rt.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 99,
            reset_on_fork: false,
        }));
        scheduler.add_task(fair);
        scheduler.add_task(rt);

        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 2);
        for tick in 0..RR_TIMESLICE_TICKS {
            assert_eq!(scheduler.task_tick(&running), tick + 1 == RR_TIMESLICE_TICKS);
        }
        scheduler.put_prev_task(running, true);

        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(
            *next.inner(),
            1,
            "periodic RT workloads must not starve fair control threads forever",
        );
    }

    #[test]
    fn fifo_rt_peers_still_yield_to_fair_control_task() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt_a = Arc::new(CFSTask::new(2));
        let rt_b = Arc::new(CFSTask::new(3));
        for task in [&rt_a, &rt_b] {
            assert!(task.configure(CfsTaskParams {
                class: CfsTaskClass::Fifo,
                nice: 0,
                rt_priority: 99,
                reset_on_fork: false,
            }));
            scheduler.add_task(task.clone());
        }
        scheduler.add_task(fair);

        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 2);
        for tick in 0..RR_TIMESLICE_TICKS {
            assert_eq!(scheduler.task_tick(&running), tick + 1 == RR_TIMESLICE_TICKS);
        }
        scheduler.put_prev_task(running, true);

        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(
            *next.inner(),
            1,
            "same-priority RT peers must not indefinitely postpone fair control work",
        );
    }

    #[test]
    fn fair_control_task_gets_bounded_budget_before_rt_resumes() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt_a = Arc::new(CFSTask::new(2));
        let rt_b = Arc::new(CFSTask::new(3));
        for task in [&rt_a, &rt_b] {
            assert!(task.configure(CfsTaskParams {
                class: CfsTaskClass::Fifo,
                nice: 0,
                rt_priority: 99,
                reset_on_fork: false,
            }));
            scheduler.add_task(task.clone());
        }
        scheduler.add_task(fair);

        let running = scheduler.pick_next_task().unwrap();
        for tick in 0..RR_TIMESLICE_TICKS {
            assert_eq!(scheduler.task_tick(&running), tick + 1 == RR_TIMESLICE_TICKS);
        }
        scheduler.put_prev_task(running, true);

        let fair = scheduler.pick_next_task().unwrap();
        assert_eq!(*fair.inner(), 1);
        assert!(
            !scheduler.task_tick(&fair),
            "fair control task should get more than one tick to finish joins or command substitutions",
        );
        assert!(
            scheduler.task_tick(&fair),
            "fair budget must stay bounded so RT tasks resume promptly",
        );
        scheduler.put_prev_task(fair, true);

        let next_rt = scheduler.pick_next_task().unwrap();
        assert_eq!(*next_rt.inner(), 3);
    }
}

mod cfs_fork {
    use alloc::sync::Arc;

    use crate::*;

    #[test]
    fn forked_fair_task_does_not_immediately_preempt_parent() {
        let mut scheduler = CFScheduler::<usize>::new();
        let parent = Arc::new(CFSTask::new(1));

        scheduler.add_task(parent.clone());
        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 1);

        for _ in 0..(RR_TIMESLICE_TICKS * 2) {
            assert!(!scheduler.task_tick(&running));
        }

        let child = Arc::new(CFSTask::new(2));
        child.inherit_fair_vruntime_from(&running);
        scheduler.add_task(child);

        assert!(
            !scheduler.task_tick(&running),
            "forked child should inherit the parent's vruntime instead of cutting to the floor",
        );
    }

    #[test]
    fn yielding_parent_lets_forked_child_run() {
        let mut scheduler = CFScheduler::<usize>::new();
        let parent = Arc::new(CFSTask::new(1));

        scheduler.add_task(parent.clone());
        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 1);

        let child = Arc::new(CFSTask::new(2));
        child.inherit_fair_vruntime_from(&running);
        scheduler.add_task(child);

        scheduler.enqueue_task(running, EnqueueReason::Yield);

        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(
            *next.inner(),
            2,
            "a yielding parent should let its freshly forked child run first",
        );
    }

    #[test]
    fn waking_fair_peer_does_not_immediately_preempt_current() {
        let mut scheduler = CFScheduler::<usize>::new();
        let current = Arc::new(CFSTask::new(1));
        let sleeper = Arc::new(CFSTask::new(2));

        scheduler.add_task(current.clone());
        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 1);

        for _ in 0..(RR_TIMESLICE_TICKS * 2) {
            assert!(!scheduler.task_tick(&running));
        }

        scheduler.enqueue_task(sleeper, EnqueueReason::Wakeup);

        assert!(
            !scheduler.task_tick(&running),
            "a freshly woken fair task should not immediately cut ahead of the current peer",
        );
    }
}
