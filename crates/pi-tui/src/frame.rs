//! Per-frame annotations collected during pure composition.

use std::cell::RefCell;
use std::thread::LocalKey;

use ratatui::layout::{Position, Rect};

/// Out-of-band raw bytes painted after the Ratatui cell draw for one region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRegion {
    /// Absolute terminal rectangle covered by the raw payload.
    pub area: Rect,
    /// Pre-encoded protocol bytes (Kitty/iTerm2 image, etc.).
    pub bytes: Vec<u8>,
    /// Optional Kitty image id that must be deleted when the region disappears.
    pub kitty_id: Option<u32>,
}

/// Annotations collected while composing one frame.
#[derive(Debug, Default, Clone)]
pub struct FrameAnnotations {
    /// Hardware-cursor position requested by a component, if any.
    cursor: Option<Position>,
    /// Raw regions to emit after the cell draw in the same write.
    raw_regions: Vec<RawRegion>,
}

impl FrameAnnotations {
    /// Create empty annotations.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Request a hardware cursor position for this frame.
    pub fn set_cursor(&mut self, position: Position) {
        self.cursor = Some(position);
    }

    /// Current hardware-cursor request.
    #[must_use]
    pub fn cursor(&self) -> Option<Position> {
        self.cursor
    }

    /// Register a raw region painted after the cell buffer.
    pub fn push_raw_region(&mut self, region: RawRegion) {
        self.raw_regions.push(region);
    }

    /// Borrow registered raw regions.
    #[must_use]
    pub fn raw_regions(&self) -> &[RawRegion] {
        &self.raw_regions
    }

    /// Consume annotations after composition.
    #[must_use]
    pub fn into_parts(self) -> (Option<Position>, Vec<RawRegion>) {
        (self.cursor, self.raw_regions)
    }
}

thread_local! {
    static ANNOTATIONS: RefCell<Option<FrameAnnotations>> = const { RefCell::new(None) };
}

fn annotations_slot() -> &'static LocalKey<RefCell<Option<FrameAnnotations>>> {
    &ANNOTATIONS
}

struct AnnotationsGuard<'a> {
    target: &'a RefCell<FrameAnnotations>,
    previous: Option<FrameAnnotations>,
    active: bool,
}

impl Drop for AnnotationsGuard<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let collected = annotations_slot().with(|slot| slot.replace(self.previous.take()));
        if let Some(collected) = collected {
            *self.target.borrow_mut() = collected;
        }
    }
}

/// Install temporary annotations for the duration of `f`.
///
/// Restores the previous thread-local slot and commits collected annotations
/// even if `f` panics.
pub fn with_annotations<R>(annotations: &RefCell<FrameAnnotations>, f: impl FnOnce() -> R) -> R {
    let previous = annotations_slot().with(|slot| slot.replace(Some(FrameAnnotations::new())));
    let mut guard = AnnotationsGuard {
        target: annotations,
        previous,
        active: true,
    };
    let result = f();
    guard.active = false;
    let collected = annotations_slot().with(|slot| slot.replace(guard.previous.take()));
    if let Some(collected) = collected {
        *annotations.borrow_mut() = collected;
    }
    result
}

/// Access the current frame annotations while inside [`with_annotations`].
pub fn with_current_annotations<R>(f: impl FnOnce(&mut FrameAnnotations) -> R) -> Option<R> {
    annotations_slot().with(|slot| {
        let mut borrow = slot.borrow_mut();
        borrow.as_mut().map(f)
    })
}

/// Request a hardware cursor for the current composition frame.
pub fn set_cursor(position: Position) {
    let _ = with_current_annotations(|annotations| annotations.set_cursor(position));
}

/// Register a raw region on the current composition frame.
pub fn push_raw_region(region: RawRegion) {
    let _ = with_current_annotations(|annotations| annotations.push_raw_region(region));
}

#[cfg(test)]
mod tests {
    use super::{
        FrameAnnotations, RawRegion, push_raw_region, set_cursor, with_annotations,
        with_current_annotations,
    };
    use ratatui::layout::{Position, Rect};
    use std::cell::RefCell;

    #[test]
    fn annotations_collect_cursor_and_raw_regions() {
        let slot = RefCell::new(FrameAnnotations::new());
        with_annotations(&slot, || {
            set_cursor(Position { x: 3, y: 4 });
            push_raw_region(RawRegion {
                area: Rect::new(0, 1, 2, 2),
                bytes: b"img".to_vec(),
                kitty_id: Some(7),
            });
        });
        let annotations = slot.into_inner();
        assert_eq!(annotations.cursor(), Some(Position { x: 3, y: 4 }));
        assert_eq!(annotations.raw_regions().len(), 1);
        assert_eq!(annotations.raw_regions()[0].kitty_id, Some(7));
    }

    #[test]
    fn annotations_restore_slot_after_composition() {
        let slot = RefCell::new(FrameAnnotations::new());
        with_annotations(&slot, || {
            set_cursor(Position { x: 1, y: 2 });
        });
        assert_eq!(slot.borrow().cursor(), Some(Position { x: 1, y: 2 }));
        assert!(with_current_annotations(|_| ()).is_none());
    }
}
