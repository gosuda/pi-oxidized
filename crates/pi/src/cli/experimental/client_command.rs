//! Experimental `client` command parsing matching coding-agent
//! `cli/experimental/commands/client.ts`. Runtime dispatch stays with the future
//! experimental entrypoint.

use super::command_options::{
    AUTH_TOKEN_FILE_OPTION, AUTH_TOKEN_OPTION, CONNECT, CONNECT_OPTION, AuthInput, OptionSpec,
    TransportAddress, parse_auth, parse_options, parse_text, unsupported_options,
};

/// Parsed `experimental client` invocation (source `ClientCommand`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientCommand {
    /// Authentication material from `--auth-token`/`--auth-token-file`.
    pub auth: Option<AuthInput>,
    /// `--connect` transport address.
    pub connect: Option<TransportAddress>,
    /// `--session-id` value.
    pub session_id: Option<String>,
    /// `--continue` or `-c`.
    pub r#continue: bool,
    /// `--resume` or `-r`.
    pub resume: bool,
    /// `--provider` value.
    pub provider: Option<String>,
    /// `--model` value.
    pub model: Option<String>,
    /// Repeated `-e` plugin package paths in order; empty when none were given.
    pub plugin_packages: Vec<String>,
    /// Single positional prompt word; `-- <prompt>` also accepts a leading dash.
    pub prompt: Option<String>,
}

const SESSION_ID: &str = "--session-id";
const CONTINUE: &str = "--continue";
const CONTINUE_SHORT: &str = "-c";
const RESUME: &str = "--resume";
const RESUME_SHORT: &str = "-r";
const PROVIDER: &str = "--provider";
const MODEL: &str = "--model";
const PLUGIN_PACKAGE: &str = "-e";

const CLIENT_OPTIONS: [OptionSpec; 11] = [
    CONNECT_OPTION,
    OptionSpec::value(SESSION_ID, parse_text),
    OptionSpec::flag(CONTINUE),
    OptionSpec::flag(CONTINUE_SHORT),
    OptionSpec::flag(RESUME),
    OptionSpec::flag(RESUME_SHORT),
    OptionSpec::value(PROVIDER, parse_text),
    OptionSpec::value(MODEL, parse_text),
    OptionSpec::value(PLUGIN_PACKAGE, parse_text).repeatable(),
    AUTH_TOKEN_OPTION,
    AUTH_TOKEN_FILE_OPTION,
];

