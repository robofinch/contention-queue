#![expect(unsafe_code, reason = "assert that a generic type is covariant over a lifetime")]

use std::{
    fmt::{Debug, Formatter, Result as FmtResult},
    mem::{MaybeUninit, transmute},
};

use variance_family::{Lend, LendFamily, UpperBound};

use crate::task_state::TaskState;


pub(super) struct Task<'t, 'v, Value: LendFamily<Upper>, Upper: UpperBound> {
    pub state: &'t TaskState,
    pub value: Option<Lend<'v, Upper, Value>>,
}

impl<'v, V: LendFamily<U>, U: UpperBound> Debug for Task<'_, 'v, V, U>
where
    Lend<'v, U, V>: Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_struct("Task")
            .field("state", &self.state)
            .field("value", &self.value)
            .finish()
    }
}

pub(super) struct ErasedTask<'erase, Value: LendFamily<&'erase ()>> {
    /// # Safety invariant
    /// Should be initialized to a value which is valid as type
    /// `Task<'t, 'varying, Value, &'erase ()>` for the `'t` and `'varying` lifetimes used in
    /// `Self::new`.
    ///
    /// If that lifetime is dead, this value may be dangling (in which case the user cannot
    /// soundly call `Self::into_inner`, `Self::inner`, or `Self::take`).
    maybe_dangling: MaybeUninit<Task<'erase, 'erase, Value, &'erase ()>>,
}

