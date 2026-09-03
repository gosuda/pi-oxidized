//! Latest-version resolution with an injectable HTTP endpoint.

use std::{cmp::Ordering, time::Duration};

use reqwest::Client;
use semver::Version;
use serde::Deserialize;
use thiserror::Error;

use super::user_agent::get_pi_user_agent;

/// Production latest-version endpoint.
pub const LATEST_VERSION_URL: &str = "https://pi.dev/api/latest-version";
/// Version probe deadline used by the product.
pub const DEFAULT_VERSION_CHECK_TIMEOUT_MS: u64 = 10_000;
/// Environment variable disabling periodic checks.
pub const ENV_SKIP_VERSION_CHECK: &str = "PI_SKIP_VERSION_CHECK";
/// Environment variable disabling all network-backed update work.
pub const ENV_OFFLINE: &str = "PI_OFFLINE";

/// Release track selected from a version response.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReleaseChannel {
    /// Latest non-prerelease release.
    #[default]
    Stable,
    /// Latest prerelease when supplied, otherwise the stable release.
    Beta,
}

/// Installable pi release metadata.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LatestPiRelease {
    /// Package version.
    pub version: String,
    /// Package name, used when the distribution is renamed.
    #[serde(default)]
    pub package_name: Option<String>,
    /// Optional release note.
    #[serde(default)]
    pub note: Option<String>,
}

/// Failure to obtain a valid release descriptor.
#[derive(Debug, Error)]
pub enum VersionCheckError {
    /// Update work was explicitly disabled.
    #[error("version checks are disabled while offline")]
    Offline,
    /// HTTP request or response body failure.
    #[error("version endpoint request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// Endpoint response did not contain a usable release.
    #[error("version endpoint returned malformed release data")]
    Malformed,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseEnvelope {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    package_name: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    stable: Option<LatestPiRelease>,
    #[serde(default)]
    beta: Option<LatestPiRelease>,
}

/// Compare two npm-compatible semantic versions.
///
/// A leading `v` is accepted, as by npm semver. Incomplete or malformed versions
/// return `None`, allowing the caller to use pi's string-inequality fallback.
#[must_use]
pub fn compare_package_versions(left: &str, right: &str) -> Option<Ordering> {
    Some(parse_version(left)?.cmp(&parse_version(right)?))
}

/// Whether a candidate is newer according to pi's compatibility rule.
#[must_use]
pub fn is_newer_package_version(candidate: &str, current: &str) -> bool {
    compare_package_versions(candidate, current)
        .map_or_else(|| candidate.trim() != current.trim(), Ordering::is_gt)
}

fn parse_version(value: &str) -> Option<Version> {
    let trimmed = value.trim();
    Version::parse(trimmed.strip_prefix('v').unwrap_or(trimmed)).ok()
}

/// Whether either environment switch disables the check.
#[must_use]
pub fn should_skip_version_check(skip: Option<&str>, offline: Option<&str>) -> bool {
    skip.is_some_and(|value| !value.is_empty()) || offline.is_some_and(|value| !value.is_empty())
}

/// Fetch a release using a fully injected client, endpoint, timeout, and channel.
///
/// # Errors
///
/// Returns [`VersionCheckError::Http`] when the request fails or the response
/// status is not success, and [`VersionCheckError::Malformed`] when the response
/// body cannot yield a usable release.
pub async fn get_latest_pi_release_from(
    client: &Client,
    endpoint: &str,
    current_version: &str,
    timeout: Duration,
    channel: ReleaseChannel,
) -> Result<LatestPiRelease, VersionCheckError> {
    let response = client
        .get(endpoint)
        .header("User-Agent", get_pi_user_agent(current_version))
        .header("accept", "application/json")
        .timeout(timeout)
        .send()
        .await?
        .error_for_status()?;
    let envelope = response.json::<ReleaseEnvelope>().await?;
    resolve_release(envelope, channel).ok_or(VersionCheckError::Malformed)
}

fn resolve_release(envelope: ReleaseEnvelope, channel: ReleaseChannel) -> Option<LatestPiRelease> {
    let release = match channel {
        ReleaseChannel::Stable => envelope.stable,
        ReleaseChannel::Beta => envelope.beta.or(envelope.stable),
    }
    .or_else(|| {
        envelope.version.map(|version| LatestPiRelease {
            version,
            package_name: envelope.package_name,
            note: envelope.note,
        })
    })?;

    let version = release.version.trim();
    if version.is_empty() {
        return None;
    }
    Some(LatestPiRelease {
        version: version.to_owned(),
        package_name: release
            .package_name
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        note: release
            .note
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
    })
}

/// Fetch the production release descriptor.
///
/// # Errors
///
/// Returns [`VersionCheckError::Offline`] when environment flags disable the
/// check; otherwise propagates the same errors as
/// [`get_latest_pi_release_from`].
pub async fn get_latest_pi_release(
    current_version: &str,
    channel: ReleaseChannel,
) -> Result<LatestPiRelease, VersionCheckError> {
    if should_skip_version_check(
        std::env::var(ENV_SKIP_VERSION_CHECK).ok().as_deref(),
        std::env::var(ENV_OFFLINE).ok().as_deref(),
    ) {
        return Err(VersionCheckError::Offline);
    }
    get_latest_pi_release_from(
        &Client::new(),
        LATEST_VERSION_URL,
        current_version,
        Duration::from_millis(DEFAULT_VERSION_CHECK_TIMEOUT_MS),
        channel,
    )
    .await
}

/// Check for a newer production release, swallowing probe failures as pi does.
pub async fn check_for_new_pi_version(current_version: &str) -> Option<LatestPiRelease> {
    let release = get_latest_pi_release(current_version, ReleaseChannel::Stable)
        .await
        .ok()?;
    is_newer_package_version(&release.version, current_version).then_some(release)
}

/// Injected check used by offline callers and tests.
pub async fn check_for_new_pi_version_from(
    client: &Client,
    endpoint: &str,
    current_version: &str,
    timeout: Duration,
    channel: ReleaseChannel,
    offline: bool,
) -> Option<LatestPiRelease> {
    if offline {
        return None;
    }
    let release = get_latest_pi_release_from(client, endpoint, current_version, timeout, channel)
        .await
        .ok()?;
    is_newer_package_version(&release.version, current_version).then_some(release)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };

