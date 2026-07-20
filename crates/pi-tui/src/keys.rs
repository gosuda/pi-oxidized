//! Key identifier grammar and matching against crossterm [`KeyEvent`]s.
//!
//! Ports the observable `KeyId` contract from `.references/pi/packages/tui/src/keys.ts`
//! onto structured terminal events rather than raw escape bytes.
//!
//! # modifyOtherKeys omission
//!
//! This crate **never emits and never parses** xterm `modifyOtherKeys`
//! (`CSI > 4 ; 2 m` / `CSI 27 ; modifiers ; keycode ~`). That is a deliberate
//! functional-parity omission: on legacy non-Kitty terminals, modified Enter
//! (`shift+enter`, `alt+enter`, `ctrl+enter`) cannot be distinguished from plain
//! Enter. The multiline-editor workaround is **backslash-Enter** (see
//! [`backslash_enter_inserts_newline`] and [`should_submit_on_backslash_enter`]).
//!
//! Kitty keyboard protocol events are consumed through crossterm's
//! [`KeyEvent`] (including [`KeyEventKind`] press/repeat/release, keypad
//! functional equivalents, and shifted-character identity). Raw CSI-u byte
//! parsing is intentionally not reimplemented here.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

/// Atomic process flag mirroring TS `setKittyProtocolActive`.
///
/// Crossterm already decodes Kitty sequences into [`KeyEvent`]. This flag is
/// retained for parity with product code paths that still branch on protocol
/// presence (for example custom `\n` / `\x1b\r` mappings before `EventStream`
/// ownership). Matching against structured events does not require it.
static KITTY_PROTOCOL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Set whether Kitty keyboard enhancement is active for this process.
pub fn set_kitty_protocol_active(active: bool) {
    KITTY_PROTOCOL_ACTIVE.store(active, Ordering::Relaxed);
}

/// Query whether Kitty keyboard enhancement is active for this process.
#[must_use]
pub fn is_kitty_protocol_active() -> bool {
    KITTY_PROTOCOL_ACTIVE.load(Ordering::Relaxed)
}

/// Kitty/crossterm key event type (flag 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyEventType {
    /// Physical key press.
    Press,
    /// Auto-repeat while held.
    Repeat,
    /// Physical key release.
    Release,
}

impl KeyEventType {
    /// Map a crossterm event kind into the pi `KeyEventType` vocabulary.
    #[must_use]
    pub const fn from_kind(kind: KeyEventKind) -> Self {
        match kind {
            KeyEventKind::Press => Self::Press,
            KeyEventKind::Repeat => Self::Repeat,
            KeyEventKind::Release => Self::Release,
        }
    }
}

/// Whether a key event should be delivered to a focused component.
///
/// Release events are filtered unless the focused component registered for
/// them (TS `wantsKeyRelease` / `FocusManager` `subscribe_release`).
#[must_use]
pub fn should_dispatch_key_event(event: &KeyEvent, wants_key_release: bool) -> bool {
    match event.kind {
        KeyEventKind::Release => wants_key_release,
        KeyEventKind::Press | KeyEventKind::Repeat => true,
    }
}

/// True when the event is a key release.
#[must_use]
pub fn is_key_release(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Release
}

/// True when the event is a key repeat. Treated like press for binding matches.
#[must_use]
pub fn is_key_repeat(event: &KeyEvent) -> bool {
    event.kind == KeyEventKind::Repeat
}

/// Parsed `KeyId` parts after lowercasing and splitting on `+`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParsedKeyId {
    /// Base key token (lowercased special name or single character).
    pub key: String,
    /// Exact modifier set (shift, control, alt, and super).
    pub modifiers: KeyModifiers,
}

impl ParsedKeyId {
    /// Modifier mask used for exact-set comparison.
    #[must_use]
    pub const fn modifiers(&self) -> KeyModifiers {
        self.modifiers
    }

    /// Canonical string form with modifiers ordered `shift`, `ctrl`, `alt`, `super`.
    #[must_use]
    pub fn canonical_id(&self) -> KeyId {
        let base = display_base_key(&self.key);
        let mut parts = Vec::new();
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            parts.push("shift");
        }
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("ctrl");
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push("alt");
        }
        if self.modifiers.contains(KeyModifiers::SUPER) {
            parts.push("super");
        }
        if parts.is_empty() {
            KeyId::from_raw(base)
        } else {
            KeyId::from_raw(format!("{}+{base}", parts.join("+")))
        }
    }
}

/// Type-safe key identifier string (`"ctrl+c"`, `"shift+enter"`, `"pageUp"`).
///
/// Grammar: optional unordered modifier prefixes among `ctrl|shift|alt|super|meta`,
/// joined by `+`, followed by a base key:
/// - letters `a`–`z`
/// - digits `0`–`9`
/// - symbol keys `` ` - = [ ] \ ; ' , . / ! @ # $ % ^ & * ( ) _ + | ~ { } : < > ? ``
/// - specials: `escape`/`esc`, `enter`/`return`, `tab`, `space`, `backspace`,
///   `delete`, `insert`, `clear`, `home`, `end`, `pageUp`/`pageup`,
///   `pageDown`/`pagedown`, arrows, `f1`–`f12`
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyId {
    raw: String,
}

