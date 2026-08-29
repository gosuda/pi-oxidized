//! Validates schema-v1 transcript artifacts.
//!
//! Accepts either one `*.artifact.json` file or a directory that is searched
//! recursively for every `*.artifact.json`. Every candidate is reported; any
//! parse or validation failure yields a nonzero exit status. Empty matches are
//! failures — there is no silent skip.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pi_tui::testkit::validate::{ValidatorError, validate_bytes};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: pi_tui_transcript_validator <artifact.json|artifact-dir>");
        return ExitCode::from(2);
    };
    if args.next().is_some() {
        eprintln!("usage: pi_tui_transcript_validator <artifact.json|artifact-dir>");
        return ExitCode::from(2);
    }

    match run(Path::new(&path)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => ExitCode::from(code),
    }
}

fn run(path: &Path) -> Result<(), u8> {
    let artifacts = collect_artifacts(path).map_err(|message| {
        eprintln!("FAIL {path}: {message}", path = path.display());
        1
    })?;
    if artifacts.is_empty() {
        eprintln!("FAIL {}: no *.artifact.json files found", path.display());
        return Err(1);
    }

    let mut failed = false;
    for artifact_path in artifacts {
        match validate_path(&artifact_path) {
            Ok(()) => println!("PASS {}", artifact_path.display()),
            Err(error) => {
                eprintln!("FAIL {}: {error}", artifact_path.display());
                failed = true;
            }
        }
    }

    if failed { Err(1) } else { Ok(()) }
}

fn validate_path(path: &Path) -> Result<(), ValidatorError> {
    let bytes = fs::read(path).map_err(|error| ValidatorError::Parse(error.to_string()))?;
    validate_bytes(&bytes).map(|_| ())
}

fn collect_artifacts(path: &Path) -> Result<Vec<PathBuf>, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if metadata.is_file() {
        if is_artifact_file(path) {
            return Ok(vec![path.to_path_buf()]);
        }
        return Err(format!(
            "expected a *.artifact.json file, got {}",
            path.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "path is neither file nor directory: {}",
            path.display()
        ));
    }

    let mut artifacts = Vec::new();
    collect_dir(path, &mut artifacts)?;
    artifacts.sort();
    Ok(artifacts)
}

fn collect_dir(path: &Path, artifacts: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(path).map_err(|error| error.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let child = entry.path();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_dir() {
            collect_dir(&child, artifacts)?;
        } else if file_type.is_file() && is_artifact_file(&child) {
            artifacts.push(child);
        }
    }
    Ok(())
}

fn is_artifact_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".artifact.json"))
}
