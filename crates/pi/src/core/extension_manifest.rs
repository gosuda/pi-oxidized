//! Strict `pi-extension.json` parsing and runtime classification.
//!
//! The manifest is a filesystem selector owned by the product crate (never
//! `pi-ext`): it decides whether one discovered extension path spawns as a
//! Mode 1 compat extension in the compiled Bun host, a Mode 2 lean
//! TypeScript extension, or a Mode 3 native executable. A discovery path
//! without a manifest (regular file or directory) always stays
//! compat, preserving upstream behavior.
//!
//! Strict schema (unknown fields rejected):
//!
//! ```json
//! {
//!   "$schema": "pi.extension.v1",
//!   "name": "my-extension",
//!   "version": "1.2.3",
//!   "runtime": "ts-lean",
//!   "entry": "dist/main.ts",
//!   "protocolVersion": 1
//! }
//! ```
//!
//! `entry` may also be a `{ "<target-triple>": "<relative path>" }` map,
//! intended for per-platform native executables and resolved uniformly for
//! every runtime. A map missing the current target yields
//! [`ManifestErrorKind::UnsupportedPlatform`], never a parse failure, so
//! sibling extensions continue to load.

use std::path::{Component, Path, PathBuf};

use semver::Version;

/// Manifest file name looked up inside extension directories.
pub const MANIFEST_FILE_NAME: &str = "pi-extension.json";

/// Exact `$schema` value required by the strict manifest parser.
pub const MANIFEST_SCHEMA: &str = "pi.extension.v1";

/// Fields accepted by the strict manifest schema.
const KNOWN_FIELDS: [&str; 6] = [
    "$schema",
    "entry",
    "name",
    "protocolVersion",
    "runtime",
    "version",
];

/// Runtime endpoint an extension path is classified into.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExtensionMode {
    /// Mode 1: upstream TypeScript in the compiled Bun host (default).
    Compat,
    /// Mode 2: lean TypeScript (`--lean` in the compiled host artifact).
    Lean,
    /// Mode 3: native executable speaking the `pi-ext` JSONL protocol.
    Native,
}

/// Validated identity fields of a `pi-extension.json` manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestIdentity {
    /// Non-blank extension name.
    pub name: String,
    /// Semver version, already validated.
    pub version: Version,
    /// Wire protocol version; always equals
    /// [`pi_ext::protocol::PROTOCOL_VERSION`] after validation.
    pub protocol_version: u32,
}

/// Outcome of classifying one discovered extension path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClassifiedExtension {
    /// Runtime endpoint mode this path belongs to.
    pub mode: ExtensionMode,
    /// Canonical extension root directory; `entry` is contained in it.
    pub root: PathBuf,
    /// Canonical entry path, guaranteed contained in `root`.
    ///
    /// For manifest-less compat paths this is the discovered file or
    /// directory itself.
    pub entry: PathBuf,
    /// Manifest identity; `None` for manifest-less compat extensions.
    pub manifest: Option<ManifestIdentity>,
}

/// Failure to classify an extension path or parse its manifest.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("extension manifest at {}: {kind}", .path.display())]
pub struct ManifestError {
    path: PathBuf,
    kind: ManifestErrorKind,
}

impl ManifestError {
    fn new(path: &Path, kind: ManifestErrorKind) -> Self {
        Self {
            path: path.to_path_buf(),
            kind,
        }
    }

    /// Discovery path (or manifest path) that failed classification.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Typed failure classification.
    #[must_use]
    pub fn kind(&self) -> &ManifestErrorKind {
        &self.kind
    }
}