/// Parses the arguments following the `client` command name
/// (source `clientCommand` builder).
pub fn parse_client_command(args: &[String]) -> Result<ClientCommand, Vec<String>> {
    let input = parse_options(args, &CLIENT_OPTIONS);
    let mut errors = input.errors().to_vec();
    let (auth, auth_errors) = parse_auth(&input);
    errors.extend(auth_errors);
    let connect = input.first_transport(CONNECT);
    let session_id = input.first_text(SESSION_ID).map(str::to_owned);
    let should_continue = input.first_flag(CONTINUE) || input.first_flag(CONTINUE_SHORT);
    let should_resume = input.first_flag(RESUME) || input.first_flag(RESUME_SHORT);
    let provider = input.first_text(PROVIDER).map(str::to_owned);
    let model = input.first_text(MODEL).map(str::to_owned);
    let plugin_packages = input.all_text(PLUGIN_PACKAGE);
    let remaining_args = input.remaining_args();
    let prompt_args: &[String] = if remaining_args.first().map(String::as_str) == Some("--") {
        &remaining_args[1..]
    } else {
        remaining_args
    };
    let prompt = if prompt_args.len() == 1
        && (remaining_args.first().map(String::as_str) == Some("--")
            || !prompt_args[0].starts_with('-'))
        && !prompt_args[0].is_empty()
    {
        Some(prompt_args[0].clone())
    } else {
        None
    };
    if provider.is_some() && model.is_none() {
        errors.push("--provider requires --model".to_owned());
    }
    if [session_id.is_some(), should_continue, should_resume]
        .into_iter()
        .filter(|selected| *selected)
        .count()
        > 1
    {
        errors.push("--session-id, --continue, and --resume are mutually exclusive".to_owned());
    }
    if !(remaining_args.is_empty() || prompt.is_some()) {
        errors.extend(unsupported_options("client", &input));
    }
    if errors.is_empty() {
        Ok(ClientCommand {
            auth,
            connect,
            session_id,
            r#continue: should_continue,
            resume: should_resume,
            provider,
            model,
            plugin_packages,
            prompt,
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
    fn accepts_explicit_session_with_double_dash_prompt() {
        let command = match parse_client_command(&strings(&[
            "--connect",
            "unix:///tmp/pi.sock",
            "--session-id",
            "session-1",
            "-e",
            "/pkg/a",
            "--provider",
            "p",
            "--model",
            "m",
            "--",
            "Summarize now",
        ])) {
            Ok(command) => command,
            Err(errors) => panic!("expected ok, got {errors:?}"),
        };
        assert_eq!(
            command.connect,
            Some(TransportAddress::Unix {
                path: "/tmp/pi.sock".to_owned()
            })
        );
        assert_eq!(command.session_id.as_deref(), Some("session-1"));
        assert!(!command.r#continue);
        assert!(!command.resume);
        assert_eq!(command.plugin_packages, strings(&["/pkg/a"]));
        assert_eq!(command.prompt.as_deref(), Some("Summarize now"));
    }

    #[test]
    fn accepts_short_resume_and_bare_prompt() {
        assert_eq!(
            parse_client_command(&strings(&["-r", "hello"])),
            Ok(ClientCommand {
                auth: None,
                connect: None,
                session_id: None,
                r#continue: false,
                resume: true,
                provider: None,
                model: None,
                plugin_packages: Vec::new(),
                prompt: Some("hello".to_owned()),
            })
        );
    }

    #[test]
    fn double_dash_prompt_keeps_leading_dash_but_bare_dash_prefix_is_unsupported() {
        let dashed = match parse_client_command(&strings(&["--", "-weird"])) {
            Ok(command) => command,
            Err(errors) => panic!("expected ok, got {errors:?}"),
        };
        assert_eq!(dashed.prompt.as_deref(), Some("-weird"));
        assert_eq!(
            parse_client_command(&strings(&["-weird"])),
            Err(vec![
                "The experimental client command does not support existing CLI options yet".to_owned()
            ])
        );
    }

    #[test]
    fn lone_double_dash_yields_no_prompt() {
        let command = match parse_client_command(&strings(&["--"])) {
            Ok(command) => command,
            Err(errors) => panic!("expected ok, got {errors:?}"),
        };
        assert_eq!(command.prompt, None);
    }

    #[test]
    fn rejects_conflicting_session_selection() {
        assert_eq!(
            parse_client_command(&strings(&["--session-id", "s", "-c"])),
            Err(vec![
                "--session-id, --continue, and --resume are mutually exclusive".to_owned()
            ])
        );
    }

    #[test]
    fn rejects_multi_word_prompts_and_flag_values() {
        assert_eq!(
            parse_client_command(&strings(&["hello", "world"])),
            Err(vec![
                "The experimental client command does not support existing CLI options yet".to_owned()
            ])
        );
        assert_eq!(
            parse_client_command(&strings(&["--continue=1"])),
            Err(vec!["--continue does not take a value".to_owned()])
        );
        assert_eq!(
            parse_client_command(&strings(&["--provider"])),
            Err(vec!["--provider requires a value".to_owned()])
        );
    }

    #[test]
    fn rejects_radius_connect_with_bad_server_id() {
        assert_eq!(
            parse_client_command(&strings(&["--connect", "radius://not-a-uuid"])),
            Err(vec![
                "Radius transport address requires a lowercase UUIDv4 server ID".to_owned()
            ])
        );
    }
}
