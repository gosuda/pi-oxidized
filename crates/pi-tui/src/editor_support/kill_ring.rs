//! Emacs-style kill ring with accumulate and rotate.

/// Options for [`KillRing::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KillPushOptions {
    /// Prepend (backward kill) vs append (forward kill) when accumulating.
    pub prepend: bool,
    /// Merge into the most recent entry instead of creating a new one.
    pub accumulate: bool,
}

/// Ring buffer for kill/yank/yank-pop.
#[derive(Debug, Clone, Default)]
pub struct KillRing {
    ring: Vec<String>,
}

impl KillRing {
    /// Create an empty kill ring.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// True when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// Push killed text, optionally accumulating into the newest entry.
    pub fn push(&mut self, text: &str, opts: KillPushOptions) {
        if text.is_empty() {
            return;
        }
        if opts.accumulate
            && let Some(last) = self.ring.pop()
        {
            let merged = if opts.prepend {
                format!("{text}{last}")
            } else {
                format!("{last}{text}")
            };
            self.ring.push(merged);
            return;
        }
        self.ring.push(text.to_owned());
    }

    /// Most recent entry without modifying the ring.
    #[must_use]
    pub fn peek(&self) -> Option<&str> {
        self.ring.last().map(String::as_str)
    }

    /// Move the last entry to the front (yank-pop cycle).
    pub fn rotate(&mut self) {
        if self.ring.len() > 1
            && let Some(last) = self.ring.pop()
        {
            self.ring.insert(0, last);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(ring: &mut KillRing, text: &str, prepend: bool, accumulate: bool) {
        ring.push(
            text,
            KillPushOptions {
                prepend,
                accumulate,
            },
        );
    }

    #[test]
    fn push_and_peek() {
        let mut ring = KillRing::new();
        assert!(ring.peek().is_none());
        push(&mut ring, "abc", false, false);
        assert_eq!(ring.peek(), Some("abc"));
        push(&mut ring, "def", false, false);
        assert_eq!(ring.peek(), Some("def"));
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn accumulate_prepend_and_append() {
        let mut ring = KillRing::new();
        push(&mut ring, "world", true, false);
        push(&mut ring, "hello ", true, true);
        assert_eq!(ring.peek(), Some("hello world"));

        let mut ring = KillRing::new();
        push(&mut ring, "hello", false, false);
        push(&mut ring, " world", false, true);
        assert_eq!(ring.peek(), Some("hello world"));
    }

    #[test]
    fn empty_push_ignored() {
        let mut ring = KillRing::new();
        push(&mut ring, "", false, false);
        assert!(ring.is_empty());
    }

    #[test]
    fn rotate_cycles_for_yank_pop() {
        let mut ring = KillRing::new();
        push(&mut ring, "a", false, false);
        push(&mut ring, "b", false, false);
        push(&mut ring, "c", false, false);
        assert_eq!(ring.peek(), Some("c"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("b"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("a"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("c"));
    }

    #[test]
    fn rotate_noop_for_single_entry() {
        let mut ring = KillRing::new();
        push(&mut ring, "only", false, false);
        ring.rotate();
        assert_eq!(ring.peek(), Some("only"));
    }
}
