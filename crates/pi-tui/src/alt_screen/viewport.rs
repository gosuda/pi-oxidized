//! Native fullscreen document viewport, selection, scrollbar, and transient effects.

use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;

use super::document::LineDocument;
use super::search::{SearchDirection, SearchIndex, SearchMatch, search_match_key};

use crate::component::{
    DisplayRowContent, EventResult, TuiMouseButton, TuiMouseEvent, TuiMouseEventType, UiEvent,
};
use crate::components::util::{paint_keyed_line, paint_line};
use crate::image::{DocumentImage, ImageCacheOutput, KittyImageCache};
use crate::keybindings::KeybindingsManager;
use crate::keys::is_key_release;
use crate::text::{
    extract_ansi_code, grapheme_width, parse_osc8_hyperlink, slice_by_column, visible_width,
};

const PAGE_SCROLL_OVERLAP: usize = 4;
const ALT_WHEEL_SCROLL_MULTIPLIER: i32 = 5;
const SCROLLBAR_REVEAL: Duration = Duration::from_millis(1000);
const SELECTION_AUTOSCROLL: Duration = Duration::from_millis(50);
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// Scrollbar visibility policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarMode {
    /// Reveal only after activity, then hide after a short idle period.
    Auto,
    /// Reserve and paint the rightmost column whenever content is laid out.
    Always,
    /// Do not reserve, paint, or hit-test a scrollbar.
    Hidden,
}

/// Overscroll behavior for nested scroll targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overscroll {
    /// Return unconsumed wheel/scroll deltas to an outer target.
    Chain,
    /// Consume deltas at this viewport's boundary.
    Contain,
}

/// Absolute scroll destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollTarget {
    /// First document row.
    Top,
    /// Last viewport-sized document position.
    Bottom,
    /// A particular document row, clamped to the valid range.
    Row(usize),
}

/// Whether a scroll destination may restore follow-end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowPolicy {
    /// Restore following when the destination is the content end.
    ResumeAtEnd,
    /// Keep following disabled, even when the destination is the content end.
    SuppressAtEnd,
}

/// Styles used by viewport-local adornments.
#[derive(Debug, Clone, Copy)]
pub struct FullscreenStyle {
    /// Scrollbar track style.
    pub scrollbar_track: Style,
    /// Scrollbar thumb style.
    pub scrollbar_thumb: Style,
    /// Non-current search match style.
    pub search_match: Style,
    /// Current search match style.
    pub search_current_match: Style,
    /// Jump-to-end label style.
    pub jump_to_end: Style,
}

impl Default for FullscreenStyle {
    fn default() -> Self {
        Self {
            scrollbar_track: Style::default(),
            scrollbar_thumb: Style::default(),
            search_match: Style::default().add_modifier(Modifier::UNDERLINED),
            search_current_match: Style::default()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::REVERSED),
            jump_to_end: Style::default().add_modifier(Modifier::REVERSED),
        }
    }
}

/// Product-independent viewport options.
#[derive(Debug, Clone, Copy)]
pub struct FullscreenOptions {
    /// Scrollbar visibility policy.
    pub scrollbar: ScrollbarMode,
    /// Nested overscroll behavior.
    pub overscroll: Overscroll,
    /// Base number of rows consumed by one wheel event.
    pub wheel_scroll_lines: u16,
    /// Automatically return a copy effect after a successful selection drag.
    pub copy_on_select: bool,
}

impl Default for FullscreenOptions {
    fn default() -> Self {
        Self {
            scrollbar: ScrollbarMode::Auto,
            overscroll: Overscroll::Chain,
            wheel_scroll_lines: 1,
            copy_on_select: true,
        }
    }
}

/// Scrollbar track/thumb geometry in absolute terminal coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarGeometry {
    /// Rightmost track column.
    pub column: u16,
    /// Track origin row.
    pub track_top: u16,
    /// Track height.
    pub track_height: u16,
    /// Thumb origin row.
    pub thumb_top: u16,
    /// Thumb height.
    pub thumb_height: u16,
    /// Maximum document top.
    pub max_top: usize,
}

/// A viewport event's product-facing result and optional side effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullscreenEventResult {
    /// Native event disposition.
    pub event_result: EventResult,
    /// Clipboard/link/paste effect for the product runtime.
    pub effect: Option<FullscreenEffect>,
}