impl KeyId {
    /// Construct from an already-validated identifier string.
    #[must_use]
    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    /// Borrow the raw identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Parse a `KeyId` string into structured parts.
    ///
    /// # Errors
    ///
    /// Returns [`KeyIdError`] when the identifier is empty, contains an empty
    /// segment, or names an unsupported modifier or base key.
    pub fn parse(input: &str) -> Result<Self, KeyIdError> {
        parse_key_id(input)?;
        Ok(Self {
            raw: input.to_owned(),
        })
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl AsRef<str> for KeyId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for KeyId {
    fn from(value: &str) -> Self {
        Self::from_raw(value.to_owned())
    }
}

impl From<String> for KeyId {
    fn from(value: String) -> Self {
        Self::from_raw(value)
    }
}

/// Failure parsing a `KeyId` string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyIdError {
    /// Empty identifier.
    #[error("empty key id")]
    Empty,
    /// Unknown base key token.
    #[error("unknown key id base `{0}`")]
    UnknownBase(String),
    /// Empty segment between `+` separators.
    #[error("empty key id segment")]
    EmptySegment,
}

/// Parse a `KeyId` into structured modifier and base parts.
///
/// Modifier order in the string is irrelevant. `meta` is accepted as an alias
/// for `super`. The base token is lowercased (`pageUp` becomes `pageup`).
///
/// # Errors
///
/// Returns [`KeyIdError`] when the identifier is empty, contains an empty
/// segment, or names an unsupported modifier or base key.
pub fn parse_key_id(key_id: &str) -> Result<ParsedKeyId, KeyIdError> {
    if key_id.is_empty() {
        return Err(KeyIdError::Empty);
    }
    let parts: Vec<&str> = key_id.split('+').collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(KeyIdError::EmptySegment);
    }
    let raw_base = parts[parts.len() - 1];
    let base = raw_base.to_ascii_lowercase();
    if !is_known_base(&base) {
        return Err(KeyIdError::UnknownBase(raw_base.to_owned()));
    }
    let mut modifiers = KeyModifiers::empty();
    for part in &parts[..parts.len() - 1] {
        let modifier = match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "shift" => KeyModifiers::SHIFT,
            "alt" | "option" => KeyModifiers::ALT,
            "super" | "meta" | "cmd" | "command" | "win" | "windows" => KeyModifiers::SUPER,
            other => return Err(KeyIdError::UnknownBase(other.to_owned())),
        };
        modifiers.insert(modifier);
    }
    Ok(ParsedKeyId {
        key: base,
        modifiers,
    })
}

/// Match a crossterm [`KeyEvent`] against a `KeyId`.
///
/// Press and repeat match identically. Release events also match identity so
/// components that subscribe to releases can reuse the same bindings; dispatch
/// filtering is separate via [`should_dispatch_key_event`].
///
/// Lock states (caps/num) are ignored. Keypad functional keys are treated as
/// their logical equivalents (digit/symbol/navigation/enter). Shifted letter
/// identity normalizes `Char('C')` + SHIFT to base `c` with shift.
///
/// # modifyOtherKeys
///
/// This function never inspects raw `CSI 27;…~` bytes. Legacy terminals that
/// only expose plain Enter cannot satisfy `shift+enter` / `alt+enter` ids.
#[must_use]
pub fn key_matches(event: &KeyEvent, key_id: &KeyId) -> bool {
    let Ok(parsed) = parse_key_id(key_id.as_str()) else {
        return false;
    };
    key_matches_parsed(event, &parsed)
}

/// Match against an already-parsed `KeyId`.
#[must_use]
pub fn key_matches_parsed(event: &KeyEvent, parsed: &ParsedKeyId) -> bool {
    let Some((base, event_mods)) = normalize_event(event) else {
        return false;
    };
    // TS: escape/esc only matches with zero modifiers.
    if matches!(parsed.key.as_str(), "escape" | "esc") && !parsed.modifiers().is_empty() {
        return false;
    }
    if event_mods != parsed.modifiers() {
        return false;
    }
    base_keys_equal(&base, &parsed.key)
}

