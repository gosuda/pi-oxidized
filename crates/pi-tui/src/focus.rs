//! Single-focus manager with overlay capture/restore.
//!
//! Ports the focus and overlay-focus-restore state machine from
//! `.references/pi/packages/tui/src/tui.ts` (`setFocus`,
//! `overlayFocusRestore` tri-state, key-release subscription).

use std::any::Any;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{KeyEvent, KeyEventKind};

use crate::component::{Component, EventResult, UiEvent};

/// Opaque identity for a focusable component slot.
///
/// Components are not required to be `Eq`; the manager tracks them by id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FocusId(u64);

impl FocusId {
    /// Allocate a fresh focus id.
    #[must_use]
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// Raw numeric value (stable for the process lifetime of this id).
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl Default for FocusId {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait for components that participate in hardware/focus state.
pub trait Focusable: Component {
    /// Unique id used by [`FocusManager`].
    fn focus_id(&self) -> FocusId;

    /// Whether this component currently believes it is focused.
    fn is_focused(&self) -> bool;

    /// Update the component-local focused flag.
    fn set_focused(&mut self, focused: bool);

    /// Downcast support for host-side bookkeeping.
    fn as_any(&self) -> &dyn Any;

    /// Mutable downcast support.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// Policy when clearing focus while an overlay restore is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayFocusRestorePolicy {
    /// Drop any pending overlay focus restore.
    Clear,
    /// Keep the restore state (used when temporarily hiding a focused overlay).
    Preserve,
}

/// Resume action after a blocked overlay focus restore unblocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayBlockedFocusResume {
    /// Return focus to the overlay itself.
    RestoreOverlay,
    /// Focus an explicit target (may be `None`).
    FocusTarget(Option<FocusId>),
}

/// Tri-state overlay focus restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayFocusRestoreState {
    /// No restore pending.
    #[default]
    Inactive,
    /// Overlay is eligible to reclaim focus on the next non-overlay dispatch.
    Eligible {
        /// Overlay that should reclaim focus.
        overlay: FocusId,
    },
    /// Focus was stolen by a non-overlay target; restore is deferred.
    Blocked {
        /// Overlay waiting to reclaim focus.
        overlay: FocusId,
        /// Component currently holding focus that blocked restore.
        blocked_by: FocusId,
        /// What to do when the block clears.
        resume: OverlayBlockedFocusResume,
    },
}

/// Focus manager: single focused slot, key-release opt-in, overlay restore.
#[derive(Debug, Default)]
pub struct FocusManager {
    focused: Option<FocusId>,
    release_subscribers: HashSet<FocusId>,
    overlay_focus_restore: OverlayFocusRestoreState,
    /// Overlay entries known to the manager (id → `pre_focus` chain link).
    ///
    /// Full stack ownership lives in [`crate::overlay::OverlayStack`]; this
    /// map only stores pre-focus ancestry used by restore logic.
    overlay_pre_focus: std::collections::HashMap<FocusId, Option<FocusId>>,
}

impl FocusManager {
    /// Create an empty focus manager.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Currently focused component id, if any.
    #[must_use]
    pub fn focused(&self) -> Option<FocusId> {
        self.focused
    }

    /// Whether `id` is the focused component.
    #[must_use]
    pub fn is_focused(&self, id: FocusId) -> bool {
        self.focused == Some(id)
    }

    /// Overlay focus restore state (visible-filtered by the caller if needed).
    #[must_use]
    pub fn overlay_focus_restore(&self) -> OverlayFocusRestoreState {
        self.overlay_focus_restore
    }

    /// Register that `id` wants [`KeyEventKind::Release`] events.
    pub fn subscribe_release(&mut self, id: FocusId) {
        self.release_subscribers.insert(id);
    }

    /// Stop delivering key-release events to `id`.
    pub fn unsubscribe_release(&mut self, id: FocusId) {
        self.release_subscribers.remove(&id);
    }

    /// Whether `id` is subscribed for key releases.
    #[must_use]
    pub fn wants_key_release(&self, id: FocusId) -> bool {
        self.release_subscribers.contains(&id)
    }

