//! Shared infrastructure and seven concrete coding tools.
//!
//! Ports `.references/pi-2.0/packages/coding-agent/src/core/tools/`: the ordered
//! built-in registry, read/bash/edit/write/grep/find/ls implementations,
//! truncation, rolling-output spill, per-realpath mutation serialization, and
//! path resolution.

pub mod bash;
pub mod edit;
pub mod edit_diff;
pub mod find;
pub mod grep;
pub mod ls;
pub mod mutation_queue;
pub mod output_accumulator;
pub mod path_utils;
pub mod read;
pub mod truncate;
pub mod write;

pub use bash::{
    BashOperations, BashSpawnContext, BashSpawnHook, BashTool, BashToolDetails, BashToolInput,
    BashToolOptions, LocalBashOperations, create_bash_tool,
};
pub use edit::{
    EditTool, EditToolDetails, EditToolInput, EditToolOptions, ReplaceEditInput, create_edit_tool,
};
pub use find::{FindTool, FindToolDetails, FindToolInput, FindToolOptions, create_find_tool};
pub use grep::{GrepTool, GrepToolDetails, GrepToolInput, GrepToolOptions, create_grep_tool};
pub use ls::{LsTool, LsToolDetails, LsToolInput, LsToolOptions, create_ls_tool};
pub use mutation_queue::{MutationQueueError, with_file_mutation_queue};
pub use output_accumulator::{
    DEFAULT_TEMP_FILE_PREFIX, OutputAccumulator, OutputAccumulatorError, OutputAccumulatorOptions,
    OutputSnapshot,
};
pub use path_utils::{
    PathResolveError, expand_path, path_exists, resolve_read_path, resolve_read_path_async,
    resolve_to_cwd,
};
pub use read::{
    ReadTool, ReadToolDetails, ReadToolInput, ReadToolOptions, create_read_tool,
    detect_supported_image_mime_type,
};
pub use truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, GREP_MAX_LINE_LENGTH, TruncatedBy, TruncatedLine,
    TruncationOptions, TruncationResult, format_size, truncate_head, truncate_line,
    truncate_line_with, truncate_tail,
};
pub use write::{WriteTool, WriteToolDetails, WriteToolInput, WriteToolOptions, create_write_tool};

use std::path::Path;
use std::sync::Arc;

use pi_agent::AgentTool;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Name of one of the seven built-in coding tools (TypeScript `ToolName`).
///
/// Wire form is the lowercase string used in settings and session activation
/// lists (`"read"`, `"bash"`, …).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolName {
    /// Read file contents (and images).
    Read,
    /// Execute a bash command.
    Bash,
    /// Exact multi-edit of a single file.
    Edit,
    /// Create or overwrite a file.
    Write,
    /// Search file contents (ripgrep).
    Grep,
    /// Find files by glob (fd).
    Find,
    /// List a directory.
    Ls,
}

/// Full registry order (TypeScript `allToolNames` insertion order and
/// `createAllToolDefinitions` key order): read → bash → edit → write → grep
/// → find → ls.
pub const ALL_TOOL_NAMES: [ToolName; 7] = [
    ToolName::Read,
    ToolName::Bash,
    ToolName::Edit,
    ToolName::Write,
    ToolName::Grep,
    ToolName::Find,
    ToolName::Ls,
];

/// Default session-active tools (TypeScript `defaultActiveToolNames` /
/// `createCodingToolDefinitions`): read, bash, edit, write. The full
/// registry still contains grep/find/ls; they stay inactive until activated.
pub const DEFAULT_ACTIVE_TOOL_NAMES: [ToolName; 4] = [
    ToolName::Read,
    ToolName::Bash,
    ToolName::Edit,
    ToolName::Write,
];

/// Read-only tool set (TypeScript `createReadOnlyToolDefinitions`): read,
/// grep, find, ls.
pub const READ_ONLY_TOOL_NAMES: [ToolName; 4] =
    [ToolName::Read, ToolName::Grep, ToolName::Find, ToolName::Ls];

/// Builds all seven tools in provider-visible registry order.
#[must_use]
pub fn create_all_tool_definitions(cwd: impl AsRef<Path>) -> [Arc<dyn AgentTool>; 7] {
    let cwd = cwd.as_ref();
    [
        create_read_tool(cwd),
        create_bash_tool(cwd),
        create_edit_tool(cwd),
        create_write_tool(cwd),
        create_grep_tool(cwd),
        create_find_tool(cwd),
        create_ls_tool(cwd),
    ]
}

