//! Braille spinner loader driven by external `advance(Instant)` ticks.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tokio_util::sync::CancellationToken;

use crate::component::{Component, EventResult, UiEvent};
use crate::keybindings::get_keybindings;

use super::text::Text;

/// Default braille frames (TS `DEFAULT_FRAMES`).
pub const DEFAULT_LOADER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Default frame interval (80 ms).
pub const DEFAULT_INTERVAL_MS: u64 = 80;

/// Optional indicator override.
#[derive(Debug, Clone)]
pub struct LoaderIndicatorOptions {
    /// Animation frames. Empty hides the indicator.
    pub frames: Option<Vec<String>>,
    /// Frame interval in milliseconds.
    pub interval_ms: Option<u64>,
}

/// Loader with braille spinner. **No internal timer** — call [`Loader::advance`]
/// from the product coalescer / event loop.
pub struct Loader {
    inner: Text,
    frames: Vec<String>,
    interval: Duration,
    current_frame: usize,
    last_advance: Option<Instant>,
    message: String,
    spinner_color: Box<dyn Fn(&str) -> String + Send>,
    message_color: Box<dyn Fn(&str) -> String + Send>,
    render_indicator_verbatim: bool,
    running: bool,
}

impl Loader {
    /// Create a loader. Padding matches TS: `padding_x=1`, `padding_y=0`.
    pub fn new(
        spinner_color: impl Fn(&str) -> String + Send + 'static,
        message_color: impl Fn(&str) -> String + Send + 'static,
        message: impl Into<String>,
        indicator: Option<LoaderIndicatorOptions>,
    ) -> Self {
        let mut loader = Self {
            inner: Text::with_padding("", 1, 0),
            frames: DEFAULT_LOADER_FRAMES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            interval: Duration::from_millis(DEFAULT_INTERVAL_MS),
            current_frame: 0,
            last_advance: None,
            message: message.into(),
            spinner_color: Box::new(spinner_color),
            message_color: Box::new(message_color),
            render_indicator_verbatim: false,
            running: false,
        };
        loader.set_indicator(indicator);
        loader
    }

    /// Update the message and refresh display text.
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.update_display();
    }

    /// Borrow the current message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Replace frames / interval. Resets to frame 0 and starts.
    pub fn set_indicator(&mut self, indicator: Option<LoaderIndicatorOptions>) {
        self.render_indicator_verbatim = indicator.is_some();
        if let Some(opts) = indicator {
            if let Some(frames) = opts.frames {
                self.frames = frames;
            } else {
                self.frames = DEFAULT_LOADER_FRAMES
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect();
            }
            let ms = opts
                .interval_ms
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_INTERVAL_MS);
            self.interval = Duration::from_millis(ms);
        } else {
            self.frames = DEFAULT_LOADER_FRAMES
                .iter()
                .map(|s| (*s).to_owned())
                .collect();
            self.interval = Duration::from_millis(DEFAULT_INTERVAL_MS);
        }
        self.current_frame = 0;
        self.start();
    }

    /// Mark running and refresh the display (does not spawn a timer).
    pub fn start(&mut self) {
        self.running = true;
        self.last_advance = None;
        self.update_display();
    }

    /// Stop animation advances.
    pub fn stop(&mut self) {
        self.running = false;
    }

    /// Whether the loader is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Advance the spinner based on `now`. Returns `true` when the frame changed
    /// (caller should request a render).
    pub fn advance(&mut self, now: Instant) -> bool {
        if !self.running || self.frames.len() <= 1 {
            return false;
        }
        match self.last_advance {
            None => {
                self.last_advance = Some(now);
                false
            }
            Some(prev) if now.duration_since(prev) >= self.interval => {
                // Advance by whole steps for deterministic catch-up.
                let steps = (now.duration_since(prev).as_millis()
                    / self.interval.as_millis().max(1)) as usize;
                if steps == 0 {
                    return false;
                }
                self.current_frame = (self.current_frame + steps) % self.frames.len();
                self.last_advance = Some(
                    prev + self
                        .interval
                        .saturating_mul(u32::try_from(steps).unwrap_or(u32::MAX)),
                );
                self.update_display();
                true
            }
            Some(_) => false,
        }
    }

    /// Force a specific frame index (testing / deterministic control).
    pub fn set_frame_index(&mut self, index: usize) {
        if self.frames.is_empty() {
            self.current_frame = 0;
        } else {
            self.current_frame = index % self.frames.len();
        }
        self.update_display();
    }

    /// Current frame index.
    #[must_use]
    pub fn frame_index(&self) -> usize {
        self.current_frame
    }

    fn update_display(&mut self) {
        let frame = self
            .frames
            .get(self.current_frame)
            .cloned()
            .unwrap_or_default();
        let rendered_frame = if self.render_indicator_verbatim {
            frame.clone()
        } else {
            (self.spinner_color)(&frame)
        };
        let indicator = if frame.is_empty() {
            String::new()
        } else {
            format!("{rendered_frame} ")
        };
        let text = format!("{indicator}{}", (self.message_color)(&self.message));
        self.inner.set_text(text);
    }
}

