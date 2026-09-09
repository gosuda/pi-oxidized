//! Shared option plumbing for the experimental `server` and `client` commands.
//!
//! Ports coding-agent `cli/experimental/command-options.ts` (auth inputs, transport
//! addresses, shared option specs) together with the option scanner from
//! `cli/experimental/command.ts`. Parsers accept the argv that follows the command
//! name, exactly like the source `Command` subcommand dispatch; development-only
//! gating stays a dispatcher responsibility.

use crate::remote::schemas::ServerId;
use url::Url;

/// Authentication material for `server`/`client` invocations (source `AuthInput`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthInput {
    /// `--auth-token <token>`.
    Token {
        /// Literal bearer token.
        token: String,
    },
    /// `--auth-token-file <path>`.
    File {
        /// Path of the file holding the token.
        path: String,
    },
}

/// Where an experimental client connects (source `TransportAddress`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportAddress {
    /// `--connect unix:///<absolute-path>`.
    Unix {
        /// Decoded absolute socket path.
        path: String,
    },
    /// `--connect radius://<server-id>`.
    Radius {
        /// Canonical lowercase `UUIDv4` server identity.
        server_id: ServerId,
    },
}

/// A scanned option value, typed by the option that produced it.
pub(crate) enum ParsedValue {
    /// Plain string option value.
    Text(String),
    /// Boolean flag presence.
    Flag,
    /// Parsed `--connect` transport address.
    Transport(TransportAddress),
    /// Parsed `--server-id` identity.
    ServerId(ServerId),
}

/// One registered command option (source `CommandOption`).
pub(crate) struct OptionSpec {
    /// Option spelling including leading dashes.
    pub(crate) name: &'static str,
    flag: bool,
    repeatable: bool,
    parse: fn(&str) -> Result<ParsedValue, String>,
}

impl OptionSpec {
    /// Declares a value-taking option.
    pub(crate) const fn value(
        name: &'static str,
        parse: fn(&str) -> Result<ParsedValue, String>,
    ) -> Self {
        Self {
            name,
            flag: false,
            repeatable: false,
            parse,
        }
    }

    /// Declares a boolean flag that rejects `=` values.
    pub(crate) const fn flag(name: &'static str) -> Self {
        Self {
            name,
            flag: true,
            repeatable: false,
            parse: parse_flag_value,
        }
    }

    /// Allows the option to be given repeatedly.
    pub(crate) const fn repeatable(mut self) -> Self {
        self.repeatable = true;
        self
    }
}

/// Option scan outcome handed to a command builder (source `ParsedCommandInput`).
pub(crate) struct ParsedCommandInput {
    values: Vec<(&'static str, ParsedValue)>,
    remaining_args: Vec<String>,
    errors: Vec<String>,
}

impl ParsedCommandInput {
    /// First recorded value for `name` (source `input.value`).
    #[must_use]
    pub(crate) fn first(&self, name: &str) -> Option<&ParsedValue> {
        self.values
            .iter()
            .find(|(option, _)| *option == name)
            .map(|(_, value)| value)
    }

    /// First recorded string value.
    #[must_use]
    pub(crate) fn first_text(&self, name: &str) -> Option<&str> {
        match self.first(name) {
            Some(ParsedValue::Text(text)) => Some(text),
            _ => None,
        }
    }