/// Product-owned side effect requested by fullscreen interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FullscreenEffect {
    /// Copy selected visible text through the product clipboard service.
    CopySelection(String),
    /// Open a validated OSC 8 link through the product URL boundary.
    OpenLink(String),
    /// Request the product clipboard paste path.
    PasteClipboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionGranularity {
    Character,
    Word,
    Line,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectionPoint {
    row: usize,
    col: usize,
    boundary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectionRange {
    start: SelectionPoint,
    end: SelectionPoint,
}

#[derive(Debug, Clone, Copy)]
struct ClickRecord {
    at: Instant,
    row: usize,
    col: usize,
    count: u8,
}

#[derive(Debug, Clone, Copy)]
struct ScrollbarDrag {
    grab_offset: u16,
}

#[derive(Debug, Clone)]
struct FlashEntry {
    id: u64,
    message: String,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchSelectionMode {
    Query,
    Retain,
    Next,
    Previous,
}

struct FollowState {
    following_end: bool,
    suppressed_at_end: bool,
}

struct SelectionState {
    anchor: Option<SelectionPoint>,
    focus: Option<SelectionPoint>,
    initial: Option<SelectionRange>,
    granularity: SelectionGranularity,
    dragging: bool,
    pointer: Option<(u16, u16)>,
    auto_scroll_direction: i8,
    auto_scroll_deadline: Option<Instant>,
}

struct ScrollbarState {
    drag: Option<ScrollbarDrag>,
    hovered: bool,
    visible_until: Option<Instant>,
}

struct SearchState {
    index: SearchIndex,
    open: bool,
    query: String,
    selected: Option<usize>,
    anchor_row: usize,
    selection_mode: SearchSelectionMode,
    selected_key: Option<String>,
}

struct ImagePaintContext<'a> {
    area: Rect,
    offset: u16,
    x: u16,
    span_width: u16,
    image_cache: &'a KittyImageCache,
    image_cache_output: &'a ImageCacheOutput,
    previous_image: Option<(Option<u32>, u16)>,
}

/// Retained fullscreen document viewport.
pub struct FullscreenViewport {
    document: LineDocument,
    options: FullscreenOptions,
    style: FullscreenStyle,
    area: Option<Rect>,
    content_width: u16,
    total_rows: usize,
    top: usize,
    follow: FollowState,
    prepared_generation: Option<u64>,
    projection: Vec<String>,
    selection: SelectionState,
    last_click: Option<ClickRecord>,
    pressed_link: Option<String>,
    scrollbar: ScrollbarState,
    jump_label: String,
    jump_rect: Option<Rect>,
    search: SearchState,
    flashes: Vec<FlashEntry>,
    image_cache: KittyImageCache,
    next_flash_id: u64,
}

impl Default for FullscreenViewport {
    fn default() -> Self {
        Self::new(FullscreenOptions::default(), FullscreenStyle::default())
    }
}

impl FullscreenViewport {
    /// Create an empty viewport whose first prepared view follows document end.
    #[must_use]
    pub fn new(options: FullscreenOptions, style: FullscreenStyle) -> Self {
        Self {
            document: LineDocument::new(),
            options: FullscreenOptions {
                wheel_scroll_lines: options.wheel_scroll_lines.max(1),
                ..options
            },
            style,
            area: None,
            content_width: 0,
            total_rows: 0,
            top: 0,
            follow: FollowState {
                following_end: true,
                suppressed_at_end: false,
            },
            prepared_generation: None,
            projection: Vec::new(),
            selection: SelectionState {
                anchor: None,
                focus: None,
                initial: None,
                granularity: SelectionGranularity::Character,
                dragging: false,
                pointer: None,
                auto_scroll_direction: 0,
                auto_scroll_deadline: None,
            },
            last_click: None,
            pressed_link: None,
            scrollbar: ScrollbarState {
                drag: None,
                hovered: false,
                visible_until: None,
            },
            jump_label: "Jump to end".to_owned(),
            jump_rect: None,
            search: SearchState {
                index: SearchIndex::new(),
                open: false,
                query: String::new(),
                selected: None,
                anchor_row: 0,
                selection_mode: SearchSelectionMode::Query,
                selected_key: None,
            },
            image_cache: KittyImageCache::new(),
            flashes: Vec::new(),
            next_flash_id: 1,
        }
    }

    /// Mutable retained document used by product composition.
    pub fn document_mut(&mut self) -> &mut LineDocument {
        &mut self.document
    }

    /// Borrow the retained document.
    #[must_use]
    pub const fn document(&self) -> &LineDocument {
        &self.document
    }

    /// Prepare document rows and viewport geometry for one frame.
    ///
    /// # Errors
    ///
    /// Propagates row-source preparation errors and checked extent failures.
    pub fn prepare(&mut self, area: Rect) -> Result<(), crate::component::RowSourceError> {
        let content_width = self.content_width_for(area.width);
        let metrics = self.document.prepare(content_width)?;
        let source_changed = self.prepared_generation != Some(metrics.generation)
            || self.content_width != content_width;
        let previous_height = self.area.map_or(0, |previous| previous.height);
        self.area = Some(area);
        self.content_width = content_width;
        self.total_rows = metrics.rows;
        let flash_bound = usize::from(area.height.max(1));
        if self.flashes.len() > flash_bound {
            let remove = self.flashes.len().saturating_sub(flash_bound);
            self.flashes.drain(0..remove);
        }
        if source_changed {
            self.clear_selection_state();
            self.scrollbar.drag = None;
            self.jump_rect = None;
            self.projection = self.build_projection()?;
            self.prepared_generation = Some(metrics.generation);
            self.search.index = SearchIndex::new();
            self.search.selected = None;
            self.search.selected_key = None;
            self.search.selection_mode = SearchSelectionMode::Query;
        }
        let max_top = self.max_top(area.height);
        if self.follow.following_end {
            self.top = max_top;
        } else {
            self.top = self.top.min(max_top);
        }
        if previous_height != area.height && !source_changed {
            self.top = self.top.min(max_top);
            self.clamp_selection_rows();
        }
        if source_changed && self.search.open {
            self.search.anchor_row = self.top;
        }
        if self.search.open {
            self.refresh_search();
        }
        Ok(())
    }

    /// Paint visible document rows and viewport-local adornments.
    ///
    /// # Errors
    ///
    /// Returns [`crate::component::RowSourceError::NotPrepared`] when `area`
    /// does not match the last successful preparation, or propagates a source
    /// row error.
    pub fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
    ) -> Result<(), crate::component::RowSourceError> {
        let image_cache_output = self.prepare_render(area)?;
        self.paint_document_rows(area, buf, &image_cache_output)?;
        self.paint_overlays(area, buf);
        Ok(())
    }

    fn prepare_render(
        &mut self,
        area: Rect,
    ) -> Result<ImageCacheOutput, crate::component::RowSourceError> {
        if self.area != Some(area) {
            return Err(crate::component::RowSourceError::NotPrepared);
        }
        self.jump_rect = None;
        let visible_images = self.collect_visible_images(area)?;
        let image_cache_output = if crate::frame::with_current_annotations(|_| ()).is_some() {
            self.image_cache.prepare_frame(visible_images.iter())
        } else {
            ImageCacheOutput::default()
        };
        for eviction in &image_cache_output.evictions {
            crate::frame::push_raw_region(crate::frame::RawRegion {
                // Eviction bytes are ordered through the existing frame writer;
                // their cursor position is intentionally irrelevant.
                area: Rect::new(0, 0, 0, 0),
                bytes: eviction.deletion.clone(),
                kitty_id: None,
            });
        }
        Ok(image_cache_output)
    }

    fn collect_visible_images(
        &mut self,
        area: Rect,
    ) -> Result<Vec<DocumentImage>, crate::component::RowSourceError> {
        // Cache decisions are made from the complete visible image set
        // before painting. Owned shallow handles: DocumentImage clones bump
        // Arc refcounts, so the HRTB visit_row callback cannot retain
        // borrows while the frame still carries validated handles without
        // copying any payload.
        let mut visible_images = Vec::new();
        let content_width = self.content_width;
        let mut visible_error = None;
        for offset in 0..area.height {
            let Some(document_row) = self.top.checked_add(usize::from(offset)) else {
                return Err(crate::component::RowSourceError::RowCountOverflow);
            };
            if document_row >= self.total_rows {
                continue;
            }
            self.document.visit_row(document_row, &mut |span| {
                let start = usize::from(span.column);
                let Some(end) = start.checked_add(usize::from(span.width)) else {
                    visible_error = Some(crate::component::RowSourceError::RowCountOverflow);
                    return;
                };
                if end > usize::from(content_width) {
                    visible_error = Some(crate::component::RowSourceError::RowCountOverflow);
                    return;
                }
                if let DisplayRowContent::Image {
                    image,
                    row_in_image,
                } = span.content
                {
                    if !image.is_fallback()
                        && usize::from(row_in_image) >= usize::from(image.rows())
                    {
                        visible_error = Some(crate::component::RowSourceError::InvalidImage);
                        return;
                    }
                    if !image.is_fallback() {
                        visible_images.push(image.clone());
                    }
                }
            })?;
            if let Some(error) = visible_error {
                return Err(error);
            }
        }
        Ok(visible_images)
    }

    fn paint_document_rows(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        image_cache_output: &ImageCacheOutput,
    ) -> Result<(), crate::component::RowSourceError> {
        let mut previous_image: Option<(Option<u32>, u16)> = None;
        for offset in 0..area.height {
            let saw_image = self.paint_document_row(
                area,
                buf,
                offset,
                image_cache_output,
                &mut previous_image,
            )?;
            if !saw_image {
                previous_image = None;
            }
        }
        Ok(())
    }

    fn paint_document_row(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        offset: u16,
        image_cache_output: &ImageCacheOutput,
        previous_image: &mut Option<(Option<u32>, u16)>,
    ) -> Result<bool, crate::component::RowSourceError> {
        let Some(document_row) = self.top.checked_add(usize::from(offset)) else {
            return Err(crate::component::RowSourceError::RowCountOverflow);
        };
        let y = area.y.saturating_add(offset);
        if document_row >= self.total_rows {
            reset_row(buf, area.x, y, self.content_width);
            reset_row(
                buf,
                area.x.saturating_add(self.content_width),
                y,
                area.width.saturating_sub(self.content_width),
            );
            *previous_image = None;
            return Ok(false);
        }
        let mut painted = Vec::new();
        let mut saw_image = false;
        let content_width = self.content_width;
        let mut paint_error = None;
        let image_cache = &self.image_cache;
        self.document.visit_row(document_row, &mut |span| {
            let start = usize::from(span.column);
            let Some(end) = start.checked_add(usize::from(span.width)) else {
                paint_error = Some(crate::component::RowSourceError::RowCountOverflow);
                return;
            };
            if end > usize::from(content_width) {
                paint_error = Some(crate::component::RowSourceError::RowCountOverflow);
                return;
            }
            let x = area.x.saturating_add(span.column);
            match span.content {
                DisplayRowContent::Text(line) => {
                    *previous_image = None;
                    if span.width > 0 {
                        paint_keyed_line(Rect::new(x, y, span.width, 1), buf, line);
                        painted.push((start, end));
                    }
                }
                DisplayRowContent::Image {
                    image,
                    row_in_image,
                } => {
                    if image.is_fallback() {
                        *previous_image = None;
                        if let Some(text) = image.fallback_text() {
                            paint_line(x, y, usize::from(span.width), buf, text);
                        }
                        painted.push((start, end));
                        return;
                    }
                    let mut image_context = ImagePaintContext {
                        area,
                        offset,
                        x,
                        span_width: span.width,
                        image_cache,
                        image_cache_output,
                        previous_image: *previous_image,
                    };
                    match Self::paint_image_span(&mut image_context, buf, image, row_in_image) {
                        Ok(painted_image) => {
                            *previous_image = image_context.previous_image;
                            saw_image |= painted_image;
                        }
                        Err(error) => {
                            paint_error = Some(error);
                            return;
                        }
                    }
                    painted.push((start, end));
                }
            }
        })?;
        if let Some(error) = paint_error {
            return Err(error);
        }
        reset_unpainted(buf, area.x, y, self.content_width, &painted);
        reset_row(
            buf,
            area.x.saturating_add(self.content_width),
            y,
            area.width.saturating_sub(self.content_width),
        );
        Ok(saw_image)
    }

    fn paint_image_span(
        context: &mut ImagePaintContext<'_>,
        buf: &mut Buffer,
        image: &DocumentImage,
        row_in_image: u16,
    ) -> Result<bool, crate::component::RowSourceError> {
        let image_row = usize::from(row_in_image);
        if image_row >= usize::from(image.rows()) {
            return Err(crate::component::RowSourceError::InvalidImage);
        }
        let id = image.image_id();
        let is_continuation = context
            .previous_image
            .is_some_and(|(previous_id, previous_row)| {
                previous_id == id && previous_row.saturating_add(1) == row_in_image
            });
        if !is_continuation {
            let remaining = usize::from(image.rows()).saturating_sub(image_row);
            let visible_rows = remaining.min(usize::from(
                context.area.height.saturating_sub(context.offset),
            ));
            if visible_rows > 0 {
                let visible_rows = u16::try_from(visible_rows).unwrap_or(u16::MAX);
                let image_area = Rect::new(
                    context.x,
                    context.area.y.saturating_add(context.offset),
                    context.span_width,
                    visible_rows,
                );
                let emission = context
                    .image_cache
                    .emission_for(image, context.image_cache_output);
                let Some(region) =
                    image.raw_region(image_area, image_row, usize::from(visible_rows), emission)
                else {
                    return Err(crate::component::RowSourceError::InvalidImage);
                };
                mark_image_cells(buf, image_area);
                crate::frame::push_raw_region(region);
            }
        }
        context.previous_image = Some((id, row_in_image));
        Ok(true)
    }

    fn paint_overlays(&mut self, area: Rect, buf: &mut Buffer) {
        self.paint_search_highlights(area, buf);
        self.paint_selection(area, buf);
        self.paint_jump_to_end(area, buf);
        self.paint_scrollbar(area, buf);
        self.paint_flashes(area, buf);
    }

    /// Process a fullscreen key, resize, focus, or native mouse event.
    #[must_use]
    pub fn handle_event(
        &mut self,
        event: &UiEvent,
        keys: &KeybindingsManager,
        now: Instant,
    ) -> FullscreenEventResult {
        match event {
            UiEvent::Mouse(mouse) => {
                let normalized = self.normalize_mouse(*mouse);
                self.handle_mouse(&normalized, now)
            }
            UiEvent::FocusLost => {
                self.clear_interaction();
                FullscreenEventResult::consumed()
            }
            UiEvent::Resize { .. } => {
                self.clear_interaction();
                FullscreenEventResult::render()
            }
            UiEvent::Key(key) => self.handle_key(key, keys),
            UiEvent::Paste(_) | UiEvent::FocusGained => FullscreenEventResult::ignored(),
        }
    }
    /// Handle an already normalized native mouse event.
    pub fn handle_mouse(&mut self, event: &TuiMouseEvent, now: Instant) -> FullscreenEventResult {
        let Some(area) = self.area else {
            return FullscreenEventResult::ignored();
        };
        let x = i32::from(event.screen_x).saturating_sub(i32::from(area.x));
        let y = i32::from(event.screen_y).saturating_sub(i32::from(area.y));
        if event.kind == TuiMouseEventType::Wheel {
            return self.handle_mouse_wheel(event);
        }
        let jump_hit = self.jump_rect.is_some_and(|rect| {
            event.screen_x >= rect.x
                && event.screen_x.saturating_sub(rect.x) < rect.width
                && event.screen_y >= rect.y
                && event.screen_y.saturating_sub(rect.y) < rect.height
        });
        if jump_hit
            && event.button == TuiMouseButton::Left
            && matches!(
                event.kind,
                TuiMouseEventType::Press | TuiMouseEventType::Click
            )
        {
            let _ = self.scroll_to(ScrollTarget::Bottom, FollowPolicy::ResumeAtEnd);
            return FullscreenEventResult::render();
        }
        if self.handle_scrollbar_mouse(event, x, y, now) {
            return FullscreenEventResult::render();
        }
        if event.kind == TuiMouseEventType::Move {
            return self.handle_mouse_move(area, x, y, now);
        }
        self.handle_mouse_buttons(event, area, x, y, now)
    }

    fn handle_mouse_wheel(&mut self, event: &TuiMouseEvent) -> FullscreenEventResult {
        if let Some(delta) = event.wheel_delta {
            let previous_top = self.top;
            let previous_following = self.follow.following_end;
            let requested = i64::from(delta);
            let remainder = self.scroll_by(requested);
            let changed =
                self.top != previous_top || self.follow.following_end != previous_following;
            let event_result = if changed || remainder != requested {
                EventResult::Render
            } else if self.options.overscroll == Overscroll::Chain {
                EventResult::Ignored
            } else {
                EventResult::Consumed
            };
            return FullscreenEventResult {
                event_result,
                effect: None,
            };
        }
        if self.options.overscroll == Overscroll::Chain {
            FullscreenEventResult::ignored()
        } else {
            FullscreenEventResult::consumed()
        }
    }

    fn handle_mouse_move(
        &mut self,
        area: Rect,
        x: i32,
        y: i32,
        now: Instant,
    ) -> FullscreenEventResult {
        let hovered = self
            .scrollbar_geometry_with_auto_visibility(true)
            .is_some_and(|geometry| {
                x == i32::from(geometry.column.saturating_sub(area.x))
                    && y >= i32::from(geometry.track_top.saturating_sub(area.y))
                    && y < i32::from(geometry.track_top.saturating_sub(area.y))
                        + i32::from(geometry.track_height)
            });
        let changed = hovered != self.scrollbar.hovered;
        self.scrollbar.hovered = hovered;
        if hovered {
            self.reveal_scrollbar(now);
        }
        FullscreenEventResult {
            event_result: if changed {
                EventResult::Render
            } else {
                EventResult::Consumed
            },
            effect: None,
        }
    }

    fn handle_mouse_buttons(
        &mut self,
        event: &TuiMouseEvent,
        area: Rect,
        x: i32,
        y: i32,
        now: Instant,
    ) -> FullscreenEventResult {
        if event.button == TuiMouseButton::Right
            && event.kind == TuiMouseEventType::Press
            && x >= 0
            && y >= 0
            && usize::try_from(x).is_ok_and(|column| column < usize::from(self.content_width))
            && usize::try_from(y).is_ok_and(|row| row < usize::from(area.height))
        {
            return FullscreenEventResult {
                event_result: EventResult::Consumed,
                effect: Some(FullscreenEffect::PasteClipboard),
            };
        }
        if event.button != TuiMouseButton::Left {
            return FullscreenEventResult::ignored();
        }
        let inside = x >= 0
            && y >= 0
            && usize::try_from(x).is_ok_and(|column| column < usize::from(self.content_width))
            && usize::try_from(y).is_ok_and(|row| row < usize::from(area.height));
        let captured_selection = self.selection.dragging
            && matches!(
                event.kind,
                TuiMouseEventType::Drag | TuiMouseEventType::Release
            );
        if !inside && !captured_selection {
            return FullscreenEventResult::ignored();
        }
        let row_offset = usize::try_from(y.max(0))
            .unwrap_or(0)
            .min(usize::from(area.height.saturating_sub(1)));
        let column = usize::try_from(x.max(0))
            .unwrap_or(0)
            .min(usize::from(self.content_width));
        let document_row = self.top.saturating_add(row_offset);
        match event.kind {
            TuiMouseEventType::Press => {
                self.begin_selection(document_row, column, event.click_count, now)
            }
            TuiMouseEventType::Drag => self.handle_selection_drag(event, document_row, column, now),
            TuiMouseEventType::Release => self.end_selection(document_row, column),
            TuiMouseEventType::Click => FullscreenEventResult::consumed(),
            TuiMouseEventType::Move | TuiMouseEventType::Wheel => FullscreenEventResult::ignored(),
        }
    }

    fn handle_selection_drag(
        &mut self,
        event: &TuiMouseEvent,
        document_row: usize,
        column: usize,
        now: Instant,
    ) -> FullscreenEventResult {
        if !self.selection.dragging {
            return FullscreenEventResult::consumed();
        }
        // A moved press is a drag, never a click. Clear both the
        // click cycle and the pending hyperlink before extending the
        // selection, including when the pointer later returns.
        self.last_click = None;
        self.pressed_link = None;
        self.selection.pointer = Some((event.screen_x, event.screen_y));
        self.selection.dragging = true;
        self.update_selection_focus(SelectionPoint {
            row: document_row,
            col: column,
            boundary: false,
        });
        self.update_selection_autoscroll(event.screen_x, event.screen_y, now);
        FullscreenEventResult::render()
    }

    /// Scroll by signed rows and return an unconsumed remainder.
    pub fn scroll_by(&mut self, rows: i64) -> i64 {
        if rows == 0 {
            return 0;
        }
        let Some(area) = self.area else {
            return rows;
        };
        let max_top = self.max_top(area.height);
        let start = if self.follow.following_end {
            max_top
        } else {
            self.top.min(max_top)
        };
        let (next, moved) = if rows > 0 {
            let amount = usize::try_from(rows).unwrap_or(usize::MAX);
            let next = start.saturating_add(amount).min(max_top);
            let moved = next.saturating_sub(start);
            (next, i64::try_from(moved).unwrap_or(i64::MAX))
        } else {
            let amount = rows.unsigned_abs();
            let amount = usize::try_from(amount).unwrap_or(usize::MAX);
            let moved = amount.min(start);
            let next = start.saturating_sub(moved);
            (next, -i64::try_from(moved).unwrap_or(i64::MAX))
        };
        self.top = next;
        self.follow.suppressed_at_end = false;
        self.follow.following_end = next == max_top;
        if next != start || rows.signum() != moved.signum() {
            self.reveal_scrollbar(Instant::now());
        }
        rows.saturating_sub(moved)
    }

    /// Scroll to a semantic target with explicit follow behavior.
    #[must_use]
    pub fn scroll_to(&mut self, target: ScrollTarget, follow: FollowPolicy) -> EventResult {
        let Some(area) = self.area else {
            return EventResult::Consumed;
        };
        let max_top = self.max_top(area.height);
        let next = match target {
            ScrollTarget::Top => 0,
            ScrollTarget::Bottom => max_top,
            ScrollTarget::Row(row) => row.min(max_top),
        };
        let next_following = match follow {
            FollowPolicy::ResumeAtEnd => next == max_top,
            FollowPolicy::SuppressAtEnd => false,
        };
        let changed = self.top != next
            || self.follow.following_end != next_following
            || self.follow.suppressed_at_end
                != (follow == FollowPolicy::SuppressAtEnd && next == max_top);
        self.top = next;
        self.follow.following_end = next_following;
        self.follow.suppressed_at_end = follow == FollowPolicy::SuppressAtEnd && next == max_top;
        if changed {
            self.reveal_scrollbar(Instant::now());
            EventResult::Render
        } else {
            EventResult::Consumed
        }
    }

    /// Change viewport adornment styles.
    pub fn set_style(&mut self, style: FullscreenStyle) {
        self.style = style;
    }

    /// Change scrollbar visibility policy.
    pub fn set_scrollbar(&mut self, scrollbar: ScrollbarMode) {
        self.options.scrollbar = scrollbar;
        if scrollbar != ScrollbarMode::Auto {
            self.scrollbar.visible_until = None;
        }
    }

    /// Enable or disable automatic copy-on-select effects.
    pub fn set_copy_on_select(&mut self, enabled: bool) {
        self.options.copy_on_select = enabled;
    }

    /// Set the product-localized jump-to-end label.
    pub fn set_jump_to_end_label(&mut self, label: impl Into<String>) {
        self.jump_label = label.into();
    }

    /// Clear pointer, selection, search reveal, and transient scrollbar state.
    pub fn clear_interaction(&mut self) {
        self.clear_selection_state();
        self.scrollbar.drag = None;
        self.scrollbar.hovered = false;
        self.scrollbar.visible_until = None;
        self.last_click = None;
        self.pressed_link = None;
        self.jump_rect = None;
    }

    /// Return the current scrollbar geometry, if it is painted/hit-testable.
    #[must_use]
    pub fn scrollbar_geometry(&self) -> Option<ScrollbarGeometry> {
        self.scrollbar_geometry_with_auto_visibility(false)
    }

    fn scrollbar_geometry_with_auto_visibility(
        &self,
        include_hidden_auto: bool,
    ) -> Option<ScrollbarGeometry> {
        let area = self.area?;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let column = self.scrollbar_column(include_hidden_auto)?;
        let track_height = area.height;
        let track = u128::from(track_height);
        let thumb_height = if self.total_rows <= usize::from(area.height) {
            // Always-visible scrollbars still paint a full thumb for a
            // fitting or empty document. Avoid dividing by zero and keep the
            // track geometry available for the width-reserving column.
            track_height
        } else {
            let total = u128::try_from(self.total_rows).ok()?;
            round_ratio(track.saturating_mul(track), total)?
                .max(u128::from(track_height.min(2)))
                .min(track)
                .try_into()
                .ok()?
        };
        let max_top = self.max_top(area.height);
        let max_thumb = track_height.saturating_sub(thumb_height);
        let thumb_offset = if max_top == 0 {
            0
        } else {
            round_ratio(
                u128::try_from(self.top.min(max_top))
                    .ok()?
                    .saturating_mul(u128::from(max_thumb)),
                u128::try_from(max_top).ok()?,
            )?
            .min(u128::from(max_thumb))
            .try_into()
            .ok()?
        };
        Some(ScrollbarGeometry {
            column,
            track_top: area.y,
            track_height,
            thumb_top: area.y.saturating_add(thumb_offset),
            thumb_height,
            max_top,
        })
    }
    /// Drop cached offscreen image generations and return their deletions.
    ///
    /// The caller must route each returned deletion through the existing frame
    /// annotation/writer path before changing terminal modes.
    pub fn clear_image_cache(&mut self) -> Vec<crate::image::ImageCacheEviction> {
        self.image_cache.clear()
    }

    /// Show a transient inverse flash until `now + duration`.
    pub fn flash(&mut self, message: String, now: Instant, duration: Duration) {
        let expires_at = now.checked_add(duration).unwrap_or(now);
        let id = self.next_flash_id;
        self.next_flash_id = self.next_flash_id.saturating_add(1);
        self.flashes.push(FlashEntry {
            id,
            message,
            expires_at,
        });
        let bound = usize::from(self.area.map_or(1, |area| area.height.max(1)));
        if self.flashes.len() > bound {
            let remove = self.flashes.len().saturating_sub(bound);
            self.flashes.drain(0..remove);
        }
    }

    /// Earliest runtime-driven expiry/deadline, if any.
    ///
    /// A held scrollbar reveal is deliberately exempt: [`Self::tick`] keeps the
    /// stored expiry while hovered or dragged, so surfacing it here would make
    /// the product wake continuously on an un-expirable deadline.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        let flash = self.flashes.iter().map(|entry| entry.expires_at).min();
        let scroll = if self.scrollbar.hovered || self.scrollbar.drag.is_some() {
            None
        } else {
            self.scrollbar.visible_until
        };
        let selection = self.selection.auto_scroll_deadline;
        [flash, scroll, selection].into_iter().flatten().min()
    }

    /// Expire flashes and advance one or more due selection auto-scroll ticks.
    #[must_use]
    pub fn tick(&mut self, now: Instant) -> EventResult {
        let mut changed = false;
        let before = self.flashes.len();
        self.flashes.retain(|entry| entry.expires_at > now);
        changed |= before != self.flashes.len();
        if self.scrollbar.visible_until.is_some_and(|deadline| {
            deadline <= now && !self.scrollbar.hovered && self.scrollbar.drag.is_none()
        }) {
            self.scrollbar.visible_until = None;
            changed = true;
        }
        while self
            .selection
            .auto_scroll_deadline
            .is_some_and(|deadline| deadline <= now)
        {
            let direction = self.selection.auto_scroll_direction;
            if direction == 0 {
                self.stop_selection_autoscroll();
                break;
            }
            let remainder = self.scroll_by(i64::from(direction));
            if remainder == i64::from(direction) {
                self.stop_selection_autoscroll();
                break;
            }
            if let (Some((screen_x, screen_y)), Some(area)) = (self.selection.pointer, self.area) {
                let row = self.top.saturating_add(
                    usize::from(screen_y.saturating_sub(area.y))
                        .min(usize::from(area.height.saturating_sub(1))),
                );
                let column = usize::from(screen_x.saturating_sub(area.x))
                    .min(usize::from(self.content_width));
                self.update_selection_focus(SelectionPoint {
                    row,
                    col: column,
                    boundary: false,
                });
            }
            self.selection.auto_scroll_deadline = self
                .selection
                .auto_scroll_deadline
                .and_then(|deadline| deadline.checked_add(SELECTION_AUTOSCROLL));
            changed = true;
        }
        if changed {
            EventResult::Render
        } else {
            EventResult::Consumed
        }
    }

    /// Take and clear the current non-empty selection text.
    pub fn take_selection(&mut self) -> Option<String> {
        let text = self.selection_text();
        self.clear_selection_state();
        text
    }

    /// Borrow current search matches.
    #[must_use]
    pub fn search_matches(&self) -> &[SearchMatch] {
        self.search.index.matches()
    }

    /// Whether the product should mount a search overlay.
    #[must_use]
    pub const fn search_open(&self) -> bool {
        self.search.open
    }

    /// Open the search handoff state; the product mounts [`TranscriptSearch`].
    pub fn open_search(&mut self) {
        if self.search.open {
            return;
        }
        self.search.open = true;
        self.search.query.clear();
        self.search.selected = None;
        self.search.selected_key = None;
        self.search.selection_mode = SearchSelectionMode::Query;
        self.search.anchor_row = self.top;
        self.refresh_search();
    }

    /// Close search and clear its reveal state.
    pub fn close_search(&mut self) {
        self.search.open = false;
        self.search.query.clear();
        self.search.selected = None;
        self.search.selected_key = None;
        self.search.selection_mode = SearchSelectionMode::Retain;
        self.search.index = SearchIndex::new();
    }

    /// Replace the product-mounted search input's query.
    pub fn set_search_query(&mut self, query: impl Into<String>) {
        let query = query.into();
        if query == self.search.query {
            return;
        }
        self.search.query = query;
        self.search.selection_mode = SearchSelectionMode::Query;
        self.search.selected = None;
        self.refresh_search();
    }

    /// Navigate to the next or previous indexed match.
    pub fn navigate_search(&mut self, direction: SearchDirection) -> EventResult {
        if self.search.query.trim().is_empty() || self.search.index.matches().is_empty() {
            return EventResult::Consumed;
        }
        self.search.selection_mode = match direction {
            SearchDirection::Previous => SearchSelectionMode::Previous,
            SearchDirection::Next => SearchSelectionMode::Next,
        };
        self.refresh_search()
    }

    /// Return the row/cell key of the current match, if any.
    #[must_use]
    pub fn selected_search_index(&self) -> Option<usize> {
        self.search.selected
    }

    fn handle_key(
        &mut self,
        key: &crossterm::event::KeyEvent,
        keys: &KeybindingsManager,
    ) -> FullscreenEventResult {
        let release = is_key_release(key);
        if keys.matches(key, "tui.altScreen.search") {
            if release {
                return FullscreenEventResult::consumed();
            }
            if self.search.open {
                self.close_search();
            } else {
                self.open_search();
            }
            return FullscreenEventResult::render();
        }
        if let Some(result) = self.handle_search_key(key, keys, release) {
            return result;
        }
        if let Some(result) = self.handle_scroll_key(key, keys, release) {
            return result;
        }
        FullscreenEventResult::ignored()
    }

    fn handle_search_key(
        &mut self,
        key: &crossterm::event::KeyEvent,
        keys: &KeybindingsManager,
        release: bool,
    ) -> Option<FullscreenEventResult> {
        if !self.search.open {
            return None;
        }
        if keys.matches(key, "tui.altScreen.searchClose") {
            if release {
                return Some(FullscreenEventResult::consumed());
            }
            self.close_search();
            return Some(FullscreenEventResult::render());
        }
        if keys.matches(key, "tui.altScreen.searchNext") {
            if release {
                return Some(FullscreenEventResult::consumed());
            }
            let result = self.navigate_search(SearchDirection::Next);
            return Some(FullscreenEventResult {
                event_result: result,
                effect: None,
            });
        }
        if keys.matches(key, "tui.altScreen.searchPrevious") {
            if release {
                return Some(FullscreenEventResult::consumed());
            }
            let result = self.navigate_search(SearchDirection::Previous);
            return Some(FullscreenEventResult {
                event_result: result,
                effect: None,
            });
        }
        None
    }

    fn handle_scroll_key(
        &mut self,
        key: &crossterm::event::KeyEvent,
        keys: &KeybindingsManager,
        release: bool,
    ) -> Option<FullscreenEventResult> {
        let action = [
            ("tui.altScreen.pageUp", -self.page_step()),
            ("tui.altScreen.pageDown", self.page_step()),
            ("tui.altScreen.halfPageUp", -self.half_page_step()),
            ("tui.altScreen.halfPageDown", self.half_page_step()),
            ("tui.altScreen.lineUp", -1),
            ("tui.altScreen.lineDown", 1),
        ];
        for (binding, delta) in action {
            if keys.matches(key, binding) {
                if release {
                    return Some(FullscreenEventResult::consumed());
                }
                let previous_top = self.top;
                let previous_following = self.follow.following_end;
                self.scroll_by(delta);
                let event_result = if self.top != previous_top
                    || self.follow.following_end != previous_following
                {
                    EventResult::Render
                } else {
                    EventResult::Consumed
                };
                return Some(FullscreenEventResult {
                    event_result,
                    effect: None,
                });
            }
        }
        if keys.matches(key, "tui.altScreen.previousPrompt") {
            if release {
                return Some(FullscreenEventResult::consumed());
            }
            let previous_top = self.top;
            let previous_following = self.follow.following_end;
            self.scroll_prompt(-1);
            let event_result =
                if self.top != previous_top || self.follow.following_end != previous_following {
                    EventResult::Render
                } else {
                    EventResult::Consumed
                };
            return Some(FullscreenEventResult {
                event_result,
                effect: None,
            });
        }
        if keys.matches(key, "tui.altScreen.nextPrompt") {
            if release {
                return Some(FullscreenEventResult::consumed());
            }
            let previous_top = self.top;
            let previous_following = self.follow.following_end;
            self.scroll_prompt(1);
            let event_result =
                if self.top != previous_top || self.follow.following_end != previous_following {
                    EventResult::Render
                } else {
                    EventResult::Consumed
                };
            return Some(FullscreenEventResult {
                event_result,
                effect: None,
            });
        }
        if keys.matches(key, "tui.altScreen.top") {
            if release {
                return Some(FullscreenEventResult::consumed());
            }
            let event_result = self.scroll_to(ScrollTarget::Top, FollowPolicy::ResumeAtEnd);
            return Some(FullscreenEventResult {
                event_result,
                effect: None,
            });
        }
        if keys.matches(key, "tui.altScreen.bottom") {
            if release {
                return Some(FullscreenEventResult::consumed());
            }
            let event_result = self.scroll_to(ScrollTarget::Bottom, FollowPolicy::ResumeAtEnd);
            return Some(FullscreenEventResult {
                event_result,
                effect: None,
            });
        }
        None
    }

    fn normalize_mouse(&self, event: MouseEvent) -> TuiMouseEvent {
        let (kind, button, wheel_delta) = match event.kind {
            MouseEventKind::Down(button) => (TuiMouseEventType::Press, mouse_button(button), None),
            MouseEventKind::Up(button) => (TuiMouseEventType::Release, mouse_button(button), None),
            MouseEventKind::Drag(button) => (TuiMouseEventType::Drag, mouse_button(button), None),
            MouseEventKind::Moved => (TuiMouseEventType::Move, TuiMouseButton::None, None),
            MouseEventKind::ScrollUp => (TuiMouseEventType::Wheel, TuiMouseButton::None, Some(-1)),
            MouseEventKind::ScrollDown => (TuiMouseEventType::Wheel, TuiMouseButton::None, Some(1)),
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                (TuiMouseEventType::Wheel, TuiMouseButton::None, None)
            }
        };
        let area = self.area.unwrap_or(Rect::new(
            0,
            0,
            event.column.saturating_add(1),
            event.row.saturating_add(1),
        ));
        let x = i32::from(event.column).saturating_sub(i32::from(area.x));
        let y = i32::from(event.row).saturating_sub(i32::from(area.y));
        let wheel_delta = wheel_delta.map(|delta: i32| {
            let multiplier = if event
                .modifiers
                .contains(crossterm::event::KeyModifiers::ALT)
            {
                ALT_WHEEL_SCROLL_MULTIPLIER
            } else {
                1
            };
            delta
                .saturating_mul(multiplier)
                .saturating_mul(i32::from(self.options.wheel_scroll_lines.max(1)))
        });
        TuiMouseEvent {
            kind,
            button,
            x,
            y,
            screen_x: event.column,
            screen_y: event.row,
            width: area.width,
            height: area.height,
            modifiers: event.modifiers,
            wheel_delta,
            click_count: if kind == TuiMouseEventType::Press {
                self.next_click_count(event.column, event.row)
            } else {
                0
            },
        }
    }

    fn next_click_count(&self, _screen_x: u16, _screen_y: u16) -> u8 {
        self.last_click
            .map_or(1, |previous| previous.count.saturating_add(1).min(3))
    }

    fn begin_selection(
        &mut self,
        row: usize,
        col: usize,
        click_count: u8,
        now: Instant,
    ) -> FullscreenEventResult {
        let point = SelectionPoint {
            row: row.min(self.total_rows.saturating_sub(1)),
            col: col.min(self.content_width.into()),
            boundary: false,
        };
        let word = self.word_selection(point);
        let effective_count = if self.last_click.is_some_and(|previous| {
            now.saturating_duration_since(previous.at) <= DOUBLE_CLICK_INTERVAL
                && previous.row == point.row
                && previous.col == point.col
        }) {
            click_count.max(2)
        } else {
            1
        };
        self.last_click = Some(ClickRecord {
            at: now,
            row: point.row,
            col: point.col,
            count: if effective_count >= 3 {
                3
            } else if effective_count == 2 {
                2
            } else {
                1
            },
        });
        let range = match effective_count {
            2 => word,
            3 => Some(self.line_selection(point)),
            _ => None,
        };
        self.selection.granularity = if effective_count == 2 {
            SelectionGranularity::Word
        } else if effective_count >= 3 {
            SelectionGranularity::Line
        } else {
            SelectionGranularity::Character
        };
        self.selection.initial = range;
        self.selection.anchor = Some(range.map_or(point, |selection| selection.start));
        self.selection.focus = Some(range.map_or(point, |selection| selection.end));
        self.selection.dragging = true;
        self.selection.pointer = None;
        self.pressed_link = if range.is_none() {
            self.link_at(row, col)
        } else {
            None
        };
        FullscreenEventResult::render()
    }

    fn end_selection(&mut self, row: usize, col: usize) -> FullscreenEventResult {
        if !self.selection.dragging {
            return FullscreenEventResult::consumed();
        }
        self.selection.dragging = false;
        self.stop_selection_autoscroll();
        let point = SelectionPoint {
            row: row.min(self.total_rows.saturating_sub(1)),
            col: col.min(self.content_width.into()),
            boundary: false,
        };
        self.update_selection_focus(point);
        let same_point = self
            .selection
            .anchor
            .zip(self.selection.focus)
            .is_some_and(|(anchor, focus)| anchor == focus);
        if same_point {
            if let Some(url) = self.pressed_link.take() {
                self.clear_selection_state();
                return FullscreenEventResult {
                    event_result: EventResult::Consumed,
                    effect: Some(FullscreenEffect::OpenLink(url)),
                };
            }
            self.pressed_link = None;
            return FullscreenEventResult::render();
        }
        self.pressed_link = None;
        let effect = if self.options.copy_on_select {
            self.selection_text().map(FullscreenEffect::CopySelection)
        } else {
            None
        };
        FullscreenEventResult {
            event_result: EventResult::Render,
            effect,
        }
    }

    fn handle_scrollbar_mouse(
        &mut self,
        event: &TuiMouseEvent,
        x: i32,
        y: i32,
        now: Instant,
    ) -> bool {
        let Some(area) = self.area else {
            return false;
        };
        if let Some(drag) = self.scrollbar.drag {
            if event.kind == TuiMouseEventType::Release {
                self.scrollbar.drag = None;
                self.scrollbar.hovered = false;
                return true;
            }
            if event.kind == TuiMouseEventType::Drag {
                if let Some(geometry) = self.scrollbar_geometry() {
                    let pointer = i32::from(event.screen_y);
                    let track_top = i32::from(geometry.track_top);
                    let max_offset = geometry.track_height.saturating_sub(geometry.thumb_height);
                    let desired = pointer
                        .saturating_sub(track_top)
                        .saturating_sub(i32::from(drag.grab_offset))
                        .clamp(0, i32::from(max_offset));
                    let top = if max_offset == 0 {
                        0
                    } else {
                        round_ratio(
                            u128::try_from(desired)
                                .unwrap_or(0)
                                .saturating_mul(u128::try_from(geometry.max_top).unwrap_or(0)),
                            u128::from(max_offset),
                        )
                        .and_then(|value| usize::try_from(value).ok())
                        .unwrap_or(0)
                    };
                    let _ = self.scroll_to(ScrollTarget::Row(top), FollowPolicy::ResumeAtEnd);
                    self.reveal_scrollbar(now);
                }
                return true;
            }
            return false;
        }
        let Some(geometry) = self.scrollbar_geometry() else {
            return false;
        };
        if x + i32::from(area.x) != i32::from(geometry.column)
            || y + i32::from(area.y) < i32::from(geometry.track_top)
            || y + i32::from(area.y)
                >= i32::from(geometry.track_top) + i32::from(geometry.track_height)
        {
            return false;
        }
        if event.kind != TuiMouseEventType::Press {
            return false;
        }
        self.clear_selection_state();
        let pointer = i32::from(event.screen_y);
        let on_thumb = pointer >= i32::from(geometry.thumb_top)
            && pointer < i32::from(geometry.thumb_top) + i32::from(geometry.thumb_height);
        let grab_offset = if on_thumb {
            u16::try_from(pointer.saturating_sub(i32::from(geometry.thumb_top))).unwrap_or(0)
        } else {
            geometry.thumb_height / 2
        };
        if !on_thumb {
            let max_offset = geometry.track_height.saturating_sub(geometry.thumb_height);
            let desired = pointer
                .saturating_sub(i32::from(geometry.track_top))
                .saturating_sub(i32::from(grab_offset))
                .clamp(0, i32::from(max_offset));
            let top = if max_offset == 0 {
                0
            } else {
                round_ratio(
                    u128::try_from(desired)
                        .unwrap_or(0)
                        .saturating_mul(u128::try_from(geometry.max_top).unwrap_or(0)),
                    u128::from(max_offset),
                )
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0)
            };
            let _ = self.scroll_to(ScrollTarget::Row(top), FollowPolicy::ResumeAtEnd);
        }
        self.scrollbar.drag = Some(ScrollbarDrag { grab_offset });
        self.scrollbar.hovered = true;
        self.reveal_scrollbar(now);
        true
    }

    fn content_width_for(&self, width: u16) -> u16 {
        if self.options.scrollbar == ScrollbarMode::Always && width > 1 {
            width.saturating_sub(1)
        } else {
            width
        }
    }
    fn scrollbar_column(&self, include_hidden_auto: bool) -> Option<u16> {
        let area = self.area?;
        match self.options.scrollbar {
            ScrollbarMode::Hidden => None,
            ScrollbarMode::Always => {
                (area.width > 0).then(|| area.x.saturating_add(area.width.saturating_sub(1)))
            }
            ScrollbarMode::Auto => {
                if self.total_rows <= usize::from(area.height) {
                    return None;
                }
                let visible = include_hidden_auto
                    || self.scrollbar.hovered
                    || self.scrollbar.drag.is_some()
                    || self.scrollbar.visible_until.is_some();
                visible.then(|| area.x.saturating_add(area.width.saturating_sub(1)))
            }
        }
    }

    fn reveal_scrollbar(&mut self, now: Instant) {
        if self.options.scrollbar == ScrollbarMode::Auto {
            self.scrollbar.visible_until = now.checked_add(SCROLLBAR_REVEAL);
        }
    }

    fn max_top(&self, height: u16) -> usize {
        self.total_rows.saturating_sub(usize::from(height))
    }

    fn page_step(&self) -> i64 {
        let height = self.area.map_or(0, |area| usize::from(area.height));
        i64::try_from(height.saturating_sub(PAGE_SCROLL_OVERLAP).max(1)).unwrap_or(i64::MAX)
    }

    fn half_page_step(&self) -> i64 {
        let height = self.area.map_or(0, |area| usize::from(area.height));
        i64::try_from((height / 2).max(1)).unwrap_or(i64::MAX)
    }

    fn scroll_prompt(&mut self, direction: i8) {
        let rows = self.document.prompt_rows();
        let target = if direction < 0 {
            rows.iter()
                .copied()
                .take_while(|row| *row < self.top)
                .last()
        } else {
            rows.iter().copied().find(|row| *row > self.top)
        };
        if let Some(row) = target {
            let _ = self.scroll_to(ScrollTarget::Row(row), FollowPolicy::ResumeAtEnd);
        }
    }

    fn build_projection(&self) -> Result<Vec<String>, crate::component::RowSourceError> {
        let mut projection = Vec::with_capacity(self.total_rows);
        for row in 0..self.total_rows {
            let mut line = String::new();
            let mut current_width = 0usize;
            self.document.visit_row(row, &mut |span| {
                if current_width < usize::from(span.column) {
                    line.push_str(
                        &" ".repeat(usize::from(span.column).saturating_sub(current_width)),
                    );
                    current_width = usize::from(span.column);
                }
                match span.content {
                    DisplayRowContent::Text(keyed) => {
                        let plain = strip_ansi_text(keyed.line());
                        line.push_str(&plain);
                        current_width = current_width.saturating_add(visible_width(&plain));
                    }
                    DisplayRowContent::Image { image, .. } => {
                        if image.is_fallback()
                            && let Some(text) = image.fallback_text()
                        {
                            line.push_str(text);
                            current_width = current_width.saturating_add(visible_width(text));
                        }
                    }
                }
            })?;
            projection.push(line);
        }
        Ok(projection)
    }

    fn refresh_search(&mut self) -> EventResult {
        if !self.search.open {
            self.search.selected = None;
            self.search.selected_key = None;
            return EventResult::Consumed;
        }
        if self.search.query.trim().is_empty() {
            let generation = self
                .prepared_generation
                .unwrap_or(self.document.generation());
            let _ = self
                .search
                .index
                .search_generation(generation, &self.projection, "");
            self.search.selected = None;
            self.search.selected_key = None;
            return EventResult::Consumed;
        }
        let generation = self
            .prepared_generation
            .unwrap_or(self.document.generation());
        let result =
            self.search
                .index
                .search_generation(generation, &self.projection, &self.search.query);
        let matches = result.matches;
        if matches.is_empty() {
            self.search.selected = None;
            self.search.selected_key = None;
            return EventResult::Consumed;
        }
        let old_index = self.search.selected;
        let exact_index = self.search.selected_key.as_deref().and_then(|key| {
            matches
                .iter()
                .position(|search_match| search_match_key(search_match) == key)
        });
        let selected = match self.search.selection_mode {
            SearchSelectionMode::Query => {
                let first = matches.iter().position(|search_match| {
                    search_match
                        .segments
                        .first()
                        .is_some_and(|segment| segment.row >= self.search.anchor_row)
                });
                first.unwrap_or(0)
            }
            SearchSelectionMode::Next => {
                let base = exact_index.or(old_index).unwrap_or(0);
                base.saturating_add(1) % matches.len()
            }
            SearchSelectionMode::Previous => {
                let base = exact_index.or(old_index).unwrap_or(0);
                if base == 0 {
                    matches.len().saturating_sub(1)
                } else {
                    base - 1
                }
            }
            SearchSelectionMode::Retain => exact_index
                .or(old_index)
                .unwrap_or(0)
                .min(matches.len().saturating_sub(1)),
        };
        self.search.selected = Some(selected);
        self.search.selected_key = matches.get(selected).map(search_match_key);
        self.search.selection_mode = SearchSelectionMode::Retain;
        let Some(search_match) = matches.get(selected) else {
            return EventResult::Consumed;
        };
        let Some(first) = search_match.segments.first() else {
            return EventResult::Consumed;
        };
        let last_row = search_match
            .segments
            .last()
            .map_or(first.row, |segment| segment.row);
        let height = self.area.map_or(0, |area| usize::from(area.height));
        if height == 0 {
            return EventResult::Consumed;
        }
        let visible_bottom = self.top.saturating_add(height.saturating_sub(1));
        if first.row < self.top || last_row > visible_bottom {
            let target = first.row.saturating_sub(height / 3);
            return self.scroll_to(ScrollTarget::Row(target), FollowPolicy::SuppressAtEnd);
        }
        EventResult::Consumed
    }

    fn paint_search_highlights(&self, area: Rect, buf: &mut Buffer) {
        let Some(selected) = self.search.selected else {
            return;
        };
        let matches = self.search.index.matches();
        for (index, search_match) in matches.iter().enumerate() {
            for segment in &search_match.segments {
                if segment.row < self.top
                    || segment.row >= self.top.saturating_add(usize::from(area.height))
                {
                    continue;
                }
                let row = area.y.saturating_add(
                    u16::try_from(segment.row.saturating_sub(self.top)).unwrap_or(u16::MAX),
                );
                let start = segment.start_col.min(usize::from(self.content_width));
                let end = segment.end_col.min(usize::from(self.content_width));
                let style = if index == selected {
                    self.style.search_current_match
                } else {
                    self.style.search_match
                };
                apply_style_range(buf, area.x, row, start, end, style);
                crate::frame::claim_foreign_span(Rect::new(area.x, row, self.content_width, 1));
            }
        }
    }

    fn paint_selection(&self, area: Rect, buf: &mut Buffer) {
        let Some(selection) = self.selection_bounds() else {
            return;
        };
        let end_row = selection.end.row.min(self.total_rows.saturating_sub(1));
        for row in selection.start.row..=end_row {
            if row < self.top || row >= self.top.saturating_add(usize::from(area.height)) {
                continue;
            }
            let line = self.projection.get(row).map_or("", String::as_str);
            let line_width = visible_width(line);
            let mut start = 0usize;
            let mut end = line_width.min(usize::from(self.content_width));
            if row == selection.start.row {
                start = grapheme_start_at_column(line, selection.start.col)
                    .unwrap_or(selection.start.col.min(line_width));
            }
            if row == selection.end.row {
                end = if selection.end.boundary {
                    selection.end.col.min(line_width)
                } else {
                    grapheme_end_at_column(line, selection.end.col)
                        .unwrap_or(selection.end.col.saturating_add(1).min(line_width))
                };
            }
            let screen_y = area
                .y
                .saturating_add(u16::try_from(row.saturating_sub(self.top)).unwrap_or(u16::MAX));
            apply_style_range(
                buf,
                area.x,
                screen_y,
                start,
                end.min(usize::from(self.content_width)),
                Style::default().add_modifier(Modifier::REVERSED),
            );
            crate::frame::claim_foreign_span(Rect::new(area.x, screen_y, self.content_width, 1));
        }
    }

    fn paint_jump_to_end(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0
            || area.height == 0
            || self.follow.following_end
            || self.total_rows <= usize::from(area.height)
            || self.jump_label.is_empty()
        {
            return;
        }
        let Some(last_document_row) = self
            .top
            .checked_add(usize::from(area.height.saturating_sub(1)))
        else {
            return;
        };
        let mut last_row_has_image = false;
        let _ = self.document.visit_row(last_document_row, &mut |span| {
            if let DisplayRowContent::Image { image, .. } = span.content
                && !image.is_fallback()
            {
                last_row_has_image = true;
            }
        });
        if last_row_has_image {
            return;
        }
        let row = area.y.saturating_add(area.height.saturating_sub(1));
        let geometry = self.scrollbar_geometry();
        let right = geometry.map_or(area.x.saturating_add(self.content_width), |bar| bar.column);
        let available = usize::from(right.saturating_sub(area.x));
        let text = slice_by_column(&self.jump_label, 0, available, true);
        let width = visible_width(&text);
        if width == 0 || width > available {
            return;
        }
        let column = area
            .x
            .saturating_add(u16::try_from((available.saturating_sub(width)) / 2).unwrap_or(0));
        paint_line(column, row, width, buf, &text);
        apply_style_range(
            buf,
            0,
            row,
            usize::from(column),
            usize::from(column).saturating_add(width),
            self.style.jump_to_end,
        );
        self.jump_rect = Some(Rect::new(
            column,
            row,
            u16::try_from(width).unwrap_or(u16::MAX),
            1,
        ));
        crate::frame::claim_foreign_span(Rect::new(
            column,
            row,
            u16::try_from(width).unwrap_or(u16::MAX),
            1,
        ));
    }

    fn paint_scrollbar(&self, area: Rect, buf: &mut Buffer) {
        let Some(geometry) = self.scrollbar_geometry() else {
            return;
        };
        let thumb_char = if self.scrollbar.drag.is_some() {
            "█"
        } else {
            "┃"
        };
        for offset in 0..geometry.track_height {
            let y = geometry.track_top.saturating_add(offset);
            let thumb = offset >= geometry.thumb_top.saturating_sub(geometry.track_top)
                && offset
                    < geometry
                        .thumb_top
                        .saturating_sub(geometry.track_top)
                        .saturating_add(geometry.thumb_height);
            let style = if thumb {
                self.style.scrollbar_thumb
            } else {
                self.style.scrollbar_track
            };
            if let Some(cell) = buf.cell_mut((geometry.column, y)) {
                cell.set_symbol(if thumb { thumb_char } else { "│" });
                cell.set_style(style);
            }
            crate::frame::claim_opaque_span(Rect::new(geometry.column, y, 1, 1));
        }
        let _ = area;
    }

    fn paint_flashes(&self, area: Rect, buf: &mut Buffer) {
        let visible = self
            .flashes
            .iter()
            .rev()
            .take(usize::from(area.height))
            .collect::<Vec<_>>();
        let start = area
            .height
            .saturating_sub(u16::try_from(visible.len()).unwrap_or(area.height));
        for (index, entry) in visible.iter().rev().enumerate() {
            let row = area
                .y
                .saturating_add(start)
                .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
            let text = format!(" {} ", entry.message);
            let width = visible_width(&text).min(usize::from(area.width));
            if width == 0 {
                continue;
            }
            let text = slice_by_column(&text, 0, width, true);
            let text_width = visible_width(&text);
            let column = area.x.saturating_add(
                area.width
                    .saturating_sub(u16::try_from(text_width).unwrap_or(u16::MAX)),
            );
            paint_line(column, row, text_width, buf, &text);
            apply_style_range(
                buf,
                0,
                row,
                usize::from(column),
                usize::from(column).saturating_add(text_width),
                Style::default().add_modifier(Modifier::REVERSED),
            );
            crate::frame::claim_foreign_span(Rect::new(
                column,
                row,
                u16::try_from(text_width).unwrap_or(u16::MAX),
                1,
            ));
            let _ = entry.id;
        }
    }

    fn clear_selection_state(&mut self) {
        self.selection.anchor = None;
        self.selection.focus = None;
        self.selection.initial = None;
        self.selection.granularity = SelectionGranularity::Character;
        self.selection.dragging = false;
        self.stop_selection_autoscroll();
        self.pressed_link = None;
    }

    fn stop_selection_autoscroll(&mut self) {
        self.selection.pointer = None;
        self.selection.auto_scroll_direction = 0;
        self.selection.auto_scroll_deadline = None;
    }

    fn clamp_selection_rows(&mut self) {
        let max_row = self.total_rows.saturating_sub(1);
        for point in [&mut self.selection.anchor, &mut self.selection.focus] {
            if let Some(point) = point.as_mut() {
                point.row = point.row.min(max_row);
                point.col = point.col.min(usize::from(self.content_width));
            }
        }
    }

    fn selection_bounds(&self) -> Option<SelectionRange> {
        let (anchor, focus) = self.selection.anchor.zip(self.selection.focus)?;
        if anchor == focus {
            return None;
        }
        if anchor.row < focus.row || (anchor.row == focus.row && anchor.col <= focus.col) {
            Some(SelectionRange {
                start: anchor,
                end: focus,
            })
        } else {
            Some(SelectionRange {
                start: focus,
                end: anchor,
            })
        }
    }

    fn selection_text(&self) -> Option<String> {
        let selection = self.selection_bounds()?;
        let mut lines = Vec::new();
        for row in selection.start.row..=selection.end.row {
            let line = self.projection.get(row).map_or("", String::as_str);
            let line_width = visible_width(line);
            let mut start = 0usize;
            let mut end = line_width;
            if row == selection.start.row {
                start = grapheme_start_at_column(line, selection.start.col)
                    .unwrap_or(selection.start.col.min(line_width));
            }
            if row == selection.end.row {
                end = if selection.end.boundary {
                    selection.end.col.min(line_width)
                } else {
                    grapheme_end_at_column(line, selection.end.col)
                        .unwrap_or(selection.end.col.saturating_add(1).min(line_width))
                };
            }
            let text = slice_by_column(line, start, end.saturating_sub(start), true)
                .trim_end()
                .to_owned();
            lines.push(text);
        }
        let text = lines.join("\n");
        (!text.is_empty()).then_some(text)
    }

    fn update_selection_focus(&mut self, point: SelectionPoint) {
        let Some(initial) = self.selection.initial else {
            self.selection.focus = Some(point);
            return;
        };
        if self.selection.granularity == SelectionGranularity::Character {
            self.selection.focus = Some(point);
            return;
        }
        let range = match self.selection.granularity {
            SelectionGranularity::Word => self.word_selection(point),
            SelectionGranularity::Line => Some(self.line_selection(point)),
            SelectionGranularity::Character => None,
        };
        let Some(range) = range else {
            return;
        };
        let before = range.start.row < initial.start.row
            || (range.start.row == initial.start.row && range.start.col < initial.start.col);
        if before {
            self.selection.anchor = Some(initial.end);
            self.selection.focus = Some(range.start);
        } else {
            self.selection.anchor = Some(initial.start);
            self.selection.focus = Some(range.end);
        }
    }

    fn update_selection_autoscroll(&mut self, screen_x: u16, screen_y: u16, now: Instant) {
        let Some(area) = self.area else {
            self.stop_selection_autoscroll();
            return;
        };
        let direction = if screen_y <= area.y {
            -1
        } else {
            i8::from(screen_y >= area.y.saturating_add(area.height.saturating_sub(1)))
        };
        self.selection.pointer = Some((screen_x, screen_y));
        self.selection.auto_scroll_direction = direction;
        if direction == 0 {
            self.stop_selection_autoscroll();
        } else if self.selection.auto_scroll_deadline.is_none() {
            self.selection.auto_scroll_deadline = now.checked_add(SELECTION_AUTOSCROLL);
        }
    }

    fn word_selection(&self, point: SelectionPoint) -> Option<SelectionRange> {
        let line = self.projection.get(point.row).map_or("", String::as_str);
        let mut segments = Vec::new();
        let mut column = 0usize;
        for part in line.split_word_bounds() {
            let start = column;
            let width = visible_width(part);
            column = column.saturating_add(width);
            let joiner = part == "/" || part == "-";
            let selectable = joiner
                || part
                    .chars()
                    .any(|character| character.is_alphanumeric() || character == '_');
            segments.push((start, column, selectable, joiner));
        }
        let index = segments
            .iter()
            .position(|(start, end, _, _)| point.col >= *start && point.col < *end)?;
        let can_join = |left: (usize, usize, bool, bool), right: (usize, usize, bool, bool)| {
            left.2 && right.2 && (left.3 || right.3)
        };
        let mut start = segments.get(index).map_or(point.col, |segment| segment.0);
        let mut end = segments.get(index).map_or(point.col, |segment| segment.1);
        let mut current = index;
        while current > 0 {
            let Some(left) = segments.get(current.saturating_sub(1)).copied() else {
                break;
            };
            let Some(right) = segments.get(current).copied() else {
                break;
            };
            if !can_join(left, right) {
                break;
            }
            start = left.0;
            current = current.saturating_sub(1);
        }
        current = index;
        while current + 1 < segments.len() {
            let Some(left) = segments.get(current).copied() else {
                break;
            };
            let Some(right) = segments.get(current.saturating_add(1)).copied() else {
                break;
            };
            if !can_join(left, right) {
                break;
            }
            end = right.1;
            current = current.saturating_add(1);
        }
        Some(SelectionRange {
            start: SelectionPoint {
                row: point.row,
                col: start,
                boundary: false,
            },
            end: SelectionPoint {
                row: point.row,
                col: end,
                boundary: true,
            },
        })
    }

    fn line_selection(&self, point: SelectionPoint) -> SelectionRange {
        let width = self
            .projection
            .get(point.row)
            .map_or(0, |line| visible_width(line));
        SelectionRange {
            start: SelectionPoint {
                row: point.row,
                col: 0,
                boundary: false,
            },
            end: SelectionPoint {
                row: point.row,
                col: width,
                boundary: true,
            },
        }
    }

    fn link_at(&self, row: usize, col: usize) -> Option<String> {
        let mut found = None;
        let _ = self.document.visit_row(row, &mut |span| {
            let DisplayRowContent::Text(line) = span.content else {
                return;
            };
            let span_col = col.saturating_sub(usize::from(span.column));
            if col < usize::from(span.column) || span_col >= usize::from(span.width) {
                return;
            }
            found = link_at_column(line.line(), span_col);
        });
        found
    }
}

