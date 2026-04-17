#![expect(unsafe_code, reason = "external synchronization with a mutex")]

use std::sync::{Condvar, MutexGuard, PoisonError};

use crate::externally_synchronized::UnsafeMutexCell;


const FRONT_BIT: u8 = 0b_01;
const PANIC_BIT: u8 = 0b_10;

#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub(super) struct ProcessingPanicked(pub bool);

/// This is the table that a woken `TaskState` should use to decide what to do. The LSB indicates
/// whether the state is at the front of the queue. The second-least significant bit indicates
/// whether a panic occurred while processing it. (Since `queue_poisoned` may be from a *prior*
/// panic.)
/// <pre overflow-x: scroll>
/// ┌─────────┬────────────────┬──────────────────┬───────────────────────────────────────────┐
/// │ `state` │ `value()`      │ `queue_poisoned` | Response                                  │
/// ├─────────┼────────────────┼──────────────────┼───────────────────────────────────────────┤
/// │ 0's X 0 │ Some(_) | None │  true || false   | Unprocessed, or processing. Keep waiting. │
/// ├─────────┼────────────────┼──────────────────┼───────────────────────────────────────────┤
/// │ 0's X 1 │ Some(_) | None │  true            | At the front. If `unwrap_queue_poison`,   │
/// │         │                │                  | wake next task, then panic. Otherwise,    │
/// │         │                │                  | ignore `queue_poisoned`; see below.       │
/// ├─────────┼────────────────┼──────────────────┼───────────────────────────────────────────┤
/// │ 0's X 1 │ Some(_)        │  false           | At the front. Start processing stuff.     │
/// │         │                │  (or ignored)    | When done, make the next task the front.  │
/// ├─────────┼────────────────┼──────────────────┼───────────────────────────────────────────┤
/// │ 0's 0 1 │ None           │  false           | ProcessedElsewhere. Make the next task    │
/// │         │                │  (or ignored)    | the front, and return.                    │
/// ├─────────┼────────────────┼──────────────────┼───────────────────────────────────────────┤
/// │ 0's 1 1 │ None           │  false           | ProcessingPanicked. Make the next task    │
/// │         │                │  (or ignored)    | the front, and return.                    │
/// ├─────────┴────────────────┴──────────────────┼───────────────────────────────────────────┤
/// │           Anything else                     | Impossible. This case can be ignored.     │
/// └─────────────────────────────────────────────┴───────────────────────────────────────────┘
/// </pre>
/// Note that an unprocessed or processing task is never added to an empty queue (if any active
/// front task is counted as making the queue nonempty); therefore, they don't need to check if
/// they're at the front. Whatever pops them from the queue is responsible for updating their
/// state.
#[derive(Debug)]
pub(super) struct TaskState {
    condvar: Condvar,
    state:   UnsafeMutexCell<u8>,
}

#[expect(unreachable_pub, reason = "control visibility at type definition")]
impl TaskState {
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            condvar: Condvar::new(),
            state:   UnsafeMutexCell::new(0),
        }
    }

    /// Returns the input guard, in addition to whether the task panicked while something else
    /// processed it (if it was processed by something else).
    ///
    /// Note that this function unwraps poison in order to avoid panicking. However, it does
    /// not clear poison.
    ///
    /// # Robust guarantee
    /// The returned guard is a guard of the same mutex as the given guard.
    ///
    /// # Safety
    /// All concurrent calls to `self`'s unsafe methods must be synchronized across threads by the
    /// `Mutex` associated with `guard`.
    pub unsafe fn wait_until_at_front<'m, M>(
        &self,
        mut guard: MutexGuard<'m, M>,
    ) -> (MutexGuard<'m, M>, ProcessingPanicked) {
        // Correctness of robust guarantee: holds by correctness of `std::sync::Condvar`.
        loop {
            guard = self.condvar.wait(guard).unwrap_or_else(PoisonError::into_inner);

            // SAFETY: We only access `self.state`'s contents within `self`'s unsafe methods,
            // so the caller asserts that we are the only function trying to access `self.state`'s
            // contents (since all such method calls are synchronized by a `Mutex` we hold
            // (as asserted by the caller), and we do not leak references to `self.state`'s
            // contents outside the `unsafe` methods of `self`).
            // Therefore, we can exclusively borrow `self.state`'s contents.
            let state = unsafe { *self.state.get_mut() };
            if state & FRONT_BIT != 0 {
                break (guard, ProcessingPanicked(state & PANIC_BIT != 0));
            }
        }
    }

    /// # Safety
    /// All concurrent calls to `self`'s unsafe methods must be synchronized across threads by a
    /// lock.
    /// (The lock must be held when calling this method.)
    pub unsafe fn wake_front_task(&self) {
        // SAFETY: Same as in `self.wait`.
        let state = unsafe { self.state.get_mut() };
        *state |= FRONT_BIT;

        self.condvar.notify_one();
    }

    /// # Safety
    /// All concurrent calls to `self`'s unsafe methods must be synchronized across threads by a
    /// lock.
    /// (The lock must be held when calling this method.)
    pub unsafe fn wake_front_task_panicking(&self) {
        // SAFETY: Same as in `self.wait`.
        let state = unsafe { self.state.get_mut() };
        *state |= PANIC_BIT | FRONT_BIT;

        self.condvar.notify_one();
    }
}
