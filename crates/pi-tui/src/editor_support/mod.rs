//! Editor support helpers: history, kill-ring, undo, word navigation, wrap, autocomplete.
//!
//! Product-agnostic pure logic shared by the multiline [`crate::components::editor`]
//! and the single-line input component.

mod autocomplete;
mod history;
mod kill_ring;
mod undo;
mod word_nav;
mod wrap;

pub use autocomplete::ApplyCompletionResult;
pub use autocomplete::{
    ATTACHMENT_AUTOCOMPLETE_DEBOUNCE_MS, AutocompleteItem, AutocompleteProvider,
    AutocompleteSuggestions, CombinedAutocompleteProvider, DEFAULT_AUTOCOMPLETE_TRIGGER_CHARACTERS,
    FileEntry, FileLister, SlashCommand, SuggestionOptions,
};
pub use history::{CursorPlacement, HISTORY_CAP, History, HistoryNavigateResult};
pub use kill_ring::{KillPushOptions, KillRing};
pub use undo::UndoStack;
pub use word_nav::{
    WordNavigationOptions, WordSegment, default_word_segments, find_word_backward,
    find_word_forward,
};
pub use wrap::{
    GraphemeSeg, TextChunk, VisualLine, build_visual_line_map, default_graphemes,
    find_visual_line_at, word_wrap_line,
};
