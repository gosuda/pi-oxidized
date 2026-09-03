//! Cross-platform product integration seams.
//!
//! Each submodule ports a TypeScript `utils/` surface and exposes it as an
//! injectable, argv-testable boundary so the cross-platform behavior contracts
//! can be unit-tested on any host without spawning real platform tools:
//!
//! - [`command`]: typed command descriptions and the injectable process runner.
//! - [`clipboard`]: clipboard text/image I/O with OSC 52 fallback.
//! - [`image`]: image MIME sniff and the inline-image pipeline facade
//!   (decoding delegates to the read-tool pipeline so the decoder is unique).
//! - [`open_browser`]: cross-platform "open URL in browser" launcher.
//! - [`external_editor`]: external editor lifecycle with cancellation and
//!   guaranteed temporary-file cleanup.
//! - [`debug_dump`]: `/debug` support dump renderer, redaction, and atomic
//!   write.
//! - [`first_run`]: first-run setup gating and persistence.
//! - [`release_packaging`]: release cargo/archive plan, runner, path safety,
//!   and reproducibility.
//! - [`process_tree`]: process-tree termination shared by every spawn site.

pub mod clipboard;
pub mod command;
pub mod debug_dump;
pub mod external_editor;
pub mod first_run;
pub mod image;
pub mod open_browser;
pub mod process_tree;
pub mod release_packaging;
