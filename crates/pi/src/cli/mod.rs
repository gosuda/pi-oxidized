//! Command-line parsing and metadata output.

pub mod args;
pub mod bootstrap;
pub mod config_selector;
pub mod entry;
pub mod help;
pub mod package_manager_cli;

pub use args::{Args, Diagnostic, DiagnosticLevel, FlagValue, ListModels, Mode, parse_args};
pub use bootstrap::{
    AppMode, BootstrapInputs, BootstrapIo, BootstrapOutcome, Dispatched, FlagValidationError,
    RuntimeFactory, RuntimeFactoryOptions, RuntimeHandle, is_plain_runtime_metadata_command,
    resolve_app_mode, validate_fork_flags, validate_name, validate_session_id_flags,
};
pub use help::format_help;
pub use package_manager_cli::{
    DispatchPlatform, ListedPackage, ListedScope, PackageCommand, PackageCommandOptions,
    PackageHandler, PackageOutcome, PackageOutput, UpdateTarget, config_command_usage,
    format_config_command_help, format_package_command_help, handle_config_command,
    handle_package_command, is_config_command, package_command_kind, package_command_usage,
    parse_package_command,
};
