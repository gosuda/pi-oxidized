//! `pi install`/`remove`/`update`/`list`/`config` subcommand router.
//!
//! Ports `.references/pi/packages/coding-agent/src/package-manager-cli.ts` into
//! a thin parser + dispatcher. The parser is pure and infallible; the
//! dispatcher maps parsed options onto exit codes and verbatim status/error
//! strings, driving all side effects through an injected [`PackageHandler`]
//! trait so tests never spawn real subprocesses or touch the network.
//!
//! # Exit code map (matches the TypeScript reference)
//!
//! | path                          | exit | notes                                  |
//! |-------------------------------|------|----------------------------------------|
//! | `--help`                      | 0    | prints usage                           |
//! | unknown option / missing val  | 1    | verbatim error + usage hint            |
//! | install/remove without source | 1    | `Missing {cmd} source.` + usage        |
//! | untrusted local install/remove| 1    | `Project is not trusted. …`            |
//! | install success               | 0    | `Installed {source}`                   |
//! | remove no-match               | 1    | `No matching package found for {src}`  |
//! | remove success                | 0    | `Removed {source}`                     |
//! | list (any)                    | 0    | formatted list                         |
//! | update models error           | 1    |                                        |
//! | update extensions error       | 1    |                                        |
//! | update self error             | 1    |                                        |
//! | update self success           | 0    | win32: `drain_quirk` flag set          |
//! | update self already-latest    | 0    | no-op                                  |

use crate::core::config::{APP_NAME, CONFIG_DIR_NAME};

/// Known subcommand discriminant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackageCommand {
    /// `install`.
    Install,
    /// `remove` (also `uninstall`).
    Remove,
    /// `update`.
    Update,
    /// `list`.
    List,
}

impl PackageCommand {
    /// Literal subcommand name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Remove => "remove",
            Self::Update => "update",
            Self::List => "list",
        }
    }
}

/// Update target resolution (the TS `UpdateTarget` union).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateTarget {
    /// `pi` + extensions.
    All,
    /// `pi` self only.
    Self_,
    /// Installed packages; optional filter `source`.
    Extensions {
        /// Optional `--extension <source>` filter.
        source: Option<String>,
    },
    /// Refresh model catalogs.
    Models,
}

/// Parsed `pi <subcommand> …` options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageCommandOptions {
    /// Resolved subcommand.
    pub command: PackageCommand,
    /// First positional source argument.
    pub source: Option<String>,
    /// Resolved update target (`update` only).
    pub update_target: Option<UpdateTarget>,
    /// Whether the `Extensions are skipped…` note should print.
    pub show_extensions_skipped_note: ExtensionsSkippedNotice,
    /// `-l`/`--local`.
    pub local: bool,
    /// `--force`.
    pub force: bool,
    /// `--approve`/`-a` (true) or `--no-approve`/`-na` (false).
    pub project_trust_override: Option<bool>,
    /// `-h`/`--help`.
    pub help: bool,
    /// First invalid option encountered (verbatim arg).
    pub invalid_option: Option<String>,
    /// First unexpected positional after `source`.
    pub invalid_argument: Option<String>,
    /// First option missing its required value.
    pub missing_option_value: Option<String>,
    /// First conflict message computed during parse.
    pub conflicting_options: Option<String>,
}

/// Outcome of dispatching one subcommand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageOutcome {
    /// Process exit code to surface.
    pub exit_code: u8,
    /// Win32 `pi update` success must drain naturally (Node assert quirk).
    /// When true the caller returns without forcing a process exit.
    pub drain_quirk: bool,
}

impl PackageOutcome {
    /// Success outcome (exit 0).
    #[must_use]
    pub const fn success() -> Self {
        Self {
            exit_code: 0,
            drain_quirk: false,
        }
    }

    /// Failure outcome with a specific code.
    #[must_use]
    pub const fn failure(code: u8) -> Self {
        Self {
            exit_code: code,
            drain_quirk: false,
        }
    }
}

/// One configured package row reported by [`PackageHandler::list`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListedPackage {
    /// Display source string (may include `(filtered)`).
    pub display: String,
    /// Absolute installed path when known.
    pub installed_path: Option<String>,
    /// Scope: `user` or `project`.
    pub scope: ListedScope,
}

/// Scope of a configured package.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListedScope {
    /// Global agent directory.
    User,
    /// Project-local.
    Project,
}

/// Side-effect surface injected by the caller.
///
/// Each method maps to one subcommand operation. Implementations may run real
/// subprocesses (`core::package_manager::PackageManager`) or fake the work in
/// tests. Strings returned by `Err` are surfaced verbatim prefixed with
/// `Error: `.
pub trait PackageHandler {
    /// Install `source` in the requested scope.
    ///
    /// # Errors
    /// Implementation-defined; the error string is shown verbatim.
    fn install(&self, source: &str, local: bool) -> Result<(), String>;

    /// Remove `source`; returns whether anything was removed.
    ///
    /// # Errors
    /// Implementation-defined.
    fn remove(&self, source: &str, local: bool) -> Result<bool, String>;

    /// List configured packages split by scope.
    ///
    /// # Errors
    /// Implementation-defined.
    fn list(&self) -> Result<Vec<ListedPackage>, String>;

    /// Whether the project is currently trusted (for the local-write gate).
    fn is_project_trusted(&self) -> bool;

    /// Refresh model catalogs (`update --models`).
    ///
    /// # Errors
    /// Implementation-defined.
    fn refresh_models(&self) -> Result<(), String>;

    /// Update installed extensions, optionally filtered by `source`.
    ///
    /// # Errors
    /// Implementation-defined.
    fn update_extensions(&self, source: Option<&str>) -> Result<(), String>;

