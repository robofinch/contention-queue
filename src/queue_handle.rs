#![expect(unsafe_code, reason = "synchronize concurrent accesses without storing a mutex inline")]

use std::process;
use std::mem::MaybeUninit;
use std::{
    fmt::{Debug, Formatter, Result as FmtResult},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Mutex, MutexGuard, PoisonError},
};

use variance_family::{Lend, LendFamily};

use crate::queue_state::MutexExclusive;
use crate::externally_synchronized::{UnsafeMutexCell, NotShared};


/// Expose access to parts of a [`ContentionQueue`] to a [`ProcessTask`] callback.
///
/// [`ContentionQueue`]: crate::queue::ContentionQueue
/// [`ProcessTask`]: crate::interface::ProcessTask
pub struct QueueHandle<'q, 'm, 'upper, MutexState, Value: LendFamily<&'upper ()>> {
    /// # Safety invariant
    /// `contention_queue.assert_mutex_good(self.mutex)` must have returned successfully, such that
    /// it is sound to access the contents of `self.mutex_exclusive` while holding a guard of
    /// `self.mutex`.
    mutex:                 NotShared<&'m Mutex<MutexState>>,
    /// # Correctness invariant
    /// We should process queued values in strict FIFO order.
    ///
    /// # Safety invariant
    /// Its contents may only be accessed if a lock used to synchronize this `ContentionQueue`
    /// -- that is, a `mutex` for which `self.assert_mutex_good(mutex)` successfully returned without
    /// panicking -- is currently held.
    mutex_exclusive:       NotShared<&'q UnsafeMutexCell<MutexExclusive<'upper, Value>>>,
    /// # Safety invariant
    /// This field must always be a reference to an initialized guard of `self.mutex`, *except* in
    /// `self.unlocked(_)`. "Except inside `self.unlocked(_)`" is meant strictly, and all means of
    /// exiting `self.unlocked(_)` (i.e. returning or unwinding) should ensure that `self.guard` is
    /// an initialized guard of `mutex`.
    ///
    /// This safety invariant ensures that the robust guarantee of `Self::new` is satisfied.
    guard:                 &'q mut MaybeUninit<MutexGuard<'m, MutexState>>,
    /// # Safety invariant
    /// This field must always be initialized to a mutable reference to the contents of
    /// `self.mutex_exclusive`, *except* in `self.unlocked(_)`. "Except inside `self.unlocked(_)`"
    /// is meant strictly, and all means of exiting `self.unlocked(_)` (i.e. returning or unwinding)
    /// should ensure that `self.mutex_exclusive_guard` is initialized.
    ///
    /// This safety invariant ensures that the robust guarantee of `Self::new` is satisfied.
    mutex_exclusive_guard: MaybeUninit<&'q mut MutexExclusive<'upper, Value>>,
    /// # Correctness invariant
    /// Everything in `self.mutex_exclusive.queue.get(..self.next_idx)` should have `None` values
    /// (i.e., should have already been popped), and everything in
    /// `self.mutex_exclusive.queue.get(self.next_idx..)` should have `Some(_)` values.
    ///
    /// Note that this is not a safety invariant, since it is easily asserted.
    next_idx:              usize,
}

