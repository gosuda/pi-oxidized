//! Classifies discovered extension paths and validates directory manifests.

use std::fs;
use std::fs::File;
use std::io;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// The host selected for an extension entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ExtensionRuntime {
    /// Load through the compatibility TypeScript host.
    TsCompat,
    /// Spawn directly as a native executable.
    Native,
}

/// A discovered extension entry after any directory manifest has been applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClassifiedExtension {
    /// The runtime selected by the manifest, or compatibility by default.
    pub runtime: ExtensionRuntime,
    /// Discovery string retained verbatim for ordering and diagnostics.
    pub discovered: String,
    /// Validated path passed to the selected host.
    pub entry: String,
}

/// Failure while inspecting or validating an extension manifest.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// The discovered path could not be inspected.
    #[error("could not inspect extension {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The discovered path is not a usable extension source.
    #[error("extension {path} is neither a file nor a directory")]
    UnsupportedPath { path: PathBuf },
    /// The directory's manifest path could not be inspected.
    #[error("could not inspect extension manifest {path}: {source}")]
    InspectManifest {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The manifest path exists but is not a regular file.
    #[error("extension manifest {path} is not a file")]
    ManifestNotFile { path: PathBuf },
    /// The manifest file could not be read.
    #[error("could not read extension manifest {path}: {source}")]
    ReadManifest {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The manifest file exceeds the maximum allowed size.
    #[error(
        "extension manifest {path} is too large: {actual_size} bytes exceeds the {limit}-byte limit"
    )]
    ManifestTooLarge {
        path: PathBuf,
        actual_size: u64,
        limit: u64,
    },
    /// The manifest is not valid under the strict schema.
    #[error("invalid extension manifest {path}: {source}")]
    ParseManifest {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// The manifest directory could not be canonicalized.
    #[error("could not canonicalize extension directory {path}: {source}")]
    CanonicalizeDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The manifest entry uses an absolute, prefix, or parent component.
    #[error("manifest entry must be a relative path without parent components: {entry}")]
    UnsafeEntry { entry: String },
    /// The requested manifest entry does not exist.
    #[error("manifest entry does not exist: {path}")]
    MissingEntry { path: PathBuf },
    /// The requested manifest entry could not be canonicalized.
    #[error("could not canonicalize manifest entry {path}: {source}")]
    CanonicalizeEntry {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The canonical manifest entry could not be inspected.
    #[error("could not inspect manifest entry {path}: {source}")]
    InspectEntry {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The canonical manifest entry is not a regular file.
    #[error("manifest entry is not a file: {path}")]
    EntryNotFile { path: PathBuf },
    /// Canonicalization showed that the entry leaves its manifest directory.
    #[error("manifest entry {entry} escapes extension directory {directory}")]
    EntryEscapesDirectory { entry: PathBuf, directory: PathBuf },
    /// The canonical manifest entry is not valid UTF-8.
    #[error("manifest entry is not valid UTF-8: {path}")]
    EntryNotUtf8 { path: PathBuf },
    /// A native extension lacks every Unix execute permission bit.
    #[cfg(unix)]
    #[error("native extension entry is not executable: {path}")]
    NativeEntryNotExecutable { path: PathBuf },
    /// A prebundled `.mjs` file cannot be loaded through the TypeScript-compat
    /// host (Mode 1). Prebundled ESM modules require the lean runner (Mode 2),
    /// which is not yet wired through the Rust endpoint planner. Rejecting
    /// `.mjs` here prevents accidental Mode 1 classification (M21).
    #[error("prebundled .mjs extension requires lean runner routing, not Mode 1 compat: {path}")]
    UnsupportedPrebundledMjs { path: PathBuf },
}

const MANIFEST_FILE_NAME: &str = "pi-extension.json";
const MANIFEST_BYTE_LIMIT: u64 = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryManifest {
    #[serde(rename = "$schema")]
    _schema: Option<String>,
    runtime: ExtensionRuntime,
    entry: String,
}

/// Classify one ordered discovery result.
///
/// A file and a directory without [`MANIFEST_FILE_NAME`] retain the discovery
/// string exactly, preserving the compatibility extension path contract.
/// Prebundled `.mjs` files are rejected: they require lean-runner (Mode 2)
/// routing, not the TypeScript-compat host (Mode 1). This prevents accidental
/// Mode 1 classification of prebundled ESM modules (M21 witness).
pub fn classify(discovered: &str) -> Result<ClassifiedExtension, ManifestError> {
    let discovered_path = Path::new(discovered);
    let metadata = fs::metadata(discovered_path).map_err(|source| ManifestError::Inspect {
        path: discovered_path.to_path_buf(),
        source,
    })?;

    if metadata.is_file() {
        if discovered_path.extension().is_some_and(|ext| ext == "mjs") {
            return Err(ManifestError::UnsupportedPrebundledMjs {
                path: discovered_path.to_path_buf(),
            });
        }
        return Ok(compat(discovered));
    }
    if !metadata.is_dir() {
        return Err(ManifestError::UnsupportedPath {
            path: discovered_path.to_path_buf(),
        });
    }

    let manifest_path = discovered_path.join(MANIFEST_FILE_NAME);
    let metadata = match fs::metadata(&manifest_path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(ManifestError::ManifestNotFile {
                path: manifest_path,
            });
        }
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(compat(discovered)),
        Err(source) => {
            return Err(ManifestError::InspectManifest {
                path: manifest_path,
                source,
            });
        }
    };

    let manifest_bytes = read_manifest_bounded(&manifest_path, metadata.len())?;
    let manifest =
        serde_json::from_slice::<DirectoryManifest>(&manifest_bytes).map_err(|source| {
            ManifestError::ParseManifest {
                path: manifest_path,
                source,
            }
        })?;

    if !is_safe_relative_entry(&manifest.entry) {
        return Err(ManifestError::UnsafeEntry {
            entry: manifest.entry,
        });
    }

    let directory = fs::canonicalize(discovered_path).map_err(|source| {
        ManifestError::CanonicalizeDirectory {
            path: discovered_path.to_path_buf(),
            source,
        }
    })?;
    let requested_entry = directory.join(&manifest.entry);
    let entry = canonicalize_entry(&requested_entry)?;

    if !entry.starts_with(&directory) {
        return Err(ManifestError::EntryEscapesDirectory { entry, directory });
    }

    if !fs::metadata(&entry)
        .map_err(|source| ManifestError::InspectEntry {
            path: entry.clone(),
            source,
        })?
        .is_file()
    {
        return Err(ManifestError::EntryNotFile { path: entry });
    }

    #[cfg(unix)]
    if manifest.runtime == ExtensionRuntime::Native && !is_executable(&entry)? {
        return Err(ManifestError::NativeEntryNotExecutable { path: entry });
    }

    let entry =
        entry
            .into_os_string()
            .into_string()
            .map_err(|os_string| ManifestError::EntryNotUtf8 {
                path: os_string.into(),
            })?;

    Ok(ClassifiedExtension {
        runtime: manifest.runtime,
        discovered: discovered.to_owned(),
        entry,
    })
}

fn compat(discovered: &str) -> ClassifiedExtension {
    ClassifiedExtension {
        runtime: ExtensionRuntime::TsCompat,
        discovered: discovered.to_owned(),
        entry: discovered.to_owned(),
    }
}

fn is_safe_relative_entry(entry: &str) -> bool {
    // Reject Windows absolute and separator forms on every host so a manifest
    // cannot smuggle drive letters, UNC shares, or backslash separators past
    // the component check on targets where `Path` does not parse them.
    if entry.contains('\\') {
        return false;
    }
    let bytes = entry.as_bytes();
    let has_drive_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if has_drive_prefix || entry.starts_with("//") {
        return false;
    }
    !Path::new(entry).components().any(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    })
}