/// Typed manifest/discovery failure classification.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ManifestErrorKind {
    /// Filesystem failure while probing or reading.
    #[error("I/O failure: {0}")]
    Io(String),
    /// Discovery path is neither a regular file nor a directory.
    #[error("not a file or directory")]
    InvalidDiscovery,
    /// Manifest bytes are not valid JSON.
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    /// Manifest top level is not a JSON object.
    #[error("manifest must be a JSON object")]
    NotAnObject,
    /// Strict schema rejects this field.
    #[error("unknown field `{0}`")]
    UnknownField(String),
    /// `$schema` is missing, not a string, or not `pi.extension.v1`.
    #[error("wrong $schema (found {found:?}, expected `{expected}`)")]
    WrongSchema {
        /// Value declared by the manifest, when present and a string.
        found: Option<String>,
        /// Expected schema identifier.
        expected: &'static str,
    },
    /// A required field is absent.
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    /// A field has the wrong JSON type.
    #[error("invalid field shape: {0}")]
    InvalidShape(String),
    /// `name` is empty or whitespace only.
    #[error("name must be a non-blank string")]
    BlankName,
    /// `version` is not a semver string.
    #[error("version `{0}` is not valid semver")]
    InvalidVersion(String),
    /// `runtime` is not one of `ts-compat` / `ts-lean` / `native`.
    #[error("unknown runtime `{0}`")]
    UnknownRuntime(String),
    /// `protocolVersion` differs from the compiled protocol version.
    #[error("protocolVersion {found} does not match compiled {expected}")]
    ProtocolMismatch {
        /// Value declared by the manifest.
        found: u64,
        /// Compiled `pi-ext` protocol version.
        expected: u32,
    },
    /// `entry` is neither a relative path string nor a target map of them.
    #[error("entry must be a non-empty relative path string or a target-triple map")]
    InvalidEntryShape,
    /// `entry` is an absolute path.
    #[error("entry `{0}` must be relative, not absolute")]
    AbsoluteEntry(String),
    /// `entry` resolves outside the extension root (traversal or symlink).
    #[error("entry `{0}` escapes the extension directory")]
    EntryEscapesRoot(String),
    /// `entry` does not exist on disk.
    #[error("entry `{0}` does not exist")]
    EntryNotFound(String),
    /// Target map has no entry for the requested target triple.
    #[error("no entry for target `{target}` (available: {available:?})")]
    UnsupportedPlatform {
        /// Requested target triple.
        target: String,
        /// Target triples present in the manifest.
        available: Vec<String>,
    },
    /// Lean entry is not a prebundled `.mjs` file (case-sensitive check on
    /// the resolved entry path).
    #[error("lean entries must be prebundled .mjs files (manifest runtime \"ts-lean\")")]
    LeanEntryNotPrebundled,
}

/// Classify one discovered extension path for `target`.
///
/// `path` is an extension discovery path (regular file or directory);
/// `target` is the compiled target triple used to resolve `entry` target
/// maps. Manifest-less paths classify as [`ExtensionMode::Compat`] with a
/// canonical entry and no identity.
///
/// # Errors
///
/// Returns a typed [`ManifestError`] for filesystem failures and any
/// strict-schema violation: unknown fields, wrong schema, blank name,
/// non-semver version, unknown runtime, protocol mismatch, malformed or
/// uncontained entries, target maps missing the requested platform, and
/// lean entries that are not prebundled `.mjs` files.
pub fn classify_extension(path: &Path, target: &str) -> Result<ClassifiedExtension, ManifestError> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| ManifestError::new(path, ManifestErrorKind::Io(error.to_string())))?;
    if metadata.is_file() {
        return classify_file(path);
    }
    if metadata.is_dir() {
        return classify_directory(path, target);
    }
    Err(ManifestError::new(
        path,
        ManifestErrorKind::InvalidDiscovery,
    ))
}

/// A single-file discovery path is always a compat extension.
fn classify_file(path: &Path) -> Result<ClassifiedExtension, ManifestError> {
    // Mode 1 preserves upstream discovery: any manifest-less regular file
    // is loaded by the compiled Bun host, not filtered by extension.
    let entry = canonicalize(path, path)?;
    let root = entry
        .parent()
        .map_or_else(|| entry.clone(), Path::to_path_buf);
    Ok(ClassifiedExtension {
        mode: ExtensionMode::Compat,
        root,
        entry,
        manifest: None,
    })
}

/// A directory discovery path is compat unless a strict manifest upgrades it.
fn classify_directory(path: &Path, target: &str) -> Result<ClassifiedExtension, ManifestError> {
    let root = canonicalize(path, path)?;
    let manifest_path = root.join(MANIFEST_FILE_NAME);
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ClassifiedExtension {
                mode: ExtensionMode::Compat,
                entry: root.clone(),
                root,
                manifest: None,
            });
        }
        Err(error) => {
            return Err(ManifestError::new(
                &manifest_path,
                ManifestErrorKind::Io(error.to_string()),
            ));
        }
    };
    let manifest = parse_manifest(&manifest_path, &text)?;
    let entry = resolve_entry(&manifest_path, &root, &manifest.entry, target)?;
    if manifest.mode == ExtensionMode::Lean
        && entry.extension() != Some(std::ffi::OsStr::new("mjs"))
    {
        return Err(ManifestError::new(
            &manifest_path,
            ManifestErrorKind::LeanEntryNotPrebundled,
        ));
    }
    Ok(ClassifiedExtension {
        mode: manifest.mode,
        root,
        entry,
        manifest: Some(ManifestIdentity {
            name: manifest.name,
            version: manifest.version,
            protocol_version: manifest.protocol_version,
        }),
    })
}

