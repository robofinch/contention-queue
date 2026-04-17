#![expect(unsafe_code, reason = "synchronize concurrent accesses without storing a mutex inline")]

use std::{mem, process, ptr};
use std::{collections::VecDeque, mem::MaybeUninit};
use std::{
    fmt::{Debug, Formatter, Result as FmtResult},
    panic::{AssertUnwindSafe, catch_unwind, RefUnwindSafe, resume_unwind, UnwindSafe},
    sync::{atomic::{AtomicUsize, Ordering}, Mutex, MutexGuard, PoisonError},
};

use variance_family::{Lend, LendFamily};

use crate::{
    externally_synchronized::UnsafeMutexCell,
    poisoning::UnwrapPoison as _,
    queue_handle::QueueHandle,
    task_state::TaskState,
};
use crate::{
    interface::{PanicOptions, ProcessResult, ProcessTask},
    queue_state::{FrontExclusive, MutexExclusive},
    task::{ErasedTask, Task},
};


/// A `ContentionQueue` can improve the performance of executing tasks which are processed one at
/// a time and may be started from multiple threads.
///
/// # Concurrency
///
/// It provides mutual exclusion over a resource, which can be accessed by the task
/// at the front of the queue. Pushing a new task requires an additional lock, but that lock can be
/// released while processing the front task. The front task also has the ability to peek at and pop
/// following tasks in the queue, enabling tasks to be merged together for the sake of efficiency.
///
/// If a task is pushed into a queue when the queue was empty, it entirely skips the process of
/// waiting to be at the front of the queue; if your tasks are not frequently executed concurrently,
/// you usually only pay the cost of checking a boolean.
///
/// Once the task at the front of the queue finishes being processed, it is popped from the queue
/// and any tasks that had been popped and merged into that front task are woken up in order. Once
/// all the processed are woken up, the following unprocessed task (if any) is woken up. The
/// sequence of wakeups is accomplished with per-task condition variables.
///
/// If processing a task panics, the queue becomes poisoned, and the panic may be propagated to
/// following tasks. However, the queue itself remains in a logically valid state, and poison may be
/// ignored. `catch_unwind` is used to ensure that following tasks are woken up, preventing unwinds
/// from causing indefinite hangs.
///
/// # Task State
///
/// All task states pushed into the queue must have the same type (modulo differences in lifetimes),
/// though the functions used to process task states need not be the same. [`variance-family`]
/// enables the task state to be some `T<'varying>` type which is covariant over `'varying`,
/// instead of simply restricting task states to be `&T` references.
///
/// The task states, despite being pushed into a (potentially) multithreaded queue, can reference
/// data in callers' stacks rather than being restricted to heap data. This is rendered sound by
/// ensuring that stack frames containing data still referenced by the queue **cannot** be popped.
///
/// Under normal conditions -- which includes the possibility of panics in user code and panics due
/// to poison -- this is accomplished by forcing tasks to be processed and popped before control
/// flow can return to callers.
///
/// However, if a panic attempts to unwind out of a stack frame which contains data referenced by
/// the queue, the entire process is aborted. The queue code attempts to strictly limit the
/// possibility of panics leading to an abort, so this scenario is unlikely to actually occur.
///
/// [`variance-family`]: variance_family
pub struct ContentionQueue<'upper, FrontState, Value: LendFamily<&'upper ()>> {
    /// # Safety invariant
    /// This must be initialized to `0`. Its management is handled by `Self::assert_mutex_good`,
    /// which has an extensive safety comment, and this field should not be mutated anywhere else.
    mutex_address:    AtomicUsize,
    /// There is no safety invariant on this field.
    options:          PanicOptions,
    /// # Safety invariant
    /// Can only be accessed by a given invocation of `self.process_unchecked` if the invocation
    /// acquired exclusive permissions over `front_exclusive` by either:
    /// - changing `front_locked` from `false` to `true`, or
    /// - successfully pushing an erased task into the queue, calling
    ///   `task_state.wait_until_at_front(_)`, and popping the erased task from the queue
    ///   (all without unwinding),
    ///
    /// **and** has not yet released exclusive permissions over `front_exclusive` by changing
    /// `front_locked` from `true` to `false`,
    /// **and** has not yet transferred exclusive permissions over `front_exclusive` to the front
    /// task of the queue (if any) by calling `wake_front_task` or `wake_front_task_panicking`
    /// on it. (By the safety invariants of `queue`, only the front task is permitted to be woken.)
    ///
    /// Additionally, the invocation of `self.process_unchecked` is allowed to release or transfer
    /// exclusive permissions over `front_exclusive` only if it had acquired those permissions and
    /// not yet released or transferred them.
    ///
    /// ## Sufficiency of requirements
    /// We need to make sure that at most one thread ever has exclusive permissions over
    /// `front_exclusive`. Note that we only ever read or write `front_locked`, wake the top task of
    /// the queue, or push something into the queue while we hold a mutex that synchronizes all
    /// those operations. This greatly simplifies the reasoning. Additionally, each invocation of
    /// `self.process` internally uses only a single thread. (User callbacks might be multithreaded,
    /// but the `QueueHandle` is `!Send + !Sync`.)
    ///
    /// - `front_locked` is `false` iff exclusive permissions over `front_exclusive` are *not*
    ///   currently held, so changing `front_locked` from `false` to `true` allows only a single
    ///   invocation of `self.process_unchecked` (and thus a single thread) to acquire exclusive
    ///   permissions.
    /// - Conversely, changing `front_locked` from `true` to `false` if permissions are currently
    ///   held - and then not continuing to act as though we still hold exclusive permissions -
    ///   means that 0 threads then hold exclusive permissions over `front_exclusive` (and the next
    ///   change from `false` to `true` will work correctly).
    /// - Since `wake_front_task` or `wake_front_task_panicking` can be called only if exclusive
    ///   permissions are being transferred, and those are the only two methods which cause
    ///   `task_state.wait_until_at_front(_)` to return (normally, and without unwinding), the
    ///   second means of acquiring exclusive permissions (from a handoff) is also sufficient. The
    ///   restriction on successfully pushing and popping a task without unwinding (together with
    ///   the other safety invariants of `queue`) are, to some extent, "merely" correctness
    ///   requirements for managing exclusive permissions over `front_exclusive`, but elevating
    ///   them to safety requirements makes the state of `queue` easier to reason about.
    ///
    /// # Correctness invariant
    /// If an invocation of `process_unchecked` holds exclusive permissions over `front_exclusive`,
    /// it should transfer them or release them (if there's nothing to transfer them to) before
    /// returning or unwinding out of the function.
    /// Otherwise, indefinite hangs could occur while other threads wait, in futility, to have
    /// exclusive permissions transferred to them.
    front_exclusive:  UnsafeMutexCell<FrontExclusive<FrontState>>,
    /// # Safety invariant
    /// Its contents may only be accessed if a lock used to synchronize this `ContentionQueue`
    /// -- that is, a `mutex` for which `self.assert_mutex_good(mutex)` successfully returned
    /// without panicking -- is currently held.
    mutex_exclusive:  UnsafeMutexCell<MutexExclusive<'upper, Value>>,
}

