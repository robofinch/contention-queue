use std::collections::VecDeque;

use variance_family::LendFamily;

use crate::task::ErasedTask;


/// Queue state that can only be accessed by the thread whose task is currently processing tasks.
pub(super) struct FrontExclusive<FS> {
    /// User-controlled state. There is no safety invariant on this field.
    pub front_state: FS,
}

/// Queue state that can only be accessed while the mutex is held.
pub(super) struct MutexExclusive<'upper, Value: LendFamily<&'upper ()>> {
    /// Whether the `FrontExclusive` state is locked. Equivalently, whether `process_unchecked`
    /// is currently processing something, or whether exclusive permissions over `front_exclusive`
    /// are currently held by *something*.
    ///
    /// # Safety invariant
    /// See [`ContentionQueue`]'s `front_exclusive` field.
    ///
    /// [`ContentionQueue`]: crate::ContentionQueue
    pub front_locked:   bool,
    /// Whether any previous task panicked since the last time `queue.clear_queue_poison()` was
    /// called.
    ///
    /// There is no safety invariant on this field.
    pub queue_poisoned: bool,
    /// The queue consists of tasks whose `value_unvarying` fields are `None`, which have
    /// semantically been popped from the queue facade presented to the user but have not yet
    /// been woken up, followed by tasks whose `value_unvarying` fields are `Some` and which
    /// have not yet been popped in the facade presented to the user.
    ///
    /// # Safety invariants
    /// - Only the front task of the queue may be woken (with `wake_front_task` or
    ///   `wake_front_task_panicking`).
    /// - An invocation of `process_unchecked` must execute the full sequence of
    ///   pushing an erased task into the queue, calling `task_state.wait_until_at_front(_)`, and
    ///   popping the erased task from the queue without unwinding. (Else, abort.)
    /// - Erased tasks must not be pushed or popped into/from the queue except as provided for
    ///   above.
    /// - The safety requirement for synchronizing accesses to the contents of `mutex_exclusive`
    ///   should be considered to extend to unsafe accesses to the tasks pushed/popped into/from
    ///   the queue.
    ///
    /// ## Implications
    /// The above requirements imply that popping an erased task from the queue can use
    /// `unwrap_unchecked` *and* that the task which an invocation of `process_unchecked` pops
    /// is the same one that it pushed.
    ///
    /// # Correctness invariants
    /// - All tasks in the queue must have a thread waiting for that task to be woken. Otherwise,
    ///   a hang would occur. (Our solution for a thread panicking after pushing a task onto
    ///   the queue and before popping it is to just abort, for simplicity :P)
    /// - A task should only be pushed onto the queue only if something else holds exclusive
    ///   permissions over `front_exclusive` (which means that something else would wake up this
    ///   task and transfer it permissions).
    pub queue:          VecDeque<ErasedTask<'upper, Value>>,
}
