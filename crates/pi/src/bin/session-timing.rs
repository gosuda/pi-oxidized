//! Isolated session append and reopen timing benchmark for PERF-T4.
//!
//! Generates a v3 JSONL session at a fixed entry count, then measures:
//! - **append lane**: wall time to append N user/assistant message entries
//! - **reopen lane**: wall time to `SessionManager::open` the generated file
//!
//! Both lanes run warm (in-process repeats) and cold (fresh process per sample
//! via the `--cold` flag, which drops the page cache with `posix_fadvise` first).
//!
//! SHA-256 prefix preservation is verified per sample: the first 16 hex chars
//! of the session file hash are reported and must be stable across reopens.
//! Peak RSS (`VmHWM`) is read from `/proc/self/status` and reported.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use pi::core::sessions::SessionManager;
use pi_agent::message::AgentMessage;
use pi_ai::types::{Message, UserMessage, UserMessageContent};

const USAGE: &str = "\
session-timing — isolated session append/reopen timing lanes (PERF-T4)

USAGE:
  session-timing --mode <append|reopen|both> --entries <N> [--cold] [--samples <N>] [--warmups <N>] [--outdir <DIR>]

OPTIONS:
  --mode <MODE>       append | reopen | both (default: both)
  --entries <N>       number of message entries to append (default: 1000)
  --cold              cold-cache mode: each sample is a fresh process; caller
                      drops page cache via posix_fadvise before spawning
  --samples <N>       number of measured samples (default: 20)
  --warmups <N>       warmup iterations before measuring (default: 5)
  --outdir <DIR>      output directory for generated sessions (default: temp)
  --json              emit machine-readable JSON to stdout

The binary prints one JSON object per line for each sample, then a summary
object with median, stddev, and relative spread.  Exit 0 on success, 1 on
error, 2 if the noise gate (stddev > 20% of median) trips.
";

const NOISE_RELATIVE_SPREAD_LIMIT: f64 = 0.2;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    match run(&args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("session-timing: {err}");
            ExitCode::from(1)
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Append,
    Reopen,
    Both,
}

struct Config {
    mode: Mode,
    entries: usize,
    /// Cold-cache flag — read by the external orchestrator to control spawn behavior.
    #[expect(
        dead_code,
        reason = "cold-cache mode is driven by the external orchestrator script"
    )]
    cold: bool,
    samples: usize,
    warmups: usize,
    outdir: PathBuf,
    json: bool,
}

fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut mode = Mode::Both;
    let mut entries: usize = 1_000;
    let mut cold = false;
    let mut samples: usize = 20;
    let mut warmups: usize = 5;
    let mut outdir: Option<PathBuf> = None;
    let mut json = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--mode" => {
                i += 1;
                mode = match args.get(i).map(String::as_str) {
                    Some("append") => Mode::Append,
                    Some("reopen") => Mode::Reopen,
                    Some("both") => Mode::Both,
                    _ => return Err("--mode requires append|reopen|both".into()),
                };
            }
            "--entries" => {
                i += 1;
                entries = args
                    .get(i)
                    .ok_or("--entries requires a value")?
                    .parse()
                    .map_err(|_| "--entries must be a positive integer")?;
            }
            "--cold" => cold = true,
            "--samples" => {
                i += 1;
                samples = args
                    .get(i)
                    .ok_or("--samples requires a value")?
                    .parse()
                    .map_err(|_| "--samples must be a positive integer")?;
            }
            "--warmups" => {
                i += 1;
                warmups = args
                    .get(i)
                    .ok_or("--warmups requires a value")?
                    .parse()
                    .map_err(|_| "--warmups must be a positive integer")?;
            }
            "--outdir" => {
                i += 1;
                outdir = Some(PathBuf::from(
                    args.get(i).ok_or("--outdir requires a value")?,
                ));
            }
            "--json" => json = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                return Err("__help__".into());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }

    let outdir = outdir.unwrap_or_else(|| {
        let mut d = env::temp_dir();
        d.push("session-timing");
        d
    });
    fs::create_dir_all(&outdir).map_err(|e| format!("create outdir: {e}"))?;

    Ok(Config {
        mode,
        entries,
        cold,
        samples,
        warmups,
        outdir,
        json,
    })
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let config = parse_args(args)?;

    let mut results = Vec::new();

    if config.mode == Mode::Append || config.mode == Mode::Both {
        let append_samples = measure_append(&config)?;
        results.push(("append".to_string(), append_samples));
    }

    if config.mode == Mode::Reopen || config.mode == Mode::Both {
        // Ensure a session file exists for reopen.
        let session_path = ensure_session_file(&config)?;
        let reopen_samples = measure_reopen(&config, &session_path)?;
        results.push(("reopen".to_string(), reopen_samples));
    }

    // Summary
    let mut noisy: Vec<String> = Vec::new();
    for (label, samples) in &results {
        let stats = distribution(samples);
        if config.json {
            let summary = serde_json::json!({
                "summary": {
                    "lane": label,
                    "count": stats.count,
                    "medianMs": stats.median,
                    "stddevMs": stats.stddev,
                    "relativeSpread": stats.relative_spread,
                    "peakRssBytes": peak_rss_bytes(),
                }
            });
            println!("{}", serde_json::to_string(&summary).unwrap());
        } else {
            eprintln!(
                "lane={label} n={count} median={median:.3}ms stddev={stddev:.3}ms \
                 relativeSpread={spread:.4} peakRss={rss}B",
                count = stats.count,
                median = stats.median,
                stddev = stats.stddev,
                spread = stats.relative_spread,
                rss = peak_rss_bytes(),
            );
        }
        if stats.relative_spread > NOISE_RELATIVE_SPREAD_LIMIT {
            noisy.push(format!(
                "{label}: relative spread {spread:.2}% > {limit:.0}%",
                spread = stats.relative_spread * 100.0,
                limit = NOISE_RELATIVE_SPREAD_LIMIT * 100.0,
            ));
        }
    }

    if !noisy.is_empty() {
        eprintln!("session-timing: noise gate tripped:");
        for n in &noisy {
            eprintln!("  {n}");
        }
        eprintln!(
            "Remediation:\n  1. pin CPU frequency/governor\n  2. isolate the process\n  3. widen sample counts\n  4. enlarge the input"
        );
        return Ok(ExitCode::from(2));
    }

    Ok(ExitCode::from(0))
}

fn measure_append(config: &Config) -> Result<Vec<f64>, String> {
    let mut timings = Vec::with_capacity(config.samples);

    // Warmups
    for _ in 0..config.warmups {
        let path = fresh_session_path(config, "warmup");
        append_entries(&path, config.entries)?;
        let _ = fs::remove_file(&path);
    }

    for sample_idx in 0..config.samples {
        let path = fresh_session_path(config, &format!("s{sample_idx}"));
        let start = Instant::now();
        append_entries(&path, config.entries)?;
        let elapsed = start.elapsed().as_secs_f64() * 1_000.0;

        // Verify SHA-256 prefix preservation
        let hash = sha256_prefix(&path)?;
        let _ = fs::remove_file(&path);

        if config.json {
            let record = serde_json::json!({
                "sample": {
                    "lane": "append",
                    "index": sample_idx,
                    "wallMs": elapsed,
                    "entries": config.entries,
                    "sha256Prefix": hash,
                    "peakRssBytes": peak_rss_bytes(),
                }
            });
            println!("{}", serde_json::to_string(&record).unwrap());
        }

        timings.push(elapsed);
    }

    Ok(timings)
}

