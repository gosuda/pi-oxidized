//! Closed capability-profile table for harness launches.

use std::collections::BTreeMap;

use super::transcript::DriverKind;

/// Pinned terminal capability profiles.
///
/// This enum is closed: new profiles require an explicit table update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityProfile {
    /// `TERM=xterm-256color` with `COLORTERM=truecolor`.
    Xterm256Truecolor,
    /// `TERM=xterm-256color` without truecolor advertisement.
    Xterm256,
    /// Minimal dumb terminal.
    Dumb,
    /// Terminal.app-class profile (`xterm-256color`).
    TerminalApp,
    /// iTerm2-class profile with truecolor.
    Iterm2Truecolor,
    /// Windows Terminal VT with truecolor.
    WindowsTerminalVt,
    /// Conhost VT profile that denies DEC 2026 synchronized-output probes.
    ConhostVtDec2026Fallback,
}

impl CapabilityProfile {
    /// Stable profile name used in artifacts and tables.
    pub fn name(self) -> &'static str {
        match self {
            Self::Xterm256Truecolor => "xterm-256-truecolor",
            Self::Xterm256 => "xterm-256",
            Self::Dumb => "dumb",
            Self::TerminalApp => "terminal-app",
            Self::Iterm2Truecolor => "iterm2-truecolor",
            Self::WindowsTerminalVt => "windows-terminal-vt",
            Self::ConhostVtDec2026Fallback => "conhost-vt-dec2026-fallback",
        }
    }

    /// Looks up a profile by its stable name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::all().into_iter().find(|profile| profile.name() == name)
    }

    /// Exhaustive profile table in declaration order.
    pub fn all() -> &'static [Self] {
        &PROFILE_TABLE
    }

    /// Environment variables contributed by this profile.
    pub fn env(self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        match self {
            Self::Xterm256Truecolor => {
                env.insert("TERM".to_owned(), "xterm-256color".to_owned());
                env.insert("COLORTERM".to_owned(), "truecolor".to_owned());
            }
            Self::Xterm256 | Self::TerminalApp => {
                env.insert("TERM".to_owned(), "xterm-256color".to_owned());
            }
            Self::Dumb => {
                env.insert("TERM".to_owned(), "dumb".to_owned());
            }
            Self::Iterm2Truecolor => {
                env.insert("TERM".to_owned(), "xterm-256color".to_owned());
                env.insert("COLORTERM".to_owned(), "truecolor".to_owned());
                env.insert("TERM_PROGRAM".to_owned(), "iTerm.app".to_owned());
            }
            Self::WindowsTerminalVt => {
                env.insert("TERM".to_owned(), "xterm-256color".to_owned());
                env.insert("COLORTERM".to_owned(), "truecolor".to_owned());
                env.insert("WT_SESSION".to_owned(), "testkit".to_owned());
            }
            Self::ConhostVtDec2026Fallback => {
                env.insert("TERM".to_owned(), "xterm-256color".to_owned());
            }
        }
        env
    }

    /// Pinned probe-reply bytes written immediately after PTY spawn.
    pub fn probe_reply(self) -> &'static [u8] {
        match self {
            // Deny synchronized-output / DEC 2026 style probes; keep DA/cursor replies.
            Self::ConhostVtDec2026Fallback => {
                b"\x1b[?0u\x1b[?1;0c\x1b[6;10;20t\x1b]11;rgb:0000/0000/0000\x07\x1b[1;1R"
            }
            Self::Dumb => b"",
            _ => b"\x1b[?0u\x1b[?1;2c\x1b[6;10;20t\x1b]11;rgb:0000/0000/0000\x07\x1b[1;1R",
        }
    }

    /// Whether this profile expects synchronized-output wrapping support.
    pub fn expects_synchronized_output(self) -> bool {
        !matches!(self, Self::Dumb | Self::ConhostVtDec2026Fallback)
    }

    /// Preferred driver kind for this profile on the current host.
    pub fn preferred_driver_kind(self) -> DriverKind {
        match self {
            Self::WindowsTerminalVt | Self::ConhostVtDec2026Fallback => DriverKind::ConPty,
            // Contingency smoke always uses the QEMU kind when selected by callers.
            _ => {
                if cfg!(windows) {
                    DriverKind::ConPty
                } else {
                    DriverKind::PosixPty
                }
            }
        }
    }
}

const PROFILE_TABLE: [CapabilityProfile; 7] = [
    CapabilityProfile::Xterm256Truecolor,
    CapabilityProfile::Xterm256,
    CapabilityProfile::Dumb,
    CapabilityProfile::TerminalApp,
    CapabilityProfile::Iterm2Truecolor,
    CapabilityProfile::WindowsTerminalVt,
    CapabilityProfile::ConhostVtDec2026Fallback,
];