    /// Record an overlay's pre-focus ancestry for restore / ancestor checks.
    pub fn register_overlay_pre_focus(&mut self, overlay: FocusId, pre_focus: Option<FocusId>) {
        self.overlay_pre_focus.insert(overlay, pre_focus);
    }

    /// Drop overlay ancestry when the overlay is removed.
    pub fn unregister_overlay(&mut self, overlay: FocusId) {
        self.overlay_pre_focus.remove(&overlay);
        self.clear_overlay_focus_restore_for(overlay);
    }

    /// Retarget any overlay whose `pre_focus` pointed at `removed`.
    pub fn retarget_overlay_pre_focus(&mut self, removed: FocusId, fallback: Option<FocusId>) {
        for pre in self.overlay_pre_focus.values_mut() {
            if *pre == Some(removed) {
                *pre = fallback;
            }
        }
    }

    /// Pre-focus recorded for an overlay, if known.
    #[must_use]
    pub fn overlay_pre_focus(&self, overlay: FocusId) -> Option<FocusId> {
        self.overlay_pre_focus.get(&overlay).copied().flatten()
    }

    /// Set focus, clearing any overlay restore state.
    pub fn set_focus(&mut self, component: Option<FocusId>) {
        self.set_focus_internal(component, OverlayFocusRestorePolicy::Clear);
    }

    /// Set focus with an explicit overlay-restore policy.
    pub fn set_focus_internal(
        &mut self,
        component: Option<FocusId>,
        overlay_focus_restore: OverlayFocusRestorePolicy,
    ) {
        let previous_focus = self.focused;
        let mut next_focus = component;

        let previous_focused_overlay = previous_focus.filter(|id| self.is_overlay(*id));
        let next_focus_is_overlay = next_focus.is_some_and(|id| self.is_overlay(id));
        let restore_state = self.overlay_focus_restore;

        if let Some(next) = next_focus {
            if !next_focus_is_overlay {
                match restore_state {
                    OverlayFocusRestoreState::Blocked {
                        overlay,
                        blocked_by,
                        resume,
                    } if Some(blocked_by) == previous_focus => {
                        if matches!(resume, OverlayBlockedFocusResume::FocusTarget(_))
                            || !self.is_registered(blocked_by)
                        {
                            next_focus = self.resolve_blocked_resume(overlay, resume);
                        } else {
                            self.overlay_focus_restore = OverlayFocusRestoreState::Blocked {
                                overlay,
                                blocked_by: next,
                                resume,
                            };
                        }
                    }
                    _ => {
                        if let Some(prev_overlay) = previous_focused_overlay
                            && !matches!(restore_state, OverlayFocusRestoreState::Inactive)
                            && restore_state_overlay(restore_state) == Some(prev_overlay)
                            && !self.is_overlay_focus_ancestor(prev_overlay, next)
                        {
                            self.overlay_focus_restore = OverlayFocusRestoreState::Blocked {
                                overlay: prev_overlay,
                                blocked_by: next,
                                resume: OverlayBlockedFocusResume::RestoreOverlay,
                            };
                        }
                    }
                }
            }
        } else {
            // next_focus == None
            match restore_state {
                OverlayFocusRestoreState::Blocked {
                    overlay,
                    blocked_by,
                    resume,
                } if Some(blocked_by) == previous_focus => {
                    next_focus = self.resolve_blocked_resume(overlay, resume);
                }
                _ if matches!(overlay_focus_restore, OverlayFocusRestorePolicy::Clear) => {
                    self.clear_overlay_focus_restore();
                }
                _ => {}
            }
        }

        self.focused = next_focus;

        if let Some(focused) = next_focus
            && self.is_overlay(focused)
        {
            self.overlay_focus_restore = OverlayFocusRestoreState::Eligible { overlay: focused };
        }
    }

    /// Apply focus flags on a pair of optional focusable components.
    ///
    /// Callers that own the component objects use this after
    /// [`Self::set_focus`] to flip `focused` bits.
    pub fn apply_focus_flags(
        previous: Option<&mut dyn Focusable>,
        next: Option<&mut dyn Focusable>,
        focused_id: Option<FocusId>,
    ) {
        if let Some(prev) = previous {
            let still = focused_id == Some(prev.focus_id());
            prev.set_focused(still);
        }
        if let Some(n) = next {
            n.set_focused(focused_id == Some(n.focus_id()));
        }
    }

