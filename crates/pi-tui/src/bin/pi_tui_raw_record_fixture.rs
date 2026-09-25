//! `pi_tui_raw_record_fixture` — Windows `ConPTY` raw input-record witness.
//!
//! Child half of the `windows_raw_input_records_vt_mode_ab` test in
//! `tests/pty_no_flicker.rs`. The parent spawns this plain binary under a
//! private `ConPTY` rather than re-invoking the test executable: a libtest
//! binary under `ConPTY` never reached the test body on the runner (the
//! transcript carried only console-mode noise), so the witness child lives
//! here as a normal process entry point.
//!
//! Contract: sets the console input mode per `PI_TUI_RAW_RECORD_ARM`
//! (`B` adds `ENABLE_VIRTUAL_TERMINAL_INPUT`), emits lifecycle/record/
//! termination events as OSC 999 lines, and restores the original mode on
//! every exit path through `ModeGuard`.
#[cfg(windows)]
mod imp {

    use std::cell::Cell;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    use crossterm_winapi::{
        Console, ConsoleMode, ControlKeyState, EventFlags, Handle, InputRecord,
    };
    use serde::Serialize;

    const NOT_RAW_MASK: u32 = 0x0007;
    const VT_INPUT: u32 = 0x0200;
    const CHILD_RECORD_LIMIT: usize = 4096;
    const CHILD_POLL_INTERVAL_MS: u64 = 5;
    const DEFAULT_CHILD_DEADLINE_MS: u64 = 15000;

    const RECORD_PREFIX: &[u8] = b"\x1b]999;PI_TUI_RAW_RECORD=";
    const READY_PREFIX: &[u8] = b"\x1b]999;PI_TUI_RAW_RECORD_READY";

    // The wire shapes below mirror the Deserialize side in
    // tests/pty_no_flicker.rs; the parent parses these exact field names.
    #[derive(Clone, Debug, Default, Serialize)]
    struct RecordEntry {
        idx: usize,
        variant: String,
        key_down: Option<bool>,
        repeat_count: Option<u16>,
        virtual_key_code: Option<u16>,
        virtual_scan_code: Option<u16>,
        u_char: Option<u16>,
        control_key_state: Option<u32>,
        mouse_x: Option<i16>,
        mouse_y: Option<i16>,
        button_state: Option<i32>,
        mouse_control_key_state: Option<u32>,
        event_flags: Option<u32>,
        observed_screen_x: Option<i16>,
        observed_screen_y: Option<i16>,
        focus_set: Option<bool>,
        menu_command_id: Option<u32>,
    }

    #[derive(Clone, Debug, Serialize)]
    struct LifecycleEntry {
        stage: String,
        arm: String,
        original: u32,
        baseline: u32,
        requested: u32,
        active: Option<u32>,
        restored: Option<u32>,
        error: Option<String>,
    }

    #[derive(Clone, Debug, Serialize)]
    struct TerminationEntry {
        cause: String,
        record_count: usize,
        message: Option<String>,
    }

    #[derive(Clone, Debug, Serialize)]
    #[serde(tag = "type")]
    enum Event {
        #[serde(rename = "record")]
        Record(RecordEntry),
        #[serde(rename = "lifecycle")]
        Lifecycle(LifecycleEntry),
        #[serde(rename = "termination")]
        Termination(TerminationEntry),
    }

    struct ModeGuard {
        cm: ConsoleMode,
        original: u32,
        restored: Cell<bool>,
    }

    impl ModeGuard {
        fn new(cm: ConsoleMode, original: u32) -> Self {
            Self {
                cm,
                original,
                restored: Cell::new(false),
            }
        }

        fn set(&self, mode: u32) -> io::Result<()> {
            self.cm.set_mode(mode)
        }

        fn mode(&self) -> io::Result<u32> {
            self.cm.mode()
        }

        fn restore(&self) -> io::Result<u32> {
            if self.restored.get() {
                return self.cm.mode();
            }
            self.cm.set_mode(self.original)?;
            let m = self.cm.mode()?;
            self.restored.set(true);
            Ok(m)
        }
    }

    impl Drop for ModeGuard {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    fn control_key_state_value(state: ControlKeyState) -> u32 {
        (0..32).fold(0u32, |acc, bit| {
            let mask = 1u32 << bit;
            if state.has_state(mask) {
                acc | mask
            } else {
                acc
            }
        })
    }

