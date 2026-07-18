//! TypeScript extension-host protocol and Rust adapters.

pub mod adapters;
pub mod client;
pub mod host;
pub mod protocol;
pub mod sanitize;

#[cfg(test)]
mod test_support;
