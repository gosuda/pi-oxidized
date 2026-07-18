//! Unicode-aware word navigation with paste-marker atomic segments.
//!
//! Ports `.references/pi/packages/tui/src/word-navigation.ts`. Word bounds use
//! `unicode_segmentation::UnicodeSegmentation::split_word_bound_indices`, which
//! is the Rust equivalent of `Intl.Segmenter({ granularity: "word" })` for the
//! editor's observable step behaviour (including CJK per-character words and
//! ASCII punctuation subdivision via [`crate::text::PUNCTUATION`]).

use unicode_segmentation::UnicodeSegmentation;

use crate::text::{PUNCTUATION, is_punctuation_char};

/// One word-bound segment, mirroring `Intl.SegmentData`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordSegment {
    /// Segment text.
    pub segment: String,
    /// Byte offset in the input this segment was produced from.
    pub index: usize,
    /// True when the segment is word-like (not pure whitespace/punctuation).
    pub is_word_like: bool,
}

/// Custom word segmenter callback.
pub type WordSegmentFn<'a> = dyn Fn(&str) -> Vec<WordSegment> + 'a;
/// Predicate identifying atomic multi-grapheme units (paste markers).
pub type AtomicSegmentFn<'a> = dyn Fn(&str) -> bool + 'a;

/// Options for word navigation.
#[derive(Default)]
pub struct WordNavigationOptions<'a> {
    /// Custom segmenter. When omitted, UAX #29 word bounds are used.
    pub segment: Option<&'a WordSegmentFn<'a>>,
    /// Predicate identifying atomic multi-grapheme units (paste markers).
    pub is_atomic_segment: Option<&'a AtomicSegmentFn<'a>>,
}

/// Segment `text` with UAX #29 word bounds and mark word-like segments.
#[must_use]
pub fn default_word_segments(text: &str) -> Vec<WordSegment> {
    text.split_word_bound_indices()
        .map(|(index, segment)| WordSegment {
            is_word_like: is_word_like(segment),
            segment: segment.to_owned(),
            index,
        })
        .collect()
}

fn is_word_like(segment: &str) -> bool {
    if segment.is_empty() || is_ws_segment(segment) {
        return false;
    }
    // Pure ASCII-punctuation runs are not word-like.
    if segment.chars().all(|c| PUNCTUATION.contains(c)) {
        return false;
    }
    true
}

fn is_ws_segment(segment: &str) -> bool {
    // Intl.Segmenter whitespace segments may be multi-char runs.
    !segment.is_empty() && segment.chars().all(char::is_whitespace)
}

fn is_atomic(opts: &WordNavigationOptions<'_>, segment: &str) -> bool {
    opts.is_atomic_segment.is_some_and(|f| f(segment))
}

fn segments_for(text: &str, opts: &WordNavigationOptions<'_>) -> Vec<WordSegment> {
    if let Some(segment_fn) = opts.segment {
        segment_fn(text)
    } else {
        default_word_segments(text)
    }
}

/// Cursor position after moving one word backward from `cursor` (byte index).
#[must_use]
pub fn find_word_backward(text: &str, cursor: usize, opts: &WordNavigationOptions<'_>) -> usize {
    if cursor == 0 || text.is_empty() {
        return 0;
    }
    let cursor = cursor.min(text.len());
    let cursor = prev_boundary(text, cursor);
    let text_before = &text[..cursor];
    let mut segments = segments_for(text_before, opts);
    let mut new_cursor = cursor;

    // Skip trailing whitespace
    while let Some(last) = segments.last() {
        if !is_atomic(opts, &last.segment) && is_ws_segment(&last.segment) {
            new_cursor = new_cursor.saturating_sub(last.segment.len());
            segments.pop();
        } else {
            break;
        }
    }

    let Some(last) = segments.last() else {
        return new_cursor;
    };

    if is_atomic(opts, &last.segment) {
        new_cursor = new_cursor.saturating_sub(last.segment.len());
    } else if last.is_word_like {
        let segment = &last.segment;
        if let Some(pos) = last_punctuation_end(segment) {
            new_cursor = new_cursor.saturating_sub(segment.len() - pos);
        } else {
            new_cursor = new_cursor.saturating_sub(segment.len());
        }
    } else {
        // Skip non-word non-whitespace run (punctuation)
        while let Some(last) = segments.last() {
            if !is_atomic(opts, &last.segment)
                && !last.is_word_like
                && !is_ws_segment(&last.segment)
            {
                new_cursor = new_cursor.saturating_sub(last.segment.len());
                segments.pop();
            } else {
                break;
            }
        }
    }

    new_cursor
}