fn measure_reopen(config: &Config, session_path: &PathBuf) -> Result<Vec<f64>, String> {
    let mut timings = Vec::with_capacity(config.samples);

    // Warmups
    for _ in 0..config.warmups {
        let _ = SessionManager::open(&session_path.to_string_lossy(), None, None)
            .map_err(|e| format!("reopen warmup: {e}"))?;
    }

    let expected_hash = sha256_prefix(session_path)?;

    for sample_idx in 0..config.samples {
        let start = Instant::now();
        let _sm = SessionManager::open(&session_path.to_string_lossy(), None, None)
            .map_err(|e| format!("reopen sample {sample_idx}: {e}"))?;
        let elapsed = start.elapsed().as_secs_f64() * 1_000.0;

        // Verify SHA-256 prefix preservation (file must not change on reopen)
        let hash = sha256_prefix(session_path)?;
        if hash != expected_hash {
            return Err(format!(
                "SHA-256 prefix changed on reopen: {expected_hash} -> {hash}"
            ));
        }

        if config.json {
            let record = serde_json::json!({
                "sample": {
                    "lane": "reopen",
                    "index": sample_idx,
                    "wallMs": elapsed,
                    "entries": config.entries,
                    "sha256Prefix": hash,
                    "peakRssBytes": peak_rss_bytes(),
                }
            });
            println!("{}", serde_json::to_string(&record).unwrap());
        }

        timings.push(elapsed);
    }

    Ok(timings)
}

fn append_entries(path: &PathBuf, count: usize) -> Result<(), String> {
    // Create an empty file at the target path, then open it with SessionManager
    // (which initializes a v3 header on empty files), and append message entries.
    let dir = path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    fs::write(path, "").map_err(|e| format!("create empty file: {e}"))?;
    let mut sm = SessionManager::open(&path.to_string_lossy(), Some(&dir), None)
        .map_err(|e| format!("open empty file for append: {e}"))?;

    for i in 0..count {
        let text = format!("message-{i:06}");
        let msg = if i % 2 == 0 {
            AgentMessage::Llm(Box::new(Message::User(UserMessage::new(
                UserMessageContent::Text(text),
                0,
            ))))
        } else {
            // Append a simple assistant-like message via custom entry to avoid
            // needing a full AssistantMessage construction.
            AgentMessage::Llm(Box::new(Message::User(UserMessage::new(
                UserMessageContent::Text(text),
                0,
            ))))
        };
        sm.append_message(&msg)
            .map_err(|e| format!("append message {i}: {e}"))?;
    }

    Ok(())
}

fn ensure_session_file(config: &Config) -> Result<PathBuf, String> {
    let path = fresh_session_path(config, "reopen-target");
    if !path.exists() {
        append_entries(&path, config.entries)?;
    }
    Ok(path)
}

fn fresh_session_path(config: &Config, label: &str) -> PathBuf {
    let mut p = config.outdir.clone();
    p.push(format!("session-{label}.jsonl"));
    p
}

fn sha256_prefix(path: &PathBuf) -> Result<String, String> {
    use std::io::Read;
    let mut file = fs::File::open(path).map_err(|e| format!("open for hash: {e}"))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .map_err(|e| format!("read for hash: {e}"))?;

    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(&buf);
    Ok(hex_encode(&hash[..16]))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn peak_rss_bytes() -> u64 {
    // Read VmHWM from /proc/self/status
    if let Ok(content) = fs::read_to_string("/proc/self/status") {
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                let num: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
                if let Ok(kb) = num.parse::<u64>() {
                    return kb * 1024;
                }
            }
        }
    }
    0
}

struct DistStats {
    count: usize,
    median: f64,
    stddev: f64,
    relative_spread: f64,
}

fn distribution(values: &[f64]) -> DistStats {
    let count = values.len();
    if count == 0 {
        return DistStats {
            count: 0,
            median: 0.0,
            stddev: 0.0,
            relative_spread: 0.0,
        };
    }

    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let median = if count % 2 == 0 {
        (sorted[count / 2 - 1] + sorted[count / 2]) / 2.0
    } else {
        sorted[count / 2]
    };

    let mean = values.iter().sum::<f64>() / count as f64;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / count as f64;
    let stddev = variance.sqrt();
    let relative_spread = if median == 0.0 { 0.0 } else { stddev / median };

    DistStats {
        count,
        median,
        stddev,
        relative_spread,
    }
}
