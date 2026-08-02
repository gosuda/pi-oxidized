//! Terminal capability detection and capability cache.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{self, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Preferred image protocol when images are supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    /// Kitty graphics protocol.
    Kitty,
    /// iTerm2 inline images.
    ITerm2,
}

/// Active terminal keyboard protocol.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardProtocol {
    /// Legacy terminal keyboard events.
    #[default]
    Legacy,
    /// Kitty progressive keyboard enhancement.
    Kitty,
}

/// Pixel dimensions of one character cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellDimensions {
    /// Cell width in pixels.
    pub width: u16,
    /// Cell height in pixels.
    pub height: u16,
}

impl Default for CellDimensions {
    fn default() -> Self {
        Self {
            width: 9,
            height: 18,
        }
    }
}

/// Cached terminal capabilities discovered at startup or reprobe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCapabilities {
    /// Preferred image protocol, if any.
    pub images: Option<ImageProtocol>,
    /// OSC 8 hyperlink support.
    pub hyperlinks: bool,
    /// Truecolor (24-bit) support.
    pub true_color: bool,
    /// DEC synchronized output (`CSI ? 2026`).
    pub sync_output: bool,
    /// Active keyboard protocol.
    pub keyboard_protocol: KeyboardProtocol,
    /// Cell pixel dimensions.
    pub cell: CellDimensions,
    /// Approximate background luminance when known (`true` = dark).
    pub dark_background: Option<bool>,
}

impl Default for TerminalCapabilities {
    fn default() -> Self {
        Self {
            images: None,
            hyperlinks: false,
            true_color: false,
            sync_output: true,
            keyboard_protocol: KeyboardProtocol::Legacy,
            cell: CellDimensions::default(),
            dark_background: None,
        }
    }
}

impl TerminalCapabilities {
    /// Detect capabilities from the process environment.
    ///
    /// Escape-based probes (Kitty flags, CSI 16t, OSC 11) refine this cache later.
    #[must_use]
    pub fn detect() -> Self {
        detect_with(|key| env::var(key).ok(), probe_tmux_hyperlinks)
    }

    /// Apply a Kitty keyboard probe result.
    pub fn set_kitty_keyboard(&mut self, enabled: bool) {
        self.keyboard_protocol = if enabled {
            KeyboardProtocol::Kitty
        } else {
            KeyboardProtocol::Legacy
        };
    }

    /// Whether Kitty progressive keyboard enhancement is active.
    #[must_use]
    pub fn kitty_keyboard(&self) -> bool {
        self.keyboard_protocol == KeyboardProtocol::Kitty
    }

    /// Apply cell dimensions from CSI 16 t.
    pub fn set_cell_dimensions(&mut self, width: u16, height: u16) {
        if width > 0 && height > 0 {
            self.cell = CellDimensions { width, height };
        }
    }

    /// Apply OSC 11 background classification.
    pub fn set_dark_background(&mut self, dark: Option<bool>) {
        self.dark_background = dark;
    }
}