/// Cursor position after moving one word forward from `cursor` (byte index).
#[must_use]
pub fn find_word_forward(text: &str, cursor: usize, opts: &WordNavigationOptions<'_>) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let cursor = next_boundary(text, cursor);
    let text_after = &text[cursor..];
    let segments = segments_for(text_after, opts);
    let mut iter = segments.into_iter();
    let mut new_cursor = cursor;
    let mut next = iter.next();

    // Skip leading whitespace
    while let Some(seg) = &next {
        if !is_atomic(opts, &seg.segment) && is_ws_segment(&seg.segment) {
            new_cursor += seg.segment.len();
            next = iter.next();
        } else {
            break;
        }
    }

    let Some(seg) = next else {
        return new_cursor;
    };

    if is_atomic(opts, &seg.segment) {
        new_cursor += seg.segment.len();
    } else if seg.is_word_like {
        new_cursor += first_punctuation_index(&seg.segment).unwrap_or(seg.segment.len());
    } else {
        new_cursor += seg.segment.len();
        for more in iter {
            if !is_atomic(opts, &more.segment)
                && !more.is_word_like
                && !is_ws_segment(&more.segment)
            {
                new_cursor += more.segment.len();
            } else {
                break;
            }
        }
    }

    new_cursor
}

fn last_punctuation_end(segment: &str) -> Option<usize> {
    let mut last_end: Option<usize> = None;
    let mut offset = 0usize;
    for ch in segment.chars() {
        let len = ch.len_utf8();
        if is_punctuation_char(&segment[offset..offset + len]) {
            last_end = Some(offset + len);
        }
        offset += len;
    }
    last_end
}

fn first_punctuation_index(segment: &str) -> Option<usize> {
    let mut offset = 0usize;
    for ch in segment.chars() {
        let len = ch.len_utf8();
        if is_punctuation_char(&segment[offset..offset + len]) {
            return Some(offset);
        }
        offset += len;
    }
    None
}

