//! Closed capability-profile table for harness launches.

use std::collections::BTreeMap;

use super::transcript::DriverKind;

pub use super::transcript::CapabilityProfile;

impl CapabilityProfile {
    /// Stable profile name used in artifacts and tables.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Xterm256ColorTruecolor => "xterm256-color-truecolor",
            Self::Xterm256Color => "xterm256-color",
            Self::Dumb => "dumb",
            Self::TerminalApp => "terminal-app",
            Self::Iterm2 => "iterm2",
            Self::WindowsTerminalVt => "windows-terminal-vt",
            Self::ConhostVtDec2026Fallback => "conhost-vt-dec2026-fallback",
        }
    }

    /// Looks up a profile by its stable name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "xterm256-color-truecolor" => Some(Self::Xterm256ColorTruecolor),
            "xterm256-color" => Some(Self::Xterm256Color),
            "dumb" => Some(Self::Dumb),
            "terminal-app" => Some(Self::TerminalApp),
            "iterm2" => Some(Self::Iterm2),
            "windows-terminal-vt" => Some(Self::WindowsTerminalVt),
            "conhost-vt-dec2026-fallback" => Some(Self::ConhostVtDec2026Fallback),
            _ => None,
        }
    }

    /// Exhaustive profile table in declaration order.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &PROFILE_TABLE
    }

    /// Environment variables contributed by this profile.
    #[must_use]
    pub fn env(self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        match self {
            Self::Xterm256ColorTruecolor => {
                env.insert("TERM".to_owned(), "xterm-256color".to_owned());
                env.insert("COLORTERM".to_owned(), "truecolor".to_owned());
            }
            Self::Xterm256Color | Self::TerminalApp | Self::ConhostVtDec2026Fallback => {
                env.insert("TERM".to_owned(), "xterm-256color".to_owned());
            }
            Self::Dumb => {
                env.insert("TERM".to_owned(), "dumb".to_owned());
            }
            Self::Iterm2 => {
                env.insert("TERM".to_owned(), "xterm-256color".to_owned());
                env.insert("COLORTERM".to_owned(), "truecolor".to_owned());
                env.insert("TERM_PROGRAM".to_owned(), "iTerm.app".to_owned());
            }
            Self::WindowsTerminalVt => {
                env.insert("TERM".to_owned(), "xterm-256color".to_owned());
                env.insert("COLORTERM".to_owned(), "truecolor".to_owned());
                env.insert("WT_SESSION".to_owned(), "testkit".to_owned());
            }
        }
        env
    }

    /// Pinned probe-reply bytes written immediately after PTY spawn.
    #[must_use]
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
    #[must_use]
    pub fn expects_synchronized_output(self) -> bool {
        !matches!(self, Self::Dumb | Self::ConhostVtDec2026Fallback)
    }

    /// Preferred driver kind for this profile on the current host.
    #[must_use]
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
    CapabilityProfile::Xterm256ColorTruecolor,
    CapabilityProfile::Xterm256Color,
    CapabilityProfile::Dumb,
    CapabilityProfile::TerminalApp,
    CapabilityProfile::Iterm2,
    CapabilityProfile::WindowsTerminalVt,
    CapabilityProfile::ConhostVtDec2026Fallback,
];