/// Detect capabilities from an injected environment lookup and tmux
/// hyperlink probe.
///
/// The environment seam (`env`) maps a variable name to its value, allowing
/// tests to exercise every authority row without mutating the process
/// environment. The `tmux_forwards_hyperlink` probe is only consulted in
/// the tmux row.
fn detect_with<E, P>(env: E, tmux_forwards_hyperlink: P) -> TerminalCapabilities
where
    E: Fn(&str) -> Option<String>,
    P: Fn() -> bool,
{
    let mut caps = TerminalCapabilities::default();
    if env("PI_TUI_NO_SYNC").is_some_and(|v| {
        matches!(
            v.as_str(),
            "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
        )
    }) {
        caps.sync_output = false;
    }

    let term = env("TERM").map(|v| v.to_ascii_lowercase());
    let term_program = env("TERM_PROGRAM").map(|v| v.to_ascii_lowercase());
    let terminal_emulator = env("TERMINAL_EMULATOR").map(|v| v.to_ascii_lowercase());
    let colorterm = env("COLORTERM").map(|v| v.to_ascii_lowercase());
    let has_true_color_hint =
        colorterm.as_deref() == Some("truecolor") || colorterm.as_deref() == Some("24bit");

    // Authority order — first match wins (matches TS `detectCapabilities`).

    // 1. tmux: images off (unreliable under multiplexer), hyperlinks only
    //    when the tmux client forwards them.
    if has_marker(&env, "TMUX") || term.as_deref().is_some_and(|t| t.starts_with("tmux")) {
        caps.images = None;
        caps.true_color = has_true_color_hint;
        caps.hyperlinks = tmux_forwards_hyperlink();
        return caps;
    }

    // 2. screen: does not forward OSC 8 hyperlinks.
    if term.as_deref().is_some_and(|t| t.starts_with("screen")) {
        caps.images = None;
        caps.true_color = has_true_color_hint;
        caps.hyperlinks = false;
        return caps;
    }

    // 3. Kitty.
    if has_marker(&env, "KITTY_WINDOW_ID") || term_program.as_deref() == Some("kitty") {
        caps.images = Some(ImageProtocol::Kitty);
        caps.true_color = true;
        caps.hyperlinks = true;
        return caps;
    }

    // 4. Ghostty.
    if term_program.as_deref() == Some("ghostty")
        || term.as_deref().is_some_and(|t| t.contains("ghostty"))
        || has_marker(&env, "GHOSTTY_RESOURCES_DIR")
    {
        caps.images = Some(ImageProtocol::Kitty);
        caps.true_color = true;
        caps.hyperlinks = true;
        return caps;
    }

    // 5. WezTerm.
    if has_marker(&env, "WEZTERM_PANE") || term_program.as_deref() == Some("wezterm") {
        caps.images = Some(ImageProtocol::Kitty);
        caps.true_color = true;
        caps.hyperlinks = true;
        return caps;
    }

    // 6. Warp.
    if term_program.as_deref() == Some("warpterminal")
        || has_marker(&env, "WARP_SESSION_ID")
        || has_marker(&env, "WARP_TERMINAL_SESSION_UUID")
    {
        caps.images = Some(ImageProtocol::Kitty);
        caps.true_color = true;
        caps.hyperlinks = true;
        return caps;
    }

    // 7. iTerm2.
    if has_marker(&env, "ITERM_SESSION_ID") || term_program.as_deref() == Some("iterm.app") {
        caps.images = Some(ImageProtocol::ITerm2);
        caps.true_color = true;
        caps.hyperlinks = true;
        return caps;
    }

    // 8. Windows Terminal.
    if has_marker(&env, "WT_SESSION") {
        caps.images = None;
        caps.true_color = true;
        caps.hyperlinks = true;
        return caps;
    }

    // 9. VS Code.
    if term_program.as_deref() == Some("vscode") {
        caps.images = None;
        caps.true_color = true;
        caps.hyperlinks = true;
        return caps;
    }

    // 10. Alacritty.
    if term_program.as_deref() == Some("alacritty") {
        caps.images = None;
        caps.true_color = true;
        caps.hyperlinks = true;
        return caps;
    }

    // 11. JetBrains.
    if terminal_emulator.as_deref() == Some("jetbrains-jediterm") {
        caps.images = None;
        caps.true_color = true;
        caps.hyperlinks = false;
        return caps;
    }

    // 12. Unknown: be conservative. CMUX alone, VTE-only, Apple Terminal,
    //     and 256color TERM do not grant capabilities. Truecolor only from
    //     the COLORTERM hint.
    caps.images = None;
    caps.true_color = has_true_color_hint;
    caps.hyperlinks = false;
    caps
}

/// True when an environment marker has the same truthiness as JavaScript.
fn has_marker<E>(env: &E, key: &str) -> bool
where
    E: Fn(&str) -> Option<String>,
{
    env(key).is_some_and(|value| !value.is_empty())
}

const TMUX_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const TMUX_FEATURES_MAX_BYTES: u64 = 4096;
static NEXT_TMUX_PROBE_OUTPUT: AtomicU64 = AtomicU64::new(0);