/// Normalize a key event into (base token, effective modifiers).
///
/// Returns `None` for keys outside the `KeyId` grammar (media,
/// modifiers-only, f13+, etc.).
#[must_use]
pub fn normalize_event(event: &KeyEvent) -> Option<(String, KeyModifiers)> {
    let mut mods = event.modifiers;
    // Caps/Num lock are carried in KeyEventState, not KeyModifiers, so nothing
    // to strip from modifiers. Ignore KEYPAD for identity — only code matters.
    let _keypad = event.state.contains(KeyEventState::KEYPAD);

    match event.code {
        KeyCode::Esc => Some(("escape".to_owned(), strip_unsupported(mods))),
        KeyCode::Enter => Some(("enter".to_owned(), strip_unsupported(mods))),
        KeyCode::Tab => Some(("tab".to_owned(), strip_unsupported(mods))),
        KeyCode::BackTab => {
            mods |= KeyModifiers::SHIFT;
            Some(("tab".to_owned(), strip_unsupported(mods)))
        }
        KeyCode::Backspace => Some(("backspace".to_owned(), strip_unsupported(mods))),
        KeyCode::Delete => Some(("delete".to_owned(), strip_unsupported(mods))),
        KeyCode::Insert => Some(("insert".to_owned(), strip_unsupported(mods))),
        KeyCode::Home => Some(("home".to_owned(), strip_unsupported(mods))),
        KeyCode::End => Some(("end".to_owned(), strip_unsupported(mods))),
        KeyCode::PageUp => Some(("pageup".to_owned(), strip_unsupported(mods))),
        KeyCode::PageDown => Some(("pagedown".to_owned(), strip_unsupported(mods))),
        KeyCode::Up => Some(("up".to_owned(), strip_unsupported(mods))),
        KeyCode::Down => Some(("down".to_owned(), strip_unsupported(mods))),
        KeyCode::Left => Some(("left".to_owned(), strip_unsupported(mods))),
        KeyCode::Right => Some(("right".to_owned(), strip_unsupported(mods))),
        KeyCode::F(n @ 1..=12) => Some((format!("f{n}"), strip_unsupported(mods))),
        // Crossterm "clear"/keypad-begin is the closest stand-in for TS clear.
        KeyCode::KeypadBegin => Some(("clear".to_owned(), strip_unsupported(mods))),
        KeyCode::Char(ch) => normalize_char(ch, mods),
        KeyCode::Null => {
            // Legacy ctrl+space often arrives as NUL.
            if mods.contains(KeyModifiers::CONTROL) || mods.is_empty() {
                let mut m = mods;
                m.insert(KeyModifiers::CONTROL);
                Some(("space".to_owned(), strip_unsupported(m)))
            } else {
                None
            }
        }
        KeyCode::CapsLock
        | KeyCode::ScrollLock
        | KeyCode::NumLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::Menu
        | KeyCode::Media(_)
        | KeyCode::Modifier(_)
        | KeyCode::F(_) => None,
    }
}

/// Return the canonical [`KeyId`] identity for a structured terminal event.
///
/// Event kind is intentionally excluded: press, repeat, and release have the
/// same binding identity and remain distinguishable on the event itself.
#[must_use]
pub fn canonical_key_id(event: &KeyEvent) -> Option<KeyId> {
    let (key, modifiers) = normalize_event(event)?;
    Some(ParsedKeyId { key, modifiers }.canonical_id())
}

/// Encode a structured key event as the canonical byte sequence delivered to
/// terminal components such as extension `handleInput` implementations.
///
/// Conventional press events use their compact legacy representation whenever
/// it preserves identity. Kitty enhanced CSI forms are used for repeat/release
/// events and for modifier combinations that legacy input would erase.
#[must_use]
pub fn encode_key_event(event: &KeyEvent) -> Option<String> {
    let (base, modifiers) = normalize_event(event)?;
    if event.kind == KeyEventKind::Press
        && let Some(legacy) = encode_legacy_key_event(event, &base, modifiers)
    {
        return Some(legacy);
    }

    let modifier_parameter = 1
        + u8::from(modifiers.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(modifiers.contains(KeyModifiers::ALT))
        + 4 * u8::from(modifiers.contains(KeyModifiers::CONTROL))
        + 8 * u8::from(modifiers.contains(KeyModifiers::SUPER));
    let event_type = match event.kind {
        KeyEventKind::Press => 1,
        KeyEventKind::Repeat => 2,
        KeyEventKind::Release => 3,
    };
    encode_kitty_key_event(&base, modifier_parameter, event_type)
}

fn encode_legacy_key_event(
    event: &KeyEvent,
    base: &str,
    modifiers: KeyModifiers,
) -> Option<String> {
    let without_alt = modifiers - KeyModifiers::ALT;
    let mut data = match event.code {
        KeyCode::Char(character) if without_alt == KeyModifiers::CONTROL => {
            legacy_control_character(character).map(|character| character.to_string())?
        }
        KeyCode::Char(character) if without_alt.is_empty() => character.to_string(),
        KeyCode::Char(character) if without_alt == KeyModifiers::SHIFT => {
            let shift_is_embodied = character.is_ascii_uppercase()
                || (is_symbol_char(character) && base == character.to_string());
            shift_is_embodied.then(|| character.to_string())?
        }
        KeyCode::Enter if modifiers.is_empty() => "\r".to_owned(),
        KeyCode::Tab if modifiers.is_empty() => "\t".to_owned(),
        KeyCode::BackTab if modifiers == KeyModifiers::SHIFT => "\u{1b}[Z".to_owned(),
        KeyCode::Backspace if modifiers.is_empty() => "\u{7f}".to_owned(),
        KeyCode::Esc if modifiers.is_empty() => "\u{1b}".to_owned(),
        KeyCode::Up if modifiers.is_empty() => "\u{1b}[A".to_owned(),
        KeyCode::Down if modifiers.is_empty() => "\u{1b}[B".to_owned(),
        KeyCode::Right if modifiers.is_empty() => "\u{1b}[C".to_owned(),
        KeyCode::Left if modifiers.is_empty() => "\u{1b}[D".to_owned(),
        KeyCode::Home if modifiers.is_empty() => "\u{1b}[H".to_owned(),
        KeyCode::End if modifiers.is_empty() => "\u{1b}[F".to_owned(),
        KeyCode::Insert if modifiers.is_empty() => "\u{1b}[2~".to_owned(),
        KeyCode::Delete if modifiers.is_empty() => "\u{1b}[3~".to_owned(),
        KeyCode::PageUp if modifiers.is_empty() => "\u{1b}[5~".to_owned(),
        KeyCode::PageDown if modifiers.is_empty() => "\u{1b}[6~".to_owned(),
        KeyCode::F(number @ 1..=4) if modifiers.is_empty() => {
            format!("\u{1b}O{}", char::from(b'P' + number - 1))
        }
        KeyCode::F(number @ 5..=12) if modifiers.is_empty() => {
            let suffix = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(number - 5)];
            format!("\u{1b}[{suffix}~")
        }
        KeyCode::KeypadBegin if modifiers.is_empty() => "\u{1b}[E".to_owned(),
        _ => return None,
    };
    if modifiers.contains(KeyModifiers::ALT) {
        data.insert(0, '\u{1b}');
    }
    Some(data)
}

