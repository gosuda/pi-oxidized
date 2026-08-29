//! REL-R3 throwaway harness: Windows ConPTY interaction witness (issue #115).
//!
//! Recipe (windows-latest, wired by REL-T7 / #114):
//!
//! ```text
//! 1. Build + unpack the x86_64-pc-windows-msvc release archive.
//! 2. cargo run --release --manifest-path prototype/rel-r3-conpty/Cargo.toml -- \
//!      --pi <unpacked-dir>/pi.exe --out rel-r3-evidence [--expect-ready <substr>]
//! 3. Require process exit 0; upload the --out directory:
//!      rel-r3-transcript.jsonl  one JSON object per line: {"seq","t_ms","kind",...}
//!      rel-r3-raw-output.bin    full raw master-side ConPTY byte stream
//! ```
//! Phases: `pi --version` probe; ConPTY spawn at 120x30 with
//! `TERM=xterm-256color` and the testkit's `ConhostVtDec2026Fallback` probe
//! reply (which also answers the `PSEUDOCONSOLE_INHERIT_CURSOR` DSR that
//! otherwise blocks all input processing); scripted input echo asserted on the
//! avt-decoded frame; resize storm 100x28 / 132x40 / 120x30 with content
//! preservation; teardown via `taskkill /PID <pid> /T /F` plus
//! `ClosePseudoConsole` (master drop) with a reader-EOF assertion for the
//! conhost sibling.
//!
//! Hard assertions gate the exit code (exit 1 lists them in the `verdict`
//! event). Advisory observations (alt-buffer bytes, DEC-2026 markers,
//! conhost-injected clears after resize) are recorded but never asserted,
//! because they are conhost-build dependent: the master-side stream is
//! conhost-derived, not the child's raw bytes. Rationale and primary sources:
//! `docs/REL-R3-conpty-witness-prototype.md`.

use std::time::Duration;

// Host check on Linux cannot see the windows witness using these helpers.
#[cfg_attr(not(windows), allow(dead_code))]
mod common;

/// Nominal geometry for the witness (issue #115).
pub const COLS: u16 = 120;
pub const ROWS: u16 = 30;

/// Idle window that ends an output settle (testkit quiescence style).
/// 800 ms — raised from 400 ms to avoid mid-burst settle on loaded runners.
pub const SETTLE_IDLE: Duration = Duration::from_millis(800);
/// Deadline for the boot settle (ConPTY first paint can lag the spawn).
pub const SETTLE_BOOT_DEADLINE: Duration = Duration::from_secs(20);
/// Deadline for later settles.
pub const SETTLE_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline for conhost EOF after `ClosePseudoConsole`.
pub const CONHOST_EOF_DEADLINE: Duration = Duration::from_secs(10);

/// The scripted input line echoed back by pi's composer.
pub const INPUT_LINE: &str = "hello conpty witness";

/// The testkit `ConhostVtDec2026Fallback` probe reply (testkit/profile.rs):
/// denies kitty-keyboard and DA1 with the conhost variants, answers the
/// window-size and OSC-11 probes, and the trailing CSI R doubles as the
/// reply to conhost's INHERIT_CURSOR cursor-position DSR.
#[cfg(windows)]
const PROBE_REPLY: &[u8] =
    b"\x1b[?0u\x1b[?1;0c\x1b[6;10;20t\x1b]11;rgb:0000/0000/0000\x07\x1b[1;1R";

/// Standalone reply if the DSR is observed in the first output batch.
#[cfg(windows)]
const DSR_REPLY: &[u8] = b"\x1b[1;1R";

fn main() -> std::process::ExitCode {
    #[cfg(windows)]
    {
        let (code, summary) = witness::run();
        println!("{summary}");
        std::process::ExitCode::from(code)
    }
    #[cfg(not(windows))]
    {
        eprintln!(
            "rel-r3-conpty-witness: windows-only harness; this host cannot drive ConPTY. \
             Run on windows-latest (REL-T7 / issue #114 wires the CI leg). \
             Source-derived behavior record: docs/REL-R3-conpty-witness-prototype.md"
        );
        std::process::ExitCode::from(2)
    }
}

#[cfg(windows)]
mod witness;

#[cfg(test)]
mod tests {
    use super::common::{count_seq, frame};

    #[test]
    fn counts_clear_and_mode_sequences() {
        let hay: &[u8] = b"\x1b[2Jmid\x1b[?2026h\x1b[?2026l\x1b[3J\x1b[?1049h";
        assert_eq!(count_seq(hay, b"\x1b[2J"), 1);
        assert_eq!(count_seq(hay, b"\x1b[3J"), 1);
        assert_eq!(count_seq(hay, b"\x1b[?2026h"), 1);
        assert_eq!(count_seq(hay, b"\x1b[?2026l"), 1);
        assert_eq!(count_seq(hay, b"\x1b[?1049h"), 1);
        assert_eq!(count_seq(b"plain", b"\x1b[2J"), 0);
    }

    #[test]
    fn frame_decode_finds_echo() {
        let lines = frame(b"\x1b[?1049hhello conpty witness", 40, 6);
        assert!(lines.iter().any(|l| l.contains("hello conpty witness")));
    }
}