    /// Self-update pi; `force` reinstalls even when on the latest version.
    ///
    /// Returns `Ok(false)` when the engine reports the current install is
    /// already latest and no reinstall was requested.
    ///
    /// # Errors
    /// Implementation-defined.
    fn update_self(&self, force: bool) -> Result<bool, String>;
}

/// Output sink for status/error lines. Implementations capture into a buffer
/// (tests) or write to stdout/stderr (production, via `ProductOutput`).
pub trait PackageOutput {
    /// Write a status line (stdout in TS).
    fn status(&self, line: &str);
    /// Write a dimmed status line (stdout, chalk.dim).
    fn status_dim(&self, line: &str);
    /// Write a success line (stdout, chalk.green).
    fn success(&self, line: &str);
    /// Write an error line (stderr, chalk.red).
    fn error(&self, line: &str);
}

/// Whether `argv[0]` is a recognized package subcommand.
#[must_use]
pub fn package_command_kind(argv0: &str) -> Option<PackageCommand> {
    match argv0 {
        "install" => Some(PackageCommand::Install),
        "remove" | "uninstall" => Some(PackageCommand::Remove),
        "update" => Some(PackageCommand::Update),
        "list" => Some(PackageCommand::List),
        _ => None,
    }
}

/// Whether `argv[0]` is the `config` command.
#[must_use]
pub fn is_config_command(argv0: &str) -> bool {
    argv0 == "config"
}

/// Whether the default self-update should explain that extensions were skipped.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ExtensionsSkippedNotice {
    /// Do not print the note.
    #[default]
    Hidden,
    /// Print the note.
    Show,
}

impl ExtensionsSkippedNotice {
    const fn should_print(self) -> bool {
        matches!(self, Self::Show)
    }
}

/// Parse `pi <subcommand> [rest…]`.
///
/// Returns `None` when the first token is not a recognized package subcommand.
/// Otherwise returns the fully resolved options including conflict checks.
/// Mirrors `parsePackageCommand` in `package-manager-cli.ts:189-387`.
#[must_use]
pub fn parse_package_command(args: &[String]) -> Option<PackageCommandOptions> {
    let (raw, rest) = args.split_first()?;
    let command = package_command_kind(raw)?;
    let mut options = PackageCommandOptions {
        command,
        source: None,
        update_target: None,
        show_extensions_skipped_note: ExtensionsSkippedNotice::Hidden,
        local: false,
        force: false,
        project_trust_override: None,
        help: false,
        invalid_option: None,
        invalid_argument: None,
        missing_option_value: None,
        conflicting_options: None,
    };
    let mut update_flags = UpdateFlagState::default();
    parse_package_arguments(rest, &mut options, &mut update_flags);

    if command == PackageCommand::Update {
        resolve_update_target(&mut options, update_flags);
    }
    Some(options)
}

fn parse_package_arguments(
    args: &[String],
    options: &mut PackageCommandOptions,
    update_flags: &mut UpdateFlagState,
) {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if parse_simple_package_flag(arg, options, update_flags) {
            index += 1;
            continue;
        }
        if arg == "--extension" {
            index += parse_extension_flag(&args[index..], options, update_flags);
            continue;
        }
        if arg.starts_with('-') {
            options.invalid_option.get_or_insert_with(|| arg.to_owned());
        } else if options.source.is_none() {
            options.source = Some(arg.to_owned());
        } else {
            options
                .invalid_argument
                .get_or_insert_with(|| arg.to_owned());
        }
        index += 1;
    }
}

fn parse_simple_package_flag(
    arg: &str,
    options: &mut PackageCommandOptions,
    update_flags: &mut UpdateFlagState,
) -> bool {
    match arg {
        "-h" | "--help" => options.help = true,
        "-l" | "--local" => {
            if matches!(
                options.command,
                PackageCommand::Install | PackageCommand::Remove
            ) {
                options.local = true;
            } else {
                options.invalid_option.get_or_insert_with(|| arg.to_owned());
            }
        }
        "--self" => record_update_flag(UpdateFlag::Self_, arg, options, update_flags),
        "--extensions" => {
            record_update_flag(UpdateFlag::Extensions, arg, options, update_flags);
        }
        "--models" => record_update_flag(UpdateFlag::Models, arg, options, update_flags),
        "--all" => record_update_flag(UpdateFlag::All, arg, options, update_flags),
        "--approve" | "-a" => options.project_trust_override = Some(true),
        "--no-approve" | "-na" => options.project_trust_override = Some(false),
        "--force" => {
            if options.command == PackageCommand::Update {
                options.force = true;
            } else {
                options.invalid_option.get_or_insert_with(|| arg.to_owned());
            }
        }
        _ => return false,
    }
    true
}

fn record_update_flag(
    flag: UpdateFlag,
    arg: &str,
    options: &mut PackageCommandOptions,
    update_flags: &mut UpdateFlagState,
) {
    if options.command == PackageCommand::Update {
        update_flags.flags.insert(flag);
    } else {
        options.invalid_option.get_or_insert_with(|| arg.to_owned());
    }
}

fn parse_extension_flag(
    args: &[String],
    options: &mut PackageCommandOptions,
    update_flags: &mut UpdateFlagState,
) -> usize {
    let arg = args[0].as_str();
    if options.command != PackageCommand::Update {
        options.invalid_option.get_or_insert_with(|| arg.to_owned());
        return 1;
    }
    match args.get(1).map(String::as_str) {
        Some(value) if !value.starts_with('-') => {
            if update_flags.extension_source.is_some() {
                options
                    .conflicting_options
                    .get_or_insert_with(|| "--extension can only be provided once".to_owned());
            } else {
                update_flags.extension_source = Some(value.to_owned());
            }
            2
        }
        _ => {
            options
                .missing_option_value
                .get_or_insert_with(|| arg.to_owned());
            1
        }
    }
}

