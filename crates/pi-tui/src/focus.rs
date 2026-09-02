//! Focus identity types for components participating in hardware/focus state.
//!
//! Provides [`FocusId`] (opaque per-component identity) and the [`Focusable`]
//! trait used by hosts to track focused slots. The single-focus manager and
//! overlay capture/restore state machine that previously lived here were
//! removed in issue #175 (zero live consumers; both registries unpublished).

use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::component::Component;

/// Opaque identity for a focusable component slot.
///
/// Components are not required to be `Eq`; hosts track them by id.
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
    /// Unique id for this component.
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