impl<'upper, FS, V: LendFamily<&'upper ()>> ContentionQueue<'upper, FS, V> {
    /// Construct a new [`ContentionQueue`].
    ///
    /// In addition to its primary role of improving the performance of contending tasks, the queue
    /// provides mutual exclusion over `front_state` by ensuring that only the task at the
    /// front of the queue can access it.
    ///
    /// By default, poison is unwrapped (leading to panics if poison is encountered, thereby
    /// propagating whichever panic caused the poison to begin with).
    #[inline]
    #[must_use]
    pub const fn new(front_state: FS) -> Self {
        let default_options = PanicOptions {
            unwrap_mutex_poison: true,
            unwrap_queue_poison: true,
        };
        Self::new_with_options(front_state, default_options)
    }

    /// Construct a new [`ContentionQueue`].
    ///
    /// In addition to its primary role of improving the performance of contending tasks, the queue
    /// provides mutual exclusion over `front_state` by ensuring that only the task at the
    /// front of the queue can access it.
    #[inline]
    #[must_use]
    pub const fn new_with_options(front_state: FS, options: PanicOptions) -> Self {
        Self {
            // Safety invariant: initialized to zero.
            mutex_address:   AtomicUsize::new(0),
            options,
            front_exclusive: UnsafeMutexCell::new(FrontExclusive {
                front_state,
            }),
            mutex_exclusive: UnsafeMutexCell::new(MutexExclusive {
                // Correctness invariant: nothing has exclusive permissions over `front_state`.
                front_locked:   false,
                queue_poisoned: false,
                // Correctness invariant: vacuously, all of the zero tasks in the queue have a
                // thread waiting for that task to be woken.
                queue:          VecDeque::new(),
            }),
        }
    }
}