fn prev_boundary(text: &str, mut idx: usize) -> usize {
    if idx > text.len() {
        return text.len();
    }
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn next_boundary(text: &str, mut idx: usize) -> usize {
    if idx >= text.len() {
        return text.len();
    }
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn back(text: &str, cursor: usize) -> usize {
        find_word_backward(text, cursor, &WordNavigationOptions::default())
    }

    fn fwd(text: &str, cursor: usize) -> usize {
        find_word_forward(text, cursor, &WordNavigationOptions::default())
    }

    #[test]
    fn basic_words_backward_forward() {
        let text = "hello world";
        assert_eq!(back(text, 11), 6);
        assert_eq!(back(text, 6), 0);
        assert_eq!(fwd(text, 0), 5);
        assert_eq!(fwd(text, 5), 11);
    }

    #[test]
    fn dotted_and_colon() {
        for text in ["foo.bar", "foo:bar"] {
            assert_eq!(back(text, 7), 4);
            assert_eq!(back(text, 4), 3);
            assert_eq!(back(text, 3), 0);
            assert_eq!(fwd(text, 0), 3);
            assert_eq!(fwd(text, 3), 4);
            assert_eq!(fwd(text, 4), 7);
        }
    }

    #[test]
    fn path_segments() {
        let text = "path/to/file";
        assert_eq!(back(text, 12), 8);
        assert_eq!(back(text, 8), 7);
        assert_eq!(back(text, 7), 5);
        assert_eq!(back(text, 5), 4);
        assert_eq!(back(text, 4), 0);
        assert_eq!(fwd(text, 0), 4);
        assert_eq!(fwd(text, 4), 5);
        assert_eq!(fwd(text, 5), 7);
        assert_eq!(fwd(text, 7), 8);
        assert_eq!(fwd(text, 8), 12);
    }

    #[test]
    fn punctuation_run() {
        let text = "foo...bar";
        assert_eq!(back(text, 9), 6);
        assert_eq!(back(text, 6), 3);
        assert_eq!(back(text, 3), 0);
        assert_eq!(fwd(text, 0), 3);
        assert_eq!(fwd(text, 3), 6);
        assert_eq!(fwd(text, 6), 9);
    }

    #[test]
    fn whitespace_boundaries() {
        let text = "  hello  ";
        assert_eq!(back(text, 9), 2);
        assert_eq!(back(text, 2), 0);
        assert_eq!(fwd(text, 0), 7);
        assert_eq!(fwd(text, 7), 9);
    }

    #[test]
    fn cursor_edges() {
        assert_eq!(back("hello", 0), 0);
        assert_eq!(fwd("hello", 5), 5);
    }

    #[test]
    fn cjk_mixed_walks_to_end() {
        // UTF-8 byte indices: 你好世界 = 12 bytes, space = 1, "test" = 4.
        let text = "你好世界 test";
        assert_eq!(back(text, text.len()), 13);
        assert_eq!(back(text, 13), 9);
        assert_eq!(back(text, 9), 6);
        assert_eq!(back(text, 6), 3);
        assert_eq!(back(text, 3), 0);

        let first_end = fwd(text, 0);
        assert!(first_end > 0 && first_end <= 12);
        let mut pos = 0;
        while pos < text.len() {
            let next = fwd(text, pos);
            if next == pos {
                break;
            }
            pos = next;
        }
        assert_eq!(pos, text.len());
    }

    #[test]
    fn atomic_segments() {
        let marker = "[paste #1 +5 lines]";
        let text = format!("hello {marker} world");
        let is_atomic = |s: &str| s == marker;

        let segment = |input: &str| -> Vec<WordSegment> {
            if input == text.as_str() {
                return vec![
                    WordSegment {
                        segment: "hello".into(),
                        index: 0,
                        is_word_like: true,
                    },
                    WordSegment {
                        segment: " ".into(),
                        index: 5,
                        is_word_like: false,
                    },
                    WordSegment {
                        segment: marker.into(),
                        index: 6,
                        is_word_like: true,
                    },
                    WordSegment {
                        segment: " ".into(),
                        index: 25,
                        is_word_like: false,
                    },
                    WordSegment {
                        segment: "world".into(),
                        index: 26,
                        is_word_like: true,
                    },
                ];
            }
            if input == &text[..26] {
                return vec![
                    WordSegment {
                        segment: "hello".into(),
                        index: 0,
                        is_word_like: true,
                    },
                    WordSegment {
                        segment: " ".into(),
                        index: 5,
                        is_word_like: false,
                    },
                    WordSegment {
                        segment: marker.into(),
                        index: 6,
                        is_word_like: true,
                    },
                    WordSegment {
                        segment: " ".into(),
                        index: 25,
                        is_word_like: false,
                    },
                ];
            }
            if input == &text[6..] {
                return vec![
                    WordSegment {
                        segment: marker.into(),
                        index: 0,
                        is_word_like: true,
                    },
                    WordSegment {
                        segment: " ".into(),
                        index: 19,
                        is_word_like: false,
                    },
                    WordSegment {
                        segment: "world".into(),
                        index: 20,
                        is_word_like: true,
                    },
                ];
            }
            Vec::new()
        };

        let opts = WordNavigationOptions {
            segment: Some(&segment),
            is_atomic_segment: Some(&is_atomic),
        };

        assert_eq!(find_word_backward(&text, text.len(), &opts), 26);
        assert_eq!(find_word_backward(&text, 26, &opts), 6);
        assert_eq!(find_word_forward(&text, 6, &opts), 6 + marker.len());
    }
}