impl<'q, 'm: 'q, 'upper, M, V: LendFamily<&'upper ()>> QueueHandle<'q, 'm, 'upper, M, V> {
    /// # Robust guarantee
    /// Whenever the given `guard` is once again usable by whatever code passed in the guard
    /// reference to this function, it will be a reference to an initialized guard of `mutex`,
    /// regardless of whether `QueueHandle` is dropped, leaked, deallocated, has one of its
    /// methods panic, etc.
    ///
    /// # Correctness
    /// Should only be called if all tasks in the queue have `Some(_)` items. Since values
    /// are popped in a strictly FIFO order by a `QueueHandle`, it suffices to ensure that the
    /// current task is unprocessed.
    ///
    /// # Safety
    /// `guard` must be a reference to an initialized guard of `mutex`, and
    /// `contention_queue.assert_mutex_good(mutex)` must have returned successfully, such that
    /// it is sound to access the contents of `mutex_exclusive` while holding a guard of `mutex`.
    /// `mutex_exclusive` must be `&contention_queue.mutex_exclusive`.
    ///
    /// No references to the contents of `mutex_exclusive` should exist when this function
    /// is called.
    ///
    /// For at least lifetime `'q`, the current task/thread should have exclusive permissions
    /// over `contention_queue.front_exclusive`, such that no other task can become the front
    /// task for at least lifetime `'q`, implying that the backing data of elements in the queue
    /// will remain valid for at least lifetime `'q`.
    #[inline]
    #[must_use]
    pub(super) const unsafe fn new(
        mutex:           &'m Mutex<M>,
        guard:           &'q mut MaybeUninit<MutexGuard<'m, M>>,
        mutex_exclusive: &'q UnsafeMutexCell<MutexExclusive<'upper, V>>,
    ) -> Self {
        // SAFETY: As proven by `guard` and by the caller's assertion, the current thread
        // has a lock for which `self.assert_mutex_good(_)` successfully returned.
        // Additionally, as asserted by the caller, no other references to the contents of
        // `self.mutex_exclusive` exist when `Self::new` is called.
        // Therefore, `mutex_exclusive_guard` is unique right now. It will continue to be unique
        // up until `self.unlocked(_)` is called, at which time the `mutex_exclusive_guard` field
        // will be treated as uninitialized (and "this" reference will not be used again).
        // Note, though, that we are "lying" about the `'q` lifetime of this reference - it might
        // not actually last that long. However, for the whole duration that the reference is
        // actually "live", it is unique; therefore, we satisfy the aliasing rules.
        let mutex_exclusive_guard = unsafe { mutex_exclusive.get_mut() };
        Self {
            // Safety invariant: asserted by caller.
            mutex:                 NotShared::new(mutex),
            // Safety invariant: asserted by caller.
            mutex_exclusive:       NotShared::new(mutex_exclusive),
            // Safety invariant: asserted by caller.
            guard,
            // Safety invariant: upheld immediately above.
            mutex_exclusive_guard: MaybeUninit::new(mutex_exclusive_guard),
            // Correctness invariant: asserted by caller.
            next_idx:              0,
        }
    }