fn legacy_control_character(character: char) -> Option<char> {
    if character == ' ' {
        return Some('\0');
    }
    character
        .is_ascii()
        .then(|| char::from((character.to_ascii_uppercase() as u8) & 0x1f))
}

fn encode_kitty_key_event(base: &str, modifier_parameter: u8, event_type: u8) -> Option<String> {
    let sequence = match base {
        "up" => format!("\u{1b}[1;{modifier_parameter}:{event_type}A"),
        "down" => format!("\u{1b}[1;{modifier_parameter}:{event_type}B"),
        "right" => format!("\u{1b}[1;{modifier_parameter}:{event_type}C"),
        "left" => format!("\u{1b}[1;{modifier_parameter}:{event_type}D"),
        "home" => format!("\u{1b}[1;{modifier_parameter}:{event_type}H"),
        "end" => format!("\u{1b}[1;{modifier_parameter}:{event_type}F"),
        "insert" => format!("\u{1b}[2;{modifier_parameter}:{event_type}~"),
        "delete" => format!("\u{1b}[3;{modifier_parameter}:{event_type}~"),
        "pageup" => format!("\u{1b}[5;{modifier_parameter}:{event_type}~"),
        "pagedown" => format!("\u{1b}[6;{modifier_parameter}:{event_type}~"),
        function if function.starts_with('f') => {
            let number = function[1..].parse::<u8>().ok()?;
            if (1..=4).contains(&number) {
                let final_byte = char::from(b'P' + number - 1);
                format!("\u{1b}[1;{modifier_parameter}:{event_type}{final_byte}")
            } else {
                let parameter = [15, 17, 18, 19, 20, 21, 23, 24]
                    .get(usize::from(number.checked_sub(5)?))
                    .copied()?;
                format!("\u{1b}[{parameter};{modifier_parameter}:{event_type}~")
            }
        }
        _ => {
            let codepoint = kitty_codepoint(base)?;
            format!("\u{1b}[{codepoint};{modifier_parameter}:{event_type}u")
        }
    };
    Some(sequence)
}

fn kitty_codepoint(base: &str) -> Option<u32> {
    let functional = match base {
        "escape" => 27,
        "enter" => 13,
        "tab" => 9,
        "backspace" => 127,
        "insert" => 57_348,
        "delete" => 57_349,
        "pageup" => 57_350,
        "pagedown" => 57_351,
        "left" => 57_352,
        "right" => 57_353,
        "up" => 57_354,
        "down" => 57_355,
        "home" => 57_356,
        "end" => 57_357,
        "clear" => 57_427,
        function if function.starts_with('f') => {
            let number = function[1..].parse::<u32>().ok()?;
            57_363 + number
        }
        "space" => u32::from(' '),
        character => return single_character(character).map(u32::from),
    };
    Some(functional)
}

fn single_character(value: &str) -> Option<char> {
    let mut characters = value.chars();
    let character = characters.next()?;
    characters.next().is_none().then_some(character)
}

fn normalize_char(ch: char, mut mods: KeyModifiers) -> Option<(String, KeyModifiers)> {
    // Shifted letter identity: 'A'..='Z' implies shift + lowercase base.
    if ch.is_ascii_uppercase() {
        mods.insert(KeyModifiers::SHIFT);
        return Some((ch.to_ascii_lowercase().to_string(), strip_unsupported(mods)));
    }
    if ch == ' ' {
        return Some(("space".to_owned(), strip_unsupported(mods)));
    }
    if ch.is_ascii_lowercase() || ch.is_ascii_digit() || is_symbol_char(ch) {
        // If SHIFT is held with a lowercase letter, keep shift (Kitty CSI-u).
        return Some((ch.to_string(), strip_unsupported(mods)));
    }
    // Non-ASCII printable (e.g. Cyrillic) — expose as the character itself so
    // callers can match explicit bindings; Latin base-layout fallback is not
    // available once crossterm has discarded the Kitty base-layout field.
    if !ch.is_control() {
        return Some((ch.to_string(), strip_unsupported(mods)));
    }
    None
}

