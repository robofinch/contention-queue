<div align="center" class="rustdoc-hidden">
<h1> Contention Queue </h1>
</div>

[<img alt="github" src="https://img.shields.io/badge/github-contention--queue-08f?logo=github" height="20">](https://github.com/robofinch/contention-queue/)
[![Latest version](https://img.shields.io/crates/v/contention-queue.svg)](https://crates.io/crates/contention-queue)
[![Documentation](https://img.shields.io/docsrs/contention-queue)](https://docs.rs/contention-queue/0)
[![Apache 2.0 or MIT license.](https://img.shields.io/badge/license-Apache--2.0_OR_MIT-blue.svg)](#license)

A [`ContentionQueue`] can improve the performance of executing tasks which are processed one at a
time and may be started from multiple threads.

# Concurrency

It provides mutual exclusion over a resource, which can be accessed by the task
at the front of the queue. Pushing a new task requires an additional lock, but that lock can be
released while processing the front task. The front task also has the ability to peek at and pop
following tasks in the queue, enabling tasks to be merged together for the sake of efficiency.

If a task is pushed into a queue when the queue was empty, it entirely skips the process of waiting
to be at the front of the queue; if your tasks are not frequently executed concurrently, you
usually only pay the cost of checking a boolean.

Once the task at the front of the queue finishes being processed, it is popped from the queue and
any tasks that had been popped and merged into that front task are woken up in order. Once all the
processed are woken up, the following unprocessed task (if any) is woken up. The sequence of
wakeups is accomplished with per-task condition variables.

If processing a task panics, the queue becomes poisoned, and the panic may be propagated to
following tasks. However, the queue itself remains in a logically valid state, and poison may be
ignored. `catch_unwind` is used to ensure that following tasks are woken up, preventing unwinds
from causing indefinite hangs.

# Task State

All task states pushed into the queue must have the same type (modulo differences in lifetimes),
though the functions used to process task states need not be the same. [`variance-family`] enables
the task state to be some `T<'varying>` type which is covariant over `'varying`, instead of simply
restricting task states to be `&T` references.

The task states, despite being pushed into a (potentially) multithreaded queue, can reference data
in callers' stacks rather than being restricted to heap data. This is rendered sound by ensuring
that stack frames containing data still referenced by the queue **cannot** be popped.

Under normal conditions -- which includes the possibility of panics in user code and panics due to
poison -- this is accomplished by forcing tasks to be processed and popped before control flow
can return to callers.

However, if a panic attempts to unwind out of a stack frame which contains data referenced by the
queue, the entire process is aborted. The queue code attempts to strictly limit the possibility
of panics leading to an abort, so this scenario is unlikely to actually occur.

# Soundness
This crate contains more lines of comments than lines of code; it is implemented extremely
carefully.

TODO: Miri, loom, and shuttle tests.


## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE][])
* MIT license ([LICENSE-MIT][])

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.

[LICENSE-APACHE]: LICENSE-APACHE
[LICENSE-MIT]: LICENSE-MIT

[`ContentionQueue`]: https://docs.rs/contention-queue/0/contention_queue/struct.ContentionQueue.html
[`variance-family`]: https://docs.rs/variance-family/0/variance_family