/// Validated manifest fields prior to entry resolution.
struct RawManifest {
    mode: ExtensionMode,
    name: String,
    version: Version,
    entry: serde_json::Value,
    protocol_version: u32,
}

/// Parse manifest text under the strict schema.
fn parse_manifest(path: &Path, text: &str) -> Result<RawManifest, ManifestError> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|error| {
        ManifestError::new(path, ManifestErrorKind::InvalidJson(error.to_string()))
    })?;
    let object = value
        .as_object()
        .ok_or_else(|| ManifestError::new(path, ManifestErrorKind::NotAnObject))?;
    for key in object.keys() {
        if !KNOWN_FIELDS.contains(&key.as_str()) {
            return Err(ManifestError::new(
                path,
                ManifestErrorKind::UnknownField(key.clone()),
            ));
        }
    }
    let schema = object.get("$schema").and_then(serde_json::Value::as_str);
    if schema != Some(MANIFEST_SCHEMA) {
        return Err(ManifestError::new(
            path,
            ManifestErrorKind::WrongSchema {
                found: schema.map(str::to_owned),
                expected: MANIFEST_SCHEMA,
            },
        ));
    }
    let name = required_string(path, object, "name")?;
    if name.trim().is_empty() {
        return Err(ManifestError::new(path, ManifestErrorKind::BlankName));
    }
    let version_raw = required_string(path, object, "version")?;
    let version = Version::parse(version_raw).map_err(|_| {
        ManifestError::new(
            path,
            ManifestErrorKind::InvalidVersion(version_raw.to_owned()),
        )
    })?;
    let runtime = required_string(path, object, "runtime")?;
    let mode = match runtime {
        "ts-compat" => ExtensionMode::Compat,
        "ts-lean" => ExtensionMode::Lean,
        "native" => ExtensionMode::Native,
        other => {
            return Err(ManifestError::new(
                path,
                ManifestErrorKind::UnknownRuntime(other.to_owned()),
            ));
        }
    };
    let protocol_raw = object.get("protocolVersion").ok_or_else(|| {
        ManifestError::new(path, ManifestErrorKind::MissingField("protocolVersion"))
    })?;
    let protocol_version = protocol_raw.as_u64().ok_or_else(|| {
        ManifestError::new(
            path,
            ManifestErrorKind::InvalidShape("protocolVersion must be an integer".to_owned()),
        )
    })?;
    if protocol_version != u64::from(pi_ext::protocol::PROTOCOL_VERSION) {
        return Err(ManifestError::new(
            path,
            ManifestErrorKind::ProtocolMismatch {
                found: protocol_version,
                expected: pi_ext::protocol::PROTOCOL_VERSION,
            },
        ));
    }
    let entry = object
        .get("entry")
        .ok_or_else(|| ManifestError::new(path, ManifestErrorKind::MissingField("entry")))?
        .clone();
    Ok(RawManifest {
        mode,
        name: name.to_owned(),
        version,
        entry,
        protocol_version: pi_ext::protocol::PROTOCOL_VERSION,
    })
}

/// Extract a required string field with a typed diagnostic.
fn required_string<'a>(
    path: &Path,
    object: &'a serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<&'a str, ManifestError> {
    let value = object
        .get(field)
        .ok_or_else(|| ManifestError::new(path, ManifestErrorKind::MissingField(field)))?;
    value.as_str().ok_or_else(|| {
        ManifestError::new(
            path,
            ManifestErrorKind::InvalidShape(format!("{field} must be a string")),
        )
    })
}

