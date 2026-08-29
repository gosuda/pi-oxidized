//! Windows-only witness phases; every diagnostic lands in the transcript,
//! the binary's `main` prints the returned summary.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use portable_pty::win::conpty::ConPtySystem;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, PtySystem};
use sonic_rs::json;

use crate::INPUT_LINE;
use crate::common::{
    Pump, Transcript, any_line_contains, count_seq, frame, spawn_pump, visible_text,
};
use crate::{
    COLS, CONHOST_EOF_DEADLINE, DSR_REPLY, PROBE_REPLY, ROWS, SETTLE_BOOT_DEADLINE,
    SETTLE_DEADLINE, SETTLE_IDLE,
};

/// Resize storm: shrink, grow, restore (content preservation each step).
const RESIZE_STORM: [(u16, u16); 3] = [(100, 28), (132, 40), (120, 30)];

struct Args {
    pi: PathBuf,
    out: PathBuf,
    expect_ready: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut pi = None;
    let mut out = PathBuf::from(".");
    let mut expect_ready = None;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--pi" => pi = Some(PathBuf::from(value)),
            "--out" => out = PathBuf::from(value),
            "--expect-ready" => expect_ready = Some(value),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let pi = pi.ok_or("--pi <path-to-pi.exe> is required")?;
    Ok(Args {
        pi,
        out,
        expect_ready,
    })
}

struct Capture {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_capture(program: &str, args: &[&str]) -> Capture {
    match Command::new(program).args(args).output() {
        Ok(out) => Capture {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
        Err(err) => Capture {
            code: -1,
            stdout: String::new(),
            stderr: err.to_string(),
        },
    }
}

/// Best-effort tree teardown for fatal paths after a successful spawn
/// (portable-pty 0.9.0 has no kill-on-Drop; see §3.8 of the record).
fn best_effort_taskkill(pid: u32) {
    if pid != 0 {
        let _ = run_capture("taskkill", &["/PID", &pid.to_string(), "/T", "/F"]);
    }
}

/// Runs the witness; returns `(exit_code, summary)`.
///
/// Exit codes: 0 all hard assertions pass; 1 hard assertion failures
/// (listed in the `verdict` event); 2 is reserved for the non-Windows stub;
/// 3 unexpected harness/PTY error (recorded as a `fatal` event).
pub fn run() -> (u8, String) {
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => return (3, format!("rel-r3: {err}")),
    };
    if let Err(err) = fs::create_dir_all(&args.out) {
        return (3, format!("rel-r3: --out dir: {err}"));
    }
    let mut transcript = match Transcript::create(&args.out.join("rel-r3-transcript.jsonl")) {
        Ok(tr) => tr,
        Err(err) => return (3, format!("rel-r3: transcript: {err}")),
    };
    let mut log: Vec<u8> = Vec::new();
    let mut hard: Vec<&'static str> = Vec::new();

    let _ = transcript.event(
        "environment",
        json!({
            "os": std::env::consts::OS,
            "pi": args.pi.display().to_string(),
            "cols": COLS,
            "rows": ROWS,
            "windows_ver": run_capture("cmd", &["/c", "ver"]).stdout.trim(),
        }),
    );

    // Phase 0: pi --version against the unpacked archive.
    let version = run_capture(&args.pi.display().to_string(), &["--version"]);
    let version_ok = version.code == 0 && !version.stdout.trim().is_empty();
    if !version_ok {
        hard.push("version_probe");
    }
    let _ = transcript.event(
        "version_probe",
        json!({
            "exit": version.code,
            "stdout": version.stdout.trim(),
            "stderr": Transcript::head(version.stderr.as_bytes(), 400),
            "pass": version_ok,
        }),
    );

