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

/// Claim on one render-buffer row: the row's painter within a frame.
///
/// Damage scoping (PERF-T11 Design B) treats the render buffer as in-place:
/// rows whose claim set is unchanged between frames are provably equal to the
/// last emitted grid and skip both repaint and diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowClaim {
    /// A `paint_line` line owns `[x, x + width)` (memo key folded with width).
    Line {
        /// Column where the claimed span starts.
        x: u16,
        /// Paint width of the claimed span.
        width: u16,
        /// Memo key of the claimed line.
        key: u128,
        /// Whether the line's derivation carries hyperlink regions (OSC 8
        /// spans re-pushed every frame). Pure function of `(key, width)`,
        /// so the flag is stable per key; regionless lines can skip the
        /// memo probe entirely on a claim match (PERF-T11 Design E).
        linked: bool,
    },
    /// A direct cell writer (editor body, image cells, clipped copies) owns
    /// `[x, x + width)`; its content is not key-derivable, so the row always
    /// diffs but the span is accounted for.
    Opaque {
        /// Column where the claimed span starts.
        x: u16,
        /// Width of the claimed span.
        width: u16,
    },
    /// A foreign writer (overlay compositing) painted over the row; base
    /// claims never match while overlaid, so the row repaints on overlay
    /// close.
    Foreign,
}

impl RowClaim {
    /// Column span `(x, width)` this claim accounts for (`None` for foreign
    /// claims, which cover the whole row).
    #[must_use]
    pub const fn span(&self) -> Option<(u16, u16)> {
        match *self {
            Self::Line { x, width, .. } | Self::Opaque { x, width } => Some((x, width)),
            Self::Foreign => None,
        }
    }

    /// Whether the claim describes a keyed, repaintable line.
    #[must_use]
    pub const fn is_line(&self) -> bool {
        matches!(self, Self::Line { .. })
    }
}

/// Prior-frame probe result for a line about to paint (see [`RowClaims::probe_line`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorLine {
    /// The identical `(x, width, key)` claim existed and the row was all-line.
    pub matched: bool,
    /// The matched prior claim carried hyperlink regions.
    pub linked: bool,
}

impl PriorLine {
    /// Probe result for an unclaimed row (no prior claims to match).
    pub const UNCLAIMED: Self = Self {
        matched: false,
        linked: false,
    };
}

/// Per-row claim bookkeeping for one frame render.
///
/// `prior` holds the previous frame's claims (installed by the writer before
/// composition); `frame` collects this frame's claims row by row.
#[derive(Debug, Default, Clone)]
pub struct RowClaims {
    /// Claims carried over from the previous frame, indexed by row.
    prior: Vec<Vec<RowClaim>>,
    /// Claims recorded during this frame, indexed by row.
    frame: Vec<Vec<RowClaim>>,
}

impl RowClaims {
    /// Create empty bookkeeping sized for `rows` rows.
    #[must_use]
    pub fn with_rows(rows: usize) -> Self {
        Self {
            prior: vec![Vec::new(); rows],
            frame: vec![Vec::new(); rows],
        }
    }

    /// Probe the prior-frame claim for a line about to paint on row `y`.
    ///
    /// The match license requires the identical claim in the prior frame AND
    /// that every prior claim on the row is a line claim: a foreign or
    /// opaque writer owned part of the row, so the row's prior final content
    /// is not what this line paints (overlay close must repaint).
    /// `linked` reports the prior claim's region flag so a regionless
    /// matched line can skip the paint-memo probe (Design E). The match
    /// itself ignores `linked`: the flag is a pure function of the key, so
    /// an identical `(x, width, key)` claim always carries the same flag.
    #[must_use]
    pub fn probe_line(&self, y: u16, x: u16, width: u16, key: u128) -> PriorLine {
        let Some(row) = self.prior.get(usize::from(y)) else {
            return PriorLine::UNCLAIMED;
        };
        let mut matched = false;
        let mut linked = false;
        for claim in row {
            let RowClaim::Line {
                x: claim_x,
                width: claim_width,
                key: claim_key,
                linked: claim_linked,
            } = claim
            else {
                // A foreign or opaque claimant owned part of the row.
                return PriorLine {
                    matched: false,
                    linked: false,
                };
            };
            if *claim_x == x && *claim_width == width && *claim_key == key {
                matched = true;
                linked = *claim_linked;
            }
        }
        PriorLine { matched, linked }
    }

    /// Record this frame's line claim on row `y` with its region flag.
    pub fn record_line(&mut self, y: u16, x: u16, width: u16, key: u128, linked: bool) {
        self.record(y, RowClaim::Line { x, width, key, linked });
    }
    /// Record an opaque claim on row `y` (direct cell writer).
    pub fn claim_opaque(&mut self, y: u16, x: u16, width: u16) {
        self.record(y, RowClaim::Opaque { x, width });
    }

    /// Record a foreign claim on row `y` (overlay compositing).
    pub fn claim_foreign(&mut self, y: u16) {
        self.record(y, RowClaim::Foreign);
    }
    fn record(&mut self, y: u16, claim: RowClaim) {
        if let Some(row) = self.frame.get_mut(usize::from(y))
            && !row.contains(&claim)
        {
            row.push(claim);
        }
    }

    /// Install `prior` as the prior-frame claims with a pooled frame table
    /// (PERF-T11 Design F).
    ///
    /// The writer owns a scratch table whose rows retain their capacity
    /// across frames (cleared here in place by the caller), so steady-state
    /// composition allocates nothing for claim bookkeeping.
    pub fn install_pooled(&mut self, prior: Vec<Vec<RowClaim>>, frame: Vec<Vec<RowClaim>>) {
        self.prior = prior;
        self.frame = frame;
    }

