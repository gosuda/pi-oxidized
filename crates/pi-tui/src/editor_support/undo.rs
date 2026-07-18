//! Unbounded clone-on-push undo stack (no redo).

/// Generic undo stack with clone-on-push semantics.
///
/// Matches TS `UndoStack<S>`: `push` stores a clone, `pop` returns the
/// detached snapshot, no capacity limit, no redo.
#[derive(Debug, Clone, Default)]
pub struct UndoStack<S> {
    stack: Vec<S>,
}

impl<S: Clone> UndoStack<S> {
    /// Create an empty stack.
    #[must_use]
    pub fn new() -> Self {
        Self { stack: Vec::new() }
    }

    /// Push a clone of `state`.
    pub fn push(&mut self, state: &S) {
        self.stack.push(state.clone());
    }

    /// Pop the most recent snapshot.
    pub fn pop(&mut self) -> Option<S> {
        self.stack.pop()
    }

    /// Remove all snapshots.
    pub fn clear(&mut self) {
        self.stack.clear();
    }

    /// Number of snapshots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.stack.len()
    }

    /// True when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_clear() {
        let mut stack = UndoStack::new();
        stack.push(&"a".to_owned());
        stack.push(&"b".to_owned());
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.pop().as_deref(), Some("b"));
        assert_eq!(stack.pop().as_deref(), Some("a"));
        assert!(stack.pop().is_none());
        stack.push(&"c".to_owned());
        stack.clear();
        assert!(stack.is_empty());
    }

    #[test]
    fn push_clones_state() {
        let mut stack = UndoStack::new();
        let mut state = vec![1, 2, 3];
        stack.push(&state);
        state.push(4);
        assert_eq!(stack.pop(), Some(vec![1, 2, 3]));
    }
}