impl<'upper, FS, V: LendFamily<&'upper ()>> ContentionQueue<'upper, FS, V> {
    /// # Robust guarantee
    /// If this function successfully returns, then the contents of `self.mutex_exclusive` can be
    /// soundly accessed while `mutex` is locked.
    ///
    /// # Panics
    /// May panic if `mutex` is not the same [`Mutex`] used by previous calls to
    /// `self.process(..)`, `self.is_queue_poisoned(_)`, or `self.clear_queue_poison(_)`.
    ///
    /// To be more precise, this function is guaranteed to panic if `mutex` is not located at the
    /// same address in memory as previously-used `Mutex`es.
    fn assert_mutex_good<M>(&self, mutex: &Mutex<M>) {
        // See `std/src/sys/sync/condvar/pthread.rs::Condvar::Verify`.

        let mutex_addr = ptr::from_ref::<Mutex<M>>(mutex).addr();

        // `Relaxed` is fine here because we never read through `mutex`, we truly care only
        // about the address of the mutex.
        #[expect(clippy::panic, reason = "panic is declared, and is effectively a manual assert!")]
        match self.mutex_address.compare_exchange(
            0,
            mutex_addr,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {}                     // Stored the address
            Err(n) if n == mutex_addr => {} // The same address had already been stored
            _ => panic!(
                "All calls to `ContentionQueue::process` \
                 on the same queue must use the same `Mutex`",
            ),
        }

        assert!(size_of_val(mutex) > 0, "`std::sync::Mutex` should not be a ZST");

        // Correctness of robust guarantee:
        // Any concurrent calls to methods of `self` must use the same mutex as this call or
        // otherwise immediately panic in the above asserts, due to the below reasons. Therefore,
        // locking `mutex` is sufficient to hold exclusive permissions over the contents of
        // `mutex_exclusive` (so long as, by convention, all accesses to `mutex_exclusive` are
        // guarded by `mutex`, which is indeed the case as per its safety invariant).
        // - Regardless of atomic ordering, across all threads, only the first call to
        //   `assert_mutex_good` can observe `0` in `self.mutex_address` and receive permission to
        //   use the mutex regardless of its address.
        // - Later calls to `assert_mutex_good` *might* coincidentally receive permission to use a
        //   different `Mutex` that was moved or otherwise placed at the same location in memory as
        //   the original mutex. However, because we are given a mutex reference, that implies that
        //   the `mutex` is temporarily pinned in memory for the duration we have the reference.
        //
        //   Moreover, the `mutex` is also pinned in memory for the duration of any concurrent
        //   calls. If would be UB if `mutex` were switched for a different mutex in the overlap
        //   in the below diagram:
        //   ┌──────────┬─────┬──────────┬──────────────────────────────────┬──────────┐
        //   │ thread 1 │ ... | ...      │          call to `self.is_queue_poisoned`   │
        //   │ mutex    │ ... │ pinned   │ still pinned, UB to move it here │   pinned │
        //   │ thread 2 │ ... |   call to `self.process`                    │      ... │
        //   └──────────┴─────┴──────────┴──────────────────────────────────┴──────────┘
        //   Therefore, `mutex` remains pinned at the same address for the duration of all
        //   concurrent calls to `self.process`. All `Mutex<T>`s are not `ZST`s... and even
        //   if `std::sync::Mutex<()>` ever did become a ZST, or if there's some technically-sound
        //   shenanigans involving types like `&std::sync::Mutex<!>`, an above assert would prevent
        //   that from causing a problem.
        //
        //   Additionally, even though we do not fix `M` (meaning that, e.g., accesses via
        //   `mutex: &Mutex<[u8; 1]>` and `mutex: &Mutex<[u8]>` could be concurrent), that should
        //   not be a problem; **distinct mutexes cannot overlap**, so this code is sound.
        //   Only marginally shakier is the claim that non-distinct mutexes have the same base
        //   address, though that is "merely" required for this function to not unexpectedly panic.
        //   (And, to be clear, this code is correct for all current implementations of
        //   `std::sync::Mutex`; I just don't feel absolutely confident in claiming that `std`
        //   **cannot** ever change that fact. It'd *probably* be a breaking change? But no need
        //   to even worry about that, since this code would remain sound.)
        //
        //   By the above reasoning, at most one `Mutex` can be at a given memory address at a
        //   given time.
        //   (And, two references to of the same `Mutex` *probably should* have the same address).
        //   Since `self.assert_mutex_good(mutex)`'s asserts only permit a single memory address to
        //   be used for `mutex`, this implies that only a single `Mutex` can pass the above
        //   asserts for the entire duration of a series of concurrent calls to `self.process`.
        // Other note:
        // - It would still be possible to switch out the `Mutex` between non-concurrent calls
        //   and pass the asserts. Note that this would not even cause a panic with `Condvar`s,
        //   since they're only used within concurrent/contending calls.
    }

    /// # Safety
    /// A `mutex` for which `self.assert_mutex_good(mutex)` successfully returned must be locked
    /// by the current thread.
    ///
    /// No other references to the contents of `self.mutex_exclusive` (not derived from the
    /// returned reference) may be active during the lifetime `'_`.
    #[allow(clippy::mut_from_ref, reason = "yeah, places a high burden on the caller")]
    #[inline]
    #[must_use]
    const unsafe fn mutex_exclusive_mut(&self) -> &mut MutexExclusive<'upper, V> {
        // SAFETY: We only need to ensure the aliasing rules are upheld. As per the safety
        // precondition of this function and the extensive reasoning above in
        // `self.assert_mutex_good(_)`, we have that this is sound. The caller does have to
        // manually uphold the aliasing rules, though.
        unsafe { self.mutex_exclusive.get_mut() }
    }

    /// # Robust guarantee
    /// This function returns `true` without unwinding only if exclusive access over
    /// `self.front_exclusive` has been acquired.
    ///
    /// # Safety
    /// A `mutex` for which `self.assert_mutex_good(mutex)` successfully returned must be locked
    /// by the current thread.
    ///
    /// No references to the contents of `self.mutex_exclusive` may exist when this function is
    /// called.
    #[allow(clippy::mut_from_ref, reason = "`UnsafeCell` is involved in this `unsafe fn`")]
    #[inline]
    #[must_use]
    const unsafe fn try_acquire_front_fast(&self) -> bool {
        // SAFETY:
        // - A `mutex` for which `self.assert_mutex_good(mutex)` successfully returned must be
        //   locked by the current thread, as asserted by the caller.
        // - No other references to the contents of `self.mutex_exclusive` are active during the
        //   lifetime `'_` of `mutex_exclusive`, whose lifetime lasts only within this function
        //   call, as asserted by the caller.
        let mutex_exclusive = unsafe { self.mutex_exclusive_mut() };

        if mutex_exclusive.front_locked {
            false
        } else {
            // Safety invariant, and correctness of robust guarantee:
            // We change `mutex_exclusive.front_locked` from `false` to `true`, thereby acquiring
            // exclusive access over `front_exclusive`, as described by the safety invariant of
            // that field.
            mutex_exclusive.front_locked = true;
            true
        }
    }

    /// Note that this function ignores poison to avoid panicking. Both mutex and queue
    /// poison need to be checked. It does, however, allow OOM.
    ///
    /// # Robust guarantees
    /// - When this function returns *or* unwinds, exclusive permissions over
    ///   `self.front_exclusive` have been acquired.
    /// - When this function returns or unwinds, the task pushed onto `queue` by this function has
    ///   already been popped. In other words, it is sound to drop the backing data of `task_state`
    ///   and `value` as soon as this function returns or unwinds and all other usages of them
    ///   (including the returned `TaskWaitResult<V::Varying<'v>>`) cease.
    /// - The returned guard is a guard of `mutex`.
    ///
    /// # Correctness
    /// Something else should have exclusive permissions over `self.front_exclusive`, or else
    /// an indefinite hang may occur.
    ///
    /// # Safety
    /// `guard` must be the guard of a `mutex` for which `self.assert_mutex_good(mutex)`
    /// successfully returned.
    ///
    /// No references to the contents of `self.mutex_exclusive` may exist when this function is
    /// called.
    #[allow(clippy::mut_from_ref, reason = "`UnsafeCell` is involved in this `unsafe fn`")]
    unsafe fn try_wait_until_at_front<'m, 't, 'v, M>(
        &self,
        abort_on_drop: AbortIfNotAtFront,
        mut guard:     MutexGuard<'m, M>,
        task_state:    &'t TaskState,
        value:         Lend<'v, &'upper (), V>,
    ) -> (MutexGuard<'m, M>, TaskWaitResult<Lend<'v, &'upper (), V>>)
    where
        V: LendFamily<&'upper ()>,
    {
        let unerased_task: Task<'t, 'v, V, &()> = Task {
            state: task_state,
            value: Some(value),
        };
        let erased_task = ErasedTask::new(unerased_task);

        {
            // SAFETY: As proven by `guard` and by the caller's assertion, the current thread
            // has a lock for which `self.assert_mutex_good(_)` successfully returned.
            // Additionally, as asserted by the caller, no other references to the contents of
            // `self.mutex_exclusive` exist when `try_wait_until_at_front` is called. Therefore,
            // `mutex_exclusive` is unique within this block (to which its lifetime is constrained).
            let mutex_exclusive = unsafe { self.mutex_exclusive_mut() };

            // This could cause OOM. We document it.
            // Correctness: this thread waits for the pushed task below. Additionally, as asserted
            // by the caller, something else holds exclusive permissions over
            // `self.front_exclusive`.
            mutex_exclusive.queue.push_back(erased_task);
        };

        // Note that this function ignores poison to avoid panicking.
        // SAFETY:
        // The sole calls to `task_state`'s unsafe methods are:
        // - `task_state.wait_until_at_front(guard)` in `try_wait_until_at_front` here,
        // - `next_front_task.state.wake_front_task()` in `self.process_unchecked`, and
        // - `next_front_task.state.wake_front_task_panicking()` in `self.process_unchecked`.
        // During all three calls, we hold a `guard` of a `mutex` for which
        // `self.assert_mutex_good(mutex)` successfully returned, so access to the task is
        // synchronized across threads by a lock.
        // This also fulfills the safety invariant of `mutex_exclusive` that extends to tasks pushed
        // or popped into/from the queue.
        let (returned_guard, processing_panicked) = unsafe {
            task_state.wait_until_at_front(guard)
        };
        // Robustness guarantee of `wait_until_at_front` implies that `returned_guard` is a guard
        // of the same mutex as `guard` was (namely, of `mutex`).
        guard = returned_guard;

        let result = {
            // SAFETY: As proven by `guard` and by the caller's assertion, the current thread
            // has a lock for which `self.assert_mutex_good(_)` successfully returned.
            // Additionally, as asserted by the caller, no other references to the contents of
            // `self.mutex_exclusive` exist when `try_wait_until_at_front` is called. Therefore,
            // `mutex_exclusive` is unique within this block (to which its lifetime is constrained).
            let mutex_exclusive = unsafe { self.mutex_exclusive_mut() };

            let this_task = mutex_exclusive.queue.pop_front();
            // SAFETY: By the safety invariants of `self.mutex_exclusive.queue`, this is sound.
            // (See the documentation of that field for full details.)
            let this_task = unsafe { this_task.unwrap_unchecked() };
            // SAFETY: By the safety invariant of `self.mutex_exclusive.queue`, `this_task`
            // is the task which we pushed, which is the erased version of `unerased_task`,
            // whose lifetimes were `'t` and `'v`. Trivially, `'t` and `'v` are at least as long
            // as `'t` and `'v`, respectively, so this call is sound.
            let this_task = unsafe { this_task.into_inner::<'t, 'v>() };

            if let Some(this_value) = this_task.value {
                TaskWaitResult::Process(this_value)
            } else if processing_panicked.0 {
                TaskWaitResult::ProcessingPanicked
            } else {
                TaskWaitResult::ProcessedElsewhere
            }
        };

        // Safety and correctness invariants of `self.mutex_exclusive.queue`:
        // If the queue cannot be guaranteed to be in an expected state (due to
        // `try_wait_until_at_front` unwinding), we abort.
        // Robustness guarantees of this function: we do not release exclusive permissions
        // over `front_exclusive` below, and we acquired them above by pushing a task onto the
        // queue, waiting for it to be the front, and popping it, all without unwinding (since we
        // do not catch unwinds in this function, and even escalate them to aborts). As described by
        // `front_exclusive`, this acquires exclusive permissions over `front_exclusive`.
        // Therefore, regardless of whether we unwind or return below, we'd still have exclusive
        // permissions over `front_exclusive`. Note that there are no early returns from this
        // function (though, even if we missed on, `abort_on_drop` would be dropped and prevent
        // unsoundness).
        //
        // If we get here, the task that we pushed onto the queue has been popped. Unwinds above
        // are prohibited by `abort_on_drop`, so this function can return or unwind only if
        // the backing data of `task_state` and `value` are no longer referenced by the queue.
        //
        // Additionally, in the sole place where `guard` is mutated,
        // it is set to a guard of `mutex`.
        #[expect(
            clippy::mem_forget,
            reason = "we need to wait until acquiring `front_exclusive` access \
                        before defusing the destructor of `abort_on_drop`",
        )]
        mem::forget(abort_on_drop);

        // Correctness of other robust guarantees: the only way to return from this function
        // (without unwinding) is to pass through all the blocks, which push a task onto the queue,
        // wait for it to be the front, and pop it, all without unwinding (since we do not catch
        // unwinds in this function, and even escalate them to aborts). As described by
        // `front_exclusive`, this acquires exclusive permissions over `front_exclusive`.
        // Additionally, in the sole place where `guard` is mutated,
        // it is set to a guard of `mutex`.
        (guard, result)
    }

    /// # Robust guarantees
    /// If this function returns *or* unwinds, then exclusive permissions over
    /// `self.front_exclusive` has been acquired. The returned guard is a guard of `mutex`.
    ///
    /// # Safety
    /// `guard` must be the guard of `mutex`, and `self.assert_mutex_good(mutex)`
    /// must have successfully returned.
    ///
    /// No references to the contents of `self.mutex_exclusive` may exist when this function is
    /// called.
    unsafe fn try_process_unchecked<'m, 'v, M, T, R>(
        &self,
        abort_on_drop: AbortIfNotAtFront,
        mutex:         &'m Mutex<M>,
        mut guard:     MutexGuard<'m, M>,
        mut value:     Lend<'v, &'upper (), V>,
        task:          T,
    ) -> (MutexGuard<'m, M>, ProcessResult<R>)
    where
        T: ProcessTask<'v, 'upper, M, FS, V, R>,
    {
        // Correctness invariant of `self.front_exclusive`: if we acquired exclusive permissions
        // over `self.front_exclusive` in `self.try_acquire_front_fast()`, then we don't
        // release them when unwinding here... but we abort, so at least no deadlock occurs, ig.
        // SAFETY:
        // - A `mutex` for which `self.assert_mutex_good(mutex)` successfully returned must be
        //   locked by the current thread, as asserted by the caller.
        // - No other references to the contents of `self.mutex_exclusive` exist when this call to
        //   `self.is_already_front()` is made, since we do not create any such reference above
        //   within this function, and the caller asserts that one did not already exist.
        let acquired_front = unsafe { self.try_acquire_front_fast() };

        if acquired_front {
            // Robustness guarantee of this function: we do not release exclusive permissions
            // over `front_exclusive` below, and we acquired them above (as per the robust
            // guarantee of `try_acquire_front_fast`). Therefore, regardless of whether we unwind
            // or return below, we'd still have exclusive permissions over `front_exclusive`.
            #[expect(
                clippy::mem_forget,
                reason = "we need to wait until acquiring `front_exclusive` access \
                          before defusing the destructor of `abort_on_drop`",
            )]
            mem::forget(abort_on_drop);
        } else {
            // Note: `task_state` is dropped only after `try_wait_until_at_front` returns or
            // unwinds, in which case it is no longer referenced by anything on the queue.
            // (The backing data of `value` is dropped even later.)
            let task_state = TaskState::new();

            // Note that this function ignores poison to avoid panicking. We need to check
            // the panic state ourselves below. It does, however, allow OOM, which is escalated
            // to an abort.

            // Robustness guarantee of this function: we do not release exclusive permissions
            // over `front_exclusive` below, and as per the robust guarantee of
            // `try_wait_until_at_front`, the below
            // we acquired them above (as per the robust
            // guarantee of `try_wait_until_at_front`). Therefore, regardless of whether we unwind
            // or return below, we'd still have exclusive permissions over `front_exclusive`.

            // Correctness: since `!acquired_front`, something else has exclusive permissions
            // over `front_exclusive`.
            // SAFETY: `guard` is the guard of a `mutex` for which
            // `self.assert_mutex_good(mutex)` successfully returned, and no references to
            // the contents of `self.mutex_exclusive` exist when this call is made.
            let (returned_guard, result) = unsafe {
                self.try_wait_until_at_front(abort_on_drop, guard, &task_state, value)
            };

            // Robustness guarantee of this function: we do not release exclusive permissions
            // over `front_exclusive` below, and as per the robust guarantee of
            // `try_wait_until_at_front`, the above method call acquires exclusive permissions
            // over `front_exclusive` (regardless of whether it returns or unwinds).

            // Robustness guarantee of `try_wait_until_at_front` implies that `returned_guard` is a
            // guard of the same mutex as `guard` was (namely, of `mutex`).
            guard = returned_guard;

            if self.options.unwrap_queue_poison {
                // SAFETY: As proven by `guard` and by the caller's assertion, the current thread
                // has a lock for which `self.assert_mutex_good(_)` successfully returned.
                // Additionally, as asserted by the caller, no other references to the contents of
                // `self.mutex_exclusive` exist when `try_process_unchecked` is called, and while
                // above method calls may create transient such references, they are encapsulated
                // within functions which do not leak references to the contents of
                // `self.mutex_exclusive` in their return values. Therefore, `mutex_exclusive` is
                // unique within this block (to which its lifetime is constrained).
                let mutex_exclusive = unsafe { self.mutex_exclusive_mut() };
                assert!(!mutex_exclusive.queue_poisoned, "a ContentionQueue was poisoned");
            }

            // Correctness of robustness guarantee about the returned guard: it is set to a guard
            // of `mutex` above.
            match result {
                TaskWaitResult::Process(this_value) => value = this_value,
                TaskWaitResult::ProcessedElsewhere
                    => return (guard, ProcessResult::ProcessedElsewhere),
                TaskWaitResult::ProcessingPanicked
                    => return (guard, ProcessResult::ProcessingPanicked),
            }
        }

        if self.options.unwrap_mutex_poison {
            assert!(!mutex.is_poisoned(), "the Mutex used for a ContentionQueue was poisoned");
        }

        let mut maybe_uninit_guard = MaybeUninit::new(guard);

        // If we get here, we are at the front of the list, and nobody else has processed our value.
        // Since we process values *strictly* in order, this also implies that all later values
        // should be `Some`. This is taken as a correctness invariant rather than a safety
        // invariant.
        let output = {
            // Correctness: all later values after this task (i.e., all the ones in the queue)
            // should have `Some` values.
            // SAFETY: `&mut maybe_uninit_guard` is a reference to an initialized guard of
            // `mutex`, such that `contention_queue.assert_mutex_good(mutex)` returned successfully,
            // and we pass `&contention_queue.mutex_exclusive`. Lastly, as explained just below,
            // we have exclusive access over `front_exclusive` for the duration of
            // `queue_handle`'s existence, so the backing data of stuff in the queue is protected
            // for at least the duration of `queue_handle`'s existence.
            let queue_handle = unsafe {
                QueueHandle::new(mutex, &mut maybe_uninit_guard, &self.mutex_exclusive)
            };

            // SAFETY: (and safety invariant:) if we get here, either we acquired exclusive access
            // over `self.front_exclusive` in `self.try_acquire_front_fast()` or
            // `self.try_wait_until_at_front(..)`, as per those methods' robust guarantees.
            // We do not release that access in this function.
            let front_exclusive = unsafe { self.front_exclusive.get_mut() };

            task.process(value, &mut front_exclusive.front_state, queue_handle)
        };

        // SAFETY: The robust guarantee of `QueueHandle` implies that `maybe_uninit_guard` is
        // initialized to a guard of `mutex`.
        guard = unsafe { maybe_uninit_guard.assume_init() };

        // We *could* check whether `mutex` is poisoned here, but if we get here, `task.process`
        // finished running without panicking, and it's better to report that result.

        // Correctness of robustness guarantee about the returned guard:
        // it is either set to a guard of `mutex` above, or left unmutated, and the caller asserts
        // that the given guard was a guard of `mutex`.
        (guard, ProcessResult::Processed(output))
    }

    /// Process potentially-concurrent tasks.
    ///
    /// At most one task will be processed at a time; other tasks will be pushed onto a queue.
    /// A `task` has the opportunity to pop other tasks off that queue in order to merge
    /// concurrent tasks together. If `task` reaches the front of the queue without being processed
    /// by something else, then `task.process(..)` is called on the given `value`.
    ///
    /// # Safety
    /// `self.assert_mutex_good(mutex)` must have successfully returned.
    ///
    /// # Deadlocks, Panics, Aborts, or other non-termination
    /// This function acquires the `mutex` lock (except inside `queue_handle.unlocked(_)`).
    /// This comes with all the usual threats of deadlocks and other non-termination.
    unsafe fn process_unchecked<'v, 'm, M, T, R>(
        &self,
        mutex:     &'m Mutex<M>,
        mut guard: MutexGuard<'m, M>,
        value:     Lend<'v, &'upper (), V>,
        task:      T,
    ) -> (MutexGuard<'m, M>, ProcessResult<R>)
    where
        T: ProcessTask<'v, 'upper, M, FS, V, R>,
    {
        // Regardless of how `catch_unwind` returns, we ensure that we are at the front of the list.
        // That is... if we aren't, we abort :)
        let abort_on_drop = AbortIfNotAtFront;

        // If an unwind occurs, we re-throw, so it doesn't matter whether `V::Varying<'v>`
        // is unwind safe.
        let process_result = match catch_unwind(AssertUnwindSafe(|| {
            // SAFETY:
            // - `guard` is the guard of a `mutex` for which `self.assert_mutex_good(mutex)`
            //   successfully returned, since we acquired it above.
            // - No references to the contents of `self.mutex_exclusive` exist when this function
            //   is called, since we create none above, and if acquiring `guard` succeeds, there
            //   cannot have been any other references to the contents of `self.mutex_exclusive`
            //   (as such references are only permitted to exist while the lock is held, by the
            //   safety invariant).
            unsafe { self.try_process_unchecked(abort_on_drop, mutex, guard, value, task) }
        })) {
            Ok((returned_guard, process_result)) => {
                guard = returned_guard;
                process_result
            }
            Err(panic_payload) => {
                // No need to cause a double-panic by unwrapping poison.
                guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);

                // Wake up the following task, and tell it that the task which may have processed it
                // panicked. (If the task is unprocessed, the panic bit is ignored.)
                {
                    // SAFETY: See above call to `try_process_unchecked`. For the same reason,
                    // no other references to the contents of `self.mutex_exclusive` exist when
                    // this call is made.
                    let mutex_exclusive = unsafe { self.mutex_exclusive_mut() };

                    mutex_exclusive.queue_poisoned = true;

                    if let Some(next_front_task) = mutex_exclusive.queue.front() {
                        // SAFETY: The only time that something is pushed into the queue
                        // is in `try_wait_until_at_front`. The `'t` and `'v` lifetimes of the
                        // task pushed onto the queue in that method (necessarily) outlive the
                        // function body itself, and `try_wait_until_at_front` makes a robust
                        // guarantee that it unwinds or returns only if the task it pushed
                        // has been popped. Clearly, it has not yet been popped, and it cannot be
                        // popped from the queue without that task's thread acquiring `mutex`.
                        // Since, for the duration of this block, we hold a guard of `mutex`,
                        // it thus follows that `try_wait_until_at_front` cannot return or unwind
                        // for the duration of this block (noting that an abort in it would not
                        // lead to unsoundness, and does not even call the panic handler), and thus
                        // the backing data of `next_front_task` outlives this block.
                        // In other words... the `'t` and `'v` lifetimes of the task from which
                        // the erased `next_front_task` was created outlive the `'_` and `'_`
                        // lifetimes provided to `inner` (which need only last within this block).
                        //
                        // TLDR: The backing data is protected by `mutex`.
                        let next_front_task = unsafe { next_front_task.inner() };

                        // Safety invariant of `front_exclusive`: as per the robust guarantee
                        // of `try_process_unchecked`, as of when we reached the `Err(_)` branch
                        // above, this thread holds exclusive permissions over `front_exclusive`.
                        // We have not since released or transferred those permissions above;
                        // therefore, we have the right to transfer them here. (By the correctness
                        // invariant, we are in fact obligated to transfer them here.)
                        // SAFETY:
                        // The sole calls to `task_state`'s unsafe methods are:
                        // - `task_state.wait_until_at_front(guard)` in `try_wait_until_at_front`,
                        // - `next_front_task.state.wake_front_task()` in `self.process_unchecked`,
                        // - `next_front_task.state.wake_front_task_panicking()` here.
                        // During all three calls, we hold a `guard` of a `mutex` for which
                        // `self.assert_mutex_good(mutex)` successfully returned, so access to the
                        // task is synchronized across threads by a lock. This also fulfills the
                        // safety invariant of `mutex_exclusive` that extends to tasks pushed
                        // or popped into/from the queue.
                        unsafe {
                            next_front_task.state.wake_front_task_panicking();
                        };
                    } else {
                        // Safety invariant of `front_exclusive`: as per the robust guarantee
                        // of `try_process_unchecked`, as of when we reached the `Err(_)` branch
                        // above, this thread holds exclusive permissions over `front_exclusive`.
                        // We have not since released or transferred those permissions above;
                        // therefore, we have the right to release them here. (The correctness
                        // invariant is fulfilled, since we are releasing our exclusive permissions
                        // even on unwind, and we first checked that there's nothing on the queue
                        // to transfer them to.)
                        mutex_exclusive.front_locked = false;
                    }
                }
                drop(guard);

                resume_unwind(panic_payload);
            }
        };

        {
            // SAFETY:
            // - `guard` is the guard of a `mutex` for which `self.assert_mutex_good(mutex)`
            //   successfully returned, since we acquired it above *or* got it back from
            //   `try_process_unchecked`, which has a robust guarantee requiring that the returned
            //   guard is also a guard of `mutex`.
            // - No references to the contents of `self.mutex_exclusive` exist when this function
            //   is called, since we create none above except within functions (which have since
            //   returned without leaking references to the contents of `self.mutex_exclusive` in
            //   their return values), and if acquiring `guard` succeeds, there cannot have been
            //   any other references to the contents of `self.mutex_exclusive` (as such references
            //   are only permitted to exist while the lock is held, by the safety invariant).
            let mutex_exclusive = unsafe { self.mutex_exclusive_mut() };

            if let Some(next_front_task) = mutex_exclusive.queue.front() {
                // SAFETY: Basically the same as the `catch_unwind` `Err` branch.
                //
                // The only time that something is pushed into the queue
                // is in `try_wait_until_at_front`. The `'t` and `'v` lifetimes of the
                // task pushed onto the queue in that method (necessarily) outlive the
                // function body itself, and `try_wait_until_at_front` makes a robust
                // guarantee that it unwinds or returns only if the task it pushed
                // has been popped. Clearly, it has not yet been popped, and it cannot be
                // popped from the queue without that task's thread acquiring `mutex`.
                // Since, for the duration of this block, we hold a guard of `mutex`,
                // it thus follows that `try_wait_until_at_front` cannot return or unwind
                // for the duration of this block (noting that an abort in it would not
                // lead to unsoundness, and does not even call the panic handler), and thus
                // the backing data of `next_front_task` outlives this block.
                // In other words... the `'t` and `'v` lifetimes of the task from which
                // the erased `next_front_task` was created outlive the `'_` and `'_`
                // lifetimes provided to `inner` (which need only last within this block).
                //
                // TLDR: The backing data is protected by `mutex`.
                let next_front_task = unsafe { next_front_task.inner() };

                // Safety invariant of `front_exclusive`:
                // Basically the same as the `catch_unwind` `Err` branch.
                //
                // As per the robust guarantee of `try_process_unchecked`, as of when we reached
                // the `Ok(_)` branch above, this thread holds exclusive permissions over
                // `front_exclusive`. We have not since released or transferred those permissions
                // above (except in the `Err(_)` branch, which diverges, implying that we would not
                // have gotten here). Therefore, we have the right to transfer them here.
                // (By the correctness invariant, we are in fact obligated to transfer them here.)
                // SAFETY:
                // The sole calls to `task_state`'s unsafe methods are:
                // - `task_state.wait_until_at_front(guard)` in `try_wait_until_at_front`,
                // - `next_front_task.state.wake_front_task()` here, and
                // - `next_front_task.state.wake_front_task_panicking()` above.
                // During all three calls, we hold a `guard` of a `mutex` for which
                // `self.assert_mutex_good(mutex)` successfully returned, so access to the task is
                // synchronized across threads by a lock.
                // This also fulfills the safety invariant of `mutex_exclusive` that extends to
                // tasks pushed or popped into/from the queue.
                unsafe {
                    next_front_task.state.wake_front_task();
                };
            } else {
                // Safety invariant of `front_exclusive`:
                // Basically the same as the `catch_unwind` `Err` branch.
                //
                // As per the robust guarantee of `try_process_unchecked`, as of when we reached
                // the `Ok(_)` branch above, this thread holds exclusive permissions over
                // `front_exclusive`. We have not since released or transferred those permissions
                // above (except in the `Err(_)` branch, which diverges, implying that we would not
                // have gotten here). Therefore, we have the right to release them here.
                // (The correctness invariant is fulfilled, since we are releasing our exclusive
                // permissions before returning, and we first checked that there's nothing on the
                // queue to transfer them to.)
                mutex_exclusive.front_locked = false;
            }
        }

        (guard, process_result)
    }

    /// Queue a task to be processed.
    ///
    /// At most one task will be processed at a time; excess tasks will be pushed onto a queue.
    /// Each `task` has the opportunity to pop other tasks off that queue in order to merge
    /// concurrent tasks together. If `task` reaches the front of the queue (without being processed
    /// by something else), then `task.process(..)` is called on the given `value`.
    ///
    /// (Semantically, a `task` that reaches the front of the queue is considered to be at the front
    /// for the whole duration that `task.process(..)` runs; the popped tasks are popped out
    /// directly from the second position in the queue.)
    ///
    /// # Panics
    /// May panic if `mutex` is not the same [`Mutex`] used by previous calls to
    /// `self.process(..)`, `self.is_queue_poisoned(_)`, and `self.clear_queue_poison(_)`.
    ///
    /// To be more precise, this function panics if `mutex` is not located at the same address
    /// in memory as previously-used `Mutex`es.
    ///
    /// May also panic due to poison.
    pub fn process<'v, 'm, M, T, R>(
        &self,
        mutex: &'m Mutex<M>,
        guard: MutexGuard<'m, M>,
        value: Lend<'v, &'upper (), V>,
        task:  T,
    ) -> (MutexGuard<'m, M>, ProcessResult<R>)
    where
        T: ProcessTask<'v, 'upper, M, FS, V, R>,
    {
        self.assert_mutex_good(mutex);

        // SAFETY: if this function call occurs, then `self.assert_mutex_good(mutex)`
        // successfully returned.
        unsafe { self.process_unchecked(mutex, guard, value, task) }
    }

    /// Acquires the mutex and queues a task to be processed.
    ///
    /// See [`ContentionQueue::process`] for more information.
    ///
    /// # Panics
    /// May panic if `mutex` is not the same [`Mutex`] used by previous calls to
    /// `self.process(..)`, `self.is_queue_poisoned(_)`, and `self.clear_queue_poison(_)`.
    ///
    /// To be more precise, this function panics if `mutex` is not located at the same address
    /// in memory as previously-used `Mutex`es.
    ///
    /// May also panic due to poison.
    ///
    /// # Deadlocks, Panics, Aborts, or other non-termination
    /// This function acquires the `mutex` lock (except inside `queue_handle.unlocked(_)`).
    /// This comes with all the usual threats of deadlocks and other non-termination.
    pub fn lock_and_process<'v, M, T, R>(
        &self,
        mutex: &Mutex<M>,
        value: Lend<'v, &'upper (), V>,
        task:  T,
    ) -> ProcessResult<R>
    where
        T: ProcessTask<'v, 'upper, M, FS, V, R>,
    {
        // NOTE: if this panics, that's fine. We have not yet interacted with the queue, so
        // we won't leave any tasks indefinitely asleep.
        let guard = mutex.lock().unwrap_poison(self.options.unwrap_mutex_poison);

        let (_guard, result) = self.process(mutex, guard, value, task);

        result
    }

    /// Whether the queue itself (not the associated mutex) is poisoned.
    ///
    /// See [`PanicOptions`] for more information.
    pub fn is_queue_poisoned<M>(&self, mutex: &Mutex<M>) -> bool {
        self.assert_mutex_good(mutex);
        let guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
        let poisoned = {
            // SAFETY: A mutex for which `self.assert_mutex_good(mutex)` returned successfully
            // is held by this thread for the duration of the returned `mutex_exclusive`
            // borrow. No other references to the contents of `self.mutex_exclusive` exist for the
            // duration of this block, since all such references are only permitted to exist while
            // the lock is held.
            let mutex_exclusive = unsafe { self.mutex_exclusive_mut() };
            mutex_exclusive.queue_poisoned
        };
        drop(guard);
        poisoned
    }

    /// Clear the poison of the queue itself (not the associated mutex).
    ///
    /// See [`PanicOptions`] for more information.
    pub fn clear_queue_poison<M>(&self, mutex: &Mutex<M>) {
        self.assert_mutex_good(mutex);

        let guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
        {
            // SAFETY: A mutex for which `self.assert_mutex_good(mutex)` returned successfully
            // is held by this thread for the duration of the returned `mutex_exclusive`
            // borrow. No other references to the contents of `self.mutex_exclusive` exist for the
            // duration of this block, since all such references are only permitted to exist while
            // the lock is held.
            let mutex_exclusive = unsafe { self.mutex_exclusive_mut() };
            mutex_exclusive.queue_poisoned = false;
        };
        drop(guard);
    }
}