/// Builds the four tools enabled by default for coding sessions.
#[must_use]
pub fn create_coding_tool_definitions(cwd: impl AsRef<Path>) -> [Arc<dyn AgentTool>; 4] {
    let cwd = cwd.as_ref();
    [
        create_read_tool(cwd),
        create_bash_tool(cwd),
        create_edit_tool(cwd),
        create_write_tool(cwd),
    ]
}

/// Builds the read-only tool set in registry order.
#[must_use]
pub fn create_read_only_tool_definitions(cwd: impl AsRef<Path>) -> [Arc<dyn AgentTool>; 4] {
    let cwd = cwd.as_ref();
    [
        create_read_tool(cwd),
        create_grep_tool(cwd),
        create_find_tool(cwd),
        create_ls_tool(cwd),
    ]
}

impl ToolName {
    /// Lowercase wire name matching the TypeScript discriminant.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Bash => "bash",
            Self::Edit => "edit",
            Self::Write => "write",
            Self::Grep => "grep",
            Self::Find => "find",
            Self::Ls => "ls",
        }
    }

    /// Whether this tool is part of the default-active coding set.
    #[must_use]
    pub const fn is_default_active(self) -> bool {
        matches!(self, Self::Read | Self::Bash | Self::Edit | Self::Write)
    }

    /// Whether this tool is part of the read-only set.
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::Read | Self::Grep | Self::Find | Self::Ls)
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned by [`ToolName`]'s [`FromStr`] implementation. Display text
/// matches TypeScript `Unknown tool name: ${toolName}` exactly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownToolName(String);

impl UnknownToolName {
    /// The unrecognized tool name string.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UnknownToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Unknown tool name: {}", self.0)
    }
}

impl Error for UnknownToolName {}

impl FromStr for ToolName {
    type Err = UnknownToolName;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "read" => Ok(Self::Read),
            "bash" => Ok(Self::Bash),
            "edit" => Ok(Self::Edit),
            "write" => Ok(Self::Write),
            "grep" => Ok(Self::Grep),
            "find" => Ok(Self::Find),
            "ls" => Ok(Self::Ls),
            other => Err(UnknownToolName(other.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn Error + Send + Sync>>;

    #[test]
    fn registry_order_is_read_bash_edit_write_grep_find_ls() {
        assert_eq!(
            ALL_TOOL_NAMES,
            [
                ToolName::Read,
                ToolName::Bash,
                ToolName::Edit,
                ToolName::Write,
                ToolName::Grep,
                ToolName::Find,
                ToolName::Ls,
            ]
        );
        assert_eq!(
            ALL_TOOL_NAMES.map(ToolName::as_str),
            ["read", "bash", "edit", "write", "grep", "find", "ls"]
        );
    }

    #[test]
    fn default_active_is_read_bash_edit_write() {
        assert_eq!(
            DEFAULT_ACTIVE_TOOL_NAMES,
            [
                ToolName::Read,
                ToolName::Bash,
                ToolName::Edit,
                ToolName::Write,
            ]
        );
        for name in ALL_TOOL_NAMES {
            assert_eq!(
                name.is_default_active(),
                DEFAULT_ACTIVE_TOOL_NAMES.contains(&name)
            );
        }
    }

    #[test]
    fn read_only_is_read_grep_find_ls() {
        assert_eq!(
            READ_ONLY_TOOL_NAMES,
            [ToolName::Read, ToolName::Grep, ToolName::Find, ToolName::Ls,]
        );
        for name in ALL_TOOL_NAMES {
            assert_eq!(name.is_read_only(), READ_ONLY_TOOL_NAMES.contains(&name));
        }
    }

    #[test]
    fn from_str_and_display_roundtrip() -> TestResult {
        for name in ALL_TOOL_NAMES {
            let parsed: ToolName = name.as_str().parse()?;
            assert_eq!(parsed, name);
            assert_eq!(parsed.to_string(), name.as_str());
        }
        Ok(())
    }

    #[test]
    fn unknown_tool_name_error_matches_typescript() -> TestResult {
        let Err(err) = "foo".parse::<ToolName>() else {
            return Err(std::io::Error::other("foo must not parse as a tool name").into());
        };
        assert_eq!(err.to_string(), "Unknown tool name: foo");
        assert_eq!(err.name(), "foo");
        Ok(())
    }

    #[test]
    fn serde_roundtrip_uses_lowercase_wire_names() -> TestResult {
        for name in ALL_TOOL_NAMES {
            let json = serde_json::to_string(&name)?;
            assert_eq!(json, format!("\"{}\"", name.as_str()));
            let parsed: ToolName = serde_json::from_str(&json)?;
            assert_eq!(parsed, name);
        }
        // Unknown variants are rejected by serde (settings validation).
        let bad: Result<ToolName, _> = serde_json::from_str("\"unknown\"");
        assert!(bad.is_err());
        Ok(())
    }
}
