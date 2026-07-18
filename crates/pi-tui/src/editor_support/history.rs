//! Prompt history: newest-first, consecutive-dedupe, cap 100, draft capture.

/// Maximum number of retained history entries (TS editor `addToHistory` cap).
pub const HISTORY_CAP: usize = 100;

/// Newest-first prompt history with draft capture while browsing.
#[derive(Debug, Clone, Default)]
pub struct History {
    entries: Vec<String>,
    /// `None` = live editor; `Some(0)` = most recent; larger = older.
    index: Option<usize>,
    /// Snapshot of the live editor text captured on first entry into history.
    draft: String,
}

impl History {
    /// Create an empty history browser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of retained history entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no history has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Current browse index (`-1` means not browsing).
    #[must_use]
    pub fn index(&self) -> isize {
        self.index.map_or(-1, usize::cast_signed)
    }

    /// True when the user is browsing history rather than the live draft.
    #[must_use]
    pub fn is_browsing(&self) -> bool {
        self.index.is_some()
    }

    /// Borrow the draft captured when history browsing started.
    #[must_use]
    pub fn draft(&self) -> Option<&str> {
        self.is_browsing().then_some(self.draft.as_str())
    }

    /// Borrow the retained entries (newest first).
    #[must_use]
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Add a prompt after successful submit.
    ///
    /// Empty/whitespace-only text is ignored. Consecutive duplicates of the
    /// newest entry are skipped. Capacity is capped at [`HISTORY_CAP`].
    pub fn add(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.entries.first().is_some_and(|newest| newest == trimmed) {
            return;
        }
        self.entries.insert(0, trimmed.to_owned());
        if self.entries.len() > HISTORY_CAP {
            self.entries.pop();
        }
    }

    /// Navigate history by `direction`: `-1` = older (Up), `+1` = newer (Down).
    ///
    /// Returns the text to load into the editor:
    /// - `Some(Ok(entry))` for a history entry
    /// - `Some(Err(draft))` when returning to the live draft (`Ok`/`Err` only
    ///   distinguishes entry vs draft; both carry the text to install)
    /// - `None` when navigation is a no-op (empty history or out of range)
    ///
    /// On first enter from live mode, `capture_draft` is stored as the draft
    /// and the caller should push an undo snapshot.
    pub fn navigate(
        &mut self,
        direction: i8,
        capture_draft: &str,
    ) -> Option<HistoryNavigateResult> {
        if self.entries.is_empty() {
            return None;
        }

        let current = match self.index {
            Some(index) => i128::try_from(index).ok()?,
            None => -1,
        };
        let new_index = current.checked_sub(i128::from(direction))?;
        if new_index < -1 {
            return None;
        }
        let next_index = if new_index == -1 {
            None
        } else {
            let index = usize::try_from(new_index).ok()?;
            if index >= self.entries.len() {
                return None;
            }
            Some(index)
        };

        let entering = self.index.is_none() && next_index.is_some();
        if entering {
            capture_draft.clone_into(&mut self.draft);
        }
        self.index = next_index;

        let Some(index) = self.index else {
            let draft = std::mem::take(&mut self.draft);
            return Some(HistoryNavigateResult {
                text: draft,
                cursor_placement: CursorPlacement::End,
                restored_draft: true,
                entered: entering,
            });
        };

        let text = self.entries[index].clone();
        // direction -1 (up/older) → cursor at start; direction +1 (down/newer) → end
        let cursor_placement = if direction < 0 {
            CursorPlacement::Start
        } else {
            CursorPlacement::End
        };
        Some(HistoryNavigateResult {
            text,
            cursor_placement,
            restored_draft: false,
            entered: entering,
        })
    }

    /// Exit history browsing without restoring the draft into the editor.
    pub fn exit_browsing(&mut self) {
        self.index = None;
        self.draft.clear();
    }
}

/// Where to place the cursor after loading a history entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorPlacement {
    /// Start of the first line.
    Start,
    /// End of the last line.
    End,
}

/// Result of a successful history navigation step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryNavigateResult {
    /// Text to install into the editor.
    pub text: String,
    /// Cursor placement after install.
    pub cursor_placement: CursorPlacement,
    /// True when the live draft was restored (`index` returned to `-1`).
    pub restored_draft: bool,
    /// True when this step first entered history from live mode.
    pub entered: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_dedupes_newest_and_caps() {
        let mut h = History::new();
        h.add("  one  ");
        h.add("one");
        h.add("");
        h.add("two");
        assert_eq!(h.entries(), &["two".to_owned(), "one".to_owned()]);

        for i in 0..120 {
            h.add(&format!("e{i}"));
        }
        assert_eq!(h.len(), HISTORY_CAP);
        assert_eq!(h.entries()[0], "e119");
    }

    #[test]
    fn navigate_captures_draft_and_restores() -> Result<(), String> {
        let mut h = History::new();
        h.add("older");
        h.add("newer");

        let first = h
            .navigate(-1, "draft text")
            .ok_or_else(|| "enter history".to_owned())?;
        assert!(first.entered);
        assert!(!first.restored_draft);
        assert_eq!(first.text, "newer");
        assert_eq!(first.cursor_placement, CursorPlacement::Start);
        assert!(h.is_browsing());

        let older = h
            .navigate(-1, "ignored")
            .ok_or_else(|| "older entry".to_owned())?;
        assert_eq!(older.text, "older");

        let back = h
            .navigate(1, "ignored")
            .ok_or_else(|| "newer again".to_owned())?;
        assert_eq!(back.text, "newer");
        assert_eq!(back.cursor_placement, CursorPlacement::End);

        let draft = h
            .navigate(1, "ignored")
            .ok_or_else(|| "restore draft".to_owned())?;
        assert!(draft.restored_draft);
        assert_eq!(draft.text, "draft text");
        assert!(!h.is_browsing());
        assert!(h.draft().is_none());
        Ok(())
    }

    #[test]
    fn navigate_out_of_range_is_noop() {
        let mut h = History::new();
        assert!(h.navigate(-1, "x").is_none());
        h.add("only");
        let _ = h.navigate(-1, "d");
        assert!(h.navigate(-1, "d").is_none());
    }

    #[test]
    fn exit_browsing_drops_draft() {
        let mut h = History::new();
        h.add("a");
        let _ = h.navigate(-1, "draft");
        h.exit_browsing();
        assert!(!h.is_browsing());
        assert!(h.draft().is_none());
    }
}