    /// Consume into the `(prior, frame)` tables so the writer can pool them
    /// for the next frame (Design F).
    #[must_use]
    pub fn into_tables(mut self) -> (Vec<Vec<RowClaim>>, Vec<Vec<RowClaim>>) {
        (
            std::mem::take(&mut self.prior),
            std::mem::take(&mut self.frame),
        )
    }

    /// Install `prior` as the prior-frame claims, resetting frame state.
    pub fn install_prior(&mut self, prior: Vec<Vec<RowClaim>>) {
        self.prior = prior;
        self.frame = vec![Vec::new(); self.prior.len()];
    }
}

/// Annotations collected while composing one frame.
#[derive(Debug, Default, Clone)]
pub struct FrameAnnotations {
    /// Hardware-cursor position requested by a component, if any.
    cursor: Option<Position>,
    /// Raw regions to emit after the cell draw in the same write.
    raw_regions: Vec<RawRegion>,
    /// Row-claim bookkeeping for damage-scoped frame rendering.
    row_claims: RowClaims,
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

    /// Install prior-frame row claims (writer, before composition).
    pub fn install_row_claims(&mut self, claims: RowClaims) {
        self.row_claims = claims;
    }

    /// Take the row-claim bookkeeping (writer, after composition).
    #[must_use]
    pub fn take_row_claims(&mut self) -> RowClaims {
        std::mem::take(&mut self.row_claims)
    }

    /// Borrow the row-claim bookkeeping mutably.
    pub fn row_claims_mut(&mut self) -> &mut RowClaims {
        &mut self.row_claims
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
    // Seed the thread-local from the target so the writer can pre-install
    // prior-frame row claims before composition.
    let seeded = std::mem::take(&mut *annotations.borrow_mut());
    let previous = annotations_slot().with(|slot| slot.replace(Some(seeded)));
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

/// Probe the prior-frame claim for a line about to paint (Design E).
///
/// Returns `None` outside a frame; inside, `Some(PriorLine { matched, linked })`
/// where `matched` licenses skipping the repaint for that row span and
/// `linked` reports whether the prior claim carried hyperlink regions.
pub fn probe_line(y: u16, x: u16, width: u16, key: u128) -> Option<PriorLine> {
    with_current_annotations(|annotations| {
        annotations.row_claims_mut().probe_line(y, x, width, key)
    })
}

/// Record this frame's line claim (no-op outside a frame).
pub fn record_line(y: u16, x: u16, width: u16, key: u128, linked: bool) {
    let _ = with_current_annotations(|annotations| {
        annotations
            .row_claims_mut()
            .record_line(y, x, width, key, linked)
    });
}

/// Record opaque row-span claims for the current frame (no-op outside).
pub fn claim_opaque_span(area: Rect) {
    let _ = with_current_annotations(|annotations| {
        for row in 0..area.height {
            annotations
                .row_claims_mut()
                .claim_opaque(area.y.saturating_add(row), area.x, area.width);
        }
    });
}

/// Record foreign claims for rows composited over (no-op outside).
pub fn claim_foreign_span(area: Rect) {
    let _ = with_current_annotations(|annotations| {
        for row in 0..area.height {
            annotations
                .row_claims_mut()
                .claim_foreign(area.y.saturating_add(row));
        }
    });
}

/// Suspend row-claim recording for the duration of `f`.
///
/// Off-screen scratch renders (bottom-clipped copies) must not record claims
/// at scratch coordinates.
pub fn suspend_row_claims<R>(f: impl FnOnce() -> R) -> R {
    let suspended = with_current_annotations(|annotations| {
        let mut empty = RowClaims::default();
        empty.install_prior(Vec::new());
        std::mem::swap(&mut empty, annotations.row_claims_mut());
        empty
    });
    let result = f();
    if let Some(claims) = suspended {
        let _ = with_current_annotations(|annotations| {
            *annotations.row_claims_mut() = claims;
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{
        FrameAnnotations, RawRegion, RowClaims, push_raw_region, set_cursor,
        with_annotations, with_current_annotations,
    };

    use std::cell::RefCell;
    use ratatui::layout::{Position, Rect};

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

    #[test]
    fn pooled_tables_round_trip_preserves_probe_and_starts_cleared() {
        // PERF-T11 Design F: the writer installs a pooled scratch table,
        // records claims, then recovers both tables — the frame table
        // becomes the next frame's prior, the consumed prior table's rows
        // are cleared (capacity retained) and become the next scratch.
        let mut claims = RowClaims::default();
        claims.install_pooled(vec![Vec::new(); 2], vec![Vec::new(); 2]);
        claims.record_line(0, 0, 10, 7, false);
        claims.record_line(1, 2, 8, 9, true);
        let (consumed_prior, frame) = claims.into_tables();
        assert!(consumed_prior.iter().all(Vec::is_empty));
        assert_eq!(frame[0].len(), 1);

        let mut scratch = consumed_prior;
        for row in &mut scratch {
            row.clear();
        }
        let mut next = RowClaims::default();
        next.install_pooled(frame, scratch);

        let regionless = next.probe_line(0, 0, 10, 7);
        assert!(regionless.matched && !regionless.linked);
        let linked = next.probe_line(1, 2, 8, 9);
        assert!(linked.matched && linked.linked);
        next.record_line(0, 0, 10, 7, false);
        let (_, frame2) = next.into_tables();
        assert_eq!(frame2[0].len(), 1);
    }
}