impl<'upper, FS, V: LendFamily<&'upper ()>> Debug for ContentionQueue<'upper, FS, V> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_struct("ContentionQueue").finish_non_exhaustive()
    }
}

// We implement poisoning by default, so it seems reasonable to indicate that this type
// is `UnwindSafe` and `RefUnwindSafe`.
impl<'upper, FS, V: LendFamily<&'upper ()>> UnwindSafe for ContentionQueue<'upper, FS, V> {}
impl<'upper, FS, V: LendFamily<&'upper ()>> RefUnwindSafe for ContentionQueue<'upper, FS, V> {}


#[derive(Debug)]
enum TaskWaitResult<T> {
    Process(T),
    ProcessedElsewhere,
    ProcessingPanicked,
}

#[derive(Debug)]
struct AbortIfNotAtFront;

impl Drop for AbortIfNotAtFront {
    fn drop(&mut self) {
        #[expect(
            clippy::disallowed_macros,
            clippy::print_stderr,
            reason = "this is not a stray debug print, \
                      and the possibility of a panic is accounted for",
        )]
        let _maybe_panic_payload = catch_unwind(|| {
            eprintln!(
                "a ContentionQueue::process task unexpectedly panicked \
                 before it could reach the front of the queue",
            );
        });
        process::abort();
    }
}