fn strip_unsupported(mods: KeyModifiers) -> KeyModifiers {
    let supported =
        KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER;
    // Treat META as SUPER for KeyId matching ("meta" alias).
    let mut out = mods & supported;
    if mods.contains(KeyModifiers::META) {
        out |= KeyModifiers::SUPER;
    }
    // HYPER is ignored (unsupported in KeyId grammar).
    out
}

fn base_keys_equal(event_base: &str, id_base: &str) -> bool {
    if event_base == id_base {
        return true;
    }
    matches!(
        (event_base, id_base),
        ("escape", "esc") | ("esc", "escape") | ("enter", "return") | ("return", "enter")
    )
}

fn is_known_base(base: &str) -> bool {
    matches!(
        base,
        "escape"
            | "esc"
            | "enter"
            | "return"
            | "tab"
            | "space"
            | "backspace"
            | "delete"
            | "insert"
            | "clear"
            | "home"
            | "end"
            | "pageup"
            | "pagedown"
            | "up"
            | "down"
            | "left"
            | "right"
            | "f1"
            | "f2"
            | "f3"
            | "f4"
            | "f5"
            | "f6"
            | "f7"
            | "f8"
            | "f9"
            | "f10"
            | "f11"
            | "f12"
    ) || (base.len() == 1
        && (base.as_bytes()[0].is_ascii_lowercase()
            || base.as_bytes()[0].is_ascii_digit()
            || is_symbol_char(base.chars().next().unwrap_or('\0'))))
}

fn is_symbol_char(ch: char) -> bool {
    matches!(
        ch,
        '`' | '-'
            | '='
            | '['
            | ']'
            | '\\'
            | ';'
            | '\''
            | ','
            | '.'
            | '/'
            | '!'
            | '@'
            | '#'
            | '$'
            | '%'
            | '^'
            | '&'
            | '*'
            | '('
            | ')'
            | '_'
            | '+'
            | '|'
            | '~'
            | '{'
            | '}'
            | ':'
            | '<'
            | '>'
            | '?'
    )
}

fn display_base_key(base: &str) -> String {
    match base {
        "esc" => "escape".to_owned(),
        "return" => "enter".to_owned(),
        "pageup" => "pageUp".to_owned(),
        "pagedown" => "pageDown".to_owned(),
        other => other.to_owned(),
    }
}

/// Helpers for building common `KeyId`s with autocomplete-friendly constructors.
pub struct Key;

impl Key {
    /// `escape`
    pub const ESCAPE: &'static str = "escape";
    /// `enter`
    pub const ENTER: &'static str = "enter";
    /// `tab`
    pub const TAB: &'static str = "tab";
    /// `space`
    pub const SPACE: &'static str = "space";
    /// `backspace`
    pub const BACKSPACE: &'static str = "backspace";
    /// `delete`
    pub const DELETE: &'static str = "delete";
    /// `up`
    pub const UP: &'static str = "up";
    /// `down`
    pub const DOWN: &'static str = "down";
    /// `left`
    pub const LEFT: &'static str = "left";
    /// `right`
    pub const RIGHT: &'static str = "right";
    /// `home`
    pub const HOME: &'static str = "home";
    /// `end`
    pub const END: &'static str = "end";
    /// `pageUp`
    pub const PAGE_UP: &'static str = "pageUp";
    /// `pageDown`
    pub const PAGE_DOWN: &'static str = "pageDown";

    /// `ctrl+{key}`
    #[must_use]
    pub fn ctrl(key: &str) -> KeyId {
        KeyId::from_raw(format!("ctrl+{key}"))
    }

    /// `shift+{key}`
    #[must_use]
    pub fn shift(key: &str) -> KeyId {
        KeyId::from_raw(format!("shift+{key}"))
    }

    /// `alt+{key}`
    #[must_use]
    pub fn alt(key: &str) -> KeyId {
        KeyId::from_raw(format!("alt+{key}"))
    }

    /// `super+{key}` (Meta/Cmd/Win)
    #[must_use]
    pub fn super_key(key: &str) -> KeyId {
        KeyId::from_raw(format!("super+{key}"))
    }

    /// Alias for [`Key::super_key`].
    #[must_use]
    pub fn meta(key: &str) -> KeyId {
        Self::super_key(key)
    }

    /// `ctrl+shift+{key}`
    #[must_use]
    pub fn ctrl_shift(key: &str) -> KeyId {
        KeyId::from_raw(format!("ctrl+shift+{key}"))
    }

    /// `ctrl+alt+{key}`
    #[must_use]
    pub fn ctrl_alt(key: &str) -> KeyId {
        KeyId::from_raw(format!("ctrl+alt+{key}"))
    }

    /// `ctrl+shift+alt+{key}`
    #[must_use]
    pub fn ctrl_shift_alt(key: &str) -> KeyId {
        KeyId::from_raw(format!("ctrl+shift+alt+{key}"))
    }

    /// `ctrl+super+{key}`
    #[must_use]
    pub fn ctrl_super(key: &str) -> KeyId {
        KeyId::from_raw(format!("ctrl+super+{key}"))
    }
}

// ---------------------------------------------------------------------------
// Backslash-Enter workaround (legacy modified-Enter omission)
// ---------------------------------------------------------------------------