impl Component for Loader {
    fn measure(&mut self, width: u16) -> u16 {
        // Leading blank row + Text height (TS: `["", ...super.render(width)]`).
        self.inner.measure(width).saturating_add(1)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        // First row blank; remaining rows from Text.
        let text_area = Rect {
            x: area.x,
            y: area.y.saturating_add(1),
            width: area.width,
            height: area.height.saturating_sub(1),
        };
        if text_area.height > 0 {
            self.inner.render(text_area, buf);
        }
    }

    fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
        EventResult::Ignored
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
    }
}

/// Loader that cancels a [`CancellationToken`] on Escape / select-cancel.
pub struct CancellableLoader {
    loader: Loader,
    token: CancellationToken,
    /// Called when the user aborts.
    pub on_abort: Option<Box<dyn FnMut() + Send>>,
}

impl CancellableLoader {
    /// Create a cancellable loader with a fresh token.
    pub fn new(
        spinner_color: impl Fn(&str) -> String + Send + 'static,
        message_color: impl Fn(&str) -> String + Send + 'static,
        message: impl Into<String>,
        indicator: Option<LoaderIndicatorOptions>,
    ) -> Self {
        Self {
            loader: Loader::new(spinner_color, message_color, message, indicator),
            token: CancellationToken::new(),
            on_abort: None,
        }
    }

    /// Cancellation token aborted on cancel key.
    #[must_use]
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Borrow the inner loader.
    pub fn loader(&mut self) -> &mut Loader {
        &mut self.loader
    }

    /// Advance the spinner (delegates).
    pub fn advance(&mut self, now: Instant) -> bool {
        self.loader.advance(now)
    }

    /// Stop the loader.
    pub fn stop(&mut self) {
        self.loader.stop();
    }
}

impl Component for CancellableLoader {
    fn measure(&mut self, width: u16) -> u16 {
        self.loader.measure(width)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.loader.render(area, buf);
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        let UiEvent::Key(key) = event else {
            return EventResult::Ignored;
        };
        let kb = get_keybindings();
        if kb.matches(key, "tui.select.cancel") {
            self.token.cancel();
            if let Some(cb) = self.on_abort.as_mut() {
                cb();
            }
            return EventResult::Consumed;
        }
        EventResult::Ignored
    }

    fn invalidate(&mut self) {
        self.loader.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::util::render_snapshot;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn id(s: &str) -> String {
        s.to_owned()
    }

    #[test]
    fn advance_is_deterministic() {
        let mut loader = Loader::new(id, id, "Loading...", None);
        loader.start();
        let t0 = Instant::now();
        assert!(!loader.advance(t0));
        assert_eq!(loader.frame_index(), 0);
        assert!(loader.advance(t0 + Duration::from_millis(80)));
        assert_eq!(loader.frame_index(), 1);
        assert!(loader.advance(t0 + Duration::from_millis(240)));
        // from last_advance ~ t0+80, +160ms = 2 steps
        assert_eq!(loader.frame_index(), 3);
    }

    #[test]
    fn single_frame_never_advances() {
        let mut loader = Loader::new(
            id,
            id,
            "x",
            Some(LoaderIndicatorOptions {
                frames: Some(vec!["*".into()]),
                interval_ms: Some(80),
            }),
        );
        let t0 = Instant::now();
        assert!(!loader.advance(t0));
        assert!(!loader.advance(t0 + Duration::from_millis(500)));
    }

    #[test]
    fn leading_blank_row() {
        let mut loader = Loader::new(id, id, "Working", None);
        let h = loader.measure(40);
        assert!(h >= 2);
        let snap = render_snapshot(&mut loader, 40);
        assert_eq!(u16::try_from(snap.len()).unwrap_or(u16::MAX), h);
    }

    #[test]
    fn cancellable_escape() {
        let mut loader = CancellableLoader::new(id, id, "x", None);
        let key = UiEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(loader.handle_event(&key), EventResult::Consumed);
        assert!(loader.is_cancelled());
    }
}