fn resolve_update_target(options: &mut PackageCommandOptions, flags: UpdateFlagState) {
    let self_flag = flags.flags.contains(UpdateFlag::Self_);
    let extensions_flag = flags.flags.contains(UpdateFlag::Extensions);
    let models_flag = flags.flags.contains(UpdateFlag::Models);
    let all_flag = flags.flags.contains(UpdateFlag::All);
    let extension_flag_source = flags.extension_source;

    if all_flag && (self_flag || extensions_flag || models_flag || extension_flag_source.is_some())
    {
        options.conflicting_options.get_or_insert_with(|| {
            "--all cannot be combined with --self, --extensions, --models, or --extension"
                .to_owned()
        });
    }
    if all_flag && options.source.is_some() {
        options
            .conflicting_options
            .get_or_insert_with(|| "--all cannot be combined with a positional source".to_owned());
    }

    if models_flag {
        if self_flag || extensions_flag || all_flag || extension_flag_source.is_some() {
            options.conflicting_options.get_or_insert_with(|| {
                "--models cannot be combined with --self, --extensions, --all, or --extension"
                    .to_owned()
            });
        }
        if options.source.is_some() {
            options.conflicting_options.get_or_insert_with(|| {
                "--models cannot be combined with a positional source".to_owned()
            });
        }
        options.update_target = Some(UpdateTarget::Models);
        return;
    }

    if let Some(ext_source) = extension_flag_source.clone() {
        if self_flag || extensions_flag || all_flag {
            options.conflicting_options.get_or_insert_with(|| {
                "--extension cannot be combined with --self, --extensions, or --all".to_owned()
            });
        }
        if options.source.is_some() {
            options.conflicting_options.get_or_insert_with(|| {
                "--extension cannot be combined with a positional source".to_owned()
            });
        }
        options.update_target = Some(UpdateTarget::Extensions {
            source: Some(ext_source),
        });
        return;
    }

    if let Some(source) = options.source.clone() {
        let source_is_self = source == "self" || source == APP_NAME;
        if source_is_self {
            options.update_target = Some(if extensions_flag {
                UpdateTarget::All
            } else {
                UpdateTarget::Self_
            });
        } else {
            if extensions_flag || self_flag || all_flag {
                options.conflicting_options.get_or_insert_with(|| {
                    "positional update targets cannot be combined with --self, --extensions, or --all"
                        .to_owned()
                });
            }
            options.update_target = Some(UpdateTarget::Extensions {
                source: Some(source),
            });
        }
        return;
    }

    if all_flag || self_flag && extensions_flag {
        options.update_target = Some(UpdateTarget::All);
    } else if self_flag {
        options.update_target = Some(UpdateTarget::Self_);
    } else if extensions_flag {
        options.update_target = Some(UpdateTarget::Extensions { source: None });
    } else {
        options.update_target = Some(UpdateTarget::Self_);
        options.show_extensions_skipped_note = ExtensionsSkippedNotice::Show;
    }
}

#[derive(Clone, Copy)]
enum UpdateFlag {
    Self_,
    Extensions,
    Models,
    All,
}

#[derive(Default)]
struct UpdateFlagSet(u8);

impl UpdateFlagSet {
    fn insert(&mut self, flag: UpdateFlag) {
        self.0 |= 1 << flag as u8;
    }

    const fn contains(&self, flag: UpdateFlag) -> bool {
        self.0 & (1 << flag as u8) != 0
    }
}

#[derive(Default)]
struct UpdateFlagState {
    flags: UpdateFlagSet,
    extension_source: Option<String>,
}

/// Usage string for one subcommand.
#[must_use]
pub fn package_command_usage(command: PackageCommand) -> String {
    match command {
        PackageCommand::Install => {
            format!("{APP_NAME} install <source> [-l] [--approve|--no-approve]")
        }
        PackageCommand::Remove => {
            format!("{APP_NAME} remove <source> [-l] [--approve|--no-approve]")
        }
        PackageCommand::Update => format!(
            "{APP_NAME} update [source|self|pi] [--self|--extensions|--models|--all] [--extension <source>] [--approve|--no-approve] [--force]"
        ),
        PackageCommand::List => format!("{APP_NAME} list [--approve|--no-approve]"),
    }
}

/// Usage string for the `config` command.
#[must_use]
pub fn config_command_usage() -> String {
    format!("{APP_NAME} config [-l] [--approve|--no-approve]")
}

/// Render the per-subcommand help block (`printPackageCommandHelp`).
#[must_use]
pub fn format_package_command_help(command: PackageCommand) -> String {
    let usage = package_command_usage(command);
    match command {
        PackageCommand::Install => format!(
            "Usage:\n  {usage}\n\nInstall a package and add it to settings.\n\nOptions:\n  -l, --local       Install project-locally ({CONFIG_DIR_NAME}/settings.json)\n  -a, --approve     Trust project-local files for this command\n  -na, --no-approve Ignore project-local files for this command\n\nExamples:\n  {APP_NAME} install npm:@foo/bar\n  {APP_NAME} install git:github.com/user/repo\n  {APP_NAME} install git:git@github.com:user/repo\n  {APP_NAME} install https://github.com/user/repo\n  {APP_NAME} install ssh://git@github.com/user/repo\n  {APP_NAME} install ./local/path\n"
        ),
        PackageCommand::Remove => format!(
            "Usage:\n  {usage}\n\nRemove a package and its source from settings.\nAlias: {APP_NAME} uninstall <source> [-l]\n\nOptions:\n  -l, --local       Remove from project settings ({CONFIG_DIR_NAME}/settings.json)\n  -a, --approve     Trust project-local files for this command\n  -na, --no-approve Ignore project-local files for this command\n\nExamples:\n  {APP_NAME} remove npm:@foo/bar\n  {APP_NAME} uninstall npm:@foo/bar\n"
        ),
        PackageCommand::Update => format!(
            "Usage:\n  {usage}\n\nUpdate pi, installed packages, or model catalogs.\n\nOptions:\n  --self                  Update pi only (default when no target is given)\n  --extensions            Update installed packages only\n  --models                Refresh model catalogs only\n  --all                   Update pi and installed packages\n  --extension <source>    Update one package only\n  -a, --approve           Trust project-local files for this command\n  -na, --no-approve       Ignore project-local files for this command\n  --force                 Reinstall pi even if the current version is latest\n\nShort forms:\n  {APP_NAME} update                Update pi only\n  {APP_NAME} update --all          Update pi and all extensions\n  {APP_NAME} update --models       Refresh model catalogs only\n  {APP_NAME} update <source>       Update one package\n  {APP_NAME} update pi             Update pi only (self works as alias to pi)\n"
        ),
        PackageCommand::List => format!(
            "Usage:\n  {usage}\n\nList installed packages from user and project settings.\n\nOptions:\n  -a, --approve      Trust project-local files for this command\n  -na, --no-approve  Ignore project-local files for this command\n"
        ),
    }
}