    fn event_flags_value(flags: EventFlags) -> u32 {
        match flags {
            EventFlags::PressOrRelease => 0x0000,
            EventFlags::MouseMoved => 0x0001,
            EventFlags::DoubleClick => 0x0002,
            EventFlags::MouseWheeled => 0x0004,
            EventFlags::MouseHwheeled => 0x0008,
            EventFlags::Unknown => 0x0021,
        }
    }

    fn convert_record(record: InputRecord, idx: usize) -> RecordEntry {
        match record {
            InputRecord::KeyEvent(k) => RecordEntry {
                idx,
                variant: "KeyEvent".into(),
                key_down: Some(k.key_down),
                repeat_count: Some(k.repeat_count),
                virtual_key_code: Some(k.virtual_key_code),
                virtual_scan_code: Some(k.virtual_scan_code),
                u_char: Some(k.u_char),
                control_key_state: Some(control_key_state_value(k.control_key_state)),
                ..RecordEntry::default()
            },
            InputRecord::MouseEvent(m) => RecordEntry {
                idx,
                variant: "MouseEvent".into(),
                mouse_x: Some(m.mouse_position.x),
                mouse_y: Some(m.mouse_position.y),
                button_state: Some(m.button_state.state()),
                mouse_control_key_state: Some(control_key_state_value(m.control_key_state)),
                event_flags: Some(event_flags_value(m.event_flags)),
                ..RecordEntry::default()
            },
            InputRecord::WindowBufferSizeEvent(w) => RecordEntry {
                idx,
                variant: "WindowBufferSizeEvent".into(),
                // crossterm_winapi 0.9.1 overwrites the record's dwSize with
                // the live screen-buffer size at read time; these are observed
                // screen values, not the record's original coordinates.
                observed_screen_x: Some(w.size.x),
                observed_screen_y: Some(w.size.y),
                ..RecordEntry::default()
            },
            InputRecord::FocusEvent(f) => RecordEntry {
                idx,
                variant: "FocusEvent".into(),
                focus_set: Some(f.set_focus),
                ..RecordEntry::default()
            },
            InputRecord::MenuEvent(m) => RecordEntry {
                idx,
                variant: "MenuEvent".into(),
                menu_command_id: Some(m.command_id),
                ..RecordEntry::default()
            },
        }
    }

    fn is_terminator(entry: &RecordEntry) -> bool {
        entry.variant == "KeyEvent" && entry.key_down == Some(true) && entry.u_char == Some(0x0004)
    }

    /// Console-side writer: `CONOUT$` under ConPTY, falling back to the
    /// standard output handle. Writing to `stdout()` proved lossy on the CI
    /// runner (a spawned child observed zero bytes reaching the ConPTY
    /// master while console-mode writes succeeded), so the witness channel
    /// opens the real console output device first.
    fn console_writer() -> &'static Mutex<Box<dyn Write + Send>> {
        static WRITER: OnceLock<Mutex<Box<dyn Write + Send>>> = OnceLock::new();
        WRITER.get_or_init(|| {
            let conout = OpenOptions::new()
                .read(true)
                .write(true)
                .open("CONOUT$")
                .map(|f| Box::new(f) as Box<dyn Write + Send>);
            Mutex::new(conout.unwrap_or_else(|_| Box::new(io::stdout())))
        })
    }

