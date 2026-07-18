//! Logical/visual wrap maps for the multiline editor.
//!
//! Ports `wordWrapLine` and `buildVisualLineMap` from
//! `.references/pi/packages/tui/src/components/editor.ts`.

use unicode_segmentation::UnicodeSegmentation;

use crate::text::{cjk_break_grapheme, is_whitespace_char, visible_width};

/// A chunk of a word-wrapped logical line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    /// Visible text of this chunk.
    pub text: String,
    /// Byte start index in the logical line.
    pub start_index: usize,
    /// Byte end index in the logical line (exclusive).
    pub end_index: usize,
}

/// One visual line in the wrap map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualLine {
    /// Index into the logical `lines` array.
    pub logical_line: usize,
    /// Starting byte column in the logical line.
    pub start_col: usize,
    /// Byte length of this visual segment.
    pub length: usize,
}

/// One grapheme (or atomic paste-marker) segment with its byte index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphemeSeg {
    /// Segment text.
    pub segment: String,
    /// Byte index in the source line.
    pub index: usize,
}

/// Split a line into word-wrapped chunks.
#[must_use]
pub fn word_wrap_line(
    line: &str,
    max_width: usize,
    pre_segmented: Option<&[GraphemeSeg]>,
) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk {
            text: String::new(),
            start_index: 0,
            end_index: 0,
        }];
    }

    if visible_width(line) <= max_width {
        return vec![TextChunk {
            text: line.to_owned(),
            start_index: 0,
            end_index: line.len(),
        }];
    }

    let owned: Vec<GraphemeSeg>;
    let segments = if let Some(pre_segmented) = pre_segmented {
        pre_segmented
    } else {
        owned = default_graphemes(line);
        &owned
    };
    wrap_segments(line, max_width, segments)
}

fn wrap_segments(line: &str, max_width: usize, segments: &[GraphemeSeg]) -> Vec<TextChunk> {
    let mut chunks = Vec::new();
    let mut current_width = 0usize;
    let mut chunk_start = 0usize;
    let mut wrap_opportunity = None;
    let mut wrap_opportunity_width = 0usize;

    let mut index = 0usize;
    while index < segments.len() {
        let segment = &segments[index];
        let grapheme = segment.segment.as_str();
        let grapheme_width = visible_width(grapheme);
        let byte_index = segment.index;
        let is_whitespace = !is_paste_marker_text(grapheme) && is_whitespace_char(grapheme);

        if current_width + grapheme_width > max_width {
            if let Some(opportunity) = wrap_opportunity {
                if current_width.saturating_sub(wrap_opportunity_width) + grapheme_width
                    <= max_width
                {
                    chunks.push(text_chunk(line, chunk_start, opportunity));
                    chunk_start = opportunity;
                    current_width -= wrap_opportunity_width;
                } else if chunk_start < byte_index {
                    chunks.push(text_chunk(line, chunk_start, byte_index));
                    chunk_start = byte_index;
                    current_width = 0;
                }
            } else if chunk_start < byte_index {
                chunks.push(text_chunk(line, chunk_start, byte_index));
                chunk_start = byte_index;
                current_width = 0;
            }
            wrap_opportunity = None;
        }

        if grapheme_width > max_width {
            (chunk_start, current_width) =
                append_wide_segment(&mut chunks, grapheme, byte_index, max_width);
            wrap_opportunity = None;
            index += 1;
            continue;
        }

        current_width += grapheme_width;
        if let Some(next) = segments.get(index + 1) {
            if is_whitespace
                && (is_paste_marker_text(&next.segment) || !is_whitespace_char(&next.segment))
            {
                wrap_opportunity = Some(next.index);
                wrap_opportunity_width = current_width;
            } else if !is_whitespace && !is_whitespace_char(&next.segment) {
                let is_cjk = !is_paste_marker_text(grapheme) && cjk_break_grapheme(grapheme);
                let next_is_cjk =
                    !is_paste_marker_text(&next.segment) && cjk_break_grapheme(&next.segment);
                if is_cjk || next_is_cjk {
                    wrap_opportunity = Some(next.index);
                    wrap_opportunity_width = current_width;
                }
            }
        }
        index += 1;
    }

    chunks.push(text_chunk(line, chunk_start, line.len()));
    chunks
}