/// Render the `config` help block (`printConfigCommandHelp`).
#[must_use]
pub fn format_config_command_help() -> String {
    format!(
        "Usage:\n  {}\n\nOpen the resource configuration TUI to enable or disable package resources.\nWithout -l, starts in global settings (~/{CONFIG_DIR_NAME}/agent/settings.json).\nPress Tab in the TUI to switch between global and project-local modes.\n\nOptions:\n  -l, --local       Edit project overrides ({CONFIG_DIR_NAME}/settings.json)\n  -a, --approve     Trust project-local files for this command with -l\n  -na, --no-approve Ignore project-local files for this command with -l\n",
        config_command_usage()
    )
}

/// Decide whether the dispatch must enforce project trust before writing.
fn writes_project_package_config(command: PackageCommand, local: bool) -> bool {
    matches!(command, PackageCommand::Install | PackageCommand::Remove) && local
}

/// Route `pi config …` if recognized. Returns `None` when `argv[0]` is not
/// `config`.
///
/// `--help` prints the config usage block and exits 0. Otherwise, `config`
/// opens the resource-config TUI (interactive) or reports that the TUI
/// requires a terminal (non-interactive), matching the reference gate at
/// `package-manager-cli.ts:603`.
pub fn handle_config_command(
    args: &[String],
    out: &dyn PackageOutput,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
) -> Option<PackageOutcome> {
    let (raw, _rest) = args.split_first()?;
    if !is_config_command(raw) {
        return None;
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        out.status(&format_config_command_help());
        return Some(PackageOutcome::success());
    }
    if !stdin_is_tty || !stdout_is_tty {
        out.error(&format!(
            "{APP_NAME} config requires an interactive terminal."
        ));
        return Some(PackageOutcome::failure(1));
    }
    // The resource-config TUI is launched by the interactive mode dispatcher.
    // The bootstrap routes `config` to interactive mode when TTY is available;
    // if we reach here, the TUI runner was not injected.
    out.error(&format!("{APP_NAME} config TUI runner not configured."));
    Some(PackageOutcome::failure(1))
}
///
/// Mirrors the verbatim status/error strings and exit-code mapping of
/// `handlePackageCommand` (`package-manager-cli.ts:676-887`).
pub fn handle_package_command(
    args: &[String],
    handler: &dyn PackageHandler,
    out: &dyn PackageOutput,
    platform: DispatchPlatform,
) -> Option<PackageOutcome> {
    let options = parse_package_command(args)?;
    if let Some(outcome) = package_command_preflight(&options, handler, out) {
        return Some(outcome);
    }

    Some(match options.command {
        PackageCommand::Install => dispatch_install(&options, handler, out),
        PackageCommand::Remove => dispatch_remove(&options, handler, out),
        PackageCommand::List => dispatch_list(handler, out),
        PackageCommand::Update => dispatch_update(&options, handler, out, platform),
    })
}

fn package_command_preflight(
    options: &PackageCommandOptions,
    handler: &dyn PackageHandler,
    out: &dyn PackageOutput,
) -> Option<PackageOutcome> {
    if options.help {
        out.status(&format_package_command_help(options.command));
        return Some(PackageOutcome::success());
    }
    if let Some(opt) = &options.invalid_option {
        out.error(&format!(
            "Unknown option {opt} for \"{}\".",
            options.command.as_str()
        ));
        out.error(&format!(
            "Use \"{APP_NAME} --help\" or \"{}\".",
            package_command_usage(options.command)
        ));
        return Some(PackageOutcome::failure(1));
    }
    if let Some(opt) = &options.missing_option_value {
        out.error(&format!("Missing value for {opt}."));
        output_package_usage(options.command, out);
        return Some(PackageOutcome::failure(1));
    }
    if let Some(arg) = &options.invalid_argument {
        out.error(&format!("Unexpected argument {arg}."));
        output_package_usage(options.command, out);
        return Some(PackageOutcome::failure(1));
    }
    if let Some(msg) = &options.conflicting_options {
        out.error(msg);
        output_package_usage(options.command, out);
        return Some(PackageOutcome::failure(1));
    }
    if matches!(
        options.command,
        PackageCommand::Install | PackageCommand::Remove
    ) && options.source.is_none()
    {
        out.error(&format!("Missing {} source.", options.command.as_str()));
        output_package_usage(options.command, out);
        return Some(PackageOutcome::failure(1));
    }
    if options.command == PackageCommand::Update
        && matches!(options.update_target, Some(UpdateTarget::Models))
    {
        return Some(match handler.refresh_models() {
            Ok(()) => PackageOutcome::success(),
            Err(msg) => {
                out.error(&format!("Error: {msg}"));
                PackageOutcome::failure(1)
            }
        });
    }
    if writes_project_package_config(options.command, options.local)
        && !handler.is_project_trusted()
    {
        out.error("Project is not trusted. Use --approve to modify local package config.");
        return Some(PackageOutcome::failure(1));
    }
    None
}

