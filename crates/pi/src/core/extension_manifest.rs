//! Classifies discovered extension paths and validates directory manifests.

use std::fs;
use std::io;
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
    /// The canonical manifest entry is not a regular file.
    #[error("manifest entry is not a file: {path}")]
    EntryNotFile { path: PathBuf },
    /// Canonicalization showed that the entry leaves its manifest directory.
    #[error("manifest entry {entry} escapes extension directory {directory}")]
    EntryEscapesDirectory { entry: PathBuf, directory: PathBuf },
    /// A native extension lacks every Unix execute permission bit.
    #[cfg(unix)]
    #[error("native extension entry is not executable: {path}")]
    NativeEntryNotExecutable { path: PathBuf },
}

const MANIFEST_FILE_NAME: &str = "pi-extension.json";

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
pub fn classify(discovered: &str) -> Result<ClassifiedExtension, ManifestError> {
    let discovered_path = Path::new(discovered);
    let metadata = fs::metadata(discovered_path).map_err(|source| ManifestError::Inspect {
        path: discovered_path.to_path_buf(),
        source,
    })?;

    if metadata.is_file() {
        return Ok(compat(discovered));
    }
    if !metadata.is_dir() {
        return Err(ManifestError::UnsupportedPath {
            path: discovered_path.to_path_buf(),
        });
    }

    let manifest_path = discovered_path.join(MANIFEST_FILE_NAME);
    match fs::metadata(&manifest_path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(ManifestError::ManifestNotFile {
                path: manifest_path,
            });
        }
        Ok(_) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(compat(discovered)),
        Err(source) => {
            return Err(ManifestError::InspectManifest {
                path: manifest_path,
                source,
            });
        }
    }

    let manifest =
        fs::read_to_string(&manifest_path).map_err(|source| ManifestError::ReadManifest {
            path: manifest_path.clone(),
            source,
        })?;
    let manifest = serde_json::from_str::<DirectoryManifest>(&manifest).map_err(|source| {
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
        .map_err(|source| ManifestError::CanonicalizeEntry {
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

    Ok(ClassifiedExtension {
        runtime: manifest.runtime,
        discovered: discovered.to_owned(),
        entry: entry.to_string_lossy().into_owned(),
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
    !Path::new(entry).components().any(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    })
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

    let metadata = fs::metadata(path).map_err(|source| ManifestError::CanonicalizeEntry {
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

    #[cfg(windows)]
    #[test]
    fn manifest_rejects_drive_prefixed_entry() -> TestResult {
        let temp = tempdir()?;
        let extension = temp.path().join("extension");
        fs::create_dir(&extension)?;
        write_manifest(&extension, "ts-compat", "C:/outside.ts")?;

        assert!(matches!(
            classify(extension.to_str().ok_or("non-UTF-8 temp path")?),
            Err(ManifestError::UnsafeEntry { .. })
        ));
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
}