    /// All recorded string values in order (source `input.values`).
    #[must_use]
    pub(crate) fn all_text(&self, name: &str) -> Vec<String> {
        self.values
            .iter()
            .filter(|(option, _)| *option == name)
            .filter_map(|(_, value)| match value {
                ParsedValue::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Whether the flag `name` was given.
    #[must_use]
    pub(crate) fn first_flag(&self, name: &str) -> bool {
        matches!(self.first(name), Some(ParsedValue::Flag))
    }

    /// First recorded transport address.
    #[must_use]
    pub(crate) fn first_transport(&self, name: &str) -> Option<TransportAddress> {
        match self.first(name) {
            Some(ParsedValue::Transport(address)) => Some(address.clone()),
            _ => None,
        }
    }

    /// First recorded server identity.
    #[must_use]
    pub(crate) fn first_server_id(&self, name: &str) -> Option<ServerId> {
        match self.first(name) {
            Some(ParsedValue::ServerId(server_id)) => Some(server_id.clone()),
            _ => None,
        }
    }

    /// Positional arguments left after the scanner stopped (source `remainingArgs`).
    #[must_use]
    pub(crate) fn remaining_args(&self) -> &[String] {
        &self.remaining_args
    }

    /// Scan errors in argument order.
    #[must_use]
    pub(crate) fn errors(&self) -> &[String] {
        &self.errors
    }
}

/// Scans `args` against the registered `options` (source `Command.parseOptions`).
///
/// The first unregistered argument (or `--`) moves the rest into
/// [`ParsedCommandInput::remaining_args`]; scan errors accumulate in argument order
/// without stopping the scan.
pub(crate) fn parse_options(args: &[String], options: &[OptionSpec]) -> ParsedCommandInput {
    let mut input = ParsedCommandInput {
        values: Vec::new(),
        remaining_args: Vec::new(),
        errors: Vec::new(),
    };
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if argument == "--" {
            input.remaining_args.extend_from_slice(&args[index..]);
            break;
        }
        let equals = argument.find('=');
        let name = match equals {
            Some(position) => &argument[..position],
            None => argument.as_str(),
        };
        let Some(spec) = options.iter().find(|spec| spec.name == name) else {
            input.remaining_args.extend_from_slice(&args[index..]);
            break;
        };
        if spec.flag && equals.is_some() {
            input.errors.push(format!("{name} does not take a value"));
            index += 1;
            continue;
        }
        let value;
        if spec.flag {
            value = String::new();
        } else {
            let mut candidate = equals.map(|position| argument[position + 1..].to_owned());
            if candidate.is_none()
                && let Some(next) = args.get(index + 1)
                && !next.starts_with('-')
            {
                candidate = Some(next.clone());
                index += 1;
            }
            match candidate {
                Some(candidate) if !candidate.is_empty() => value = candidate,
                _ => {
                    input.errors.push(format!("{name} requires a value"));
                    index += 1;
                    continue;
                }
            }
        }
        let seen = input.values.iter().any(|(option, _)| *option == spec.name);
        if seen && !spec.repeatable {
            input
                .errors
                .push(format!("{name} may only be specified once"));
            index += 1;
            continue;
        }
        match (spec.parse)(&value) {
            Ok(parsed) => input.values.push((spec.name, parsed)),
            Err(error) => input.errors.push(error),
        }
        index += 1;
    }
    input
}

/// `--auth-token` option name.
pub(crate) const AUTH_TOKEN: &str = "--auth-token";
/// `--auth-token-file` option name.
pub(crate) const AUTH_TOKEN_FILE: &str = "--auth-token-file";
/// `--connect` option name.
pub(crate) const CONNECT: &str = "--connect";

/// Shared `--auth-token` option (source `authTokenOption`).
pub(crate) const AUTH_TOKEN_OPTION: OptionSpec = OptionSpec::value(AUTH_TOKEN, parse_text);
/// Shared `--auth-token-file` option (source `authTokenFileOption`).
pub(crate) const AUTH_TOKEN_FILE_OPTION: OptionSpec =
    OptionSpec::value(AUTH_TOKEN_FILE, parse_text);
/// Shared `--connect` option (source `connectOption`).
pub(crate) const CONNECT_OPTION: OptionSpec = OptionSpec::value(CONNECT, parse_connect_value);

/// Plain string option parser (source `stringOption`).
#[expect(
    clippy::unnecessary_wraps,
    reason = "OptionSpec stores a uniform fn(&str) -> Result parser so fallible parsers can report errors; this infallible parser matches that signature"
)]
pub(crate) fn parse_text(value: &str) -> Result<ParsedValue, String> {
    Ok(ParsedValue::Text(value.to_owned()))
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "OptionSpec stores a uniform fn(&str) -> Result parser so fallible parsers can report errors; this infallible parser matches that signature"
)]
fn parse_flag_value(_value: &str) -> Result<ParsedValue, String> {
    Ok(ParsedValue::Flag)
}