fn output_package_usage(command: PackageCommand, out: &dyn PackageOutput) {
    out.error(&format!("Usage: {}", package_command_usage(command)));
}

fn dispatch_install(
    options: &PackageCommandOptions,
    handler: &dyn PackageHandler,
    out: &dyn PackageOutput,
) -> PackageOutcome {
    let source = options.source.as_deref().unwrap_or("");
    match handler.install(source, options.local) {
        Ok(()) => {
            out.success(&format!("Installed {source}"));
            PackageOutcome::success()
        }
        Err(msg) => {
            out.error(&format!("Error: {msg}"));
            PackageOutcome::failure(1)
        }
    }
}

fn dispatch_remove(
    options: &PackageCommandOptions,
    handler: &dyn PackageHandler,
    out: &dyn PackageOutput,
) -> PackageOutcome {
    let source = options.source.as_deref().unwrap_or("");
    match handler.remove(source, options.local) {
        Ok(true) => {
            out.success(&format!("Removed {source}"));
            PackageOutcome::success()
        }
        Ok(false) => {
            out.error(&format!("No matching package found for {source}"));
            PackageOutcome::failure(1)
        }
        Err(msg) => {
            out.error(&format!("Error: {msg}"));
            PackageOutcome::failure(1)
        }
    }
}

fn dispatch_list(handler: &dyn PackageHandler, out: &dyn PackageOutput) -> PackageOutcome {
    match handler.list() {
        Ok(packages) => {
            output_package_list(&packages, out);
            PackageOutcome::success()
        }
        Err(msg) => {
            out.error(&format!("Error: {msg}"));
            PackageOutcome::failure(1)
        }
    }
}

fn output_package_list(packages: &[ListedPackage], out: &dyn PackageOutput) {
    if packages.is_empty() {
        out.status_dim("No packages installed.");
        return;
    }
    let user: Vec<&ListedPackage> = packages
        .iter()
        .filter(|package| package.scope == ListedScope::User)
        .collect();
    let project: Vec<&ListedPackage> = packages
        .iter()
        .filter(|package| package.scope == ListedScope::Project)
        .collect();
    output_package_scope("User packages:", &user, out);
    if !project.is_empty() {
        if !user.is_empty() {
            out.status("");
        }
        output_package_scope("Project packages:", &project, out);
    }
}

fn output_package_scope(heading: &str, packages: &[&ListedPackage], out: &dyn PackageOutput) {
    if packages.is_empty() {
        return;
    }
    out.status(heading);
    for package in packages {
        out.status(&format!("  {}", package.display));
        if let Some(path) = &package.installed_path {
            out.status_dim(&format!("    {path}"));
        }
    }
}

fn dispatch_update(
    options: &PackageCommandOptions,
    handler: &dyn PackageHandler,
    out: &dyn PackageOutput,
    platform: DispatchPlatform,
) -> PackageOutcome {
    let target = options.update_target.clone().unwrap_or(UpdateTarget::Self_);
    if options.show_extensions_skipped_note.should_print() {
        out.status_dim(&format!(
            "Extensions are skipped. Run {APP_NAME} update --extensions to update extensions."
        ));
    }
    if matches!(target, UpdateTarget::All | UpdateTarget::Extensions { .. })
        && let Err(outcome) = dispatch_extensions_update(&target, handler, out)
    {
        return outcome;
    }
    if matches!(target, UpdateTarget::All | UpdateTarget::Self_) {
        match handler.update_self(options.force) {
            Ok(true) if matches!(platform, DispatchPlatform::Windows) => {
                return PackageOutcome {
                    exit_code: 0,
                    drain_quirk: true,
                };
            }
            Ok(false | true) => {}
            Err(msg) => {
                out.error(&format!("Error: {msg}"));
                return PackageOutcome::failure(1);
            }
        }
    }
    PackageOutcome::success()
}

fn dispatch_extensions_update(
    target: &UpdateTarget,
    handler: &dyn PackageHandler,
    out: &dyn PackageOutput,
) -> Result<(), PackageOutcome> {
    let source = match target {
        UpdateTarget::Extensions { source } => source.as_deref(),
        _ => None,
    };
    match handler.update_extensions(source) {
        Ok(()) => {
            if let Some(source) = source {
                out.success(&format!("Updated {source}"));
            } else {
                out.success("Updated packages");
            }
            Ok(())
        }
        Err(msg) => {
            out.error(&format!("Error: {msg}"));
            Err(PackageOutcome::failure(1))
        }
    }
}