/// Resolve the manifest `entry` to a canonical path contained in `root`.
fn resolve_entry(
    manifest_path: &Path,
    root: &Path,
    raw: &serde_json::Value,
    target: &str,
) -> Result<PathBuf, ManifestError> {
    let entry = match raw {
        serde_json::Value::String(entry) => entry.clone(),
        serde_json::Value::Object(map) => {
            let value = map.get(target).ok_or_else(|| {
                ManifestError::new(
                    manifest_path,
                    ManifestErrorKind::UnsupportedPlatform {
                        target: target.to_owned(),
                        available: map.keys().cloned().collect(),
                    },
                )
            })?;
            value.as_str().map(str::to_owned).ok_or_else(|| {
                ManifestError::new(manifest_path, ManifestErrorKind::InvalidEntryShape)
            })?
        }
        _ => {
            return Err(ManifestError::new(
                manifest_path,
                ManifestErrorKind::InvalidEntryShape,
            ));
        }
    };
    let relative = Path::new(&entry);
    if entry.is_empty() || relative.is_absolute() {
        let kind = if entry.is_empty() {
            ManifestErrorKind::InvalidEntryShape
        } else {
            ManifestErrorKind::AbsoluteEntry(entry)
        };
        return Err(ManifestError::new(manifest_path, kind));
    }
    for component in relative.components() {
        let kind = match component {
            Component::Normal(_) | Component::CurDir => continue,
            Component::ParentDir => ManifestErrorKind::EntryEscapesRoot(entry.clone()),
            Component::RootDir | Component::Prefix(_) => {
                ManifestErrorKind::AbsoluteEntry(entry.clone())
            }
        };
        return Err(ManifestError::new(manifest_path, kind));
    }
    let candidate = root.join(relative);
    let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
        let kind = if error.kind() == std::io::ErrorKind::NotFound {
            ManifestErrorKind::EntryNotFound(entry.clone())
        } else {
            ManifestErrorKind::Io(error.to_string())
        };
        ManifestError::new(manifest_path, kind)
    })?;
    if !canonical.starts_with(root) {
        return Err(ManifestError::new(
            manifest_path,
            ManifestErrorKind::EntryEscapesRoot(entry),
        ));
    }
    Ok(canonical)
}

