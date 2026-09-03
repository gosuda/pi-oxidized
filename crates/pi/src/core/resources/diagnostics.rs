//! Resource load diagnostics and name-collision reports.
//!
//! Port of `.references/pi-2.0/packages/coding-agent/src/core/diagnostics.ts`.

/// Resource kind that participated in a name collision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceType {
    /// TypeScript/JavaScript extension path.
    Extension,
    /// Skill markdown file.
    Skill,
    /// Prompt template markdown file.
    Prompt,
    /// Theme JSON file.
    Theme,
}

impl ResourceType {
    /// Wire discriminant.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Extension => "extension",
            Self::Skill => "skill",
            Self::Prompt => "prompt",
            Self::Theme => "theme",
        }
    }
}

/// Collision between two resources of the same logical name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceCollision {
    /// Kind of resource that collided.
    pub resource_type: ResourceType,
    /// Shared logical name (skill name, prompt name, theme name, …).
    pub name: String,
    /// Path of the winner (first-wins after precedence).
    pub winner_path: String,
    /// Path of the discarded loser.
    pub loser_path: String,
    /// Optional source label for the winner.
    pub winner_source: Option<String>,
    /// Optional source label for the loser.
    pub loser_source: Option<String>,
}

/// Severity of a resource diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticType {
    /// Non-fatal validation or load issue.
    Warning,
    /// Hard failure for an explicit path (e.g. missing CLI path).
    Error,
    /// Name collision (first path wins).
    Collision,
}

impl DiagnosticType {
    /// Wire discriminant.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Collision => "collision",
        }
    }
}

/// Single diagnostic emitted while discovering or loading resources.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceDiagnostic {
    /// Severity.
    pub type_: DiagnosticType,
    /// Human-readable message (exact TypeScript wording where applicable).
    pub message: String,
    /// Optional path the diagnostic refers to.
    pub path: Option<String>,
    /// Present when `type_` is collision.
    pub collision: Option<ResourceCollision>,
}

impl ResourceDiagnostic {
    /// Warning diagnostic.
    #[must_use]
    pub fn warning(message: impl Into<String>, path: Option<String>) -> Self {
        Self {
            type_: DiagnosticType::Warning,
            message: message.into(),
            path,
            collision: None,
        }
    }

    /// Error diagnostic.
    #[must_use]
    pub fn error(message: impl Into<String>, path: Option<String>) -> Self {
        Self {
            type_: DiagnosticType::Error,
            message: message.into(),
            path,
            collision: None,
        }
    }

    /// Collision diagnostic.
    #[must_use]
    pub fn collision(
        message: impl Into<String>,
        path: Option<String>,
        collision: ResourceCollision,
    ) -> Self {
        Self {
            type_: DiagnosticType::Collision,
            message: message.into(),
            path,
            collision: Some(collision),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_set_fields() {
        let w = ResourceDiagnostic::warning("x", Some("/a".into()));
        assert_eq!(w.type_, DiagnosticType::Warning);
        assert_eq!(w.message, "x");
        assert_eq!(w.path.as_deref(), Some("/a"));
        let c = ResourceDiagnostic::collision(
            "name \"s\" collision",
            Some("/loser".into()),
            ResourceCollision {
                resource_type: ResourceType::Skill,
                name: "s".into(),
                winner_path: "/win".into(),
                loser_path: "/loser".into(),
                winner_source: None,
                loser_source: None,
            },
        );
        assert_eq!(c.type_, DiagnosticType::Collision);
        assert!(c.collision.is_some());
    }
}