    // Phase 1: ConPTY spawn at 120x30.
    let cwd = args
        .pi
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let spawned =
        (|| -> Result<(Box<dyn MasterPty + Send>, Box<dyn Child + Send + Sync>, u32), String> {
            let system = ConPtySystem::default();
            let pair = system
                .openpty(PtySize {
                    rows: ROWS,
                    cols: COLS,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|err| format!("openpty: {err}"))?;
            let mut cmd = CommandBuilder::new(&args.pi);
            cmd.cwd(&cwd);
            cmd.env("TERM", "xterm-256color");
            let child = pair
                .slave
                .spawn_command(cmd)
                .map_err(|err| format!("spawn_command: {err}"))?;
            drop(pair.slave);
            let pid = child.process_id().unwrap_or(0);
            Ok((pair.master, child, pid))
        })();

    let (master, mut child, pid) = match spawned {
        Ok(parts) => parts,
        Err(err) => {
            let _ = transcript.event("fatal", json!({"phase": "spawn", "error": err.clone()}));
            return (3, format!("rel-r3: {err}"));
        }
    };

    // Post-spawn wiring. Any failure here must still tear the live child
    // down: portable-pty 0.9.0's WinChild has no kill-on-Drop (its Drop
    // only closes the handle), so a bare return would leak pi.exe and its
    // sidecar on the runner.
    let wiring = (|| -> Result<(Box<dyn Write + Send>, Pump), String> {
        let mut writer = master
            .take_writer()
            .map_err(|err| format!("take_writer: {err}"))?;
        let reader = master
            .try_clone_reader()
            .map_err(|err| format!("try_clone_reader: {err}"))?;
        let pump = spawn_pump(reader).map_err(|err| format!("spawn_pump: {err}"))?;
        // Probe reply first: answers the INHERIT_CURSOR DSR (its trailing
        // CSI R) so conhost resumes processing input, and denies DEC 2026
        // (ConhostVtDec2026Fallback) so pi never emits synchronized wrappers.
        writer
            .write_all(PROBE_REPLY)
            .map_err(|err| format!("probe reply: {err}"))?;
        Ok((writer, pump))
    })();
    let (mut writer, mut pump) = match wiring {
        Ok(parts) => parts,
        Err(err) => {
            best_effort_taskkill(pid);
            let _ = child.wait();
            let _ = transcript.event("fatal", json!({"phase": "wiring", "error": err.clone()}));
            return (3, format!("rel-r3: {err}"));
        }
    };
    let _ = transcript.event(
        "spawn",
        json!({
            "pid": pid,
            "cols": COLS,
            "rows": ROWS,
            "cwd": cwd.display().to_string(),
            "probe_reply_bytes": PROBE_REPLY.len(),
            "driver": "portable-pty 0.9.0 ConPtySystem",
        }),
    );

    // Boot settle; observe conhost DSR and reply again if it is coalesced
    // after the probe reply was written.
    let (boot, eof) = pump.settle(SETTLE_IDLE, SETTLE_BOOT_DEADLINE);
    log.extend_from_slice(&boot);
    let dsr_observed = boot.windows(4).any(|w| w == b"\x1b[6n");
    if dsr_observed {
        let _ = writer.write_all(DSR_REPLY);
    }
    let _ = transcript.event(
        "output",
        json!({
            "phase": "boot",
            "len": boot.len(),
            "eof": eof,
            "dsr_observed": dsr_observed,
            "head": Transcript::head(&boot, 300),
        }),
    );

    // Advisory observations on the pre-input stream.
    let alt_enter = count_seq(&log, b"\x1b[?1049h");
    let clears_pre = count_seq(&log, b"\x1b[2J") + count_seq(&log, b"\x1b[3J");
    let sync_pre = count_seq(&log, b"\x1b[?2026h") + count_seq(&log, b"\x1b[?2026l");
    let _ = transcript.event(
        "observation",
        json!({
            "alt_buffer_enter": alt_enter,
            "clear_sequences_pre_input": clears_pre,
            "sync_2026_markers_pre_input": sync_pre,
        }),
    );

    // Hard: the archive TUI rendered a non-empty frame at 120x30.
    let boot_frame = frame(&log, COLS, ROWS);
    let boot_rendered = !visible_text(&boot_frame).is_empty();
    if !boot_rendered {
        hard.push("boot_render");
    }
    let boot_text = visible_text(&boot_frame);
    if let Some(marker) = args.expect_ready.as_deref() {
        let ready = boot_text.contains(marker);
        if !ready {
            hard.push("ready_marker");
        }
        let _ = transcript.event(
            "frame",
            json!({
                "phase": "boot",
                "rendered": boot_rendered,
                "ready_marker": marker,
                "ready_pass": ready,
                "text": Transcript::head(boot_text.as_bytes(), 1200),
            }),
        );
    } else {
        let _ = transcript.event(
            "frame",
            json!({
                "phase": "boot",
                "rendered": boot_rendered,
                "text": Transcript::head(boot_text.as_bytes(), 1200),
            }),
        );
    }

    // Phase 2: scripted input echo, asserted on the decoded frame.
    let _ = writer.write_all(INPUT_LINE.as_bytes());
    let _ = transcript.event(
        "input",
        json!({"len": INPUT_LINE.len(), "text": INPUT_LINE}),
    );
    let (echo_batch, eof) = pump.settle(SETTLE_IDLE, SETTLE_DEADLINE);
    log.extend_from_slice(&echo_batch);
    let echo_frame = frame(&log, COLS, ROWS);
    let echo_present = any_line_contains(&echo_frame, INPUT_LINE);
    if !echo_present {
        hard.push("input_echo");
    }
    let _ = transcript.event(
        "frame",
        json!({
            "phase": "echo",
            "echo_present": echo_present,
            "eof": eof,
            "len": echo_batch.len(),
            "text": Transcript::head(visible_text(&echo_frame).as_bytes(), 1200),
        }),
    );

    // Enter key delivery (recorded; no response assertion).
    let _ = writer.write_all(b"\r");
    let _ = transcript.event("input", json!({"bytes": 1, "text": "\\r"}));
    let (enter_batch, _) = pump.settle(SETTLE_IDLE, SETTLE_DEADLINE);
    log.extend_from_slice(&enter_batch);

    // No-clear hard assertion is scoped to the pre-resize stream: after a
    // resize the renderer may inject repaint bytes itself (conhost-derived).
    let clears_before_resize = count_seq(&log, b"\x1b[2J") + count_seq(&log, b"\x1b[3J");
    if clears_before_resize > clears_pre {
        hard.push("no_clear_pre_resize");
    }
    let _ = transcript.event(
        "no_clear_scope",
        json!({
            "clears_boot_baseline": clears_pre,
            "clears_before_resize": clears_before_resize,
            "asserted_delta": true,
        }),
    );

    // Phase 3: resize storm with content preservation.
    for &(cols, rows) in &RESIZE_STORM {
        if let Err(err) = master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            best_effort_taskkill(pid);
            let _ = child.wait();
            let _ = transcript.event(
                "fatal",
                json!({"phase": "resize", "cols": cols, "rows": rows, "error": err.to_string()}),
            );
            return (3, format!("rel-r3: resize {cols}x{rows}: {err}"));
        }
        let (batch, _) = pump.settle(SETTLE_IDLE, SETTLE_DEADLINE);
        log.extend_from_slice(&batch);
        let storm_frame = frame(&log, cols, rows);
        let preserved = any_line_contains(&storm_frame, INPUT_LINE);
        if !preserved {
            hard.push("resize_content");
        }
        let _ = transcript.event(
            "resize",
            json!({
                "cols": cols,
                "rows": rows,
                "batch_len": batch.len(),
                "marker_preserved": preserved,
                "clears_in_window": count_seq(&batch, b"\x1b[2J") + count_seq(&batch, b"\x1b[3J"),
            }),
        );
    }