fn parse_connect_value(value: &str) -> Result<ParsedValue, String> {
    parse_transport_address(value).map(ParsedValue::Transport)
}

/// Resolves `--auth-token`/`--auth-token-file` (source `parseAuthInput`).
pub(crate) fn parse_auth(input: &ParsedCommandInput) -> (Option<AuthInput>, Vec<String>) {
    match (
        input.first_text(AUTH_TOKEN),
        input.first_text(AUTH_TOKEN_FILE),
    ) {
        (Some(_), Some(_)) => (
            None,
            vec!["--auth-token and --auth-token-file are mutually exclusive".to_owned()],
        ),
        (Some(token), None) => (
            Some(AuthInput::Token {
                token: token.to_owned(),
            }),
            Vec::new(),
        ),
        (None, Some(path)) => (
            Some(AuthInput::File {
                path: path.to_owned(),
            }),
            Vec::new(),
        ),
        (None, None) => (None, Vec::new()),
    }
}

/// Reports leftover arguments an experimental command cannot interpret
/// (source `unsupportedOptions`).
pub(crate) fn unsupported_options(command: &str, input: &ParsedCommandInput) -> Vec<String> {
    if input.remaining_args.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "The experimental {command} command does not support existing CLI options yet"
        )]
    }
}

/// Parses `--connect` values (source `parseTransportAddress`).
fn parse_transport_address(value: &str) -> Result<TransportAddress, String> {
    let invalid_address = || format!("Invalid --connect address \"{value}\"");
    let url = Url::parse(value).map_err(|_| invalid_address())?;
    if url.scheme() == "radius" {
        let hostname = url.host_str().unwrap_or_default();
        let pathname = url.path();
        if has_userinfo(&url)
            || url.port().is_some()
            || (!pathname.is_empty() && pathname != "/")
            || url.query().is_some()
            || url.fragment().is_some()
            || value != format!("radius://{hostname}{pathname}")
        {
            return Err(invalid_address());
        }
        let server_id = ServerId::try_from(hostname).map_err(|_| {
            "Radius transport address requires a lowercase UUIDv4 server ID".to_owned()
        })?;
        return Ok(TransportAddress::Radius { server_id });
    }
    if url.scheme() != "unix" {
        return Err(format!(
            "Unsupported --connect transport \"{}:\"",
            url.scheme()
        ));
    }
    if has_userinfo(&url)
        || url.port().is_some()
        || url.host_str().is_some_and(|host| !host.is_empty())
    {
        return Err("Unix transport address must not include an authority".to_owned());
    }
    if !value.starts_with("unix:///")
        || value.starts_with("unix:////")
        || value.contains('?')
        || value.contains('#')
        || url.as_str() != value
    {
        return Err(invalid_address());
    }
    let Some(path) = decode_uri_component(url.path()) else {
        return Err(invalid_address());
    };
    if path.contains('\0') {
        return Err(invalid_address());
    }
    if !path.starts_with('/') {
        return Err("Unix transport address requires an absolute path".to_owned());
    }
    Ok(TransportAddress::Unix { path })
}

fn has_userinfo(url: &Url) -> bool {
    !url.username().is_empty() || url.password().is_some_and(|password| !password.is_empty())
}

