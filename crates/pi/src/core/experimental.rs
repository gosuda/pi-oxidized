//! Experimental product feature gating.

/// Environment variable that enables experimental product features.
pub const EXPERIMENTAL_ENV: &str = "PI_EXPERIMENTAL";

/// Return whether experimental product features are enabled in the process.
#[must_use]
pub fn are_experimental_features_enabled() -> bool {
    std::env::var(EXPERIMENTAL_ENV).is_ok_and(|value| value == "1")
}

/// Evaluate the experimental feature gate against an injected value.
#[must_use]
pub fn are_experimental_features_enabled_with(value: Option<&str>) -> bool {
    value == Some("1")
}

#[cfg(test)]
mod tests {
    use super::are_experimental_features_enabled_with;

    #[test]
    fn only_literal_one_enables_experimental_features() {
        assert!(are_experimental_features_enabled_with(Some("1")));
        for value in [None, Some(""), Some("0"), Some("true"), Some("01")] {
            assert!(!are_experimental_features_enabled_with(value));
        }
    }
}