    /// Peek at the second-place position of the queue (noting that the task with access to this
    /// handle is considered to still be at the front of the queue).
    #[must_use]
    pub fn peek<'s>(&'s self) -> Option<&'s Lend<'q, &'upper (), V>> {
        #![expect(clippy::missing_panics_doc, reason = "could only panic due to a bug")]

        // SAFETY: By the safety invariant of `self.mutex_exclusive_guard`,
        // the field is initialized.
        let mutex_exclusive = unsafe { self.mutex_exclusive_guard.assume_init_ref() };

        if let Some(next_task) = mutex_exclusive.queue.get(self.next_idx) {
            // SAFETY: The only time that something is pushed into the queue is in
            // `try_wait_until_at_front`. The `'t` and `'v` lifetimes of the task pushed onto the
            // queue in that method (necessarily) outlive the function body itself, and
            // `try_wait_until_at_front` makes a robust guarantee that it unwinds or returns only
            // if the task it pushed has been popped. Clearly, it has not yet been popped, and it
            // cannot be popped from the queue without becoming the front task. The caller of
            // `Self::new` asserts that that cannot happen for at least lifetime `'q` (which still
            // holds here, noting that we are covariant and not contravariant over `'q`).
            // Therefore, the backing data of `next_task` outlives lifetime `'q` (and thus also
            // outlives a short `'_` limited to this block). In other words... the `'t` and `'v`
            // lifetimes of the task from which the erased `next_task` was created outlive the
            // `'_` and `'q` liftimes, respectively, provided to `inner`.
            //
            // TLDR: The backing data is protected by exclusive access to `front_exclusive`.
            let task = unsafe { next_task.inner::<'s, '_, 'q>() };
            let value = task.value.as_ref();

            #[expect(
                clippy::expect_used,
                reason = "should always succeed, by a correctness invariant of QueueHandle",
            )]
            let value = value
                .expect(
                    "`QueueHandle.next_idx` should be the index of the next task to process, \
                     and unprocessed tasks' values should be `Some(_)`",
                );

            Some(value)
        } else {
            None
        }
    }

    /// Pop the next task value off the queue. Semantically (that is, ignoring implementation
    /// details), it is popped directly from second place; the popped task is never at the front
    /// of the queue, and therefore does not have its associated [`ProcessTask`] callback run.
    ///
    /// [`ProcessTask`]: crate::interface::ProcessTask
    pub fn pop(&mut self) -> Option<Lend<'q, &'upper (), V>> {
        #![expect(clippy::missing_panics_doc, reason = "could only panic due to a bug")]

        // SAFETY: By the safety invariant of `self.mutex_exclusive_guard`,
        // the field is initialized.
        let mutex_exclusive = unsafe { self.mutex_exclusive_guard.assume_init_mut() };

        if let Some(next_task) = mutex_exclusive.queue.get_mut(self.next_idx) {
            // SAFETY: Same as `self.peek()`. The backing data is protected by exclusive access to
            // `front_exclusive`, which (as asserted by the caller of `Self::new`) is held
            // for at least lifetime `'q`.
            let value = unsafe { next_task.take::<'q, '_>() };

            // Since `ErasedTask` is not a `ZST`, the queue needs a nonzero-sized allocation
            // for any nonzero length, and allocations have length at most `isize::MAX`.
            // Therefore, we cannot overflow a `usize`.
            self.next_idx += 1;

            #[expect(
                clippy::expect_used,
                reason = "should always succeed, by a correctness invariant of QueueHandle",
            )]
            let value = value
                .expect(
                    "`QueueHandle.next_idx` should be the index of the next task to process, \
                     and unprocessed tasks' values should be `Some(_)`",
                );

            Some(value)
        } else {
            None
        }
    }

    /// Access the mutex-protected state.
    #[inline]
    #[must_use]
    pub fn mutex_state(&self) -> &M {
        // SAFETY: By the safety invariant of `self.guard`, this field is initialized.
        unsafe { self.guard.assume_init_ref() }
    }

    /// Mutably access the mutex-protected state.
    #[inline]
    #[must_use]
    pub fn mutex_state_mut(&mut self) -> &mut M {
        // SAFETY: By the safety invariant of `self.guard`, this field is initialized.
        unsafe { self.guard.assume_init_mut() }
    }

    /// Temporarily unlock the mutex, execute the provided callback, and then re-lock the mutex.
    ///
    /// If re-locking the mutex fails, the process will be aborted. (Poison errors are ignored,
    /// so an abort should be immensely unlikely.)
    pub fn unlocked<U: FnOnce() -> R, R>(&mut self, with: U) -> R {
        struct ReLock<'a, 'm, 'q, MutexState, ME> {
            mutex:                &'m Mutex<MutexState>,
            mutex_exclusive:      &'q UnsafeMutexCell<ME>,
            /// # Safety invariants
            /// Must be a guard of `self.mutex`.
            ///
            /// *May* briefly be initialized during initialization and destruction.
            ///
            /// **Must** be initialized when this type is dropped.
            guard:                 &'a mut MaybeUninit<MutexGuard<'m, MutexState>>,
            /// # Safety invariants
            /// Must be acquired from `self.mutex_exclusive`.
            ///
            /// *May* briefly be initialized during initialization and destruction.
            ///
            /// **Must** be initialized when this type is dropped.
            mutex_exclusive_guard: &'a mut MaybeUninit<&'q mut ME>,
        }

        impl<'a, 'm, 'q, M, ME> ReLock<'a, 'm, 'q, M, ME> {
            /// Unlock `mutex`, and ensure that the pointee of guard will be restored to a guard
            /// of `mutex` and that the pointee of `mutex_exclusive_guard` will be restored to
            /// a reference to `mutex_exclusive`'s contents **no matter what**.
            ///
            /// # Robust guarantee
            /// If the returned value is dropped (including during an unwind), then either
            /// the pointee of `guard` will be initialized to a valid guard of `mutex` **and**
            /// the pointee of `mutex_exclusive_guard` will be initialized to a reference to the
            /// contents of `mutex_exclusive`, or the process will be aborted.
            ///
            /// # Safety
            /// `guard` must be a reference to an initialized guard of `mutex`.
            /// `mutex_exclusive` must be protected by `mutex`, such that
            /// it is sound to access its contents while holding a guard of `mutex`.
            ///
            /// `mutex_exclusive_guard` should be initialized to the only reference to the contents
            /// of `mutex_exclusive` when this function is called.
            #[must_use]
            unsafe fn new(
                mutex:                 &'m Mutex<M>,
                mutex_exclusive:       &'q UnsafeMutexCell<ME>,
                guard:                 &'a mut MaybeUninit<MutexGuard<'m, M>>,
                mutex_exclusive_guard: &'a mut MaybeUninit<&'q mut ME>,
            ) -> Self {
                let this = Self {
                    mutex,
                    mutex_exclusive,
                    guard,
                    mutex_exclusive_guard,
                };
                // If this somehow panics *before* the mutex becomes unlocked, then the
                // `Drop` implementation of `this` could result in a deadlock or abort. Otherwise,
                // the `Drop` impl would re-lock the mutex and re-initialize `guard`, and possibly
                // panic only after `guard` is initialized.

                // For clarity, show that we are discarding the old reference.
                *this.mutex_exclusive_guard = MaybeUninit::uninit();

                // SAFETY: The caller asserts that `guard`, and therefore `this.guard`,
                // is initialized to a guard of `mutex`.
                unsafe {
                    this.guard.assume_init_drop();
                };
                this
            }
        }

        impl<M, ME> Drop for ReLock<'_, '_, '_, M, ME> {
            fn drop(&mut self) {
                // Safety invariant of `self.guard` and `self.mutex_exclusive_guard`:
                // one way or another, if this `Drop` impl is exited (and other parts of the
                // program begin to be run), `self.guard` and `self.mutex_exclusive_guard` will be
                // initialized. In other words, if initializing them fails, it is a robust guarantee
                // that this function will abort the process.
                // (We do our best to print an error message first.)

                // `eprintln` can panic, and we might as well make sure to avoid any other unwinds.
                // `&mut MaybeUninit<MutexGuard<'_, M>>` is not unwind safe. We write it in
                // a single step that should not unwind (though, constructing the guard value to
                // write could unwind).
                let try_abort_with_error_message = catch_unwind(AssertUnwindSafe(|| {

                    // `&mut MaybeUninit<MutexGuard<'_, M>>` is not unwind safe.
                    let try_relock = catch_unwind(AssertUnwindSafe(|| {
                        let guard = self.mutex.lock().unwrap_or_else(PoisonError::into_inner);
                        // SAFETY: the caller of `Self::new` asserted that the contents
                        // of `self.mutex_exclusive` are protected by `self.mutex`, and we
                        // acquired a guard of `self.mutex`. Additionally, the caller of
                        // `Self::new` asserted that they gave us the only existing reference
                        // to its contents; we discarded that reference. Therefore, we can soundly
                        // acquire a new mutable reference; it is unique. Also see
                        // `QueueHandle::new`; technically, the lifetime we give to this created
                        // reference is too long, and we may discard it in `self.unlocked(_)`
                        // before lifetime `'q` ends. However, we are still careful to follow
                        // the aliasing rules, so this should be sound.
                        let mutex_exclusive_guard = unsafe { self.mutex_exclusive.get_mut() };

                        self.guard.write(guard);
                        self.mutex_exclusive_guard.write(mutex_exclusive_guard);
                    }));

                    #[expect(
                        clippy::disallowed_macros,
                        clippy::print_stderr,
                        reason = "this is not a stray debug print, \
                                  and the possibility of a panic is accounted for",
                    )]
                    if let Err(err) = try_relock {
                        let payload = if let Some(string) = err.downcast_ref::<&'static str>() {
                            string
                        } else if let Some(string) = err.downcast_ref::<String>() {
                            string
                        } else {
                            "an unknown Box<dyn Any> payload"
                        };

                        eprintln!(
                            "QueueHandle::unlocked could not relock its mutex due to to a panic, \
                            whose payload is: {payload}",
                        );

                        process::abort();
                    }
                }));

                if try_abort_with_error_message.is_err() {
                    // Give up :3
                    process::abort();
                }
            }
        }

        // The real function body is just the following four statements.

        // SAFETY: By the safety invariant of `self.guard`, it is a reference to an initialized
        // guard of `self.mutex`.
        //
        // `self.mutex_exclusive` is protected by `self.mutex`, such that it is sound to access
        // the contents of `self.mutex_exclusive` while holding a guard of `self.mutex`
        // (as asserted by the caller of `QueueHandle::new`).
        //
        // Lastly, by the safety invariant of `self.mutex_exclusive_guard`, it is currently
        // initialized to a mutable reference to the contents of `self.mutex_exclusive`.
        let re_lock = unsafe { ReLock::new(
            self.mutex.get_mut(),
            self.mutex_exclusive.get_mut(),
            self.guard,
            &mut self.mutex_exclusive_guard,
        ) };
        let output = with();
        drop(re_lock);
        output
    }

    /// Whether the queue itself (not the associated mutex) is poisoned.
    ///
    /// See [`PanicOptions`] for more information.
    ///
    /// [`PanicOptions`]: crate::interface::PanicOptions
    #[inline]
    #[must_use]
    pub const fn is_queue_poisoned(&self) -> bool {
        // SAFETY: By the safety invariant of `self.mutex_exclusive_guard`,
        // the field is initialized.
        let mutex_exclusive = unsafe { self.mutex_exclusive_guard.assume_init_ref() };

        mutex_exclusive.queue_poisoned
    }

    /// Clear the poison of the queue itself (not the associated mutex).
    ///
    /// See [`PanicOptions`] for more information.
    ///
    /// [`PanicOptions`]: crate::interface::PanicOptions
    #[inline]
    pub const fn clear_queue_poison(&mut self) {
        // SAFETY: By the safety invariant of `self.mutex_exclusive_guard`,
        // the field is initialized.
        let mutex_exclusive = unsafe { self.mutex_exclusive_guard.assume_init_mut() };

        mutex_exclusive.queue_poisoned = false;
    }
}

impl<'upper, M: Debug, V: LendFamily<&'upper ()>> Debug for QueueHandle<'_, '_, 'upper, M, V> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_struct("QueueHandle")
            .field("mutex_state", self.mutex_state())
            .finish_non_exhaustive()
    }
}
