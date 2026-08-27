//! Hermetic fixture transport for OAuth login-flow HTTP interception.
//!
//! When the `PI_AI_OAUTH_FIXTURE` environment variable names a fixture JSON
//! file (the shape produced by the PAR-CLI-PROTO harness under
//! `prototype/par-cli-proto/fixtures/`), every [`super::http::AuthHttpClient`]
//! request to a known OAuth endpoint domain is answered from the fixture
//! instead of hitting the network. This lets the `pi-ai` CLI binary replay all
//! seven OAuth provider flows hermetically and byte-compare its
//! stdout/stderr/exit-code/`auth.json` output against committed goldens.
//!
//! The transport mirrors `prototype/par-cli-proto/harness/fetch-shim.ts`:
//!   * routes are matched by `METHOD host/path` (exact, then `startsWith`);
//!   * per-route call counts advance so a polling endpoint can return
//!     `authorization_pending` on the first call and the token on the second;
//!   * calls beyond the response array clamp to the last response.
//!
//! The fixture layer is inert in production: with no env var set, [`enabled`]
//! is `false` and [`lookup`] returns `None`, so the real `reqwest` client runs
//! unchanged.

use std::collections::HashMap;
use std::fs;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

/// Env var naming the fixture JSON file to replay.
pub const FIXTURE_ENV: &str = "PI_AI_OAUTH_FIXTURE";

/// OAuth endpoint domains intercepted by the fixture transport.
///
/// Mirrors the `oauthDomains` set in `fetch-shim.ts`.
const OAUTH_DOMAINS: &[&str] = &[
    "auth.x.ai",
    "github.com",
    "api.github.com",
    "api.individual.githubcopilot.com",
    "claude.ai",
    "platform.claude.com",
    "openrouter.ai",
    "auth.openai.com",
    "radius.pi.dev",
    "auth.kimi.com",
];

/// A single fixture response entry.
#[derive(Clone, Debug, Deserialize)]
struct FixtureResponse {
    #[serde(default)]
    status: Option<u16>,
    #[serde(default)]
    headers: Option<HashMap<String, String>>,
    body: Value,
}

/// A route inside the fixture: method + `host/path` pattern + ordered responses.
#[derive(Clone, Debug, Deserialize)]
struct FixtureRoute {
    method: String,
    pattern: String,
    responses: Vec<FixtureResponse>,
}
/// The top-level fixture file. The fixture's `provider` tag is informational
/// only and ignored by serde (unknown fields are dropped by default).
#[derive(Clone, Debug, Deserialize)]
struct Fixture {
    routes: Vec<FixtureRoute>,
}

/// A matched fixture hit ready to serve.
#[derive(Clone, Debug)]
pub struct FixtureHit {
    /// HTTP status code (defaults to 200 when absent in the fixture).
    pub status: u16,
    /// Serialized response body (JSON text or raw string).
    pub body: String,
}

struct Transport {
    fixture: Fixture,
    counts: std::sync::Mutex<HashMap<String, usize>>,
}

static TRANSPORT: OnceLock<Option<Transport>> = OnceLock::new();

fn load() -> Option<&'static Transport> {
    TRANSPORT
        .get_or_init(|| {
            let path = std::env::var(FIXTURE_ENV).ok()?;
            let text = fs::read_to_string(&path).ok()?;
            let fixture: Fixture = serde_json::from_str(&text).ok()?;
            Some(Transport {
                fixture,
                counts: std::sync::Mutex::new(HashMap::new()),
            })
        })
        .as_ref()
}

/// Whether the fixture transport is active (env var set and fixture loaded).
#[must_use]
pub fn enabled() -> bool {
    load().is_some()
}

/// Look up a fixture response for `method` + `url`.
///
/// Returns `None` when the transport is inactive, the host is not an OAuth
/// endpoint domain, or no fixture route matches. The caller then falls through
/// to the real `reqwest` client.
#[must_use]
pub fn lookup(method: &str, url: &str) -> Option<FixtureHit> {
    let transport = load()?;
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if !OAUTH_DOMAINS.contains(&host) {
        return None;
    }
    let path = parsed.path();
    let method_upper = method.to_ascii_uppercase();
    let target = format!("{method_upper} {host}{path}");

    // Exact match first.
    let mut route = transport.fixture.routes.iter().find(|r| {
        target == format!("{} {}", r.method.to_ascii_uppercase(), r.pattern)
    });

    // Fallback: partial match on host+path (dynamic paths like /models/{id}/policy).
    if route.is_none() {
        let full = format!("{host}{path}");
        route = transport.fixture.routes.iter().find(|r| {
            method_upper == r.method.to_ascii_uppercase() && full.starts_with(&r.pattern)
        });
    }

    let route = route?;
    let key = format!("{method_upper} {host}{path}");
    let idx = {
        let mut counts = transport.counts.lock().ok()?;
        let entry = counts.entry(key.clone()).or_insert(0);
        *entry += 1;
        *entry
    };
    let clamped = idx.min(route.responses.len()) - 1;
    let resp = &route.responses[clamped];
    let body = if let Value::String(s) = &resp.body {
        s.clone()
    } else {
        serde_json::to_string(&resp.body).unwrap_or_default()
    };
    // Fixture headers are ignored — the OAuth flows read only status + body.
    // Default 200 mirrors fetch-shim.ts.
    Some(FixtureHit {
        status: resp.status.unwrap_or(200),
        body,
    })
}
