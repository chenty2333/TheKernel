//! Pure `process_mrelease(2)` eligibility arithmetic.
//!
//! Mirrors Linux v7.2.3 `mm/oom_kill.c:__do_sys_process_mrelease()`,
//! `task_will_free_mem()` and `__task_will_free_mem()`.

use crate::MmError;

/// `__task_will_free_mem()` inputs for one specific task.
///
/// Linux v7.2.3 `mm/oom_kill.c`:
///
/// ```c
/// static inline bool __task_will_free_mem(struct task_struct *task)
/// {
/// 	struct signal_struct *sig = task->signal;
///
/// 	/*
/// 	 * A coredumping process may sleep for an extended period in
/// 	 * coredump_task_exit(), so the oom killer cannot assume that
/// 	 * the process will promptly exit and release memory.
/// 	 */
/// 	if (sig->core_state)
/// 		return false;
///
/// 	if (sig->flags & SIGNAL_GROUP_EXIT)
/// 		return true;
///
/// 	if (thread_group_empty(task) && (task->flags & PF_EXITING))
/// 		return true;
///
/// 	return false;
/// }
/// ```
///
/// Note the ordering inside Linux `do_exit()`: `synchronize_group_exit()` runs
/// *before* `exit_signals()` sets `PF_EXITING`, and it assigns
/// `signal->flags = SIGNAL_GROUP_EXIT` as soon as `quick_threads` reaches zero
/// (that is, once every thread of the group has entered `do_exit`).  For a
/// single-threaded process the group-exit branch therefore fires at the very
/// top of `do_exit()`, before the thread has detached its `mm`; the
/// `thread_group_empty() && PF_EXITING` branch is a redundant second chance for
/// the same state.  A kernel that only publishes group exit later (for example
/// at zombie publication) is strictly narrower than Linux here.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FreeMemFacts {
    /// `sig->core_state != NULL`: the group is coredumping.
    pub core_dumping: bool,
    /// `sig->flags & SIGNAL_GROUP_EXIT`.
    pub group_exit: bool,
    /// `thread_group_empty(task)`.
    pub thread_group_empty: bool,
    /// `task->flags & PF_EXITING`.
    pub pf_exiting: bool,
    /// `MMF_OOM_SKIP`: the oom reaper has already given up on this mm.
    pub oom_skip: bool,
    /// `atomic_read(&mm->mm_users) <= 1`.
    pub mm_users_at_most_one: bool,
    /// Every *other* process sharing the mm satisfies `__task_will_free_mem()`.
    /// Linux skips same-thread-group sharers and only consults this when
    /// `mm_users > 1`.
    pub other_sharers_dying: bool,
}

/// `__task_will_free_mem()`.
pub const fn task_dying(facts: FreeMemFacts) -> bool {
    if facts.core_dumping {
        return false;
    }
    facts.group_exit || (facts.thread_group_empty && facts.pf_exiting)
}

/// Linux v7.2.3 `mm/oom_kill.c:task_will_free_mem()`:
///
/// ```c
/// static bool task_will_free_mem(struct task_struct *task)
/// {
/// 	struct mm_struct *mm = task->mm;
/// 	struct task_struct *p;
/// 	bool ret = true;
///
/// 	/*
/// 	 * Skip tasks without mm because it might have passed its exit_mm
/// 	 * and exit_oom_victim. ...
/// 	 */
/// 	if (!mm)
/// 		return false;
///
/// 	if (!__task_will_free_mem(task))
/// 		return false;
///
/// 	/*
/// 	 * This task has already been drained by the oom reaper so there are
/// 	 * only small chances it will free some more
/// 	 */
/// 	if (mm_flags_test(MMF_OOM_SKIP, mm))
/// 		return false;
///
/// 	if (atomic_read(&mm->mm_users) <= 1)
/// 		return true;
/// 	...
/// 	return ret;
/// }
/// ```
pub const fn task_will_free_mem(facts: FreeMemFacts) -> bool {
    if !task_dying(facts) {
        return false;
    }
    if facts.oom_skip {
        return false;
    }
    if facts.mm_users_at_most_one {
        return true;
    }
    facts.other_sharers_dying
}

/// Outcome of Linux `__do_sys_process_mrelease()`'s eligibility phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MreleaseEligible {
    /// `task_will_free_mem()` was true: run the reaper.
    Reap,
    /// Not reapable, but the work was already done (`MMF_OOM_SKIP`), so Linux
    /// reports success.
    AlreadySkipped,
    /// Not reapable and not already skipped: `-EINVAL`.
    NotDying,
}

/// Linux `__do_sys_process_mrelease()` ordering, which is the ABI:
///
/// ```c
/// if (flags)
/// 	return -EINVAL;
///
/// task = pidfd_get_task(pidfd, &f_flags);
/// if (IS_ERR(task))
/// 	return PTR_ERR(task);
///
/// /*
///  * Make sure to choose a thread which still has a reference to mm
///  * during the group exit
///  */
/// p = find_lock_task_mm(task);
/// if (!p) {
/// 	ret = -ESRCH;
/// 	goto put_task;
/// }
///
/// mm = p->mm;
/// mmgrab(mm);
///
/// if (task_will_free_mem(p))
/// 	reap = true;
/// else {
/// 	/* Error only if the work has not been done already */
/// 	if (!mm_flags_test(MMF_OOM_SKIP, mm))
/// 		ret = -EINVAL;
/// }
/// ```
///
/// `find_lock_task_mm()` scans the whole thread group for a thread that still
/// holds the mm — a group leader that already ran `exit_mm()` does not make the
/// mm unreachable.  Its failure is `-ESRCH`, and it is evaluated *before*
/// eligibility, so a task with no mm-holding thread reports `-ESRCH` even when
/// it is also not dying.
pub const fn eligibility(
    has_mm_owning_thread: bool,
    will_free_mem: bool,
    oom_skip: bool,
) -> Result<MreleaseEligible, MmError> {
    if !has_mm_owning_thread {
        return Err(MmError::NoMmOwner);
    }
    if will_free_mem {
        return Ok(MreleaseEligible::Reap);
    }
    if oom_skip {
        return Ok(MreleaseEligible::AlreadySkipped);
    }
    Ok(MreleaseEligible::NotDying)
}
