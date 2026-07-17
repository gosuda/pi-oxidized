//! Coding-agent product services and executable.

/// The version of the pi crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() -> Result<(), &'static str> {
        let version = std::hint::black_box(VERSION);
        if version.is_empty() {
            Err("VERSION is empty")
        } else {
            Ok(())
        }
    }
}
