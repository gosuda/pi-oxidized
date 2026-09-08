//! Native fullscreen document and viewport primitives.

mod document;
mod search;
mod viewport;

pub use document::{DocumentBlock, DocumentBlockId, DocumentMetrics, LineDocument, PromptZone};
pub use search::{
    AltScreenSearchIndex, AltScreenSearchMatch, AltScreenSearchSegment, SearchDirection,
    SearchIndex, SearchMatch, SearchResult, SearchSegment, TranscriptSearch,
    find_search_matches, get_alt_screen_search_match_key, search_match_key,
    transcript_search_rect,
};
pub use viewport::{
    FollowPolicy, FullscreenEffect, FullscreenEventResult, FullscreenOptions, FullscreenStyle,
    FullscreenViewport, Overscroll, ScrollTarget, ScrollbarGeometry, ScrollbarMode,
};