fn create_tmux_probe_output() -> io::Result<(PathBuf, File)> {
    for _ in 0..16 {
        let path = env::temp_dir().join(format!(
            "pi-tui-tmux-probe-{}-{}",
            process::id(),
            NEXT_TMUX_PROBE_OUTPUT.fetch_add(1, Ordering::Relaxed),
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create tmux probe output file",
    ))
}

/// Probe whether the attached tmux client forwards OSC 8 hyperlinks.
///
/// tmux only re-emits them when its `client_termfeatures` lists `hyperlinks`.
/// Any spawn, exit-status, timeout, oversized output, or UTF-8 failure is
/// conservative and returns `false`. The output file prevents a descendant
/// retaining stdout from extending the probe past its deadline.
fn probe_tmux_hyperlinks() -> bool {
    let (output_path, output_file) = match create_tmux_probe_output() {
        Ok(output) => output,
        Err(_) => return false,
    };
    let mut child = match Command::new("tmux")
        .args(["display-message", "-p", "#{client_termfeatures}"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(output_file))
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            let _ = fs::remove_file(output_path);
            return false;
        }
    };

    let deadline = Instant::now() + TMUX_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };

    let result = match status {
        Some(status) if status.success() => {
            let mut output = Vec::new();
            File::open(&output_path)
                .and_then(|file| {
                    file.take(TMUX_FEATURES_MAX_BYTES + 1)
                        .read_to_end(&mut output)
                })
                .is_ok()
                && output.len() <= TMUX_FEATURES_MAX_BYTES as usize
                && String::from_utf8(output).is_ok_and(|features| {
                    features
                        .split(',')
                        .map(str::trim)
                        .any(|feature| feature == "hyperlinks")
                })
        }
        _ => false,
    };
    let _ = fs::remove_file(output_path);
    result
}

#[cfg(test)]
mod tests {
    use super::{ImageProtocol, TerminalCapabilities, detect_with};
    use std::collections::HashMap;

    /// Build an env-lookup closure from a slice of `(key, value)` pairs.
    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    // -- Authority row 12: unknown --

    #[test]
    fn unknown_terminal_defaults_conservative() {
        let caps = detect_with(env_from(&[]), || false);
        assert_eq!(caps.images, None);
        assert!(!caps.hyperlinks);
        assert!(!caps.true_color);
    }