/// Strict `decodeURIComponent`: rejects truncated or non-hex escapes and invalid `UTF-8`.
fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let high = char::from(hex[0]).to_digit(16)?;
            let low = char::from(hex[1]).to_digit(16)?;
            decoded.push(u8::try_from(high * 16 + low).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().copied().map(str::to_owned).collect()
    }

    #[test]
    fn unix_connect_accepts_absolute_paths_and_decodes_escapes() {
        assert_eq!(
            parse_transport_address("unix:///run/pi/a%20b.sock"),
            Ok(TransportAddress::Unix {
                path: "/run/pi/a b.sock".to_owned()
            })
        );
        assert!(matches!(
            parse_transport_address("unix:///run/pi.sock"),
            Ok(TransportAddress::Unix { .. })
        ));
    }

    #[test]
    fn unix_connect_rejects_decorated_forms() {
        assert_eq!(
            parse_transport_address("unix://host/run/pi.sock"),
            Err("Unix transport address must not include an authority".to_owned())
        );
        assert_eq!(
            parse_transport_address("unix:///a b"),
            Err("Invalid --connect address \"unix:///a b\"".to_owned())
        );
        assert_eq!(
            parse_transport_address("unix:///ok/../traversal"),
            Err("Invalid --connect address \"unix:///ok/../traversal\"".to_owned())
        );
        assert_eq!(
            parse_transport_address("unix:///a%00b"),
            Err("Invalid --connect address \"unix:///a%00b\"".to_owned())
        );
        assert_eq!(
            parse_transport_address("unix:relative"),
            Err("Invalid --connect address \"unix:relative\"".to_owned())
        );
    }

    #[test]
    fn radius_connect_accepts_canonical_server_id() {
        let server_id = "00000000-0000-4000-8000-000000000001";
        assert!(matches!(
            parse_transport_address(&format!("radius://{server_id}")),
            Ok(TransportAddress::Radius { .. })
        ));
        assert!(matches!(
            parse_transport_address(&format!("radius://{server_id}/")),
            Ok(TransportAddress::Radius { .. })
        ));
    }

    #[test]
    fn radius_connect_rejects_decorated_or_foreign_addresses() {
        let server_id = "00000000-0000-4000-8000-000000000001";
        assert_eq!(
            parse_transport_address(&format!("radius://{server_id}:443")),
            Err(format!(
                "Invalid --connect address \"radius://{server_id}:443\""
            ))
        );
        assert_eq!(
            parse_transport_address(&format!("radius://{server_id}/extra")),
            Err(format!(
                "Invalid --connect address \"radius://{server_id}/extra\""
            ))
        );
        assert_eq!(
            parse_transport_address("radius://not-a-uuid"),
            Err("Radius transport address requires a lowercase UUIDv4 server ID".to_owned())
        );
        assert_eq!(
            parse_transport_address("http://example.com"),
            Err("Unsupported --connect transport \"http:\"".to_owned())
        );
    }

    #[test]
    fn scanner_reports_missing_duplicate_and_flag_value_errors() {
        let options = [
            CONNECT_OPTION,
            OptionSpec::value("--auth-token", parse_text),
        ];
        let input = parse_options(
            &strings(&["--connect", "--auth-token", "t", "--auth-token", "u"]),
            &options,
        );
        assert_eq!(
            input
                .errors()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "--connect requires a value",
                "--auth-token may only be specified once"
            ]
        );
        assert_eq!(input.first_text("--auth-token"), Some("t"));
        assert!(input.remaining_args().is_empty());
    }

    #[test]
    fn scanner_moves_unregistered_arguments_and_double_dash_to_remaining() {
        let options = [OptionSpec::value("--model", parse_text)];
        let input = parse_options(&strings(&["--model", "m", "extra", "--", "rest"]), &options);
        assert_eq!(input.remaining_args(), strings(&["extra", "--", "rest"]));
    }

    #[test]
    fn parse_auth_rejects_both_sources_and_accepts_each() {
        let options = [AUTH_TOKEN_OPTION, AUTH_TOKEN_FILE_OPTION];
        let both = parse_options(
            &strings(&["--auth-token", "t", "--auth-token-file", "f"]),
            &options,
        );
        assert_eq!(
            parse_auth(&both),
            (
                None,
                vec!["--auth-token and --auth-token-file are mutually exclusive".to_owned()]
            )
        );
        let token = parse_options(&strings(&["--auth-token", "t"]), &options);
        assert_eq!(
            parse_auth(&token).0,
            Some(AuthInput::Token {
                token: "t".to_owned()
            })
        );
        let file = parse_options(&strings(&["--auth-token-file", "f"]), &options);
        assert_eq!(
            parse_auth(&file).0,
            Some(AuthInput::File {
                path: "f".to_owned()
            })
        );
    }
}