/// Platform-specific dispatch behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchPlatform {
    /// Linux/macOS.
    Unix,
    /// Windows: successful `pi update` returns without forcing process exit.
    Windows,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn args(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|s| (*s).to_owned()).collect()
    }

    /// In-memory output sink capturing lines by kind.
    #[derive(Default)]
    struct CapturedOutput {
        status: Vec<String>,
        status_dim: Vec<String>,
        success: Vec<String>,
        error: Vec<String>,
    }

    impl PackageOutput for Rc<RefCell<CapturedOutput>> {
        fn status(&self, line: &str) {
            self.borrow_mut().status.push(line.to_owned());
        }
        fn status_dim(&self, line: &str) {
            self.borrow_mut().status_dim.push(line.to_owned());
        }
        fn success(&self, line: &str) {
            self.borrow_mut().success.push(line.to_owned());
        }
        fn error(&self, line: &str) {
            self.borrow_mut().error.push(line.to_owned());
        }
    }

    /// Handler that records calls and returns configured results.
    struct FakeHandler {
        install_results: Vec<Result<(), String>>,
        remove_results: Vec<Result<bool, String>>,
        list_result: Result<Vec<ListedPackage>, String>,
        refresh_result: Result<(), String>,
        update_ext_result: Result<(), String>,
        update_self_result: Result<bool, String>,
        trusted: bool,
        calls: Vec<String>,
    }

    impl Default for FakeHandler {
        fn default() -> Self {
            Self {
                install_results: Vec::new(),
                remove_results: Vec::new(),
                list_result: Ok(Vec::new()),
                refresh_result: Ok(()),
                update_ext_result: Ok(()),
                update_self_result: Ok(false),
                trusted: false,
                calls: Vec::new(),
            }
        }
    }

    impl PackageHandler for Rc<RefCell<FakeHandler>> {
        fn install(&self, source: &str, local: bool) -> Result<(), String> {
            self.borrow_mut()
                .calls
                .push(format!("install:{source}:{local}"));
            (self.borrow_mut().install_results.remove(0)).clone()
        }
        fn remove(&self, source: &str, local: bool) -> Result<bool, String> {
            self.borrow_mut()
                .calls
                .push(format!("remove:{source}:{local}"));
            (self.borrow_mut().remove_results.remove(0)).clone()
        }
        fn list(&self) -> Result<Vec<ListedPackage>, String> {
            self.borrow().list_result.clone()
        }
        fn is_project_trusted(&self) -> bool {
            self.borrow().trusted
        }
        fn refresh_models(&self) -> Result<(), String> {
            self.borrow_mut().calls.push("refresh".to_owned());
            self.borrow().refresh_result.clone()
        }
        fn update_extensions(&self, source: Option<&str>) -> Result<(), String> {
            self.borrow_mut()
                .calls
                .push(format!("update_ext:{}", source.unwrap_or("-")));
            self.borrow().update_ext_result.clone()
        }
        fn update_self(&self, force: bool) -> Result<bool, String> {
            self.borrow_mut().calls.push(format!("update_self:{force}"));
            self.borrow().update_self_result.clone()
        }
    }

    #[test]
    fn parser_recognizes_subcommands() {
        assert_eq!(
            parse_package_command(&args(&["install", "npm:x"])).map(|o| o.command),
            Some(PackageCommand::Install)
        );
        assert_eq!(
            parse_package_command(&args(&["uninstall", "npm:x"])).map(|o| o.command),
            Some(PackageCommand::Remove)
        );
        assert_eq!(
            parse_package_command(&args(&["list"])).map(|o| o.command),
            Some(PackageCommand::List)
        );
        assert!(parse_package_command(&args(&["--help"])).is_none());
        assert!(parse_package_command(&args(&["run"])).is_none());
    }

    #[test]
    fn parser_install_with_local_and_trust_flags() -> Result<(), String> {
        let opts = parse_package_command(&args(&["install", "npm:foo", "-l", "-a"]))
            .ok_or_else(|| "expected install command to parse".to_owned())?;
        assert_eq!(opts.command, PackageCommand::Install);
        assert_eq!(opts.source.as_deref(), Some("npm:foo"));
        assert!(opts.local);
        assert_eq!(opts.project_trust_override, Some(true));
        assert!(!opts.help);
        assert!(opts.invalid_option.is_none());
        Ok(())
    }

    #[test]
    fn parser_rejects_local_for_list() -> Result<(), String> {
        let opts = parse_package_command(&args(&["list", "-l"]))
            .ok_or_else(|| "expected list command to parse".to_owned())?;
        assert_eq!(opts.invalid_option.as_deref(), Some("-l"));
        Ok(())
    }

    #[test]
    fn parser_update_self_default() -> Result<(), String> {
        let opts = parse_package_command(&args(&["update"]))
            .ok_or_else(|| "expected update command to parse".to_owned())?;
        assert_eq!(opts.command, PackageCommand::Update);
        assert_eq!(opts.update_target, Some(UpdateTarget::Self_));
        assert!(opts.show_extensions_skipped_note.should_print());
        Ok(())
    }

    #[test]
    fn parser_update_all_flag() -> Result<(), String> {
        let opts = parse_package_command(&args(&["update", "--all"]))
            .ok_or_else(|| "expected update --all command to parse".to_owned())?;
        assert_eq!(opts.update_target, Some(UpdateTarget::All));
        assert!(!opts.show_extensions_skipped_note.should_print());
        Ok(())
    }

    #[test]
    fn parser_update_models_isolated() -> Result<(), String> {
        let opts = parse_package_command(&args(&["update", "--models"]))
            .ok_or_else(|| "expected update --models command to parse".to_owned())?;
        assert_eq!(opts.update_target, Some(UpdateTarget::Models));
        Ok(())
    }

    #[test]
    fn parser_update_models_conflicts_with_self() -> Result<(), String> {
        let opts = parse_package_command(&args(&["update", "--models", "--self"]))
            .ok_or_else(|| "expected conflicting update command to parse".to_owned())?;
        assert!(opts.conflicting_options.is_some());
        Ok(())
    }

    #[test]
    fn parser_update_extension_source() -> Result<(), String> {
        let opts = parse_package_command(&args(&["update", "--extension", "npm:bar"]))
            .ok_or_else(|| "expected extension update command to parse".to_owned())?;
        assert_eq!(
            opts.update_target,
            Some(UpdateTarget::Extensions {
                source: Some("npm:bar".to_owned())
            })
        );
        Ok(())
    }

    #[test]
    fn parser_update_extension_missing_value() -> Result<(), String> {
        let opts = parse_package_command(&args(&["update", "--extension"]))
            .ok_or_else(|| "expected incomplete extension update command to parse".to_owned())?;
        assert_eq!(opts.missing_option_value.as_deref(), Some("--extension"));
        Ok(())
    }

    #[test]
    fn parser_update_pi_alias() -> Result<(), String> {
        let opts = parse_package_command(&args(&["update", "pi"]))
            .ok_or_else(|| "expected update pi alias to parse".to_owned())?;
        assert_eq!(opts.update_target, Some(UpdateTarget::Self_));
        let opts = parse_package_command(&args(&["update", "pi", "--extensions"]))
            .ok_or_else(|| "expected update pi --extensions alias to parse".to_owned())?;
        assert_eq!(opts.update_target, Some(UpdateTarget::All));
        Ok(())
    }

    #[test]
    fn parser_update_positional_source() -> Result<(), String> {
        let opts = parse_package_command(&args(&["update", "npm:foo"]))
            .ok_or_else(|| "expected positional extension update to parse".to_owned())?;
        assert_eq!(
            opts.update_target,
            Some(UpdateTarget::Extensions {
                source: Some("npm:foo".to_owned())
            })
        );
        Ok(())
    }

    #[test]
    fn parser_help_flag() -> Result<(), String> {
        let opts = parse_package_command(&args(&["install", "--help"]))
            .ok_or_else(|| "expected install help command to parse".to_owned())?;
        assert!(opts.help);
        Ok(())
    }

    #[test]
    fn parser_invalid_argument_after_source() -> Result<(), String> {
        let opts = parse_package_command(&args(&["install", "a", "b"]))
            .ok_or_else(|| "expected invalid install command to parse".to_owned())?;
        assert_eq!(opts.invalid_argument.as_deref(), Some("b"));
        Ok(())
    }

    #[test]
    fn help_block_install_contains_usage_and_examples() {
        let text = format_package_command_help(PackageCommand::Install);
        assert!(text.contains("Usage:"));
        assert!(text.contains(&format!("{APP_NAME} install <source>")));
        assert!(text.contains("npm:@foo/bar"));
    }

    #[test]
    fn help_block_update_contains_all_flags() {
        let text = format_package_command_help(PackageCommand::Update);
        assert!(text.contains("--self"));
        assert!(text.contains("--extensions"));
        assert!(text.contains("--models"));
        assert!(text.contains("--all"));
        assert!(text.contains("--force"));
    }

    #[test]
    fn config_help_mentions_tui_and_local() {
        let text = format_config_command_help();
        assert!(text.contains("Usage:"));
        assert!(text.contains("-l, --local"));
        assert!(text.contains("Tab"));
    }

    #[test]
    fn dispatch_help_short_circuits_with_usage() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler::default()));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_package_command(
            &args(&["install", "--help"]),
            &handler,
            &out,
            DispatchPlatform::Unix,
        )
        .ok_or_else(|| "expected install help dispatch outcome".to_owned())?;
        assert_eq!(outcome.exit_code, 0);
        assert!(!out.borrow().status.is_empty());
        assert!(handler.borrow().calls.is_empty());
        Ok(())
    }

    #[test]
    fn dispatch_install_success() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler {
            install_results: vec![Ok(())],
            trusted: true,
            ..FakeHandler::default()
        }));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_package_command(
            &args(&["install", "npm:foo"]),
            &handler,
            &out,
            DispatchPlatform::Unix,
        )
        .ok_or_else(|| "expected install dispatch outcome".to_owned())?;
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(out.borrow().success, vec!["Installed npm:foo"]);
        assert_eq!(handler.borrow().calls, vec!["install:npm:foo:false"]);
        Ok(())
    }

    #[test]
    fn dispatch_install_local_untrusted_exits_one() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler {
            trusted: false,
            ..FakeHandler::default()
        }));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_package_command(
            &args(&["install", "npm:foo", "-l"]),
            &handler,
            &out,
            DispatchPlatform::Unix,
        )
        .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 1);
        assert_eq!(
            out.borrow().error[0],
            "Project is not trusted. Use --approve to modify local package config."
        );
        assert!(handler.borrow().calls.is_empty());
        Ok(())
    }

    #[test]
    fn dispatch_remove_no_match_exits_one() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler {
            remove_results: vec![Ok(false)],
            trusted: true,
            ..FakeHandler::default()
        }));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_package_command(
            &args(&["remove", "npm:foo"]),
            &handler,
            &out,
            DispatchPlatform::Unix,
        )
        .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 1);
        assert_eq!(
            out.borrow().error[0],
            "No matching package found for npm:foo"
        );
        Ok(())
    }

    #[test]
    fn dispatch_remove_success() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler {
            remove_results: vec![Ok(true)],
            trusted: true,
            ..FakeHandler::default()
        }));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_package_command(
            &args(&["uninstall", "npm:foo"]),
            &handler,
            &out,
            DispatchPlatform::Unix,
        )
        .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(out.borrow().success, vec!["Removed npm:foo"]);
        Ok(())
    }

    #[test]
    fn dispatch_missing_source_exits_one() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler::default()));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome =
            handle_package_command(&args(&["install"]), &handler, &out, DispatchPlatform::Unix)
                .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 1);
        assert!(out.borrow().error[0].contains("Missing install source"));
        Ok(())
    }

    #[test]
    fn dispatch_list_empty_prints_dim_notice() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler {
            list_result: Ok(Vec::new()),
            ..FakeHandler::default()
        }));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome =
            handle_package_command(&args(&["list"]), &handler, &out, DispatchPlatform::Unix)
                .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(out.borrow().status_dim, vec!["No packages installed."]);
        Ok(())
    }

    #[test]
    fn dispatch_list_with_packages() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler {
            list_result: Ok(vec![
                ListedPackage {
                    display: "npm:a".to_owned(),
                    installed_path: Some("/path/a".to_owned()),
                    scope: ListedScope::User,
                },
                ListedPackage {
                    display: "git:b".to_owned(),
                    installed_path: None,
                    scope: ListedScope::Project,
                },
            ]),
            ..FakeHandler::default()
        }));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome =
            handle_package_command(&args(&["list"]), &handler, &out, DispatchPlatform::Unix)
                .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 0);
        let captured = out.borrow();
        assert!(captured.status.iter().any(|s| s == "User packages:"));
        assert!(captured.status.iter().any(|s| s == "Project packages:"));
        assert!(captured.status.iter().any(|s| s == "  npm:a"));
        assert!(captured.status_dim.iter().any(|s| s == "    /path/a"));
        Ok(())
    }

    #[test]
    fn dispatch_update_models_routes_to_refresh() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler {
            refresh_result: Ok(()),
            ..FakeHandler::default()
        }));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_package_command(
            &args(&["update", "--models"]),
            &handler,
            &out,
            DispatchPlatform::Unix,
        )
        .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(handler.borrow().calls, vec!["refresh"]);
        Ok(())
    }

    #[test]
    fn dispatch_update_models_error_propagates() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler {
            refresh_result: Err("boom".to_owned()),
            ..FakeHandler::default()
        }));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_package_command(
            &args(&["update", "--models"]),
            &handler,
            &out,
            DispatchPlatform::Unix,
        )
        .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 1);
        assert_eq!(out.borrow().error[0], "Error: boom");
        Ok(())
    }

    #[test]
    fn dispatch_update_self_already_latest_is_success() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler {
            update_self_result: Ok(false),
            ..FakeHandler::default()
        }));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome =
            handle_package_command(&args(&["update"]), &handler, &out, DispatchPlatform::Unix)
                .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 0);
        // Bare `update` prints the extensions-skipped dim note before self-update
        // (package-manager-cli.ts:820-823); already-latest still exits 0.
        assert!(
            out.borrow()
                .status_dim
                .iter()
                .any(|s| s.contains("Extensions are skipped"))
        );
        assert!(out.borrow().error.is_empty());
        Ok(())
    }

    #[test]
    fn dispatch_update_self_success_windows_sets_drain_quirk() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler {
            update_self_result: Ok(true),
            ..FakeHandler::default()
        }));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_package_command(
            &args(&["update"]),
            &handler,
            &out,
            DispatchPlatform::Windows,
        )
        .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.drain_quirk);
        Ok(())
    }

    #[test]
    fn dispatch_update_self_success_unix_no_quirk() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler {
            update_self_result: Ok(true),
            ..FakeHandler::default()
        }));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome =
            handle_package_command(&args(&["update"]), &handler, &out, DispatchPlatform::Unix)
                .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 0);
        assert!(!outcome.drain_quirk);
        Ok(())
    }

    #[test]
    fn dispatch_update_all_runs_extensions_then_self() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler {
            update_ext_result: Ok(()),
            update_self_result: Ok(true),
            ..FakeHandler::default()
        }));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_package_command(
            &args(&["update", "--all"]),
            &handler,
            &out,
            DispatchPlatform::Unix,
        )
        .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 0);
        let calls = &handler.borrow().calls;
        assert!(calls.iter().any(|c| c.starts_with("update_ext:")));
        assert!(calls.iter().any(|c| c.starts_with("update_self:")));
        assert_eq!(out.borrow().success, vec!["Updated packages"]);
        Ok(())
    }

    #[test]
    fn dispatch_update_extensions_filtered_source() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler {
            update_ext_result: Ok(()),
            ..FakeHandler::default()
        }));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_package_command(
            &args(&["update", "--extension", "npm:x"]),
            &handler,
            &out,
            DispatchPlatform::Unix,
        )
        .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(handler.borrow().calls, vec!["update_ext:npm:x"]);
        assert_eq!(out.borrow().success, vec!["Updated npm:x"]);
        Ok(())
    }

    #[test]
    fn dispatch_invalid_option_exits_one() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler::default()));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_package_command(
            &args(&["list", "--bogus"]),
            &handler,
            &out,
            DispatchPlatform::Unix,
        )
        .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 1);
        assert!(out.borrow().error[0].contains("Unknown option --bogus"));
        assert!(handler.borrow().calls.is_empty());
        Ok(())
    }

    #[test]
    fn dispatch_conflicting_options_exits_one() -> Result<(), String> {
        let handler = Rc::new(RefCell::new(FakeHandler::default()));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_package_command(
            &args(&["update", "--all", "--self"]),
            &handler,
            &out,
            DispatchPlatform::Unix,
        )
        .ok_or_else(|| "expected dispatch to succeed".to_owned())?;
        assert_eq!(outcome.exit_code, 1);
        assert!(out.borrow().error[0].contains("--all cannot be combined"));
        Ok(())
    }

    #[test]
    fn dispatch_non_package_command_returns_none() {
        let handler = Rc::new(RefCell::new(FakeHandler::default()));
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        assert!(
            handle_package_command(&args(&["--help"]), &handler, &out, DispatchPlatform::Unix,)
                .is_none()
        );
    }

    #[test]
    fn config_command_dispatch_help() -> Result<(), String> {
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_config_command(&args(&["config", "--help"]), &out, true, true)
            .ok_or_else(|| "expected config help dispatch outcome".to_owned())?;
        assert_eq!(outcome.exit_code, 0);
        assert!(out.borrow().status[0].contains("Usage:"));
        Ok(())
    }

    #[test]
    fn config_command_dispatch_non_tty_reports_error() -> Result<(), String> {
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        let outcome = handle_config_command(&args(&["config"]), &out, false, true)
            .ok_or_else(|| "expected non-TTY config dispatch outcome".to_owned())?;
        assert_eq!(outcome.exit_code, 1);
        assert!(out.borrow().error[0].contains("interactive terminal"));
        Ok(())
    }

    #[test]
    fn config_command_returns_none_for_other_subcommands() {
        let out = Rc::new(RefCell::new(CapturedOutput::default()));
        assert!(handle_config_command(&args(&["install"]), &out, true, true).is_none());
    }
}