    // Phase 4: teardown. Tree snapshot (advisory), then taskkill /T /F.
    let pid_text = pid.to_string();
    let tree = run_capture(
        "powershell",
        &[
            "-NoProfile",
            "-Command",
            &format!(
                "Get-CimInstance Win32_Process -Filter 'ParentProcessId={pid}' | \
                 Select-Object ProcessId,Name,ParentProcessId | ConvertTo-Json -Compress"
            ),
        ],
    );
    let _ = transcript.event(
        "tree_snapshot",
        json!({"stdout": tree.stdout.trim(), "stderr": Transcript::head(tree.stderr.as_bytes(), 200)}),
    );

    let kill = run_capture("taskkill", &["/PID", pid_text.as_str(), "/T", "/F"]);
    let kill_ok = kill.code == 0;
    if !kill_ok {
        hard.push("taskkill_exit");
    }
    let _ = transcript.event(
        "taskkill",
        json!({
            "argv": format!("taskkill /PID {pid} /T /F"),
            "exit": kill.code,
            "stdout": kill.stdout.trim(),
            "stderr": kill.stderr.trim(),
        }),
    );

    let exit_code = if kill_ok {
        child.wait().ok().map(|status| status.exit_code())
    } else {
        // taskkill already failed; WinChild::wait blocks INFINITE, so the
        // failure stays observable instead of hanging the witness.
        None
    };
    let _ = transcript.event(
        "child_exit",
        json!({"exit_code": exit_code, "waited": kill_ok}),
    );