impl FullscreenEventResult {
    fn ignored() -> Self {
        Self {
            event_result: EventResult::Ignored,
            effect: None,
        }
    }

    fn consumed() -> Self {
        Self {
            event_result: EventResult::Consumed,
            effect: None,
        }
    }

    fn render() -> Self {
        Self {
            event_result: EventResult::Render,
            effect: None,
        }
    }
}

fn mouse_button(button: MouseButton) -> TuiMouseButton {
    match button {
        MouseButton::Left => TuiMouseButton::Left,
        MouseButton::Middle => TuiMouseButton::Middle,
        MouseButton::Right => TuiMouseButton::Right,
    }
}

fn round_ratio(numerator: u128, denominator: u128) -> Option<u128> {
    if denominator == 0 {
        return None;
    }
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    Some(quotient.saturating_add(u128::from(
        remainder >= denominator.saturating_sub(remainder),
    )))
}
fn mark_image_cells(buf: &mut Buffer, area: Rect) {
    for row in 0..area.height {
        for column in 0..area.width {
            if let Some(cell) =
                buf.cell_mut((area.x.saturating_add(column), area.y.saturating_add(row)))
            {
                cell.reset();
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
    }
    crate::frame::claim_opaque_span(area);
}

fn reset_row(buf: &mut Buffer, x: u16, y: u16, width: u16) {
    for offset in 0..width {
        if let Some(cell) = buf.cell_mut((x.saturating_add(offset), y)) {
            cell.reset();
        }
    }
}

fn reset_unpainted(buf: &mut Buffer, x: u16, y: u16, width: u16, painted: &[(usize, usize)]) {
    if painted.is_empty() {
        reset_row(buf, x, y, width);
        return;
    }
    let mut covered = painted.to_vec();
    covered.sort_unstable_by_key(|range| range.0);
    let mut cursor = 0usize;
    for (start, end) in covered {
        if start > cursor {
            reset_range(buf, x, y, cursor, start.min(usize::from(width)));
        }
        cursor = cursor.max(end);
    }
    if cursor < usize::from(width) {
        reset_range(buf, x, y, cursor, usize::from(width));
    }
}

fn reset_range(buf: &mut Buffer, x: u16, y: u16, start: usize, end: usize) {
    let start = u16::try_from(start).unwrap_or(u16::MAX);
    let end = u16::try_from(end).unwrap_or(u16::MAX);
    for offset in start..end {
        if let Some(cell) = buf.cell_mut((x.saturating_add(offset), y)) {
            cell.reset();
        }
    }
}

fn apply_style_range(buf: &mut Buffer, x: u16, y: u16, start: usize, end: usize, style: Style) {
    let start = u16::try_from(start).unwrap_or(u16::MAX);
    let end = u16::try_from(end).unwrap_or(u16::MAX);
    for offset in start..end {
        if let Some(cell) = buf.cell_mut((x.saturating_add(offset), y)) {
            let current = cell.style();
            cell.set_style(current.patch(style));
        }
    }
}

fn strip_ansi_text(line: &str) -> String {
    let mut result = String::new();
    let mut index = 0usize;
    while index < line.len() {
        if let Some(ansi) = extract_ansi_code(line, index) {
            index = index.saturating_add(ansi.len);
            continue;
        }
        let Some(character) = line.get(index..).and_then(|tail| tail.chars().next()) else {
            break;
        };
        result.push(character);
        index = index.saturating_add(character.len_utf8());
    }
    result
}

fn grapheme_start_at_column(line: &str, column: usize) -> Option<usize> {
    let mut current = 0usize;
    let mut index = 0usize;
    for grapheme in line.graphemes(true) {
        let width = grapheme_width(grapheme);
        if column >= current && column < current.saturating_add(width) {
            return Some(current);
        }
        current = current.saturating_add(width);
        index = index.saturating_add(grapheme.len());
    }
    (column == current).then_some(index)
}

fn grapheme_end_at_column(line: &str, column: usize) -> Option<usize> {
    let mut current = 0usize;
    for grapheme in line.graphemes(true) {
        let width = grapheme_width(grapheme);
        if column >= current && column < current.saturating_add(width) {
            return Some(current.saturating_add(width));
        }
        current = current.saturating_add(width);
    }
    None
}

fn link_at_column(line: &str, column: usize) -> Option<String> {
    let mut active = None;
    let mut current = 0usize;
    let mut index = 0usize;
    while index < line.len() {
        if let Some(ansi) = extract_ansi_code(line, index) {
            if let Some(parsed) = parse_osc8_hyperlink(ansi.code) {
                active = parsed.map(|link| link.url);
            }
            index = index.saturating_add(ansi.len);
            continue;
        }
        let Some(grapheme) = line
            .get(index..)
            .and_then(|tail| tail.graphemes(true).next())
        else {
            break;
        };
        let width = grapheme_width(grapheme);
        if column >= current && column < current.saturating_add(width) {
            return active;
        }
        current = current.saturating_add(width);
        index = index.saturating_add(grapheme.len());
    }
    None
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test code"
)]
mod tests {
    use super::*;
    use crate::alt_screen::document::{DocumentBlock, DocumentBlockId};
    use crate::components::Text;

    fn viewport(lines: &[&str]) -> FullscreenViewport {
        let mut viewport = FullscreenViewport::default();
        for line in lines {
            let id = DocumentBlockId::new().expect("id");
            viewport.document_mut().upsert(DocumentBlock {
                id,
                zone: None,
                component: Box::new(Text::with_padding(*line, 0, 0)),
            });
        }
        viewport.prepare(Rect::new(0, 0, 20, 2)).expect("prepare");
        viewport
    }

    #[test]
    fn follow_end_suppression_survives_end_reveal() {
        let mut viewport = viewport(&["one", "two", "three"]);
        assert!(viewport.follow.following_end);
        let _ = viewport.scroll_to(ScrollTarget::Bottom, FollowPolicy::SuppressAtEnd);
        assert!(!viewport.follow.following_end);
        viewport.document_mut().upsert(DocumentBlock {
            id: DocumentBlockId::new().expect("id"),
            zone: None,
            component: Box::new(Text::with_padding("four", 0, 0)),
        });
        viewport.prepare(Rect::new(0, 0, 20, 2)).expect("prepare");
        assert!(!viewport.follow.following_end);
    }

    #[test]
    fn scrollbar_thumb_is_bounded_and_rounds() {
        let mut viewport = viewport(&["a", "b", "c", "d", "e", "f"]);
        viewport.set_scrollbar(ScrollbarMode::Always);
        viewport.prepare(Rect::new(0, 0, 20, 3)).expect("prepare");
        let geometry = viewport.scrollbar_geometry().expect("scrollbar");
        assert!(geometry.thumb_height >= 2);
        assert!(
            geometry.thumb_top + geometry.thumb_height
                <= geometry.track_top + geometry.track_height
        );
    }

    #[test]
    fn selection_respects_wide_grapheme_boundaries() {
        let mut viewport = viewport(&["a界b"]);
        viewport.selection.anchor = Some(SelectionPoint {
            row: 0,
            col: 1,
            boundary: false,
        });
        viewport.selection.focus = Some(SelectionPoint {
            row: 0,
            col: 2,
            boundary: false,
        });
        assert_eq!(viewport.selection_text().as_deref(), Some("界"));
    }

    fn pointer(
        kind: TuiMouseEventType,
        button: TuiMouseButton,
        screen_x: u16,
        screen_y: u16,
    ) -> TuiMouseEvent {
        TuiMouseEvent {
            kind,
            button,
            x: i32::from(screen_x),
            y: i32::from(screen_y),
            screen_x,
            screen_y,
            width: 20,
            height: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
            wheel_delta: None,
            click_count: 0,
        }
    }

    #[test]
    fn hovered_scrollbar_reveal_offers_no_wakeup() {
        let start = Instant::now();
        let mut viewport = viewport(&["a", "b", "c", "d", "e", "f"]);
        viewport.set_scrollbar(ScrollbarMode::Auto);
        assert_eq!(viewport.next_deadline(), None);

        let _ = viewport.handle_mouse(
            &pointer(TuiMouseEventType::Move, TuiMouseButton::None, 19, 0),
            start,
        );
        let expiry = start + SCROLLBAR_REVEAL;
        // Hovering holds the reveal open, so the product must not be woken for it.
        assert_eq!(viewport.next_deadline(), None);
        assert_eq!(viewport.tick(expiry), EventResult::Consumed);
        assert_eq!(viewport.next_deadline(), None);
        // Other deadlines stay reachable while the scrollbar is held.
        viewport.flash("outlast".to_owned(), start, SCROLLBAR_REVEAL * 2);
        assert_eq!(viewport.next_deadline(), Some(start + SCROLLBAR_REVEAL * 2));

        // Leaving the track re-enables expiration of the retained reveal.
        let _ = viewport.handle_mouse(
            &pointer(TuiMouseEventType::Move, TuiMouseButton::None, 0, 0),
            start + Duration::from_millis(400),
        );
        assert_eq!(viewport.next_deadline(), Some(expiry));
    }

    #[test]
    fn dragged_scrollbar_reveal_offers_no_wakeup_until_release() {
        let start = Instant::now();
        let mut viewport = viewport(&["a", "b", "c", "d", "e", "f"]);
        viewport.set_scrollbar(ScrollbarMode::Auto);
        let _ = viewport.handle_mouse(
            &pointer(TuiMouseEventType::Move, TuiMouseButton::None, 19, 0),
            start,
        );
        let grab = start + Duration::from_millis(200);
        let _ = viewport.handle_mouse(
            &pointer(TuiMouseEventType::Press, TuiMouseButton::Left, 19, 0),
            grab,
        );
        let expiry = grab + SCROLLBAR_REVEAL;

        // Pointer leaves the track while the button is still held.
        let wander = start + Duration::from_millis(300);
        let _ = viewport.handle_mouse(
            &pointer(TuiMouseEventType::Move, TuiMouseButton::None, 0, 0),
            wander,
        );
        assert_eq!(viewport.next_deadline(), None);
        assert_eq!(viewport.tick(expiry), EventResult::Consumed);
        assert_eq!(viewport.next_deadline(), None);

        let _ = viewport.handle_mouse(
            &pointer(TuiMouseEventType::Release, TuiMouseButton::Left, 0, 5),
            expiry,
        );
        assert_eq!(viewport.next_deadline(), Some(expiry));
        assert_eq!(viewport.tick(expiry), EventResult::Render);
        assert_eq!(viewport.next_deadline(), None);
    }
}