fn read_manifest_bounded(path: &Path, declared_len: u64) -> Result<Vec<u8>, ManifestError> {
    if declared_len > MANIFEST_BYTE_LIMIT {
        return Err(ManifestError::ManifestTooLarge {
            path: path.to_path_buf(),
            actual_size: declared_len,
            limit: MANIFEST_BYTE_LIMIT,
        });
    }
    let mut file = File::open(path).map_err(|source| ManifestError::ReadManifest {
        path: path.to_path_buf(),
        source,
    })?;
    let mut buffer = Vec::new();
    file.by_ref()
        .take(MANIFEST_BYTE_LIMIT + 1)
        .read_to_end(&mut buffer)
        .map_err(|source| ManifestError::ReadManifest {
            path: path.to_path_buf(),
            source,
        })?;
    let actual_size = buffer.len() as u64;
    if actual_size > MANIFEST_BYTE_LIMIT {
        return Err(ManifestError::ManifestTooLarge {
            path: path.to_path_buf(),
            actual_size,
            limit: MANIFEST_BYTE_LIMIT,
        });
    }
    Ok(buffer)
}

fn canonicalize_entry(path: &Path) -> Result<PathBuf, ManifestError> {
    fs::canonicalize(path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ManifestError::MissingEntry {
                path: path.to_path_buf(),
            }
        } else {
            ManifestError::CanonicalizeEntry {
                path: path.to_path_buf(),
                source,
            }
        }
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> Result<bool, ManifestError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path).map_err(|source| ManifestError::InspectEntry {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;

    type TestResult = Result<(), Box<dyn Error>>;

    fn write_manifest(directory: &Path, runtime: &str, entry: &str) -> Result<(), io::Error> {
        fs::write(
            directory.join(MANIFEST_FILE_NAME),
            format!(r#"{{"$schema":"ignored","runtime":"{runtime}","entry":"{entry}"}}"#),
        )
    }

    #[test]
    fn manifestless_file_and_directory_preserve_discovery_bytes() -> TestResult {
        let temp = tempdir()?;
        let file = temp.path().join("plain.ts");
        let directory = temp.path().join("directory");
        fs::write(&file, "")?;
        fs::create_dir(&directory)?;

        for discovered in [
            format!("{}/./plain.ts", temp.path().display()),
            format!("{}/./directory", temp.path().display()),
        ] {
            assert_eq!(
                classify(&discovered)?,
                ClassifiedExtension {
                    runtime: ExtensionRuntime::TsCompat,
                    discovered: discovered.clone(),
                    entry: discovered,
                }
            );
        }
        Ok(())
    }

    #[test]
    fn strict_manifest_accepts_schema_and_canonicalizes_entry() -> TestResult {
        let temp = tempdir()?;
        let extension = temp.path().join("extension");
        fs::create_dir(&extension)?;
        let entry = extension.join("plugin.ts");
        fs::write(&entry, "")?;
        write_manifest(&extension, "ts-compat", "plugin.ts")?;

        assert_eq!(
            classify(extension.to_str().ok_or("non-UTF-8 temp path")?)?,
            ClassifiedExtension {
                runtime: ExtensionRuntime::TsCompat,
                discovered: extension.to_string_lossy().into_owned(),
                entry: fs::canonicalize(&entry)?.to_string_lossy().into_owned(),
            }
        );

        fs::write(
            extension.join(MANIFEST_FILE_NAME),
            r#"{"runtime":"ts-compat","entry":"plugin.ts"}"#,
        )?;
        assert_eq!(
            classify(extension.to_str().ok_or("non-UTF-8 temp path")?)?.runtime,
            ExtensionRuntime::TsCompat
        );
        Ok(())
    }

    #[test]
    fn strict_manifest_rejects_unknown_and_missing_required_fields() -> TestResult {
        let temp = tempdir()?;
        let extension = temp.path().join("extension");
        fs::create_dir(&extension)?;
        fs::write(extension.join("plugin.ts"), "")?;

        for manifest in [
            r#"{"runtime":"ts-compat","entry":"plugin.ts","extra":true}"#,
            r#"{"$schema":1,"runtime":"ts-compat","entry":"plugin.ts"}"#,
            r#"{"runtime":"unknown","entry":"plugin.ts"}"#,
            r#"{"runtime":"ts-lean","entry":"plugin.ts"}"#,
            r#"{"entry":"plugin.ts"}"#,
            r#"{"runtime":"ts-compat"}"#,
        ] {
            fs::write(extension.join(MANIFEST_FILE_NAME), manifest)?;
            assert!(matches!(
                classify(extension.to_str().ok_or("non-UTF-8 temp path")?),
                Err(ManifestError::ParseManifest { .. })
            ));
        }
        Ok(())
    }

    #[test]
    fn manifest_rejects_unsafe_and_missing_entries() -> TestResult {
        let temp = tempdir()?;
        let extension = temp.path().join("extension");
        fs::create_dir(&extension)?;
        let discovered = extension.to_str().ok_or("non-UTF-8 temp path")?;

        for entry in ["../outside.ts", "/outside.ts"] {
            write_manifest(&extension, "ts-compat", entry)?;
            assert!(matches!(
                classify(discovered),
                Err(ManifestError::UnsafeEntry { .. })
            ));
        }

        write_manifest(&extension, "ts-compat", "missing.ts")?;
        assert!(matches!(
            classify(discovered),
            Err(ManifestError::MissingEntry { .. })
        ));
        Ok(())
    }

    #[test]
    fn manifest_rejects_directory_entries() -> TestResult {
        let temp = tempdir()?;
        let extension = temp.path().join("extension");
        fs::create_dir(&extension)?;
        fs::create_dir(extension.join("not-a-file"))?;
        let discovered = extension.to_str().ok_or("non-UTF-8 temp path")?;

        for runtime in ["ts-compat", "native"] {
            write_manifest(&extension, runtime, "not-a-file")?;
            assert!(matches!(
                classify(discovered),
                Err(ManifestError::EntryNotFile { .. })
            ));
        }
        Ok(())
    }

    #[test]
    fn manifest_rejects_windows_absolute_entries_on_every_host() -> TestResult {
        let temp = tempdir()?;
        let extension = temp.path().join("extension");
        fs::create_dir(&extension)?;
        let discovered = extension.to_str().ok_or("non-UTF-8 temp path")?;

        // Each entry is written as valid JSON with backslashes properly escaped
        // so the rejection comes from `is_safe_relative_entry`, not the parser.
        let cases: [(&str, &str); 5] = [
            (
                "C:/outside.ts",
                r#"{"runtime":"ts-compat","entry":"C:/outside.ts"}"#,
            ),
            (
                "C:outside.ts",
                r#"{"runtime":"ts-compat","entry":"C:outside.ts"}"#,
            ),
            (
                r"C:\outside.ts",
                r#"{"runtime":"ts-compat","entry":"C:\\outside.ts"}"#,
            ),
            (
                r"\outside.ts",
                r#"{"runtime":"ts-compat","entry":"\\outside.ts"}"#,
            ),
            (
                r"sub\plugin.ts",
                r#"{"runtime":"ts-compat","entry":"sub\\plugin.ts"}"#,
            ),
        ];
        for (entry, manifest) in cases {
            fs::write(extension.join(MANIFEST_FILE_NAME), manifest)?;
            assert!(
                matches!(classify(discovered), Err(ManifestError::UnsafeEntry { .. })),
                "entry {entry:?} should be rejected as unsafe"
            );
        }

        // UNC shares expressed with forward slashes are also rejected.
        fs::write(
            extension.join(MANIFEST_FILE_NAME),
            r#"{"runtime":"ts-compat","entry":"//server/share/plugin.ts"}"#,
        )?;
        assert!(matches!(
            classify(discovered),
            Err(ManifestError::UnsafeEntry { .. })
        ));
        Ok(())
    }

    #[test]
    fn manifest_rejects_oversize_file() -> TestResult {
        let temp = tempdir()?;
        let extension = temp.path().join("extension");
        fs::create_dir(&extension)?;
        fs::write(extension.join("plugin.ts"), "")?;

        let base = r#"{"runtime":"ts-compat","entry":"plugin.ts"}"#;
        let limit = usize::try_from(MANIFEST_BYTE_LIMIT)?;
        let padding = limit + 1 - base.len();
        fs::write(
            extension.join(MANIFEST_FILE_NAME),
            format!("{}{}", base, " ".repeat(padding)),
        )?;

        let discovered = extension.to_str().ok_or("non-UTF-8 temp path")?;
        let result = classify(discovered);
        let Err(ManifestError::ManifestTooLarge {
            actual_size, limit, ..
        }) = result
        else {
            return Err(format!("expected ManifestTooLarge, got {result:?}").into());
        };
        assert_eq!(limit, MANIFEST_BYTE_LIMIT);
        assert_eq!(actual_size, MANIFEST_BYTE_LIMIT + 1);
        Ok(())
    }

    #[test]
    fn manifest_accepts_up_to_byte_limit() -> TestResult {
        let temp = tempdir()?;
        let extension = temp.path().join("extension");
        fs::create_dir(&extension)?;
        fs::write(extension.join("plugin.ts"), "")?;

        let base = r#"{"runtime":"ts-compat","entry":"plugin.ts"}"#;
        let limit = usize::try_from(MANIFEST_BYTE_LIMIT)?;
        let padding = limit - base.len();
        let bounded = format!("{}{}", base, " ".repeat(padding));
        assert_eq!(bounded.len(), limit);
        fs::write(extension.join(MANIFEST_FILE_NAME), &bounded)?;

        let discovered = extension.to_str().ok_or("non-UTF-8 temp path")?;
        assert_eq!(classify(discovered)?.runtime, ExtensionRuntime::TsCompat);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn manifest_rejects_symlink_escape() -> TestResult {
        use std::os::unix::fs::symlink;

        let temp = tempdir()?;
        let extension = temp.path().join("extension");
        let outside = temp.path().join("outside.ts");
        fs::create_dir(&extension)?;
        fs::write(&outside, "")?;
        symlink(&outside, extension.join("plugin.ts"))?;
        write_manifest(&extension, "ts-compat", "plugin.ts")?;

        assert!(matches!(
            classify(extension.to_str().ok_or("non-UTF-8 temp path")?),
            Err(ManifestError::EntryEscapesDirectory { .. })
        ));
        Ok(())
    }

    #[cfg(not(unix))]
    #[test]
    fn native_manifest_defers_executability_to_spawn() -> TestResult {
        let temp = tempdir()?;
        let extension = temp.path().join("extension");
        fs::create_dir(&extension)?;
        fs::write(extension.join("plugin"), "")?;
        write_manifest(&extension, "native", "plugin")?;

        assert_eq!(
            classify(extension.to_str().ok_or("non-UTF-8 temp path")?)?.runtime,
            ExtensionRuntime::Native
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn native_manifest_requires_an_executable_file() -> TestResult {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir()?;
        let extension = temp.path().join("extension");
        fs::create_dir(&extension)?;
        let entry = extension.join("plugin");
        fs::write(&entry, "")?;
        write_manifest(&extension, "native", "plugin")?;
        let discovered = extension.to_str().ok_or("non-UTF-8 temp path")?;

        assert!(matches!(
            classify(discovered),
            Err(ManifestError::NativeEntryNotExecutable { .. })
        ));

        fs::set_permissions(&entry, fs::Permissions::from_mode(0o100))?;
        assert_eq!(classify(discovered)?.runtime, ExtensionRuntime::Native);
        Ok(())
    }

    #[test]
    fn manifest_rejects_directory_manifest_path() -> TestResult {
        let temp = tempdir()?;
        let extension = temp.path().join("extension");
        fs::create_dir(&extension)?;
        // The manifest path exists but is a directory, not a regular file.
        fs::create_dir(extension.join(MANIFEST_FILE_NAME))?;
        let discovered = extension.to_str().ok_or("non-UTF-8 temp path")?;
        assert!(matches!(
            classify(discovered),
            Err(ManifestError::ManifestNotFile { .. })
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn manifest_rejects_non_utf8_canonical_entry() -> TestResult {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        let temp = tempdir()?;
        let extension = temp.path().join("extension");
        fs::create_dir(&extension)?;

        // Create a subdirectory whose name is invalid UTF-8 (0xFF). A symlink
        // with a UTF-8 name inside the extension directory points at a file
        // inside it, so canonicalization resolves to a non-UTF-8 path that is
        // still within the extension directory (passes the escape check).
        let non_utf8_dir = extension.join(OsString::from_vec(vec![0xFF]));
        fs::create_dir(&non_utf8_dir)?;
        let target = non_utf8_dir.join("plugin.ts");
        fs::write(&target, "")?;
        symlink(&target, extension.join("link.ts"))?;
        write_manifest(&extension, "ts-compat", "link.ts")?;

        let discovered = extension.to_str().ok_or("non-UTF-8 temp path")?;
        assert!(matches!(
            classify(discovered),
            Err(ManifestError::EntryNotUtf8 { .. })
        ));
        Ok(())
    }

    #[test]
    fn inspect_entry_error_classifies_distinctly_from_canonicalize() {
        // `InspectEntry` is a defensive variant for metadata failures on an
        // already-canonicalized entry. In practice `fs::canonicalize` stats
        // the target, so a metadata call immediately after cannot fail without
        // a TOCTOU race. Verify the variant is wired with a distinct label so
        // metadata inspection failures are never misreported as canonicalization
        // failures.
        let err = ManifestError::InspectEntry {
            path: Path::new("/tmp/plugin.ts").to_path_buf(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("could not inspect manifest entry"),
            "InspectEntry message should describe inspection, got: {msg}"
        );
        assert!(
            !msg.contains("could not canonicalize"),
            "InspectEntry must not be labeled as canonicalization, got: {msg}"
        );
    }

    // ──────────────────────────────────────────────────────────────────
    // XC-9 / M21: classification witness — .mjs not Mode 1 compat
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn m21_prebundled_mjs_rejected_not_ts_compat() -> TestResult {
        let temp = tempdir()?;
        let mjs = temp.path().join("prebundled.mjs");
        fs::write(&mjs, "export default {}")?;
        let discovered = mjs.to_str().ok_or("non-UTF-8 temp path")?;
        match classify(discovered) {
            Err(ManifestError::UnsupportedPrebundledMjs { .. }) => Ok(()),
            other => Err(format!("expected UnsupportedPrebundledMjs, got {other:?}").into()),
        }
    }

    #[test]
    fn m21_ts_and_js_files_remain_ts_compat() -> TestResult {
        let temp = tempdir()?;
        for ext in ["plugin.ts", "plugin.js"] {
            let file = temp.path().join(ext);
            fs::write(&file, "export default {}")?;
            let discovered = file.to_str().ok_or("non-UTF-8 temp path")?;
            assert_eq!(classify(discovered)?.runtime, ExtensionRuntime::TsCompat, "{ext} must be TsCompat");
        }
        Ok(())
    }

}