fn append_wide_segment(
    chunks: &mut Vec<TextChunk>,
    grapheme: &str,
    byte_index: usize,
    max_width: usize,
) -> (usize, usize) {
    let subsegments = default_graphemes(grapheme);
    if subsegments.len() <= 1 {
        chunks.push(TextChunk {
            text: grapheme.to_owned(),
            start_index: byte_index,
            end_index: byte_index + grapheme.len(),
        });
        return (byte_index + grapheme.len(), 0);
    }

    let subchunks = word_wrap_line(grapheme, max_width, Some(&subsegments));
    for chunk in subchunks.iter().take(subchunks.len().saturating_sub(1)) {
        chunks.push(TextChunk {
            text: chunk.text.clone(),
            start_index: byte_index + chunk.start_index,
            end_index: byte_index + chunk.end_index,
        });
    }
    match subchunks.last() {
        Some(last) => (byte_index + last.start_index, visible_width(&last.text)),
        None => (byte_index, 0),
    }
}

fn text_chunk(line: &str, start_index: usize, end_index: usize) -> TextChunk {
    TextChunk {
        text: line[start_index..end_index].to_owned(),
        start_index,
        end_index,
    }
}

/// Build the visual-line map for a multi-line buffer.
#[must_use]
pub fn build_visual_line_map(
    lines: &[String],
    width: usize,
    segment_line: impl Fn(&str) -> Vec<GraphemeSeg>,
) -> Vec<VisualLine> {
    let mut visual_lines = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.is_empty() {
            visual_lines.push(VisualLine {
                logical_line: i,
                start_col: 0,
                length: 0,
            });
            continue;
        }
        let line_vis = visible_width(line);
        if line_vis <= width {
            visual_lines.push(VisualLine {
                logical_line: i,
                start_col: 0,
                length: line.len(),
            });
        } else {
            let segs = segment_line(line);
            let chunks = word_wrap_line(line, width, Some(&segs));
            for chunk in chunks {
                visual_lines.push(VisualLine {
                    logical_line: i,
                    start_col: chunk.start_index,
                    length: chunk.end_index - chunk.start_index,
                });
            }
        }
    }
    if visual_lines.is_empty() {
        visual_lines.push(VisualLine {
            logical_line: 0,
            start_col: 0,
            length: 0,
        });
    }
    visual_lines
}

/// Find the visual-line index that contains the given logical position.
#[must_use]
pub fn find_visual_line_at(visual_lines: &[VisualLine], line: usize, col: usize) -> usize {
    for (i, vl) in visual_lines.iter().enumerate() {
        if vl.logical_line != line {
            continue;
        }
        if col < vl.start_col {
            continue;
        }
        let offset = col - vl.start_col;
        let is_last_segment_of_line =
            i + 1 >= visual_lines.len() || visual_lines[i + 1].logical_line != vl.logical_line;
        if offset < vl.length || (is_last_segment_of_line && offset == vl.length) {
            return i;
        }
    }
    visual_lines.len().saturating_sub(1)
}

/// Default grapheme segmentation (no paste-marker merging).
#[must_use]
pub fn default_graphemes(text: &str) -> Vec<GraphemeSeg> {
    text.grapheme_indices(true)
        .map(|(index, segment)| GraphemeSeg {
            segment: segment.to_owned(),
            index,
        })
        .collect()
}

fn is_paste_marker_text(segment: &str) -> bool {
    segment.len() >= 10 && segment.starts_with("[paste #") && segment.ends_with(']')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_line_single_chunk() {
        let chunks = word_wrap_line("hello", 80, None);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello");
        assert_eq!(chunks[0].start_index, 0);
        assert_eq!(chunks[0].end_index, 5);
    }

    #[test]
    fn wraps_at_word_boundary() {
        let line = "hello world!";
        let chunks = word_wrap_line(line, 8, None);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|c| visible_width(&c.text) <= 8));
        // Prefer wrap after whitespace: first chunk should end at/after "hello".
        assert!(chunks[0].text.starts_with("hello"));
        assert!(!chunks[0].text.contains('!'));
    }

    #[test]
    fn force_breaks_long_token() {
        let line = "abcdefghijklmnopqrstuvwxyz";
        let chunks = word_wrap_line(line, 10, None);
        assert!(chunks.len() >= 3);
        for c in &chunks {
            assert!(visible_width(&c.text) <= 10);
        }
        let mut rebuilt = String::new();
        for c in &chunks {
            rebuilt.push_str(&c.text);
        }
        assert_eq!(rebuilt, line);
    }

    #[test]
    fn visual_map_empty_and_wrapped() {
        let lines = vec![String::new(), "hello world".to_owned()];
        let map = build_visual_line_map(&lines, 5, default_graphemes);
        assert_eq!(map[0].logical_line, 0);
        assert_eq!(map[0].length, 0);
        assert!(map.iter().any(|vl| vl.logical_line == 1));
        let idx = find_visual_line_at(&map, 1, 0);
        assert_eq!(map[idx].logical_line, 1);
    }

    #[test]
    fn empty_input_chunk() {
        let chunks = word_wrap_line("", 10, None);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "");
    }
}