    /// The evidence file is the authoritative channel: every emitted line —
    /// stage markers and OSC-999 event payloads alike — lands here before it
    /// is mirrored to the console. The parent parses this file when the
    /// transcript carries no event bytes, so a completely dead console
    /// channel still yields a complete arm report.
    fn stage_log() -> Option<&'static Mutex<File>> {
        static LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();
        LOG.get_or_init(|| {
            let path = std::env::var_os("PI_TUI_RAW_RECORD_STAGE_LOG").map(PathBuf::from)?;
            File::create(path).ok().map(Mutex::new)
        })
        .as_ref()
    }

    /// Emit one line to the evidence file, then mirror it to the console.
    /// Both writes are best-effort: a broken console channel must never
    /// panic the witness before its evidence reaches disk.
    fn emit_line(body: &[u8]) {
        if let Some(log) = stage_log()
            && let Ok(mut f) = log.lock()
        {
            let _ = f.write_all(body);
            let _ = f.flush();
        }
        if let Ok(mut out) = console_writer().lock() {
            let _ = out.write_all(body);
            let _ = out.flush();
        }
    }

    fn write_osc999_line(prefix: &[u8], body: &[u8]) {
        let mut line = Vec::with_capacity(prefix.len() + body.len() + 1);
        line.extend_from_slice(prefix);
        line.extend_from_slice(body);
        line.push(0x07);
        emit_line(&line);
    }

    fn stage(name: &str) {
        let mut line = Vec::with_capacity(name.len() + 26);
        line.extend_from_slice(b"PI_TUI_RAW_RECORD_STAGE=");
        line.extend_from_slice(name.as_bytes());
        line.push(b'\n');
        emit_line(&line);
    }

    fn emit_event(event: &Event, prefix: &[u8]) {
        let json = serde_json::to_string(event).expect("serialize event");
        write_osc999_line(prefix, json.as_bytes());
    }

    fn emit_ready(arm: &str, active: u32) {
        let body = format!("=1;arm={arm};active={active}");
        write_osc999_line(READY_PREFIX, body.as_bytes());
    }

    fn emit_lifecycle_and_finish(
        arm: &str,
        original: u32,
        baseline: u32,
        requested: u32,
        guard: &ModeGuard,
        termination: TerminationEntry,
    ) {
        let mut termination = termination;
        let mut errors: Vec<String> = Vec::new();

        let restored = match guard.restore() {
            Ok(m) => Some(m),
            Err(e) => {
                errors.push(format!("restore failed: {e}"));
                None
            }
        };

        // The post-restore mode is observed evidence: a failed read is
        // recorded as an error and reported as `None`, never fabricated
        // as a mode word.
        let active = match guard.mode() {
            Ok(m) => Some(m),
            Err(e) => {
                errors.push(format!("read post-restore mode failed: {e}"));
                None
            }
        };

        if let Some(m) = restored
            && m != original
        {
            errors.push(format!("restored {m:#06x} != original {original:#06x}"));
        }

        let error = if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        };
        if let Some(e) = &error {
            termination.message = Some(match termination.message.take() {
                Some(prev) => format!("{prev}; {e}"),
                None => e.clone(),
            });
        }

        emit_event(
            &Event::Lifecycle(LifecycleEntry {
                stage: "teardown".into(),
                arm: arm.into(),
                original,
                baseline,
                requested,
                active,
                restored,
                error,
            }),
            RECORD_PREFIX,
        );
        emit_event(&Event::Termination(termination), RECORD_PREFIX);
    }

    pub(crate) fn main() {
        // Plain-text stage markers: unlike the OSC 999 event channel these
        // always pass ConPTY untransformed, so the last marker in the
        // transcript localizes any stall to a single stage.
        stage("entry");
        let arm = std::env::var("PI_TUI_RAW_RECORD_ARM")
            .unwrap_or_else(|e| panic!("PI_TUI_RAW_RECORD_ARM must be set to A, B, or IDLE: {e}"));

        assert!(
            matches!(arm.as_str(), "A" | "B" | "IDLE"),
            "PI_TUI_RAW_RECORD_ARM must be A, B, or IDLE, got {arm}"
        );

        let deadline_ms: u64 = match std::env::var("PI_TUI_RAW_RECORD_DEADLINE_MS") {
            Ok(s) => s.parse().unwrap_or_else(|e| {
                panic!("PI_TUI_RAW_RECORD_DEADLINE_MS {s:?} is not a u64: {e}")
            }),
            Err(std::env::VarError::NotPresent) => DEFAULT_CHILD_DEADLINE_MS,
            Err(e) => panic!("PI_TUI_RAW_RECORD_DEADLINE_MS is not readable: {e}"),
        };
        let deadline = Duration::from_millis(deadline_ms);

        stage("in_handle");
        let in_handle = match Handle::current_in_handle() {
            Ok(h) => h,
            Err(e) => {
                emit_event(
                    &Event::Termination(TerminationEntry {
                        cause: "error".into(),
                        record_count: 0,
                        message: Some(format!("current_in_handle failed: {e}")),
                    }),
                    RECORD_PREFIX,
                );
                return;
            }
        };

        stage("mode_read");
        let cm = ConsoleMode::from(in_handle.clone());
        let console = Console::from(in_handle);

        let original = match cm.mode() {
            Ok(m) => m,
            Err(e) => {
                emit_event(
                    &Event::Termination(TerminationEntry {
                        cause: "error".into(),
                        record_count: 0,
                        message: Some(format!("read original mode failed: {e}")),
                    }),
                    RECORD_PREFIX,
                );
                return;
            }
        };

        let baseline = original & !NOT_RAW_MASK;
        let requested = match arm.as_str() {
            "B" => baseline | VT_INPUT,
            _ => baseline,
        };

        let guard = ModeGuard::new(cm, original);

        let mut termination = TerminationEntry {
            cause: "inconclusive".into(),
            record_count: 0,
            message: None,
        };

        stage("set_mode");
        if let Err(e) = guard.set(requested) {
            termination.cause = "error".into();
            termination.message = Some(format!("set_mode({requested:#06x}) failed: {e}"));
            emit_lifecycle_and_finish(&arm, original, baseline, requested, &guard, termination);
            return;
        }

        let active = match guard.mode() {
            Ok(m) => m,
            Err(e) => {
                termination.cause = "error".into();
                termination.message = Some(format!("read active mode failed: {e}"));
                emit_lifecycle_and_finish(&arm, original, baseline, requested, &guard, termination);
                return;
            }
        };

        stage("setup_emit");
        emit_event(
            &Event::Lifecycle(LifecycleEntry {
                stage: "setup".into(),
                arm: arm.clone(),
                original,
                baseline,
                requested,
                active: Some(active),
                restored: None,
                error: if active == requested {
                    None
                } else {
                    Some(format!(
                        "active {active:#06x} != requested {requested:#06x}"
                    ))
                },
            }),
            RECORD_PREFIX,
        );

        if active != requested {
            termination.cause = "inconclusive".into();
            termination.message = Some(format!(
                "mode setup mismatch: active {active:#06x} != requested {requested:#06x}"
            ));
            emit_lifecycle_and_finish(&arm, original, baseline, requested, &guard, termination);
            return;
        }

        stage("ready_emit");
        emit_ready(&arm, active);
        emit_event(
            &Event::Lifecycle(LifecycleEntry {
                stage: "ready".into(),
                arm: arm.clone(),
                original,
                baseline,
                requested,
                active: Some(active),
                restored: None,
                error: None,
            }),
            RECORD_PREFIX,
        );

        stage("read_loop");
        let started = Instant::now();
        let mut record_count: usize = 0;
        while started.elapsed() < deadline {
            let count = match console.number_of_console_input_events() {
                Ok(0) => {
                    thread::sleep(Duration::from_millis(CHILD_POLL_INTERVAL_MS));
                    continue;
                }
                Ok(c) => c,
                Err(e) => {
                    termination.cause = "error".into();
                    termination.message =
                        Some(format!("number_of_console_input_events failed: {e}"));
                    break;
                }
            };

            for _ in 0..count {
                if started.elapsed() >= deadline {
                    break;
                }
                if record_count >= CHILD_RECORD_LIMIT {
                    termination.cause = "record_limit".into();
                    termination.message =
                        Some(format!("reached record limit {CHILD_RECORD_LIMIT}"));
                    break;
                }

                match console.read_single_input_event() {
                    Ok(record) => {
                        record_count += 1;
                        let entry = convert_record(record, record_count);
                        let term = is_terminator(&entry);
                        emit_event(&Event::Record(entry), RECORD_PREFIX);
                        if term {
                            termination.cause = "normal".into();
                            termination.message =
                                Some(format!("saw Ctrl+D terminator at record {record_count}"));
                            break;
                        }
                    }
                    Err(e) => {
                        termination.cause = "error".into();
                        termination.message = Some(format!("read_single_input_event failed: {e}"));
                        break;
                    }
                }
            }

            if !matches!(termination.cause.as_str(), "inconclusive") {
                break;
            }

            thread::sleep(Duration::from_millis(CHILD_POLL_INTERVAL_MS));
        }

        if termination.cause == "inconclusive" {
            termination.cause = "deadline".into();
            termination.message = Some(format!(
                "reached child deadline {} ms",
                deadline.as_millis()
            ));
        }
        termination.record_count = record_count;

        stage("finish");
        emit_lifecycle_and_finish(&arm, original, baseline, requested, &guard, termination);
        stage("done");
    }
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("pi_tui_raw_record_fixture is a Windows ConPTY witness child");
    std::process::exit(2);
}
