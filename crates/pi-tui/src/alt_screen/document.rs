//! Retained, width-prepared document rows for the fullscreen viewport.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::component::{Component, DisplayRowSpan, RowSourceError};

/// Process-local identity for one retained document block.
///
/// IDs are deliberately not serializable. They identify a live block while a
/// runtime retains it, and are safe to allocate concurrently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DocumentBlockId(u64);

impl DocumentBlockId {
    /// Allocate a new process-local block identity.
    ///
    /// # Errors
    ///
    /// Returns [`RowSourceError::RowCountOverflow`] if the process-local
    /// identity space is exhausted.
    pub fn new() -> Result<Self, RowSourceError> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            next.checked_add(1)
        });
        id.map(Self).map_err(|_| RowSourceError::RowCountOverflow)
    }

    /// Numeric identity for diagnostics and tests.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Semantic zone attached to a retained block by product composition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptZone {
    /// A user prompt boundary.
    Prompt,
    /// Visible assistant output boundary.
    AssistantOutput,
}

/// One retained message/component root.
pub struct DocumentBlock {
    /// Stable process-local identity.
    pub id: DocumentBlockId,
    /// Optional prompt-navigation semantic zone.
    pub zone: Option<PromptZone>,
    /// Native component tree that owns the styled row cache.
    pub component: Box<dyn Component>,
}

/// Prepared row count and document generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentMetrics {
    /// Number of prepared logical rows.
    pub rows: usize,
    /// Monotonic generation for width/document replacements.
    pub generation: u64,
}

struct DocumentBlockEntry {
    block: DocumentBlock,
    rows: usize,
    dirty: bool,
}

/// Retained component roots and checked cumulative row extents.
///
/// The document keeps only the component-owned styled caches plus one active
/// width's row index. Consumers borrow rows through [`Self::visit_row`]
/// instead of copying a transcript-sized terminal buffer.
pub struct LineDocument {
    blocks: Vec<DocumentBlockEntry>,
    prefixes: Vec<usize>,
    prompt_rows: Vec<usize>,
    width: Option<u16>,
    rows: usize,
    generation: u64,
    prepared: bool,
}

impl Default for LineDocument {
    fn default() -> Self {
        Self::new()
    }
}

