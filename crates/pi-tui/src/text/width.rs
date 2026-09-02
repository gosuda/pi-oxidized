//! Grapheme / visible terminal column width.
//!
//! Semantics match the TypeScript `graphemeWidth` / `visibleWidth` helpers:
//! tab = 3, regional-indicator (incl. singleton) = 2, RGI-style emoji = 2,
//! combining / control / zero-width = 0, East-Asian wide = 2.

use std::sync::{LazyLock, Mutex};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::ansi::extract_ansi_code;

/// ASCII punctuation set used by editor word navigation / break helpers.
pub const PUNCTUATION: &str = "(){}[]<>.,;:'\"!?+-=*/\\|&%^$#@~`";

const WIDTH_CACHE_SIZE: usize = 512;

static WIDTH_CACHE: LazyLock<Mutex<lru_like::LruMap>> =
    LazyLock::new(|| Mutex::new(lru_like::LruMap::new(WIDTH_CACHE_SIZE)));

fn width_cache() -> &'static Mutex<lru_like::LruMap> {
    &WIDTH_CACHE
}

/// Small insertion-order map used only as a bounded width cache.
mod lru_like {
    use std::collections::{HashMap, hash_map::Entry};

    pub(super) struct LruMap {
        map: HashMap<String, usize>,
        order: Vec<String>,
        cap: usize,
    }

    impl LruMap {
        pub(super) fn new(cap: usize) -> Self {
            Self {
                map: HashMap::new(),
                order: Vec::new(),
                cap,
            }
        }

        pub(super) fn get(&self, key: &str) -> Option<usize> {
            self.map.get(key).copied()
        }

        pub(super) fn insert(&mut self, key: String, value: usize) {
            if let Entry::Occupied(mut entry) = self.map.entry(key.clone()) {
                entry.insert(value);
                return;
            }
            if self.order.len() >= self.cap
                && let Some(oldest) = self.order.first().cloned()
            {
                self.order.remove(0);
                self.map.remove(&oldest);
            }
            self.map.insert(key.clone(), value);
            self.order.push(key);
        }
    }
}

/// Return `true` when every character is printable ASCII (`0x20..=0x7E`).
#[must_use]
pub fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// East-Asian / terminal cell width of a single code point (non-CJK ambiguous = 1).
#[must_use]
pub fn east_asian_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

fn is_zero_width_char(c: char) -> bool {
    c.width().is_none_or(|width| width == 0)
}

fn is_zero_width_segment(segment: &str) -> bool {
    !segment.is_empty() && segment.chars().all(is_zero_width_char)
}

fn strip_leading_non_printing(segment: &str) -> &str {
    let byte_index = segment
        .char_indices()
        .find_map(|(index, ch)| (!is_zero_width_char(ch)).then_some(index))
        .unwrap_or(segment.len());
    &segment[byte_index..]
}

/// Fast pre-filter mirroring the TS `couldBeEmoji` heuristic.
#[must_use]
pub fn could_be_emoji(segment: &str) -> bool {
    let Some(cp) = segment.chars().next().map(|c| c as u32) else {
        return false;
    };
    (0x1f_000..=0x1f_bff).contains(&cp)
        || (0x2300..=0x23ff).contains(&cp)
        || (0x2600..=0x27bf).contains(&cp)
        || (0x2b50..=0x2b55).contains(&cp)
        || segment.contains('\u{FE0F}')
        || segment.chars().count() > 2
}

/// Approximate RGI emoji detection using `unicode-width` sequence rules.
fn is_rgi_emojiish(segment: &str) -> bool {
    if segment.width() == 2 && could_be_emoji(segment) {
        return true;
    }
    // ZWJ / skin-tone / presentation sequences that some tables report as width 1
    // still render as emoji cells in terminals pi targets.
    segment.contains('\u{200D}')
        || segment.contains('\u{FE0F}')
        || segment
            .chars()
            .any(|c| (0x1f_3fb..=0x1f_3ff).contains(&(c as u32)))
}

/// Terminal width of a single grapheme cluster.
#[must_use]
pub fn grapheme_width(segment: &str) -> usize {
    if segment == "\t" {
        return 3;
    }
    if segment.is_empty() || is_zero_width_segment(segment) {
        return 0;
    }
    if could_be_emoji(segment) && is_rgi_emojiish(segment) {
        return 2;
    }

    let base = strip_leading_non_printing(segment);
    let Some(cp) = base.chars().next() else {
        return 0;
    };
    let cp_u = cp as u32;
    if (0x1f_1e6..=0x1f_1ff).contains(&cp_u) {
        return 2;
    }

    let mut width = east_asian_width(cp);
    if segment.chars().count() > 1 {
        for c in segment.chars().skip(1) {
            let cu = c as u32;
            if (0xff00..=0xffef).contains(&cu) {
                width = width.saturating_add(east_asian_width(c));
            } else if cu == 0x0e33 || cu == 0x0eb3 {
                width = width.saturating_add(1);
            }
        }
    }
    width
}

