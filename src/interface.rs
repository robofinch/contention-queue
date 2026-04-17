use variance_family::{Lend, LendFamily};

use crate::QueueHandle;


/// One of the tasks submitted to [`ContentionQueue::process`].
///
/// Equivalently, if the `value`s submitted to a `ContentionQueue` are seen as "tasks",
/// implementors of this trait are what process those tasks.
///
/// Choose whichever interpretation of this trait's name.
///
/// [`ContentionQueue::process`]: crate::queue::ContentionQueue::process
pub trait ProcessTask<'v, 'upper, MutexState, FrontState, Value, Return>
where
    Value: LendFamily<&'upper ()>,
{
    /// Process this task. For the duration of this callback, this task is considered to be at
    /// the front of the queue, and therefore has exclusive access over `front_state`.
    fn process<'q>(
        self,
        value:        Lend<'v, &'upper (), Value>,
        front_state:  &'q mut FrontState,
        queue_handle: QueueHandle<'q, '_, 'upper, MutexState, Value>,
    ) -> Return;
}

impl<'v, 'upper, MutexState, FrontState, Value, Return, P>
    ProcessTask<'v, 'upper, MutexState, FrontState, Value, Return>
for P
where
    Value: LendFamily<&'upper ()>,
    P: for<'q> FnOnce(
        Lend<'v, &'upper (), Value>,
        &'q mut FrontState,
        QueueHandle<'q, '_, 'upper, MutexState, Value>,
    ) -> Return,
{
    #[inline]
    fn process<'q>(
        self,
        value:        Lend<'v, &'upper (), Value>,
        front_state:  &'q mut FrontState,
        queue_handle: QueueHandle<'q, '_, 'upper, MutexState, Value>,
    ) -> Return {
        self(value, front_state, queue_handle)
    }
}

/// Whether poison should be unwrapped, thereby propagating panics, or ignored, which risks the
/// observation of structures whose logical invariants have been violated.
#[derive(Debug, Clone, Copy)]
pub struct PanicOptions {
    /// If a thread panics while holding a mutex, other threads are informed of that panic
    /// via mutex poisoning.
    ///
    /// Processing a value with `unwrap_mutex_poison = true` will unwrap mutex poison errors, and
    /// using `unwrap_mutex_poison = false` will silently ignore any poison.
    ///
    /// # Default
    /// Defaults to `true`.
    pub unwrap_mutex_poison: bool,
    /// If a task in a [`ContentionQueue`] panics, following tasks are informed of the panic
    /// via queue poisoning.
    ///
    /// Processing a value with `unwrap_queue_poison = true` will panic if a preceding task
    /// panicked since the last time `queue.clear_queue_poison()` was called, and using
    /// `propagate_panics = false` will either:
    /// - return [`ProcessResult::ProcessingPanicked`], if the value was being processed by a
    ///   different task which panicked, or
    /// - silently ignore the panic and process the value, if the value had not begun to be
    ///   processed elsewhere yet.
    ///
    /// # Default
    /// Defaults to `true`.
    ///
    /// [`ContentionQueue`]: crate::queue::ContentionQueue
    pub unwrap_queue_poison: bool,
}

impl Default for PanicOptions {
    #[inline]
    fn default() -> Self {
        Self {
            unwrap_mutex_poison: true,
            unwrap_queue_poison: true,
        }
    }
}

/// Information about how a task was processed (if at all), possibly including the return value of
/// a [`ProcessTask`] implementation.
#[derive(Debug, Clone, Copy)]
pub enum ProcessResult<R> {
    /// This call to [`ContentionQueue::process`] processed the task.
    ///
    /// [`ContentionQueue::process`]: crate::queue::ContentionQueue::process
    Processed(R),
    /// The task has been processed by a different call to [`ContentionQueue::process`].
    ///
    /// [`ContentionQueue::process`]: crate::queue::ContentionQueue::process
    ProcessedElsewhere,
    /// The task was being processed by a different call to [`ContentionQueue::process`], but that
    /// call panicked. It is unknown to what extent this task has been processed.
    ///
    /// [`ContentionQueue::process`]: crate::queue::ContentionQueue::process
    ProcessingPanicked,
}
