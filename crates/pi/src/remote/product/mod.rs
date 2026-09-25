//! Development-only product remote host surfaces.
//!
//! This module is intentionally separate from the installed `pi` command
//! surface. The product service contracts below mirror the source
//! `experimental/services` declarations; providers, clients, process
//! lifecycles, and plugin hosts live in their owning product modules. The
//! Unix-only portions retain their local `#[cfg(unix)]` implementations while
//! portable APIs remain reachable on every supported target.

pub mod coordinator;
pub mod plugins;
pub mod process;
pub mod providers;
pub mod relay;
pub mod relay_auth;
pub mod services;
pub mod worker;