/// Default binding: Enter after `\` inserts a newline instead of submitting.
///
/// Used when `tui.input.submit` is plain Enter and `tui.input.newLine` includes
/// `shift+enter` (the shipped defaults). On legacy terminals that cannot report
/// Shift+Enter, users type `\` then Enter.
#[must_use]
pub fn backslash_enter_inserts_newline(event: &KeyEvent, char_before_cursor: Option<char>) -> bool {
    if char_before_cursor != Some('\\') {
        return false;
    }
    key_matches(event, &KeyId::from_raw("enter"))
}

/// Inverted-rebind case from TS `shouldSubmitOnBackslashEnter`.
///
/// When the user rebinds **submit** to include `shift+enter` (so Enter becomes
/// a newline path via other heuristics), a bare Enter that arrives while the
/// previous character is `\` should submit after deleting the backslash.
#[must_use]
pub fn should_submit_on_backslash_enter(
    event: &KeyEvent,
    char_before_cursor: Option<char>,
    submit_keys: &[KeyId],
    disable_submit: bool,
) -> bool {
    if disable_submit {
        return false;
    }
    if !key_matches(event, &KeyId::from_raw("enter")) {
        return false;
    }
    let has_shift_enter = submit_keys.iter().any(|key| {
        parse_key_id(key.as_str()).is_ok_and(|parsed| {
            parsed.modifiers == KeyModifiers::SHIFT
                && matches!(parsed.key.as_str(), "enter" | "return")
        })
    });
    if !has_shift_enter {
        return false;
    }
    char_before_cursor == Some('\\')
}

/// Documented omission marker used by tests and the key-matrix report.
pub const MODIFY_OTHER_KEYS_OMISSION: &str = concat!(
    "modifyOtherKeys (CSI >4;2m / CSI 27;mod;key~) is never emitted or parsed. ",
    "Legacy non-Kitty terminals cannot distinguish shift/alt/ctrl+Enter from Enter; ",
    "use backslash-Enter for newline."
);

// ---------------------------------------------------------------------------
// Test helpers for synthesizing crossterm events
// ---------------------------------------------------------------------------

/// Build a press event.
#[must_use]
pub fn key_press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new_with_kind(code, modifiers, KeyEventKind::Press)
}

/// Build a press event with keyboard state (keypad/caps/num).
#[must_use]
pub fn key_press_state(code: KeyCode, modifiers: KeyModifiers, state: KeyEventState) -> KeyEvent {
    KeyEvent::new_with_kind_and_state(code, modifiers, KeyEventKind::Press, state)
}

/// Build a repeat event.
#[must_use]
pub fn key_repeat(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new_with_kind(code, modifiers, KeyEventKind::Repeat)
}