    use super::*;

    #[test]
    fn semantic_order_covers_prereleases_and_malformed_fallback() {
        assert_eq!(
            compare_package_versions("1.2.3", "1.2.3"),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_package_versions("v1.2.4", "1.2.3"),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_package_versions("2.0.0-beta.1", "2.0.0"),
            Some(Ordering::Less)
        );
        assert_eq!(compare_package_versions("1.2", "1.2.0"), None);
        assert!(is_newer_package_version("malformed-a", "malformed-b"));
        assert!(!is_newer_package_version(" malformed ", "malformed"));
    }

    #[test]
    fn semver_handles_v_prefix_whitespace_and_prerelease_precedence() {
        // Leading v is accepted by both npm semver and this parser.
        assert_eq!(
            compare_package_versions("v1.0.0", "1.0.0"),
            Some(Ordering::Equal)
        );
        // Whitespace is trimmed.
        assert_eq!(
            compare_package_versions("  1.0.0  ", "1.0.0"),
            Some(Ordering::Equal)
        );
        // Prerelease ordering: alpha < beta < rc < release.
        assert_eq!(
            compare_package_versions("1.0.0-alpha.1", "1.0.0-beta.1"),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_package_versions("1.0.0-rc.1", "1.0.0-alpha.1"),
            Some(Ordering::Greater)
        );
        // Higher prerelease identifiers are greater.
        assert_eq!(
            compare_package_versions("1.0.0-rc.1", "1.0.0-alpha.1"),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn is_newer_package_version_uses_semver_then_string_fallback() {
        // Semver-greater is newer.
        assert!(is_newer_package_version("1.2.4", "1.2.3"));
        // Equal is not newer.
        assert!(!is_newer_package_version("1.0.0", "1.0.0"));
        // Semver-older is not newer.
        assert!(!is_newer_package_version("0.9.0", "1.0.0"));
        // Prerelease of same release is not newer than the release.
        assert!(!is_newer_package_version("1.0.0-beta.1", "1.0.0"));
        // Both malformed: string inequality decides.
        assert!(is_newer_package_version("zzz", "aaa"));
        assert!(!is_newer_package_version("aaa", "aaa"));
        // Both malformed but equal after trim: not newer.
        assert!(!is_newer_package_version(" aaa ", "aaa"));
    }

    #[test]
    fn should_skip_version_check_respects_both_env_switches() {
        // Both unset: do not skip.
        assert!(!should_skip_version_check(None, None));
        // Empty values do not skip (matches TS truthy check).
        assert!(!should_skip_version_check(Some(""), Some("")));
        // PI_SKIP_VERSION_CHECK set to any non-empty value.
        assert!(should_skip_version_check(Some("1"), None));
        assert!(should_skip_version_check(Some("false"), None));
        // PI_OFFLINE set to any non-empty value.
        assert!(should_skip_version_check(None, Some("1")));
        assert!(should_skip_version_check(None, Some("true")));
        // Either is enough.
        assert!(should_skip_version_check(Some("1"), Some("1")));
    }

    #[test]
    fn resolve_release_stable_uses_stable_field() -> Result<(), serde_json::Error> {
        let envelope: ReleaseEnvelope = serde_json::from_value(serde_json::json!({
            "stable": {"version": "  2.0.0  ", "packageName": "  pi-new  "},
            "beta": {"version": "2.1.0-beta.1"}
        }))?;
        let result = resolve_release(envelope, ReleaseChannel::Stable);
        assert!(result.is_some());
        let release = result.unwrap_or_default();
        assert_eq!(release.version, "2.0.0");
        assert_eq!(release.package_name, Some("pi-new".to_owned()));
        assert!(release.note.is_none());
        Ok(())
    }

    #[test]
    fn resolve_release_beta_falls_back_to_stable() -> Result<(), serde_json::Error> {
        // No beta field: beta channel falls back to stable.
        let envelope: ReleaseEnvelope = serde_json::from_value(serde_json::json!({
            "stable": {"version": "2.0.0"}
        }))?;
        let release = resolve_release(envelope.clone(), ReleaseChannel::Beta);
        assert_eq!(release.map(|r| r.version), Some("2.0.0".to_owned()));

        // Both present: beta takes priority.
        let envelope: ReleaseEnvelope = serde_json::from_value(serde_json::json!({
            "stable": {"version": "2.0.0"},
            "beta": {"version": "2.1.0-beta.1"}
        }))?;
        let result = resolve_release(envelope, ReleaseChannel::Beta);
        assert!(result.is_some());
        let release = result.unwrap_or_default();
        assert_eq!(release.version, "2.1.0-beta.1");
        Ok(())
    }

    #[test]
    fn resolve_release_falls_back_to_top_level_version() -> Result<(), serde_json::Error> {
        // Top-level version with no stable/beta fields.
        let envelope: ReleaseEnvelope = serde_json::from_value(serde_json::json!({
            "version": "3.0.0",
            "packageName": "pi-renamed",
            "note": " major release "
        }))?;
        let stable = resolve_release(envelope.clone(), ReleaseChannel::Stable);
        let beta = resolve_release(envelope, ReleaseChannel::Beta);
        assert_eq!(stable.as_ref().map(|r| r.version.as_str()), Some("3.0.0"));
        assert_eq!(beta.as_ref().map(|r| r.version.as_str()), Some("3.0.0"));
        assert_eq!(
            stable.and_then(|r| r.package_name),
            Some("pi-renamed".to_owned())
        );
        assert_eq!(beta.and_then(|r| r.note), Some("major release".to_owned()));
        Ok(())
    }

    #[test]
    fn resolve_release_returns_none_for_empty_or_missing_version() -> Result<(), serde_json::Error>
    {
        // Stable with empty version string.
        let envelope: ReleaseEnvelope = serde_json::from_value(serde_json::json!({
            "stable": {"version": "   "}
        }))?;
        assert!(resolve_release(envelope, ReleaseChannel::Stable).is_none());

        // Completely empty envelope.
        let envelope: ReleaseEnvelope = serde_json::from_value(serde_json::json!({}))?;
        assert!(resolve_release(envelope.clone(), ReleaseChannel::Stable).is_none());
        assert!(resolve_release(envelope, ReleaseChannel::Beta).is_none());
        Ok(())
    }

    #[test]
    fn resolve_release_trims_and_filters_empty_optional_fields() -> Result<(), serde_json::Error> {
        let envelope: ReleaseEnvelope = serde_json::from_value(serde_json::json!({
            "version": "1.0.0",
            "packageName": "   ",
            "note": "   "
        }))?;
        let result = resolve_release(envelope, ReleaseChannel::Stable);
        assert!(result.is_some());
        let release = result.unwrap_or_default();
        assert_eq!(release.version, "1.0.0");
        // Whitespace-only optionals become None.
        assert!(release.package_name.is_none());
        assert!(release.note.is_none());
        Ok(())
    }

    #[test]
    fn stable_and_beta_envelopes_resolve_deterministically() -> Result<(), serde_json::Error> {
        let envelope: ReleaseEnvelope = serde_json::from_value(serde_json::json!({
            "stable": {"version":"2.0.0"},
            "beta": {"version":"2.1.0-beta.1", "note":" preview "}
        }))?;
        let stable = resolve_release(envelope, ReleaseChannel::Stable);
        assert_eq!(stable.map(|value| value.version), Some("2.0.0".to_owned()));

        let envelope: ReleaseEnvelope = serde_json::from_value(serde_json::json!({
            "stable": {"version":"2.0.0"},
            "beta": {"version":"2.1.0-beta.1", "note":" preview "}
        }))?;
        let beta = resolve_release(envelope, ReleaseChannel::Beta);
        assert_eq!(
            beta.as_ref().map(|value| value.version.as_str()),
            Some("2.1.0-beta.1")
        );
        assert_eq!(
            beta.and_then(|value| value.note),
            Some("preview".to_owned())
        );
        Ok(())
    }

    fn fake_endpoint(
        body: &'static str,
        delay: Duration,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        thread::spawn(move || {
            for _ in 0..16 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut request = [0_u8; 4096];
                let mut total = 0;
                while total < request.len() {
                    match stream.read(&mut request[total..]) {
                        Ok(n) if n > 0 => {
                            total += n;
                            if request[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                let text = String::from_utf8_lossy(&request[..total]);
                if !text.starts_with("GET /latest ") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }
                thread::sleep(delay);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                return;
            }
        });
        Ok(format!("http://{address}/latest"))
    }

    #[tokio::test]
    async fn fake_http_covers_newer_equal_older_malformed_and_timeout()
    -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new();
        for (body, expected) in [
            (r#"{"version":"2.0.0"}"#, true),
            (r#"{"version":"1.0.0"}"#, false),
            (r#"{"version":"0.9.0"}"#, false),
            (r#"{"wrong":"shape"}"#, false),
        ] {
            let endpoint = fake_endpoint(body, Duration::ZERO)?;
            let release = check_for_new_pi_version_from(
                &client,
                &endpoint,
                "1.0.0",
                Duration::from_secs(1),
                ReleaseChannel::Stable,
                false,
            )
            .await;
            assert_eq!(release.is_some(), expected);
        }
        let endpoint = fake_endpoint(r#"{"version":"2.0.0"}"#, Duration::from_millis(100))?;
        let timed_out = check_for_new_pi_version_from(
            &client,
            &endpoint,
            "1.0.0",
            Duration::from_millis(5),
            ReleaseChannel::Stable,
            false,
        )
        .await;
        assert!(timed_out.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn fake_http_resolves_beta_channel_and_package_rename()
    -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new();
        // Beta channel with both stable and beta present.
        let endpoint = fake_endpoint(
            r#"{"stable":{"version":"2.0.0"},"beta":{"version":"2.1.0-beta.1","packageName":"pi-new"}}"#,
            Duration::ZERO,
        )?;
        let release = check_for_new_pi_version_from(
            &client,
            &endpoint,
            "2.0.0",
            Duration::from_secs(1),
            ReleaseChannel::Beta,
            false,
        )
        .await;
        assert!(release.is_some(), "beta must be newer than stable 2.0.0");
        let release = release.unwrap_or_default();
        assert_eq!(release.version, "2.1.0-beta.1");
        assert_eq!(release.package_name, Some("pi-new".to_owned()));
        Ok(())
    }

    #[tokio::test]
    async fn offline_injected_check_never_touches_endpoint() {
        let result = check_for_new_pi_version_from(
            &Client::new(),
            "http://127.0.0.1:1/should-not-connect",
            "1.0.0",
            Duration::from_millis(1),
            ReleaseChannel::Stable,
            true,
        )
        .await;
        assert!(result.is_none());
    }

    /// A localhost sweep (`GET /` at every new listener) must not consume
    /// the scripted stub reply: after probing the endpoint, the real
    /// version check still succeeds and returns the newer version.
    #[tokio::test]
    async fn sweep_get_does_not_consume_stub_scripts() -> Result<(), Box<dyn std::error::Error>> {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let endpoint = fake_endpoint(r#"{"version":"2.0.0"}"#, Duration::ZERO)?;
        let addr = endpoint
            .strip_prefix("http://")
            .and_then(|rest| rest.split('/').next())
            .ok_or("bad endpoint url")?;

        // Send a sweeper-style GET / to the stub.
        {
            let mut sweep = TcpStream::connect(addr)?;
            sweep.write_all(b"GET / HTTP/1.1\r\nHost: sweep\r\nConnection: close\r\n\r\n")?;
            let mut buf = [0_u8; 1024];
            let _ = sweep.read(&mut buf);
        }

        // The real version check must still succeed.
        let client = Client::new();
        let release = check_for_new_pi_version_from(
            &client,
            &endpoint,
            "1.0.0",
            Duration::from_secs(1),
            ReleaseChannel::Stable,
            false,
        )
        .await;
        assert!(release.is_some(), "real request must succeed after sweep");
        assert_eq!(release.unwrap_or_default().version, "2.0.0");
        Ok(())
    }
}