/// Visible terminal columns of `str`, ignoring ANSI/OSC/APC and counting tabs as 3.
#[must_use]
pub fn visible_width(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    if is_printable_ascii(s) {
        return s.len();
    }
    if let Ok(cache) = width_cache().lock()
        && let Some(width) = cache.get(s)
    {
        return width;
    }

    let mut clean = s.to_owned();
    if clean.contains('\t') {
        clean = clean.replace('\t', "   ");
    }
    if clean.contains('\u{1b}') {
        let mut stripped = String::with_capacity(clean.len());
        let mut i = 0;
        while i < clean.len() {
            if let Some(ansi) = extract_ansi_code(&clean, i) {
                i += ansi.len;
                continue;
            }
            // Copy one UTF-8 char.
            let ch = clean[i..].chars().next().map_or(1, char::len_utf8);
            // SAFETY: i is a char boundary by construction.
            stripped.push_str(&clean[i..i + ch]);
            i += ch;
        }
        clean = stripped;
    }

    let mut width = 0usize;
    for grapheme in clean.graphemes(true) {
        width = width.saturating_add(grapheme_width(grapheme));
    }

    if let Ok(mut cache) = width_cache().lock() {
        cache.insert(s.to_owned(), width);
    }
    width
}

/// Normalize text for terminal output without changing logical editor content.
///
/// Expands standalone tabs outside escapes to three spaces and splits Thai/Lao
/// AM vowels so terminal cells match editor width accounting.
#[must_use]
pub fn normalize_terminal_output(s: &str) -> String {
    let mut normalized = s.to_owned();
    if normalized.contains('\u{0e33}') || normalized.contains('\u{0eb3}') {
        let mut out = String::with_capacity(normalized.len() + 8);
        for ch in normalized.chars() {
            match ch {
                '\u{0e33}' => out.push_str("\u{0e4d}\u{0e32}"),
                '\u{0eb3}' => out.push_str("\u{0ecd}\u{0eb2}"),
                other => out.push(other),
            }
        }
        normalized = out;
    }
    if !normalized.contains('\t') {
        return normalized;
    }

    let mut result = String::with_capacity(normalized.len());
    let mut i = 0;
    while i < normalized.len() {
        if let Some(ansi) = extract_ansi_code(&normalized, i) {
            result.push_str(ansi.code);
            i += ansi.len;
            continue;
        }
        let ch = normalized[i..].chars().next().unwrap_or('\0');
        if ch == '\t' {
            result.push_str("   ");
            i += 1;
        } else {
            let len = ch.len_utf8();
            result.push_str(&normalized[i..i + len]);
            i += len;
        }
    }
    result
}

/// `true` when the grapheme is a CJK break opportunity (Han/Hira/Kata/Hangul/Bopomofo).
#[must_use]
pub fn cjk_break_grapheme(segment: &str) -> bool {
    let Some(c) = segment.chars().next() else {
        return false;
    };
    matches!(
        c,
        // Bopomofo
        '\u{3100}'..='\u{312F}'
            | '\u{31A0}'..='\u{31BF}'
            // Hiragana / Katakana
            | '\u{3040}'..='\u{309F}'
            | '\u{30A0}'..='\u{30FF}'
            | '\u{31F0}'..='\u{31FF}'
            | '\u{FF65}'..='\u{FF9F}'
            // Hangul
            | '\u{1100}'..='\u{11FF}'
            | '\u{3130}'..='\u{318F}'
            | '\u{A960}'..='\u{A97F}'
            | '\u{AC00}'..='\u{D7AF}'
            | '\u{D7B0}'..='\u{D7FF}'
            // Han / CJK
            | '\u{2E80}'..='\u{2EFF}'
            | '\u{2F00}'..='\u{2FDF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2A6DF}'
            | '\u{2A700}'..='\u{2B73F}'
            | '\u{2B740}'..='\u{2B81F}'
            | '\u{2B820}'..='\u{2CEAF}'
            | '\u{2CEB0}'..='\u{2EBEF}'
            | '\u{30000}'..='\u{3134F}'
            | '\u{31350}'..='\u{323AF}'
    )
}

/// Whitespace character (Unicode `White_Space`).
#[must_use]
pub fn is_whitespace_char(ch: &str) -> bool {
    ch.chars().next().is_some_and(char::is_whitespace) && ch.chars().count() == 1
}

/// Punctuation character from the editor punctuation set.
#[must_use]
pub fn is_punctuation_char(ch: &str) -> bool {
    ch.chars().count() == 1 && ch.chars().next().is_some_and(|c| PUNCTUATION.contains(c))
}

/// Apply a background painter to a line after padding it to `width`.
///
/// `bg_fn` receives the already-padded content and returns styled text.
#[must_use]
pub fn apply_background_to_line(
    line: &str,
    width: usize,
    bg_fn: impl FnOnce(&str) -> String,
) -> String {
    let visible_len = visible_width(line);
    let padding_needed = width.saturating_sub(visible_len);
    let with_padding = format!("{line}{}", " ".repeat(padding_needed));
    bg_fn(&with_padding)
}