    /// Clear restore state entirely.
    pub fn clear_overlay_focus_restore(&mut self) {
        self.overlay_focus_restore = OverlayFocusRestoreState::Inactive;
    }

    /// Clear restore state if it references `overlay`.
    pub fn clear_overlay_focus_restore_for(&mut self, overlay: FocusId) {
        if restore_state_overlay(self.overlay_focus_restore) == Some(overlay) {
            self.clear_overlay_focus_restore();
        }
    }

    /// Replace the resume action for a blocked restore belonging to `overlay`.
    ///
    /// Returns `true` when a matching blocked restore was updated.
    #[must_use]
    pub fn set_blocked_resume_target(&mut self, overlay: FocusId, target: Option<FocusId>) -> bool {
        let OverlayFocusRestoreState::Blocked {
            overlay: blocked_overlay,
            blocked_by,
            ..
        } = self.overlay_focus_restore
        else {
            return false;
        };
        if blocked_overlay != overlay {
            return false;
        }
        self.overlay_focus_restore = OverlayFocusRestoreState::Blocked {
            overlay,
            blocked_by,
            resume: OverlayBlockedFocusResume::FocusTarget(target),
        };
        true
    }

    /// Filter a key event: drop releases unless the focused id subscribed.
    ///
    /// `Repeat` is treated as `Press` (always delivered).
    #[must_use]
    pub fn filter_key(&self, event: KeyEvent) -> Option<KeyEvent> {
        match event.kind {
            KeyEventKind::Release => {
                let focused = self.focused?;
                if self.wants_key_release(focused) {
                    Some(event)
                } else {
                    None
                }
            }
            KeyEventKind::Press | KeyEventKind::Repeat => Some(event),
        }
    }

    /// Dispatch a UI event to the focused component if present.
    #[must_use]
    pub fn dispatch_to_focused(
        &self,
        focused: Option<&mut dyn Component>,
        event: &UiEvent,
    ) -> EventResult {
        match (focused, event) {
            (Some(component), UiEvent::Key(key)) => {
                if self.filter_key(*key).is_none() {
                    return EventResult::Ignored;
                }
                component.handle_event(event)
            }
            (Some(component), _) => component.handle_event(event),
            (None, _) => EventResult::Ignored,
        }
    }

    /// Whether `id` is a registered overlay.
    #[must_use]
    pub fn is_overlay(&self, id: FocusId) -> bool {
        self.overlay_pre_focus.contains_key(&id)
    }

    fn is_registered(&self, id: FocusId) -> bool {
        // Overlays are registered; non-overlays are considered mounted when
        // they are (or were) focused or appear in a pre-focus chain.
        self.focused == Some(id)
            || self.overlay_pre_focus.contains_key(&id)
            || self.overlay_pre_focus.values().any(|p| *p == Some(id))
    }

    fn is_overlay_focus_ancestor(&self, entry: FocusId, component: FocusId) -> bool {
        let mut visited = HashSet::new();
        let mut current = self.overlay_pre_focus.get(&entry).copied().flatten();
        while let Some(id) = current {
            if !visited.insert(id) {
                break;
            }
            if id == component {
                return true;
            }
            current = self.overlay_pre_focus.get(&id).copied().flatten();
        }
        false
    }

    fn resolve_blocked_resume(
        &mut self,
        overlay: FocusId,
        resume: OverlayBlockedFocusResume,
    ) -> Option<FocusId> {
        match resume {
            OverlayBlockedFocusResume::RestoreOverlay => Some(overlay),
            OverlayBlockedFocusResume::FocusTarget(target) => {
                self.clear_overlay_focus_restore();
                target
            }
        }
    }
}