/// Canonicalize an existing path, mapping failures to a typed diagnostic.
fn canonicalize(path: &Path, error_path: &Path) -> Result<PathBuf, ManifestError> {
    std::fs::canonicalize(path)
        .map_err(|error| ManifestError::new(error_path, ManifestErrorKind::Io(error.to_string())))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    const TARGET: &str = "x86_64-unknown-linux-gnu";

    /// Build manifest text; `entry` is a raw JSON fragment.
    fn manifest_json(name: &str, runtime: &str, entry: &str) -> String {
        let protocol = pi_ext::protocol::PROTOCOL_VERSION;
        format!(
            concat!(
                "{{\"$schema\":\"pi.extension.v1\",\"name\":\"{name}\",",
                "\"version\":\"1.2.3\",\"runtime\":\"{runtime}\",",
                "\"entry\":{entry},\"protocolVersion\":{protocol}}}"
            ),
            name = name,
            runtime = runtime,
            entry = entry,
            protocol = protocol,
        )
    }

    /// Create an extension directory with a manifest and the given files.
    fn extension_dir(
        temp: &TempDir,
        label: &str,
        manifest: &str,
        files: &[&str],
    ) -> TestResult<PathBuf> {
        let dir = temp.path().join(label);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(MANIFEST_FILE_NAME), manifest)?;
        for file in files {
            let file_path = dir.join(file);
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&file_path, b"")?;
        }
        Ok(dir)
    }

    fn expect_kind(
        result: Result<ClassifiedExtension, ManifestError>,
    ) -> TestResult<ManifestErrorKind> {
        match result {
            Ok(classified) => Err(format!("expected manifest error, got {classified:?}").into()),
            Err(error) => Ok(error.kind().clone()),
        }
    }

    #[test]
    fn regular_files_without_manifest_stay_compat() -> TestResult {
        let temp = tempfile::tempdir()?;
        // Regression: Mode 1 discovery must accept every regular file,
        // not only lowercase .ts/.js. No extension whitelist is used.
        for name in [
            "standalone.ts",
            "standalone.js",
            "standalone.mjs",
            "standalone.cjs",
            "standalone.tsx",
            "STANDALONE.TS",
            "extensionless",
        ] {
            let file = temp.path().join(name);
            std::fs::write(&file, b"export default {}")?;
            let classified = classify_extension(&file, TARGET)?;
            assert_eq!(classified.mode, ExtensionMode::Compat);
            assert_eq!(classified.manifest, None);
            assert_eq!(classified.entry, std::fs::canonicalize(&file)?);
            assert!(classified.entry.starts_with(&classified.root));
        }
        Ok(())
    }

    #[test]
    fn directory_without_manifest_stays_compat() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("plain-ext");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("index.ts"), b"export default {}")?;
        let classified = classify_extension(&dir, TARGET)?;
        assert_eq!(classified.mode, ExtensionMode::Compat);
        assert_eq!(classified.manifest, None);
        let canonical = std::fs::canonicalize(&dir)?;
        assert_eq!(classified.root, canonical);
        assert_eq!(classified.entry, canonical);
        Ok(())
    }

    #[test]
    fn lean_manifest_classifies_lean_with_identity() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "lean-ext",
            &manifest_json("lean-ext", "ts-lean", "\"dist/main.mjs\""),
            &["dist/main.mjs"],
        )?;
        let classified = classify_extension(&dir, TARGET)?;
        assert_eq!(classified.mode, ExtensionMode::Lean);
        let identity = classified.manifest.ok_or("manifest identity")?;
        assert_eq!(identity.name, "lean-ext");
        assert_eq!(identity.version, Version::parse("1.2.3")?);
        assert_eq!(
            identity.protocol_version,
            pi_ext::protocol::PROTOCOL_VERSION
        );
        assert_eq!(
            classified.entry,
            std::fs::canonicalize(dir.join("dist/main.mjs"))?
        );
        assert!(classified.entry.starts_with(&classified.root));
        Ok(())
    }

    #[test]
    fn lean_scalar_entry_must_be_prebundled_mjs() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "lean-ts",
            &manifest_json("lean-ts", "ts-lean", "\"dist/main.ts\""),
            &["dist/main.ts"],
        )?;
        let error = classify_extension(&dir, TARGET)
            .err()
            .ok_or("expected lean .mjs rejection")?;
        assert_eq!(error.kind(), &ManifestErrorKind::LeanEntryNotPrebundled);
        assert!(
            error.to_string().contains(
                "lean entries must be prebundled .mjs files (manifest runtime \"ts-lean\")"
            ),
            "loader diagnostic missing from Display: {error}"
        );
        Ok(())
    }

    #[test]
    fn lean_target_map_entry_must_be_prebundled_mjs() -> TestResult {
        let temp = tempfile::tempdir()?;
        let entry = format!("{{\"{TARGET}\":\"dist/main.ts\"}}");
        let dir = extension_dir(
            &temp,
            "lean-map-ts",
            &manifest_json("lean-map-ts", "ts-lean", &entry),
            &["dist/main.ts"],
        )?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(kind, ManifestErrorKind::LeanEntryNotPrebundled);
        Ok(())
    }

    #[test]
    fn lean_target_map_mjs_entry_is_accepted() -> TestResult {
        let temp = tempfile::tempdir()?;
        let entry = format!("{{\"{TARGET}\":\"./dist/main.mjs\"}}");
        let dir = extension_dir(
            &temp,
            "lean-map-mjs",
            &manifest_json("lean-map-mjs", "ts-lean", &entry),
            &["dist/main.mjs"],
        )?;
        let classified = classify_extension(&dir, TARGET)?;
        assert_eq!(classified.mode, ExtensionMode::Lean);
        assert_eq!(
            classified.entry,
            std::fs::canonicalize(dir.join("dist/main.mjs"))?
        );
        Ok(())
    }

    #[test]
    fn lean_uppercase_mjs_extension_is_rejected() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "lean-upper",
            &manifest_json("lean-upper", "ts-lean", "\"dist/main.MJS\""),
            &["dist/main.MJS"],
        )?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(kind, ManifestErrorKind::LeanEntryNotPrebundled);
        Ok(())
    }

    #[test]
    fn lean_extensionless_entry_must_be_prebundled_mjs() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "lean-extensionless",
            &manifest_json("lean-extensionless", "ts-lean", "\"dist/main\""),
            &["dist/main"],
        )?;
        let error = classify_extension(&dir, TARGET)
            .err()
            .ok_or("expected lean extensionless rejection")?;
        assert_eq!(error.kind(), &ManifestErrorKind::LeanEntryNotPrebundled);
        assert!(
            error.to_string().contains(
                "lean entries must be prebundled .mjs files (manifest runtime \"ts-lean\")"
            ),
            "loader diagnostic missing from Display: {error}"
        );
        Ok(())
    }

    #[test]
    fn ts_compat_manifest_classifies_compat_with_identity() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "compat-ext",
            &manifest_json("compat-ext", "ts-compat", "\"main.ts\""),
            &["main.ts"],
        )?;
        let classified = classify_extension(&dir, TARGET)?;
        assert_eq!(classified.mode, ExtensionMode::Compat);
        let identity = classified.manifest.ok_or("manifest identity")?;
        assert_eq!(identity.name, "compat-ext");
        assert_eq!(
            classified.entry,
            std::fs::canonicalize(dir.join("main.ts"))?
        );
        Ok(())
    }

    #[test]
    fn native_manifest_with_string_entry_classifies_native() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "native-ext",
            &manifest_json("native-ext", "native", "\"bin/tool\""),
            &["bin/tool"],
        )?;
        let classified = classify_extension(&dir, TARGET)?;
        assert_eq!(classified.mode, ExtensionMode::Native);
        assert_eq!(
            classified.entry,
            std::fs::canonicalize(dir.join("bin/tool"))?
        );
        assert!(classified.entry.starts_with(&classified.root));
        Ok(())
    }

    #[test]
    fn native_target_map_resolves_current_target() -> TestResult {
        let temp = tempfile::tempdir()?;
        let entry = format!(
            "{{\"{TARGET}\":\"bin/linux-tool\",\"aarch64-apple-darwin\":\"bin/mac-tool\"}}"
        );
        let dir = extension_dir(
            &temp,
            "native-map",
            &manifest_json("native-map", "native", &entry),
            &["bin/linux-tool", "bin/mac-tool"],
        )?;
        let classified = classify_extension(&dir, TARGET)?;
        assert_eq!(classified.mode, ExtensionMode::Native);
        assert_eq!(
            classified.entry,
            std::fs::canonicalize(dir.join("bin/linux-tool"))?
        );
        assert!(classified.entry.starts_with(&classified.root));
        Ok(())
    }

    #[test]
    fn unknown_field_is_rejected() -> TestResult {
        let temp = tempfile::tempdir()?;
        let mut manifest: serde_json::Value =
            serde_json::from_str(&manifest_json("ext", "ts-lean", "\"main.ts\""))?;
        manifest
            .as_object_mut()
            .ok_or("manifest object")?
            .insert("extra".to_owned(), serde_json::Value::Bool(true));
        let dir = extension_dir(&temp, "ext", &manifest.to_string(), &["main.ts"])?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(kind, ManifestErrorKind::UnknownField("extra".to_owned()));
        Ok(())
    }

    #[test]
    fn wrong_schema_is_rejected() -> TestResult {
        let temp = tempfile::tempdir()?;
        let manifest = manifest_json("ext", "ts-lean", "\"main.ts\"")
            .replace("pi.extension.v1", "pi.extension.v2");
        let dir = extension_dir(&temp, "ext", &manifest, &["main.ts"])?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(
            kind,
            ManifestErrorKind::WrongSchema {
                found: Some("pi.extension.v2".to_owned()),
                expected: MANIFEST_SCHEMA,
            }
        );
        Ok(())
    }

    #[test]
    fn invalid_json_is_rejected() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(&temp, "ext", "{ not json ", &[])?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert!(matches!(kind, ManifestErrorKind::InvalidJson(_)));
        Ok(())
    }

    #[test]
    fn blank_name_is_rejected() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "ext",
            &manifest_json("   ", "ts-lean", "\"main.ts\""),
            &["main.ts"],
        )?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(kind, ManifestErrorKind::BlankName);
        Ok(())
    }

    #[test]
    fn non_semver_version_is_rejected() -> TestResult {
        let temp = tempfile::tempdir()?;
        let manifest =
            manifest_json("ext", "ts-lean", "\"main.ts\"").replace("\"1.2.3\"", "\"1.0\"");
        let dir = extension_dir(&temp, "ext", &manifest, &["main.ts"])?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(kind, ManifestErrorKind::InvalidVersion("1.0".to_owned()));
        Ok(())
    }

    #[test]
    fn unknown_runtime_is_rejected() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "ext",
            &manifest_json("ext", "ts-fast", "\"main.ts\""),
            &["main.ts"],
        )?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(
            kind,
            ManifestErrorKind::UnknownRuntime("ts-fast".to_owned())
        );
        Ok(())
    }

    #[test]
    fn protocol_mismatch_is_rejected() -> TestResult {
        let temp = tempfile::tempdir()?;
        let manifest = manifest_json("ext", "ts-lean", "\"main.ts\"")
            .replace("\"protocolVersion\":1", "\"protocolVersion\":99");
        let dir = extension_dir(&temp, "ext", &manifest, &["main.ts"])?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(
            kind,
            ManifestErrorKind::ProtocolMismatch {
                found: 99,
                expected: pi_ext::protocol::PROTOCOL_VERSION,
            }
        );
        Ok(())
    }

    #[test]
    fn traversal_entry_is_rejected() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "ext",
            &manifest_json("ext", "native", "\"../escape\""),
            &[],
        )?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(
            kind,
            ManifestErrorKind::EntryEscapesRoot("../escape".to_owned())
        );
        Ok(())
    }

    #[test]
    fn dot_relative_string_entry_is_accepted() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "dot-relative",
            &manifest_json("dot-relative", "native", "\"./index.js\""),
            &["index.js"],
        )?;
        let classified = classify_extension(&dir, TARGET)?;
        assert_eq!(
            classified.entry,
            std::fs::canonicalize(dir.join("index.js"))?
        );
        Ok(())
    }

    #[test]
    fn dot_relative_platform_map_entry_is_accepted() -> TestResult {
        let temp = tempfile::tempdir()?;
        let entry = format!("{{\"{TARGET}\":\"./bin/extension\"}}");
        let dir = extension_dir(
            &temp,
            "dot-relative-map",
            &manifest_json("dot-relative-map", "native", &entry),
            &["bin/extension"],
        )?;
        let classified = classify_extension(&dir, TARGET)?;
        assert_eq!(
            classified.entry,
            std::fs::canonicalize(dir.join("bin/extension"))?
        );
        Ok(())
    }

    #[test]
    fn dot_relative_traversal_is_rejected() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "dot-relative-traversal",
            &manifest_json("dot-relative-traversal", "native", "\"./../escape\""),
            &[],
        )?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(
            kind,
            ManifestErrorKind::EntryEscapesRoot("./../escape".to_owned())
        );
        Ok(())
    }

    #[test]
    fn absolute_entry_is_rejected() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "ext",
            &manifest_json("ext", "native", "\"/etc/passwd\""),
            &[],
        )?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(
            kind,
            ManifestErrorKind::AbsoluteEntry("/etc/passwd".to_owned())
        );
        Ok(())
    }

    #[test]
    fn missing_entry_file_is_rejected() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "ext",
            &manifest_json("ext", "native", "\"bin/missing\""),
            &[],
        )?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(
            kind,
            ManifestErrorKind::EntryNotFound("bin/missing".to_owned())
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlink_entry_escaping_root_is_rejected() -> TestResult {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()?;
        let outside = temp.path().join("outside-bin");
        std::fs::write(&outside, b"#!/bin/sh\n")?;
        let dir = extension_dir(
            &temp,
            "ext",
            &manifest_json("ext", "native", "\"link\""),
            &[],
        )?;
        symlink(&outside, dir.join("link"))?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(kind, ManifestErrorKind::EntryEscapesRoot("link".to_owned()));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlink_entry_staying_inside_root_is_accepted() -> TestResult {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "ext",
            &manifest_json("ext", "native", "\"link\""),
            &["bin/tool"],
        )?;
        symlink(dir.join("bin/tool"), dir.join("link"))?;
        let classified = classify_extension(&dir, TARGET)?;
        assert_eq!(classified.mode, ExtensionMode::Native);
        assert_eq!(
            classified.entry,
            std::fs::canonicalize(dir.join("bin/tool"))?
        );
        Ok(())
    }

    #[test]
    fn missing_target_is_unsupported_platform_not_parse_corruption() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = extension_dir(
            &temp,
            "ext",
            &manifest_json(
                "ext",
                "native",
                "{\"aarch64-apple-darwin\":\"bin/mac-tool\"}",
            ),
            &["bin/mac-tool"],
        )?;
        let kind = expect_kind(classify_extension(&dir, TARGET))?;
        assert_eq!(
            kind,
            ManifestErrorKind::UnsupportedPlatform {
                target: TARGET.to_owned(),
                available: vec!["aarch64-apple-darwin".to_owned()],
            }
        );
        Ok(())
    }
}
