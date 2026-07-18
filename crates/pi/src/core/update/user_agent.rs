//! pi user-agent string.
//!
//! Ports `.references/pi/packages/coding-agent/src/utils/pi-user-agent.ts`.
//!
//! TypeScript emits `pi/{version} ({platform}; {bun|node}/{ver}; {arch})`
//! because pi historically shipped as a Node/Bun application. The native Rust
//! binary has no JavaScript runtime, so the middle token is replaced with the
//! literal `native`. This is the single documented user-agent divergence: the
//! pi.dev version endpoint only logs the header, so the runtime token carries
//! no behavioral contract.

use std::env::consts::{ARCH, OS};

/// Build the pi user-agent string for the supplied crate version.
///
/// Wire shape: `pi/{version} ({os}; native; {arch})`. The `native` runtime
/// token is the intentional divergence from the TypeScript helper documented
/// above.
#[must_use]
pub fn get_pi_user_agent(version: &str) -> String {
    format!("pi/{version} ({OS}; native; {ARCH})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_shape_matches_documented_divergence() {
        let ua = get_pi_user_agent("1.2.3");
        assert!(
            ua.starts_with("pi/1.2.3 ("),
            "ua must start with pi/<version> (, got {ua}"
        );
        assert!(
            ua.contains("; native; "),
            "ua must contain the native runtime token, got {ua}"
        );
        // Three semicolon-separated segments inside the parens, closing paren last.
        let inner = ua
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(inner, _)| inner)
            .unwrap_or_default();
        let segments: Vec<&str> = inner.split("; ").collect();
        assert_eq!(segments.len(), 3, "expected 3 inner segments, got {ua}");
        assert_eq!(segments[1], "native");
    }

    #[test]
    fn user_agent_includes_os_and_arch_tokens() {
        let ua = get_pi_user_agent("0.0.0");
        assert!(ua.contains(OS), "ua must include the OS token, got {ua}");
        assert!(
            ua.contains(ARCH),
            "ua must include the ARCH token, got {ua}"
        );
    }
}
