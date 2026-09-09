//! Session-worker wire grammar and lifecycle state.
//!
//! These modules define the worker's process-neutral control contract. The
//! executable role dispatcher remains outside this library surface.

pub mod lifecycle;
pub mod protocol;
