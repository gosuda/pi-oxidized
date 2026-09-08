//! Development-only product remote host surfaces.
//!
//! This module is intentionally separate from the installed `pi` command
//! surface. The product service contracts below mirror the source
//! `experimental/services` declarations; providers, clients, and process
//! lifecycles live in their owning product modules.

pub mod services;
