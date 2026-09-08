//! Experimental `server` command parsing matching coding-agent
//! `cli/experimental/commands/server.ts`. Runtime dispatch stays with the future
//! experimental entrypoint.

use crate::remote::schemas::ServerId;

use super::command_options::{
    AUTH_TOKEN_FILE_OPTION, AUTH_TOKEN_OPTION, AuthInput, OptionSpec, ParsedValue, parse_auth,
    parse_options, parse_text, unsupported_options,
};

/// Parsed `experimental server` invocation (source `ServerCommand`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerCommand {
    /// Authentication material from `--auth-token`/`--auth-token-file`.
    pub auth: Option<AuthInput>,
    /// `--provider` value.
    pub provider: Option<String>,
    /// `--model` value.
    pub model: Option<String>,
    /// Repeated `-e` plugin package paths in order; empty when none were given.
    pub plugin_packages: Vec<String>,
    /// `--server-id` value.
    pub server_id: Option<ServerId>,
    /// `--session-dir` value.
    pub session_dir: Option<String>,
}

const SERVER_ID: &str = "--server-id";
const SESSION_DIR: &str = "--session-dir";
const PROVIDER: &str = "--provider";
const MODEL: &str = "--model";
const PLUGIN_PACKAGE: &str = "-e";

const SERVER_OPTIONS: [OptionSpec; 7] = [
    OptionSpec::value(SERVER_ID, parse_server_id_value),
    OptionSpec::value(SESSION_DIR, parse_text),
    OptionSpec::value(PROVIDER, parse_text),
    OptionSpec::value(MODEL, parse_text),
    OptionSpec::value(PLUGIN_PACKAGE, parse_text).repeatable(),
    AUTH_TOKEN_OPTION,
    AUTH_TOKEN_FILE_OPTION,
];

fn parse_server_id_value(value: &str) -> Result<ParsedValue, String> {
    ServerId::try_from(value)
        .map(ParsedValue::ServerId)
        .map_err(|_| format!("Invalid --server-id \"{value}\"; expected a lowercase UUIDv4"))
}

/// Parses the arguments following the `server` command name
/// (source `serverCommand` builder).
pub fn parse_server_command(args: &[String]) -> Result<ServerCommand, Vec<String>> {
    let input = parse_options(args, &SERVER_OPTIONS);
    let mut errors = input.errors().to_vec();
    let (auth, auth_errors) = parse_auth(&input);
    errors.extend(auth_errors);
    let server_id = input.first_server_id(SERVER_ID);
    let session_dir = input.first_text(SESSION_DIR).map(str::to_owned);
    let provider = input.first_text(PROVIDER).map(str::to_owned);
    let model = input.first_text(MODEL).map(str::to_owned);
    let plugin_packages = input.all_text(PLUGIN_PACKAGE);
    if provider.is_some() && model.is_none() {
        errors.push("--provider requires --model".to_owned());
    }
    errors.extend(unsupported_options("server", &input));
    if errors.is_empty() {
        Ok(ServerCommand {
            auth,
            provider,
            model,
            plugin_packages,
            server_id,
            session_dir,
        })
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().copied().map(str::to_owned).collect()
    }

    #[test]
    fn accepts_full_source_flag_set() {
        let command = match parse_server_command(&strings(&[
            "--server-id",
            "00000000-0000-4000-8000-000000000001",
            "--session-dir",
            "/tmp/sessions",
            "--provider",
            "anthropic",
            "--model",
            "claude-x",
            "-e",
            "/pkg/a",
            "-e=/pkg/b",
            "--auth-token-file",
            "/tmp/token",
        ])) {
            Ok(command) => command,
            Err(errors) => panic!("expected ok, got {errors:?}"),
        };
        assert_eq!(
            command.server_id.as_ref().map(ServerId::as_str),
            Some("00000000-0000-4000-8000-000000000001")
        );
        assert_eq!(command.session_dir.as_deref(), Some("/tmp/sessions"));
        assert_eq!(command.provider.as_deref(), Some("anthropic"));
        assert_eq!(command.model.as_deref(), Some("claude-x"));
        assert_eq!(command.plugin_packages, strings(&["/pkg/a", "/pkg/b"]));
        assert_eq!(
            command.auth,
            Some(AuthInput::File {
                path: "/tmp/token".to_owned()
            })
        );
    }

    #[test]
    fn empty_argv_parses_defaults() {
        assert_eq!(
            parse_server_command(&[]),
            Ok(ServerCommand {
                auth: None,
                provider: None,
                model: None,
                plugin_packages: Vec::new(),
                server_id: None,
                session_dir: None,
            })
        );
    }

    #[test]
    fn orders_scanner_errors_before_builder_errors() {
        assert_eq!(
            parse_server_command(&strings(&[
                "--session-dir",
                "/a",
                "--session-dir",
                "/b",
                "--provider",
                "p"
            ])),
            Err(vec![
                "--session-dir may only be specified once".to_owned(),
                "--provider requires --model".to_owned(),
            ])
        );
    }

    #[test]
    fn rejects_unsupported_positionals_and_unknown_options() {
        assert_eq!(
            parse_server_command(&strings(&["extra"])),
            Err(vec![
                "The experimental server command does not support existing CLI options yet".to_owned()
            ])
        );
        assert_eq!(
            parse_server_command(&strings(&["--connect", "unix:///tmp/pi.sock"])),
            Err(vec![
                "The experimental server command does not support existing CLI options yet".to_owned()
            ])
        );
    }

    #[test]
    fn rejects_mutually_exclusive_auth_and_invalid_server_id() {
        assert_eq!(
            parse_server_command(&strings(&["--auth-token", "t", "--auth-token-file", "f"])),
            Err(vec!["--auth-token and --auth-token-file are mutually exclusive".to_owned()])
        );
        assert_eq!(
            parse_server_command(&strings(&["--server-id", "00000000-0000-4000-8000-0000000000010"])),
            Err(vec![
                "Invalid --server-id \"00000000-0000-4000-8000-0000000000010\"; expected a lowercase UUIDv4"
                    .to_owned()
            ])
        );
    }
}