/// Build a release event.
#[must_use]
pub fn key_release(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new_with_kind(code, modifiers, KeyEventKind::Release)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> KeyId {
        KeyId::from_raw(s)
    }

    #[test]
    fn parse_key_id_modifiers_any_order_and_meta_alias() -> Result<(), KeyIdError> {
        let a = parse_key_id("ctrl+shift+p")?;
        let b = parse_key_id("shift+ctrl+p")?;
        assert_eq!(a.modifiers(), b.modifiers());
        assert_eq!(a.key, "p");
        let meta = parse_key_id("meta+k")?;
        assert!(meta.modifiers.contains(KeyModifiers::SUPER));
        assert_eq!(meta.key, "k");
        assert_eq!(parse_key_id("pageUp")?.key, "pageup");
        assert_eq!(parse_key_id("ctrl+alt+]")?.key, "]");
        Ok(())
    }

    #[test]
    fn parse_rejects_unknown_base() {
        assert!(matches!(
            parse_key_id("ctrl+foo"),
            Err(KeyIdError::UnknownBase(_))
        ));
        assert!(matches!(parse_key_id(""), Err(KeyIdError::Empty)));
    }

    #[test]
    fn matches_plain_and_modified_letters() {
        assert!(key_matches(
            &key_press(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &id("ctrl+c")
        ));
        assert!(!key_matches(
            &key_press(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &id("ctrl+d")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Char('c'), KeyModifiers::empty()),
            &id("c")
        ));
        assert!(key_matches(
            &key_press(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            &id("ctrl+shift+p")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Char('k'), KeyModifiers::SUPER),
            &id("super+k")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Char('k'), KeyModifiers::SUPER),
            &id("meta+k")
        ));
        assert!(key_matches(
            &key_press(
                KeyCode::Char('k'),
                KeyModifiers::CONTROL | KeyModifiers::SUPER
            ),
            &Key::ctrl_super("k")
        ));
    }

    #[test]
    fn shifted_letter_identity() {
        // Uppercase char implies shift.
        assert!(key_matches(
            &key_press(KeyCode::Char('C'), KeyModifiers::SHIFT),
            &id("shift+c")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Char('C'), KeyModifiers::empty()),
            &id("shift+c")
        ));
        // Lowercase + SHIFT (Kitty CSI-u style).
        assert!(key_matches(
            &key_press(KeyCode::Char('c'), KeyModifiers::SHIFT),
            &id("shift+c")
        ));
        // Ctrl+Shift+E via uppercase E.
        assert!(key_matches(
            &key_press(
                KeyCode::Char('E'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            &id("ctrl+shift+e")
        ));
    }

    #[test]
    fn keypad_equivalents() {
        let kp1 = key_press_state(
            KeyCode::Char('1'),
            KeyModifiers::empty(),
            KeyEventState::KEYPAD,
        );
        assert!(key_matches(&kp1, &id("1")));
        let kp_div = key_press_state(
            KeyCode::Char('/'),
            KeyModifiers::empty(),
            KeyEventState::KEYPAD,
        );
        assert!(key_matches(&kp_div, &id("/")));
        let kp_left = key_press_state(KeyCode::Left, KeyModifiers::empty(), KeyEventState::KEYPAD);
        assert!(key_matches(&kp_left, &id("left")));
        let kp_del = key_press_state(
            KeyCode::Delete,
            KeyModifiers::empty(),
            KeyEventState::KEYPAD,
        );
        assert!(key_matches(&kp_del, &id("delete")));
        let kp_enter =
            key_press_state(KeyCode::Enter, KeyModifiers::empty(), KeyEventState::KEYPAD);
        assert!(key_matches(&kp_enter, &id("enter")));
        assert!(key_matches(
            &key_press_state(KeyCode::Enter, KeyModifiers::SHIFT, KeyEventState::KEYPAD),
            &id("shift+enter")
        ));
    }

    #[test]
    fn special_keys_and_arrows() {
        assert!(key_matches(
            &key_press(KeyCode::Esc, KeyModifiers::empty()),
            &id("escape")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Esc, KeyModifiers::empty()),
            &id("esc")
        ));
        assert!(!key_matches(
            &key_press(KeyCode::Esc, KeyModifiers::CONTROL),
            &id("escape")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Enter, KeyModifiers::empty()),
            &id("enter")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Enter, KeyModifiers::empty()),
            &id("return")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Tab, KeyModifiers::empty()),
            &id("tab")
        ));
        assert!(key_matches(
            &key_press(KeyCode::BackTab, KeyModifiers::empty()),
            &id("shift+tab")
        ));
        assert!(key_matches(
            &key_press(KeyCode::BackTab, KeyModifiers::SHIFT),
            &id("shift+tab")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Char(' '), KeyModifiers::empty()),
            &id("space")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Char(' '), KeyModifiers::CONTROL),
            &id("ctrl+space")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Backspace, KeyModifiers::empty()),
            &id("backspace")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Backspace, KeyModifiers::ALT),
            &id("alt+backspace")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Left, KeyModifiers::ALT),
            &id("alt+left")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Right, KeyModifiers::CONTROL),
            &id("ctrl+right")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Up, KeyModifiers::empty()),
            &id("up")
        ));
        assert!(key_matches(
            &key_press(KeyCode::PageUp, KeyModifiers::empty()),
            &id("pageUp")
        ));
        assert!(key_matches(
            &key_press(KeyCode::PageDown, KeyModifiers::empty()),
            &id("pageDown")
        ));
        assert!(key_matches(
            &key_press(KeyCode::F(1), KeyModifiers::empty()),
            &id("f1")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Char(']'), KeyModifiers::CONTROL),
            &id("ctrl+]")
        ));
        assert!(key_matches(
            &key_press(
                KeyCode::Char(']'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ),
            &id("ctrl+alt+]")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Char('-'), KeyModifiers::CONTROL),
            &id("ctrl+-")
        ));
        assert!(key_matches(
            &key_press(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &id("ctrl+j")
        ));
    }

    #[test]
    fn press_repeat_release_semantics() {
        let press = key_press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let repeat = key_repeat(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let release = key_release(KeyCode::Char('c'), KeyModifiers::CONTROL);

        // Identity matches for all kinds (TS matchesKey on release sequences).
        assert!(key_matches(&press, &id("ctrl+c")));
        assert!(key_matches(&repeat, &id("ctrl+c")));
        assert!(key_matches(&release, &id("ctrl+c")));

        // Dispatch filters releases unless subscribed.
        assert!(should_dispatch_key_event(&press, false));
        assert!(should_dispatch_key_event(&repeat, false));
        assert!(!should_dispatch_key_event(&release, false));
        assert!(should_dispatch_key_event(&release, true));
        assert!(is_key_release(&release));
        assert!(is_key_repeat(&repeat));
    }

    #[test]
    fn table_driven_keyid_x_event_cross_product() {
        let control = KeyModifiers::CONTROL;
        let shift = KeyModifiers::SHIFT;
        let alt = KeyModifiers::ALT;
        let super_key = KeyModifiers::SUPER;
        let cases = [
            ("ctrl+c", key_press(KeyCode::Char('c'), control), true),
            (
                "ctrl+c",
                key_press(KeyCode::Char('c'), KeyModifiers::empty()),
                false,
            ),
            ("shift+enter", key_press(KeyCode::Enter, shift), true),
            (
                "shift+enter",
                key_press(KeyCode::Enter, KeyModifiers::empty()),
                false,
            ),
            ("alt+enter", key_press(KeyCode::Enter, alt), true),
            ("ctrl+enter", key_press(KeyCode::Enter, control), true),
            (
                "enter",
                key_press(KeyCode::Enter, KeyModifiers::empty()),
                true,
            ),
            ("super+enter", key_press(KeyCode::Enter, super_key), true),
            (
                "ctrl+shift+super+k",
                key_press(KeyCode::Char('k'), control | shift | super_key),
                true,
            ),
            (
                "1",
                key_press_state(
                    KeyCode::Char('1'),
                    KeyModifiers::empty(),
                    KeyEventState::KEYPAD,
                ),
                true,
            ),
            ("ctrl+1", key_press(KeyCode::Char('1'), control), true),
            (
                "shift+tab",
                key_press(KeyCode::BackTab, KeyModifiers::empty()),
                true,
            ),
            ("alt+b", key_press(KeyCode::Char('b'), alt), true),
            ("ctrl+b", key_press(KeyCode::Char('b'), control), true),
            ("ctrl+c", key_release(KeyCode::Char('c'), control), true),
            ("ctrl+c", key_repeat(KeyCode::Char('c'), control), true),
        ];
        for (key_id, event, expected) in cases {
            assert_eq!(
                key_matches(&event, &id(key_id)),
                expected,
                "id={key_id} event={event:?}"
            );
        }
    }

    #[test]
    fn legacy_modified_enter_cannot_be_distinguished() {
        // Without Kitty enhancement, crossterm only reports plain Enter.
        // shift/alt Enter ids must not match that event.
        let plain = key_press(KeyCode::Enter, KeyModifiers::empty());
        assert!(key_matches(&plain, &id("enter")));
        assert!(!key_matches(&plain, &id("shift+enter")));
        assert!(!key_matches(&plain, &id("alt+enter")));
        assert!(!key_matches(&plain, &id("ctrl+enter")));
        // Document the protocol omission.
        assert!(MODIFY_OTHER_KEYS_OMISSION.contains("modifyOtherKeys"));
        assert!(MODIFY_OTHER_KEYS_OMISSION.contains("backslash-Enter"));
    }

    #[test]
    fn backslash_enter_workaround() {
        let enter = key_press(KeyCode::Enter, KeyModifiers::empty());
        assert!(backslash_enter_inserts_newline(&enter, Some('\\')));
        assert!(!backslash_enter_inserts_newline(&enter, Some('x')));
        assert!(!backslash_enter_inserts_newline(&enter, None));
        assert!(!backslash_enter_inserts_newline(
            &key_press(KeyCode::Char('a'), KeyModifiers::empty()),
            Some('\\')
        ));

        // Inverted rebind: submit includes shift+enter → \+Enter submits.
        let submit = [id("shift+enter")];
        assert!(should_submit_on_backslash_enter(
            &enter,
            Some('\\'),
            &submit,
            false
        ));
        assert!(!should_submit_on_backslash_enter(
            &enter,
            Some('\\'),
            &[id("enter")],
            false
        ));
        assert!(!should_submit_on_backslash_enter(
            &enter,
            Some('\\'),
            &submit,
            true
        ));
    }

    #[test]
    fn kitty_protocol_flag_roundtrip() {
        set_kitty_protocol_active(true);
        assert!(is_kitty_protocol_active());
        set_kitty_protocol_active(false);
        assert!(!is_kitty_protocol_active());
    }

    #[test]
    fn normalize_event_ignores_lock_state() {
        let event = key_press_state(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventState::CAPS_LOCK | KeyEventState::NUM_LOCK,
        );
        assert!(key_matches(&event, &id("ctrl+c")));
    }

    #[test]
    fn canonical_identity_matches_for_press_repeat_and_release() -> Result<(), String> {
        for event in [
            key_press(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            key_repeat(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            key_release(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        ] {
            let canonical = canonical_key_id(&event)
                .ok_or_else(|| format!("unsupported key event: {event:?}"))?;
            assert_eq!(canonical.as_str(), "shift+ctrl+p");
            assert!(key_matches(&event, &canonical));
        }
        Ok(())
    }

    #[test]
    fn terminal_encoding_uses_legacy_only_when_identity_survives() {
        assert_eq!(
            encode_key_event(&key_press(KeyCode::Char('x'), KeyModifiers::ALT)).as_deref(),
            Some("\u{1b}x")
        );
        assert_eq!(
            encode_key_event(&key_press(KeyCode::Char('c'), KeyModifiers::CONTROL)).as_deref(),
            Some("\u{3}")
        );
        assert_eq!(
            encode_key_event(&key_press(KeyCode::PageDown, KeyModifiers::NONE)).as_deref(),
            Some("\u{1b}[6~")
        );
        assert_eq!(
            encode_key_event(&key_press(KeyCode::Enter, KeyModifiers::SHIFT)).as_deref(),
            Some("\u{1b}[13;2:1u")
        );
        assert_eq!(
            encode_key_event(&key_press(
                KeyCode::Up,
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ))
            .as_deref(),
            Some("\u{1b}[1;7:1A")
        );
    }

    #[test]
    fn terminal_encoding_preserves_kitty_event_kind() {
        assert_eq!(
            encode_key_event(&key_repeat(KeyCode::F(5), KeyModifiers::NONE)).as_deref(),
            Some("\u{1b}[15;1:2~")
        );
        assert_eq!(
            encode_key_event(&key_release(KeyCode::Char('a'), KeyModifiers::SUPER)).as_deref(),
            Some("\u{1b}[97;9:3u")
        );
    }
}