#[expect(unreachable_pub, reason = "control visibility at type definition")]
impl<'erase, V: LendFamily<&'erase ()>> ErasedTask<'erase, V> {
    #[inline]
    #[must_use]
    pub const fn new<'t: 't, 'varying: 'varying>(task: Task<'t, 'varying, V, &'erase ()>) -> Self {
        let not_dangling = MaybeUninit::new(task);

        // SAFETY: `not_dangling` is trivially valid
        // as type `MaybeUninit<Task<'t, 'varying, V, &'erase ()>>`
        // or as type `MaybeUninit<Task<'erase, 'erase, V, &'erase ()>>`,
        // since `MaybeUninit` has no validity (or safety) requirements.
        let maybe_dangling = unsafe {
            transmute::<
                MaybeUninit<Task<'t, 'varying, V, &'erase ()>>,
                MaybeUninit<Task<'erase, 'erase, V, &'erase ()>>,
            >(not_dangling)
        };

        // Safety invariant: Trivially, `maybe_dangling` is initialized to a valid value of
        // type `Task<'t, 'varying, V, &'erase ()>`.
        Self { maybe_dangling }
    }

    /// # Safety
    /// The unerased task from which `self` was created (via [`Self::new`]) must have had lifetimes
    /// which were at least as long as `'t` and `'varying`, respectively.
    #[inline]
    #[must_use]
    pub unsafe fn into_inner<'t, 'varying>(self) -> Task<'t, 'varying, V, &'erase ()> {
        let maybe_dangling = self.maybe_dangling;

        // SAFETY: `maybe_dangling` is trivially valid
        // as type `MaybeUninit<Task<'erase, 'erase, V, &'erase ()>>`,
        // or as type `MaybeUninit<Task<'t, 'varying, V, &'erase ()>>`
        // since `MaybeUninit` has no validity (or safety) requirements.
        let not_dangling = unsafe {
            transmute::<
                MaybeUninit<Task<'erase, 'erase, V, &'erase ()>>,
                MaybeUninit<Task<'t, 'varying, V, &'erase ()>>,
            >(maybe_dangling)
        };

        // SAFETY: By the callers assertion, and by the safety invariant of `self.maybe_dangling`,
        // the round-trip between `Self::new` and `Self::into_inner` is equivalent to a lifetime
        // transmute of `Task<'long_a, 'long_varying, V, &'erase ()>`
        // to `Task<'t, 'varying, V, &'erase ()>`. Since `Task` is covariant over `'t`,
        // and the unsafe trait bound on `V` implies that it's sound to perform covariant
        // casts of `'varying`, and since covariant coecions allow lifetimes to be shortened
        // in this position, this lifetime transmute is sound. That is, `not_dangling` is properly
        // inititalized and valid for its output type of `Task<'t, 'varying, V, &'erase ()>`.
        unsafe { not_dangling.assume_init() }
    }

    /// # Safety
    /// The unerased task from which `self` was created (via [`Self::new`]) must have had lifetimes
    /// which were at least as long as `'t` and `'varying`, respectively.
    #[inline]
    #[must_use]
    pub unsafe fn inner<'s: 's, 't, 'varying>(&'s self) -> &'s Task<'t, 'varying, V, &'erase ()> {
        let maybe_dangling = &self.maybe_dangling;

        // SAFETY: `maybe_dangling` is valid
        // as type `&'_ MaybeUninit<Task<'erase, 'erase, V, &'erase ()>>`,
        // or as type `&'_ MaybeUninit<Task<'t, 'varying, V, &'erase ()>>`
        // since `MaybeUninit` has no validity (or safety) requirements, and they have the same
        // size and alignment, since they only differ in lifetimes; therefore, a reference
        // to a pointee of either type is valid as a reference to a pointee of the other type.
        let not_dangling = unsafe {
            transmute::<
                &'_ MaybeUninit<Task<'erase, 'erase, V, &'erase ()>>,
                &'_ MaybeUninit<Task<'t, 'varying, V, &'erase ()>>,
            >(maybe_dangling)
        };

        // SAFETY: By the callers assertion, and by the safety invariant of `self.maybe_dangling`,
        // the round-trip between `Self::new` and `Self::inner` is equivalent to a lifetime
        // transmute of `&'_ Task<'long_a, 'long_varying, V, &'erase ()>`
        // to `&'_ Task<'t, 'varying, V, &'erase ()>`. Since `Task` is covariant over `'t`,
        // and the unsafe trait bound on `V` implies that it's sound to perform covariant
        // casts of `'varying`, and since covariant coecions allow lifetimes to be shortened
        // in this position, this lifetime transmute is sound. That is, `not_dangling` is properly
        // inititalized and valid for its output type of `Task<'t, 'varying, V, &'erase ()>`.
        unsafe { not_dangling.assume_init_ref() }
    }

    /// # Safety
    /// The unerased task from which `self` was created (via [`Self::new`]) must have had a
    /// `'long_varying` lifetime which was at least as long as `'varying`.
    #[inline]
    #[must_use]
    pub unsafe fn take<'varying: 'varying, 's: 's>(
        &'s mut self,
    ) -> Option<Lend<'varying, &'erase (), V>> {
        // Safety invariant: see `value.replace(None)` below, which is the only place in this
        // function where we mutate `self.maybe_dangling`.
        let task: *mut Task<'erase, 'erase, V, &'erase ()> = self.maybe_dangling.as_mut_ptr();

        // SAFETY: `self.maybe_dangling: MaybeUninit<Task<'_, '_, V, &'erase ()>>`, so its
        // allocation is large enough to contain a value of type
        // `Task<'erase, 'erase, V, &'erase ()>`. Therefore, adding the offset of the `value`
        // field to the `task` pointer remains in-bounds of its source allocation, and the
        // addition does not wrap around the address space or exceed `isize::MAX`.
        // Note that, despite this looking like a dereference, no `Deref` operation or similar
        // coercion occurs (since `task` is a raw pointer to a type which has a field named
        // `value`), meaning that the safety requirements of
        // <https://doc.rust-lang.org/std/ptr/macro.addr_of_mut.html#safety> apply, which are the
        // same as those of <https://doc.rust-lang.org/std/primitive.pointer.html#method.offset>,
        // which we have met.
        let value: *mut Option<Lend<'erase, &'erase (), V>> = unsafe { &raw mut (*task).value };

        // We are *shortening* the lifetime of `Option<V<'erase>>` in an invariant position.
        // **Writing a `Some(V)` value to the reference might be unsound**,
        // since writes require that only contravariant casts have occurred.
        // We only ever write `None` below, which is fine. We may read an owned `Some(V<'erase>)`
        // as a `V<'varying>` value, and since `V` is covariant over `'varying` and since the
        // caller asserts that the backing data remains valid, this is fine.
        let value: *mut Option<Lend<'varying, &'erase (), V>> = value.cast();

        // SAFETY: `value` points to a valid value of type `Option<V::Varying<'long_varying>>`,
        // where `'long_varying: 'varying`, as asserted by the caller. (Since `'varying` is
        // currently active, so must `'long_varying` be active.) As required by the unsafe trait
        // bound on `V`, covariant coercions of the `'varying` lifetime of `V::Varying<'varying>`
        // must be sound. Since `Option<_>` is covariant over its generic parameter, and since
        // this read (combined with `Self::new`) amounts to a lifetime transmute from
        // `Option<V::Varying<'long_varying>>` to `Option<V::Varying<'varying>>` (which shortens
        // the lifetime), and since covariant coercions allow lifetimes to be shortened in this
        // position, it follows that any value of type `Option<V::Varying<'long_varying>>` must
        // be valid for type `Option<V::Varying<'varying>>` as well, and thus this is sound.
        //
        // NOTE: `value.replace` places no safety requirements on what it's replacing the pointee
        // of `value` with. However, we also need to fulfill the safety invariant of
        // `self.maybe_dangling`.
        // Safety invariant: After this operation, the `value` field of the task is still a
        // valid value of type `Option<V::Varying<'long_varying>>`, since
        // `Option::<V::Varying<'varying>>::None` is still valid as a value of type
        // `Option<V::Varying<'any>>` for `'any` lifetime, since that enum variant holds no data.
        // Since that's the only field of `self.maybe_dangling` which we mutate, we thus have
        // that `self.maybe_dangling` continues to be initialized to a valid value of type
        // `Option<V::Varying<'long_varying>>`.
        unsafe { value.replace(None) }
    }
}

impl<'erase, V: LendFamily<&'erase ()>> Debug for ErasedTask<'erase, V> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_struct("ErasedTask").finish_non_exhaustive()
    }
}
