// See https://linebender.org/blog/doc-include for this README inclusion strategy
// File links are not supported by rustdoc
//!
//! [LICENSE-APACHE]: https://github.com/robofinch/contention-queue/blob/main/LICENSE-APACHE
//! [LICENSE-MIT]: https://github.com/robofinch/contention-queue/blob/main/LICENSE-MIT
//!
//! <style>
//! .rustdoc-hidden { display: none; }
//! </style>
#![cfg_attr(doc, doc = include_str!("../README.md"))]

mod interface;

mod queue;
mod queue_handle;
mod queue_state;

mod task_state;
mod task;

mod externally_synchronized;
mod poisoning;


pub use self::{queue::ContentionQueue, queue_handle::QueueHandle};
pub use self::interface::{PanicOptions, ProcessResult, ProcessTask};