    #[test]
    fn unknown_with_colorterm_truecolor_hint() {
        let caps = detect_with(env_from(&[("COLORTERM", "truecolor")]), || false);
        assert!(caps.true_color);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, None);
    }

    #[test]
    fn unknown_with_colorterm_24bit_hint() {
        let caps = detect_with(env_from(&[("COLORTERM", "24bit")]), || false);
        assert!(caps.true_color);
    }

    #[test]
    fn unknown_256color_term_does_not_grant_truecolor() {
        let caps = detect_with(env_from(&[("TERM", "xterm-256color")]), || false);
        assert!(!caps.true_color);
        assert!(!caps.hyperlinks);
    }

    #[test]
    fn vte_only_does_not_grant_hyperlinks() {
        let caps = detect_with(env_from(&[("VTE_VERSION", "6800")]), || false);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, None);
    }

    #[test]
    fn apple_terminal_stays_unknown() {
        let caps = detect_with(env_from(&[("TERM_PROGRAM", "Apple_Terminal")]), || false);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, None);
    }

    #[test]
    fn cmux_alone_stays_unknown() {
        let caps = detect_with(env_from(&[("CMUX_WORKSPACE_ID", "workspace")]), || false);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, None);
    }

    // -- Authority row 1: tmux --

    #[test]
    fn tmux_enables_hyperlinks_when_probe_true() {
        let caps = detect_with(
            env_from(&[
                ("TMUX", "/tmp/tmux-1000/default,1234,0"),
                ("TERM_PROGRAM", "ghostty"),
            ]),
            || true,
        );
        assert!(caps.hyperlinks);
        assert_eq!(caps.images, None);
    }

    #[test]
    fn tmux_disables_hyperlinks_when_probe_false() {
        let caps = detect_with(
            env_from(&[
                ("TMUX", "/tmp/tmux-1000/default,1234,0"),
                ("TERM_PROGRAM", "ghostty"),
            ]),
            || false,
        );
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, None);
    }

    #[test]
    fn tmux_via_term_prefix_checks_probe() {
        let env = env_from(&[("TERM", "tmux-256color"), ("TERM_PROGRAM", "iterm.app")]);
        let caps_true = detect_with(&env, || true);
        assert!(caps_true.hyperlinks);
        assert_eq!(caps_true.images, None);
        let caps_false = detect_with(&env, || false);
        assert!(!caps_false.hyperlinks);
    }

    #[test]
    fn tmux_does_not_inherit_wt_truecolor() {
        let caps = detect_with(
            env_from(&[
                ("WT_SESSION", "session"),
                ("TMUX", "/tmp/tmux-1000/default,1234,0"),
                ("TERM", "tmux-256color"),
            ]),
            || false,
        );
        assert!(!caps.true_color);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, None);
    }

    #[test]
    fn tmux_trusts_explicit_colorterm_hint() {
        let caps = detect_with(
            env_from(&[
                ("COLORTERM", "truecolor"),
                ("TMUX", "/tmp/tmux-1000/default,1234,0"),
                ("TERM", "tmux-256color"),
            ]),
            || false,
        );
        assert!(caps.true_color);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, None);
    }

    #[test]
    fn tmux_disables_warp_images() {
        let caps = detect_with(
            env_from(&[
                ("TERM_PROGRAM", "WarpTerminal"),
                ("TMUX", "/tmp/tmux-1000/default,1234,0"),
                ("TERM", "tmux-256color"),
            ]),
            || true,
        );
        assert_eq!(caps.images, None);
        assert!(caps.hyperlinks);
    }

    // -- Authority row 2: screen --

    #[test]
    fn screen_forces_hyperlinks_false() {
        let caps = detect_with(env_from(&[("TERM", "screen-256color")]), || false);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, None);
    }

    #[test]
    fn screen_truecolor_from_hint() {
        let caps = detect_with(
            env_from(&[("TERM", "screen-256color"), ("COLORTERM", "truecolor")]),
            || false,
        );
        assert!(caps.true_color);
        assert!(!caps.hyperlinks);
    }

    // -- Authority row 3: Kitty --

    #[test]
    fn kitty_via_window_id() {
        let caps = detect_with(env_from(&[("KITTY_WINDOW_ID", "1")]), || false);
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
        assert!(caps.hyperlinks);
        assert!(caps.true_color);
    }

    #[test]
    fn kitty_via_term_program() {
        let caps = detect_with(env_from(&[("TERM_PROGRAM", "kitty")]), || false);
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
        assert!(caps.hyperlinks);
    }

    // -- Authority row 4: Ghostty --

    #[test]
    fn ghostty_via_term_program() {
        let caps = detect_with(env_from(&[("TERM_PROGRAM", "ghostty")]), || false);
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
        assert!(caps.hyperlinks);
        assert!(caps.true_color);
    }

    #[test]
    fn ghostty_via_resources_dir() {
        let caps = detect_with(
            env_from(&[("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty")]),
            || false,
        );
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
        assert!(caps.hyperlinks);
    }

    #[test]
    fn ghostty_via_term_contains() {
        let caps = detect_with(env_from(&[("TERM", "xterm-ghostty")]), || false);
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
        assert!(caps.hyperlinks);
    }

    #[test]
    fn ghostty_plus_cmux_stays_kitty_capable() {
        let caps = detect_with(
            env_from(&[
                ("TERM_PROGRAM", "ghostty"),
                ("CMUX_WORKSPACE_ID", "workspace"),
            ]),
            || false,
        );
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
        assert!(caps.hyperlinks);
    }

    // -- Authority row 5: WezTerm --

    #[test]
    fn wezterm_via_pane() {
        let caps = detect_with(env_from(&[("WEZTERM_PANE", "0")]), || false);
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
        assert!(caps.hyperlinks);
    }

    #[test]
    fn wezterm_via_term_program() {
        let caps = detect_with(env_from(&[("TERM_PROGRAM", "wezterm")]), || false);
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
        assert!(caps.hyperlinks);
    }

    // -- Authority row 6: Warp --

    #[test]
    fn warp_via_term_program() {
        let caps = detect_with(env_from(&[("TERM_PROGRAM", "WarpTerminal")]), || false);
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
        assert!(caps.true_color);
        assert!(caps.hyperlinks);
    }

    #[test]
    fn warp_via_session_id() {
        let caps = detect_with(env_from(&[("WARP_SESSION_ID", "some-session-id")]), || {
            false
        });
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
        assert!(caps.hyperlinks);
    }

    #[test]
    fn warp_via_terminal_session_uuid() {
        let caps = detect_with(
            env_from(&[(
                "WARP_TERMINAL_SESSION_UUID",
                "d0e1a2e5-7ca7-44cd-9037-ac7222011161",
            )]),
            || false,
        );
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
        assert!(caps.hyperlinks);
    }

    // -- Authority row 7: iTerm2 --

    #[test]
    fn iterm2_via_session_id() {
        let caps = detect_with(env_from(&[("ITERM_SESSION_ID", "w0t0p1:12345")]), || false);
        assert_eq!(caps.images, Some(ImageProtocol::ITerm2));
        assert!(caps.hyperlinks);
        assert!(caps.true_color);
    }

    #[test]
    fn iterm2_via_term_program() {
        let caps = detect_with(env_from(&[("TERM_PROGRAM", "iterm.app")]), || false);
        assert_eq!(caps.images, Some(ImageProtocol::ITerm2));
        assert!(caps.hyperlinks);
    }

    // -- Authority row 8: Windows Terminal --

    #[test]
    fn windows_terminal_truecolor_and_hyperlinks() {
        let caps = detect_with(
            env_from(&[("WT_SESSION", "session"), ("TERM", "xterm-256color")]),
            || false,
        );
        assert!(caps.true_color);
        assert!(caps.hyperlinks);
        assert_eq!(caps.images, None);
    }

    // -- Authority row 9: VS Code --

    #[test]
    fn vscode_enables_hyperlinks() {
        let caps = detect_with(env_from(&[("TERM_PROGRAM", "vscode")]), || false);
        assert!(caps.hyperlinks);
        assert!(caps.true_color);
        assert_eq!(caps.images, None);
    }

    // -- Authority row 10: Alacritty --

    #[test]
    fn alacritty_enables_hyperlinks() {
        let caps = detect_with(env_from(&[("TERM_PROGRAM", "alacritty")]), || false);
        assert!(caps.hyperlinks);
        assert!(caps.true_color);
        assert_eq!(caps.images, None);
    }

    // -- Authority row 11: JetBrains --

    #[test]
    fn jetbrains_truecolor_without_hyperlinks() {
        let caps = detect_with(
            env_from(&[
                ("TERMINAL_EMULATOR", "JetBrains-JediTerm"),
                ("TERM", "xterm-256color"),
            ]),
            || false,
        );
        assert!(caps.true_color);
        assert!(!caps.hyperlinks);
        assert_eq!(caps.images, None);
    }

    // -- Authority ordering: earlier rows win --

    #[test]
    fn tmux_takes_precedence_over_kitty() {
        let caps = detect_with(
            env_from(&[
                ("TMUX", "/tmp/tmux-1000/default,1234,0"),
                ("KITTY_WINDOW_ID", "1"),
            ]),
            || false,
        );
        assert_eq!(caps.images, None);
        assert!(!caps.hyperlinks);
    }

    #[test]
    fn screen_takes_precedence_over_kitty() {
        let caps = detect_with(
            env_from(&[("TERM", "screen-256color"), ("KITTY_WINDOW_ID", "1")]),
            || false,
        );
        assert_eq!(caps.images, None);
        assert!(!caps.hyperlinks);
    }

    #[test]
    fn kitty_takes_precedence_over_iterm2() {
        let caps = detect_with(
            env_from(&[("KITTY_WINDOW_ID", "1"), ("TERM_PROGRAM", "iterm.app")]),
            || false,
        );
        assert_eq!(caps.images, Some(ImageProtocol::Kitty));
    }

    // -- sync_output behavior unchanged --

    #[test]
    fn default_sync_output_is_enabled() {
        let caps = TerminalCapabilities::default();
        assert!(caps.sync_output);
    }

    #[test]
    fn pi_tui_no_sync_disables_sync_output() {
        let caps = detect_with(env_from(&[("PI_TUI_NO_SYNC", "1")]), || false);
        assert!(!caps.sync_output);
    }

    #[test]
    fn empty_terminal_markers_do_not_grant_or_mask_capabilities() {
        let kitty = detect_with(env_from(&[("TMUX", ""), ("KITTY_WINDOW_ID", "1")]), || {
            panic!("empty TMUX must not run the tmux probe")
        });
        assert_eq!(kitty.images, Some(ImageProtocol::Kitty));

        for marker in [
            "TMUX",
            "KITTY_WINDOW_ID",
            "GHOSTTY_RESOURCES_DIR",
            "WEZTERM_PANE",
            "WARP_SESSION_ID",
            "WARP_TERMINAL_SESSION_UUID",
            "ITERM_SESSION_ID",
            "WT_SESSION",
        ] {
            assert_eq!(
                detect_with(env_from(&[(marker, "")]), || false),
                TerminalCapabilities::default(),
                "empty {marker} must be falsey"
            );
        }
    }
}
