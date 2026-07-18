//! Terminal capability detection and capability cache.

use std::env;

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
        let mut caps = Self::default();
        if env_truthy("PI_TUI_NO_SYNC") {
            caps.sync_output = false;
        }

        let term = env_lower("TERM");
        let term_program = env_lower("TERM_PROGRAM");
        let colorterm = env_lower("COLORTERM");
        let wt_session = env::var_os("WT_SESSION").is_some();
        let tmux = env::var_os("TMUX").is_some()
            || term.as_deref() == Some("tmux")
            || term.as_deref().is_some_and(|t| t.starts_with("tmux-"));
        let screen = term.as_deref().is_some_and(|t| t.starts_with("screen"));

        caps.true_color = colorterm.as_deref() == Some("truecolor")
            || colorterm.as_deref() == Some("24bit")
            || term_program.as_deref() == Some("iterm.app")
            || term_program.as_deref() == Some("apple_terminal")
            || term_program.as_deref() == Some("wezterm")
            || term_program.as_deref() == Some("ghostty")
            || term_program.as_deref() == Some("warpterminal")
            || term_program.as_deref() == Some("vscode")
            || wt_session
            || term
                .as_deref()
                .is_some_and(|t| t.contains("256color") || t.contains("truecolor"));

        // Hyperlinks: most modern terminals; disable under classic screen/tmux without overrides.
        caps.hyperlinks = !screen && !tmux
            || term_program.as_deref() == Some("wezterm")
            || term_program.as_deref() == Some("ghostty")
            || term_program.as_deref() == Some("kitty")
            || term_program.as_deref() == Some("iterm.app")
            || term_program.as_deref() == Some("warpterminal")
            || wt_session
            || env::var_os("VTE_VERSION").is_some();

        caps.images = detect_image_protocol(
            term_program.as_deref(),
            term.as_deref(),
            tmux,
            screen,
            wt_session,
        );

        // JetBrains / vscode / alacritty rows match the TS table: images off unless protocol env.
        if matches!(
            term_program.as_deref(),
            Some("vscode" | "jetbrains-jedi-term" | "alacritty")
        ) && caps.images.is_none()
        {
            caps.images = None;
        }

        if env::var_os("KITTY_WINDOW_ID").is_some() {
            caps.images = Some(ImageProtocol::Kitty);
        }

        caps
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

fn detect_image_protocol(
    term_program: Option<&str>,
    term: Option<&str>,
    tmux: bool,
    screen: bool,
    wt_session: bool,
) -> Option<ImageProtocol> {
    if tmux || screen {
        return None;
    }
    match term_program {
        Some("kitty" | "ghostty" | "wezterm" | "warpterminal") => Some(ImageProtocol::Kitty),
        Some("iterm.app") => Some(ImageProtocol::ITerm2),
        Some("vscode" | "jetbrains-jedi-term" | "alacritty") => None,
        _ if wt_session => None,
        _ if term.is_some_and(|t| t.contains("kitty")) => Some(ImageProtocol::Kitty),
        _ => None,
    }
}

fn env_lower(key: &str) -> Option<String> {
    env::var(key).ok().map(|value| value.to_ascii_lowercase())
}

fn env_truthy(key: &str) -> bool {
    env::var(key).is_ok_and(|value| {
        matches!(
            value.as_str(),
            "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
        )
    })
}

/// Encode a Kitty image deletion by id (`ESC _Ga=d,d=I,i=N ST`).
#[must_use]
pub fn kitty_delete_id(id: u32) -> Vec<u8> {
    format!("\x1b_Ga=d,d=I,i={id}\x1b\\").into_bytes()
}

/// Encode deletion of every Kitty image (`ESC _Ga=d,d=A ST`).
#[must_use]
pub fn kitty_delete_all() -> Vec<u8> {
    b"\x1b_Ga=d,d=A\x1b\\".to_vec()
}

#[cfg(test)]
mod tests {
    use super::{TerminalCapabilities, kitty_delete_id};

    #[test]
    fn kitty_delete_id_format() {
        assert_eq!(kitty_delete_id(42), b"\x1b_Ga=d,d=I,i=42\x1b\\");
    }

    #[test]
    fn default_sync_output_is_enabled() {
        let caps = TerminalCapabilities::default();
        assert!(caps.sync_output);
    }

    #[test]
    fn detect_returns_caps_struct() {
        // Environment-dependent; only assert the call is side-effect free enough
        // to construct a value without panicking.
        let caps = TerminalCapabilities::detect();
        let _ = caps.cell;
    }
}
