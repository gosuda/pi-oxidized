//! Coding-agent product services and executable.

pub mod cli;
pub mod core;
pub mod modes;
pub mod remote;

/// The version of the pi crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