    let listing = run_capture("tasklist", &["/FI", &format!("PID eq {pid}"), "/NH"]);
    let pid_gone = !listing
        .stdout
        .split_whitespace()
        .any(|token| token == pid_text.as_str());
    if !pid_gone {
        hard.push("pid_survived_taskkill");
    }
    let _ = transcript.event(
        "tasklist_check",
        json!({"stdout": listing.stdout.trim(), "pid_gone": pid_gone}),
    );

    // conhost is a sibling of pi.exe (a child of this harness), not part of
    // pi.exe's taskkill tree: dropping writer + master runs
    // ClosePseudoConsole and the reference-counted server pipe closes; EOF
    // on the reader proves conhost was reaped.
    let _ = writer.flush();
    drop(writer);
    drop(master);
    let (trailing, conhost_eof) = pump.wait_eof(CONHOST_EOF_DEADLINE);
    log.extend_from_slice(&trailing);
    pump.join();
    if !conhost_eof {
        hard.push("conhost_eof");
    }
    let _ = transcript.event(
        "conhost_reap",
        json!({"eof": conhost_eof, "trailing_len": trailing.len()}),
    );

    let final_sync = count_seq(&log, b"\x1b[?2026h") + count_seq(&log, b"\x1b[?2026l");
    let final_alt_leave = count_seq(&log, b"\x1b[?1049l");
    let _ = fs::write(args.out.join("rel-r3-raw-output.bin"), &log);

    // DEC2026-fallback contract: pi must never emit 2026 wrappers under the
    // ConhostVtDec2026Fallback profile; any marker is a hard failure (§3.6).
    if final_sync > 0 {
        hard.push("sync_2026_markers");
    }
    if transcript.io_failed() {
        hard.push("transcript_io");
    }

    let hard_failures: sonic_rs::Array = hard
        .iter()
        .map(|name| sonic_rs::Value::from(*name))
        .collect();
    let pass = hard.is_empty();
    let _ = transcript.event(
        "verdict",
        json!({
            "pass": pass,
            "hard_failures": hard_failures,
            "advisory": {
                "alt_buffer_enter": alt_enter,
                "alt_buffer_leave": final_alt_leave,
                "sync_2026_markers_total": final_sync,
                "dsr_observed": dsr_observed,
            },
            "deferred_to": "REL-T7 (issue #114) windows-latest execution",
        }),
    );

    let summary = if pass {
        format!(
            "rel-r3 PASS pid={pid} boot_render={boot_rendered} echo={echo_present} \
             resize_preserved teardown=taskkill+eof pid_gone={pid_gone} \
             (advisory: alt={alt_enter} sync2026={final_sync})"
        )
    } else {
        format!("rel-r3 FAIL hard_failures={}", hard.join(","))
    };
    (u8::from(!pass), summary)
}