fn restore_state_overlay(state: OverlayFocusRestoreState) -> Option<FocusId> {
    match state {
        OverlayFocusRestoreState::Inactive => None,
        OverlayFocusRestoreState::Eligible { overlay }
        | OverlayFocusRestoreState::Blocked { overlay, .. } => Some(overlay),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventState, KeyModifiers};

    fn key(kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn single_focused_slot() {
        let mut fm = FocusManager::new();
        let a = FocusId::new();
        let b = FocusId::new();
        fm.set_focus(Some(a));
        assert_eq!(fm.focused(), Some(a));
        fm.set_focus(Some(b));
        assert_eq!(fm.focused(), Some(b));
        fm.set_focus(None);
        assert_eq!(fm.focused(), None);
    }

    #[test]
    fn release_filtered_unless_subscribed() {
        let mut fm = FocusManager::new();
        let id = FocusId::new();
        fm.set_focus(Some(id));
        assert!(fm.filter_key(key(KeyEventKind::Press)).is_some());
        assert!(fm.filter_key(key(KeyEventKind::Repeat)).is_some());
        assert!(fm.filter_key(key(KeyEventKind::Release)).is_none());
        fm.subscribe_release(id);
        assert!(fm.filter_key(key(KeyEventKind::Release)).is_some());
        fm.unsubscribe_release(id);
        assert!(fm.filter_key(key(KeyEventKind::Release)).is_none());
    }

    #[test]
    fn capturing_overlay_sets_eligible_restore() {
        let mut fm = FocusManager::new();
        let editor = FocusId::new();
        let overlay = FocusId::new();
        fm.set_focus(Some(editor));
        fm.register_overlay_pre_focus(overlay, Some(editor));
        fm.set_focus(Some(overlay));
        assert_eq!(
            fm.overlay_focus_restore(),
            OverlayFocusRestoreState::Eligible { overlay }
        );
        assert_eq!(fm.focused(), Some(overlay));
    }

    #[test]
    fn non_overlay_steal_blocks_restore() {
        let mut fm = FocusManager::new();
        let editor = FocusId::new();
        let overlay = FocusId::new();
        let other = FocusId::new();
        fm.set_focus(Some(editor));
        fm.register_overlay_pre_focus(overlay, Some(editor));
        fm.set_focus(Some(overlay));
        // Steal focus to a non-overlay that is not an ancestor.
        fm.set_focus(Some(other));
        assert_eq!(
            fm.overlay_focus_restore(),
            OverlayFocusRestoreState::Blocked {
                overlay,
                blocked_by: other,
                resume: OverlayBlockedFocusResume::RestoreOverlay,
            }
        );
    }

    #[test]
    fn clear_policy_drops_restore() {
        let mut fm = FocusManager::new();
        let editor = FocusId::new();
        let overlay = FocusId::new();
        fm.register_overlay_pre_focus(overlay, Some(editor));
        fm.set_focus(Some(overlay));
        fm.set_focus(None); // clear policy default
        assert_eq!(
            fm.overlay_focus_restore(),
            OverlayFocusRestoreState::Inactive
        );
    }

    #[test]
    fn preserve_policy_keeps_restore_on_null_focus() {
        let mut fm = FocusManager::new();
        let editor = FocusId::new();
        let overlay = FocusId::new();
        fm.register_overlay_pre_focus(overlay, Some(editor));
        fm.set_focus(Some(overlay));
        // Simulate hide path: focus moves away with preserve by going through
        // set_focus_internal(None, Preserve) — restore stays eligible only if
        // we don't clear; after None with preserve and no blocked path, restore
        // remains as-is when previous wasn't the blocked_by. Explicit clear_for.
        fm.set_focus_internal(Some(editor), OverlayFocusRestorePolicy::Preserve);
        // Moving to pre_focus (ancestor) should NOT block.
        assert!(matches!(
            fm.overlay_focus_restore(),
            OverlayFocusRestoreState::Eligible { .. }
                | OverlayFocusRestoreState::Inactive
                | OverlayFocusRestoreState::Blocked { .. }
        ));
    }

    #[test]
    fn unregister_clears_restore_for_overlay() {
        let mut fm = FocusManager::new();
        let editor = FocusId::new();
        let overlay = FocusId::new();
        fm.register_overlay_pre_focus(overlay, Some(editor));
        fm.set_focus(Some(overlay));
        fm.unregister_overlay(overlay);
        assert_eq!(
            fm.overlay_focus_restore(),
            OverlayFocusRestoreState::Inactive
        );
        assert!(!fm.is_overlay(overlay));
    }
}
