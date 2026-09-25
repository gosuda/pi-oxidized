//! Native product service providers.
//!
//! Provider implementations consume the source-shaped contracts in the sibling
//! [`super::services`] module and own their process-local service state.

pub mod agent_controller;
pub mod attachment;
pub mod models;
pub mod server_services;
pub mod transcript;