impl LineDocument {
    /// Create an empty document.
    #[must_use]
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            prefixes: Vec::new(),
            prompt_rows: Vec::new(),
            width: None,
            rows: 0,
            generation: 0,
            prepared: false,
        }
    }

    /// Insert or replace a retained block by stable identity.
    pub fn upsert(&mut self, block: DocumentBlock) {
        let existing = self
            .blocks
            .iter()
            .position(|entry| entry.block.id == block.id);
        if let Some(index) = existing {
            if let Some(entry) = self.blocks.get_mut(index) {
                entry.block = block;
                entry.rows = 0;
                entry.dirty = true;
            }
        } else {
            self.blocks.push(DocumentBlockEntry {
                block,
                rows: 0,
                dirty: true,
            });
        }
        self.generation = self.generation.saturating_add(1);
        self.prepared = false;
    }

    /// Remove every retained block and invalidate all prior row identities.
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.prefixes.clear();
        self.prompt_rows.clear();
        self.width = None;
        self.rows = 0;
        self.generation = self.generation.saturating_add(1);
        self.prepared = false;
    }

    /// Prepare all invalidated roots at exactly one display width.
    ///
    /// # Errors
    ///
    /// Propagates an unsupported/unprepared source error from a component, or
    /// reports [`RowSourceError::RowCountOverflow`] if cumulative row extents
    /// do not fit in `usize`.
    pub fn prepare(&mut self, width: u16) -> Result<DocumentMetrics, RowSourceError> {
        if width == 0 {
            let changed =
                self.width != Some(width) || self.blocks.iter().any(|entry| entry.rows != 0);
            for entry in &mut self.blocks {
                entry.rows = 0;
                entry.dirty = false;
            }
            self.prefixes.clear();
            self.prefixes.resize(self.blocks.len(), 0);
            self.prompt_rows.clear();
            self.rows = 0;
            self.width = Some(width);
            self.prepared = true;
            if changed {
                self.generation = self.generation.saturating_add(1);
            }
            return Ok(DocumentMetrics {
                rows: 0,
                generation: self.generation,
            });
        }

        // A failed width/dirty preparation must invalidate the published index,
        // even when an earlier frame had a valid one.
        self.prepared = false;
        let width_changed = self.width != Some(width);
        let first_dirty = if width_changed {
            0
        } else if let Some(index) = self.blocks.iter().position(|entry| entry.dirty) {
            index
        } else {
            self.prepared = true;
            return Ok(DocumentMetrics {
                rows: self.rows,
                generation: self.generation,
            });
        };
        let mut prepared_rows = Vec::with_capacity(self.blocks.len());
        for entry in &mut self.blocks {
            let rows = if width_changed || entry.dirty {
                entry.block.component.prepare_rows(width)?
            } else {
                entry.rows
            };
            prepared_rows.push(rows);
        }

        let mut prefixes = if first_dirty == 0 {
            Vec::with_capacity(prepared_rows.len())
        } else {
            self.prefixes
                .get(..first_dirty)
                .ok_or(RowSourceError::RowCountOverflow)?
                .to_vec()
        };
        let mut total = prefixes.last().copied().unwrap_or(0);
        for rows in prepared_rows.iter().copied().skip(first_dirty) {
            total = total
                .checked_add(rows)
                .ok_or(RowSourceError::RowCountOverflow)?;
            prefixes.push(total);
        }

        let mut prompt_rows = Vec::new();
        let mut start = 0usize;
        for (index, rows) in prepared_rows.iter().copied().enumerate() {
            if rows > 0
                && self
                    .blocks
                    .get(index)
                    .is_some_and(|entry| entry.block.zone == Some(PromptZone::Prompt))
            {
                prompt_rows.push(start);
            }
            start = start
                .checked_add(rows)
                .ok_or(RowSourceError::RowCountOverflow)?;
        }

        for (entry, rows) in self.blocks.iter_mut().zip(prepared_rows) {
            entry.rows = rows;
            entry.dirty = false;
        }
        self.prefixes = prefixes;
        self.prompt_rows = prompt_rows;
        self.rows = total;
        self.width = Some(width);
        self.prepared = true;
        if width_changed {
            self.generation = self.generation.saturating_add(1);
        }

        Ok(DocumentMetrics {
            rows: self.rows,
            generation: self.generation,
        })
    }

    /// Visit one prepared logical row with borrowed, positioned spans.
    ///
    /// # Errors
    ///
    /// Returns [`RowSourceError::NotPrepared`] before a successful
    /// [`Self::prepare`], [`RowSourceError::RowOutOfBounds`] for an invalid
    /// row, or the source error reported by the owning component.
    pub fn visit_row(
        &self,
        row: usize,
        emit: &mut dyn FnMut(DisplayRowSpan<'_>),
    ) -> Result<(), RowSourceError> {
        if !self.prepared {
            return Err(RowSourceError::NotPrepared);
        }
        if row >= self.rows {
            return Err(RowSourceError::RowOutOfBounds {
                row,
                rows: self.rows,
            });
        }

        let mut low = 0usize;
        let mut high = self.prefixes.len();
        while low < high {
            let distance = high
                .checked_sub(low)
                .ok_or(RowSourceError::RowCountOverflow)?;
            let middle = low
                .checked_add(distance / 2)
                .ok_or(RowSourceError::RowCountOverflow)?;
            let end = self
                .prefixes
                .get(middle)
                .copied()
                .ok_or(RowSourceError::RowCountOverflow)?;
            if row < end {
                high = middle;
            } else {
                low = middle
                    .checked_add(1)
                    .ok_or(RowSourceError::RowCountOverflow)?;
            }
        }

        let block_start = if low == 0 {
            0
        } else {
            self.prefixes
                .get(low - 1)
                .copied()
                .ok_or(RowSourceError::RowCountOverflow)?
        };
        let block = self
            .blocks
            .get(low)
            .ok_or(RowSourceError::RowCountOverflow)?;
        let relative_row = row
            .checked_sub(block_start)
            .ok_or(RowSourceError::RowCountOverflow)?;
        block.block.component.visit_row(relative_row, emit)
    }

    /// Sorted document rows that begin prompt-navigation blocks.
    #[must_use]
    pub fn prompt_rows(&self) -> &[usize] {
        &self.prompt_rows
    }

    /// Current prepared row count, or zero before preparation.
    #[must_use]
    pub const fn rows(&self) -> usize {
        self.rows
    }

    /// Active prepared width, if any.
    #[must_use]
    pub const fn width(&self) -> Option<u16> {
        self.width
    }

    /// Current document generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}
