//! Standalone interactive project-trust selector used at CLI startup.
//!
//! Owns the terminal loop for [`crate::core::trust::TrustUi`] while keeping
//! `core::trust` terminal-agnostic. Reuses the same TerminalGuard / TerminalInput
//! / Tui machinery as [`super::config_selector`].

use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyModifiers};
use pi_tui::component::{Component, EventResult, UiEvent};
use pi_tui::components::{SelectItem, SelectList, SelectListTheme, Spacer, Text};
use pi_tui::terminal::{
    TerminalCapabilities, TerminalGuard, TerminalInput, Tui, Txn, install_panic_emergency_hook,
    write_emergency_restore_bytes,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::core::trust::TrustUi;
use crate::modes::interactive::selectors::{SELECTOR_EXIT_HINT, SELECTOR_MAX_VISIBLE};
use crate::modes::interactive::theme;

/// Production [`TrustUi`] adapter for interactive CLI startup only.
pub struct StartupTrustUi {
    interactive: bool,
}

impl StartupTrustUi {
    /// Build an adapter that may open a dialog only in interactive mode.
    #[must_use]
    pub const fn new(interactive: bool) -> Self {
        Self { interactive }
    }

    /// Convenience constructor for interactive prompt mode.
    #[must_use]
    pub const fn interactive() -> Self {
        Self::new(true)
    }
}

impl TrustUi for StartupTrustUi {
    fn has_ui(&self) -> bool {
        self.interactive && io::stdin().is_terminal() && io::stdout().is_terminal()
    }

    fn select(&mut self, prompt: &str, options: &[String]) -> Option<String> {
        if !self.has_ui() || options.is_empty() {
            return None;
        }
        // Bootstrap owns a multi-thread runtime; park this worker while the
        // standalone TUI drives TerminalInput / paint futures.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(run_trust_selector(prompt, options))
        })
        .unwrap_or(None)
    }
}

type SharedResult = Arc<Mutex<Option<Option<String>>>>;

/// Vertical title + detail + select-list view used by the startup trust dialog.
pub struct TrustPromptView {
    title: Text,
    detail: Text,
    spacer: Spacer,
    list: SelectList,
    result: SharedResult,
    finish_count: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
}

/// Replace C0/C1 and bidi controls so a hostile cwd cannot alter the trust prompt.
#[must_use]
pub fn sanitize_trust_path_detail(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        let code = u32::from(ch);
        let is_c0 = code < 0x20;
        let is_del_or_c1 = (0x7F..=0x9F).contains(&code);
        let is_bidi = matches!(
            code,
            0x061C
                | 0x200E
                | 0x200F
                | 0x202A
                | 0x202B
                | 0x202C
                | 0x202D
                | 0x202E
                | 0x2066
                | 0x2067
                | 0x2068
                | 0x2069
        );
        if is_c0 || is_del_or_c1 || is_bidi {
            out.push('\u{FFFD}');
        } else {
            out.push(ch);
        }
    }
    out
}

fn split_trust_prompt(prompt: &str) -> (String, String) {
    let mut lines = prompt.lines();
    let title = lines
        .next()
        .unwrap_or("Trust project folder?")
        .trim()
        .to_owned();
    // One structural detail line: the path (second prompt line), sanitized.
    let detail = lines
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(sanitize_trust_path_detail)
        .unwrap_or_default();
    (title, detail)
}

impl TrustPromptView {
    /// Build a focused trust prompt with row 0 selected.
    #[must_use]
    pub fn new(prompt: &str, options: &[String]) -> Self {
        let items = options
            .iter()
            .map(|label| SelectItem::new(label.clone(), sanitize_trust_path_detail(label)))
            .collect::<Vec<_>>();
        let mut list = SelectList::new(items, SELECTOR_MAX_VISIBLE, select_list_theme())
            .with_hint(SELECTOR_EXIT_HINT);
        list.set_selected_index(0);

        let result: SharedResult = Arc::new(Mutex::new(None));
        let finish_count = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));

        {
            let result = Arc::clone(&result);
            let finish_count = Arc::clone(&finish_count);
            let closed = Arc::clone(&closed);
            list.on_select = Some(Box::new(move |item| {
                finish_once(&result, &finish_count, &closed, Some(item.value.clone()));
            }));
        }
        {
            let result = Arc::clone(&result);
            let finish_count = Arc::clone(&finish_count);
            let closed = Arc::clone(&closed);
            list.on_cancel = Some(Box::new(move || {
                finish_once(&result, &finish_count, &closed, None);
            }));
        }

        let (title, detail) = split_trust_prompt(prompt);
        Self {
            title: Text::with_padding(title, 1, 0),
            detail: Text::with_padding(detail, 1, 0),
            spacer: Spacer::new(1),
            list,
            result,
            finish_count,
            closed,
        }
    }

    /// Shared finish counter for cleanup-once assertions.
    #[must_use]
    pub fn finish_count(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.finish_count)
    }

    /// Whether the dialog has produced a result.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Take the dialog result (`Some(label)` or `None` on cancel).
    pub fn take_result(&mut self) -> Option<Option<String>> {
        self.result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn cancel_from_ctrl_c(&mut self) {
        finish_once(&self.result, &self.finish_count, &self.closed, None);
    }
}

fn finish_once(
    result: &SharedResult,
    finish_count: &AtomicUsize,
    closed: &AtomicBool,
    value: Option<String>,
) {
    if finish_count.fetch_add(1, Ordering::SeqCst) != 0 {
        return;
    }
    if let Ok(mut guard) = result.lock() {
        *guard = Some(value);
    }
    closed.store(true, Ordering::SeqCst);
}

impl Component for TrustPromptView {
    fn measure(&mut self, width: u16) -> u16 {
        self.title
            .measure(width)
            .saturating_add(self.detail.measure(width))
            .saturating_add(self.spacer.measure(width))
            .saturating_add(self.list.measure(width))
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        // Reserve option-list rows first so a hostile/long path cannot consume the
        // viewport and visually hide Trust / Do not trust.
        let list_h = self.list.measure(area.width).min(area.height);
        let header_budget = area.height.saturating_sub(list_h);
        let mut y = area.y;
        let mut header_remaining = header_budget;

        let title_h = self.title.measure(area.width).min(header_remaining);
        if title_h > 0 {
            self.title
                .render(Rect::new(area.x, y, area.width, title_h), buf);
            y = y.saturating_add(title_h);
            header_remaining = header_remaining.saturating_sub(title_h);
        }

        let detail_h = self.detail.measure(area.width).min(header_remaining);
        if detail_h > 0 {
            self.detail
                .render(Rect::new(area.x, y, area.width, detail_h), buf);
            y = y.saturating_add(detail_h);
            header_remaining = header_remaining.saturating_sub(detail_h);
        }

        let spacer_h = self.spacer.measure(area.width).min(header_remaining);
        if spacer_h > 0 {
            self.spacer
                .render(Rect::new(area.x, y, area.width, spacer_h), buf);
            y = y.saturating_add(spacer_h);
        }

        if list_h > 0 {
            // Pin the list to the bottom of the reserved region.
            let list_y = area.bottom().saturating_sub(list_h).max(y);
            let list_h = area.bottom().saturating_sub(list_y).min(list_h);
            if list_h > 0 {
                self.list
                    .render(Rect::new(area.x, list_y, area.width, list_h), buf);
            }
        }
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        if self.is_finished() {
            return EventResult::Consumed;
        }
        if let UiEvent::Key(key) = event
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c'))
        {
            self.cancel_from_ctrl_c();
            return EventResult::Consumed;
        }
        self.list.handle_event(event)
    }

    fn invalidate(&mut self) {
        self.title.invalidate();
        self.detail.invalidate();
        self.spacer.invalidate();
        self.list.invalidate();
    }
}

fn select_list_theme() -> SelectListTheme {
    theme::select_list_theme()
}

/// Run the standalone trust selector and return the chosen label (or `None`).
///
/// # Errors
///
/// Returns a terminal/TUI initialization or paint error. Cancellation is `Ok(None)`.
pub async fn run_trust_selector(
    prompt: &str,
    options: &[String],
) -> Result<Option<String>, String> {
    let mut view = TrustPromptView::new(prompt, options);
    run_standalone_trust_view(&mut view).await?;
    Ok(view.take_result().unwrap_or(None))
}

async fn run_standalone_trust_view(view: &mut TrustPromptView) -> Result<(), String> {
    let reported = crossterm::terminal::size().unwrap_or((80, 24));
    let size = if reported.0 == 0 || reported.1 == 0 {
        (80, 24)
    } else {
        reported
    };
    let mut guard = TerminalGuard::new(io::stdout());
    guard.set_viewport_bottom_row(size.1.saturating_sub(1));
    let emergency = guard.emergency_flag();
    {
        let restore_writer = Arc::new(Mutex::new(io::stdout()));
        install_panic_emergency_hook(
            Arc::clone(&emergency),
            Arc::new(move || {
                if let Ok(mut writer) = restore_writer.lock() {
                    let _ = write_emergency_restore_bytes(&mut *writer);
                }
            }),
        );
    }
    // Keep legacy keyboard encoding for the short-lived startup dialog so the
    // PTY harness can drive Enter/Esc with CR / ESC, and so handoff back to
    // the interactive runtime does not leave Kitty progressive keys pushed.
    guard
        .activate(false)
        .map_err(|error| format!("terminal activation failed: {error}"))?;

    // Already running under block_in_place; detect synchronously to avoid
    // nested spawn_blocking pool contention during startup.
    let caps = TerminalCapabilities::detect();
    let mut tui = Tui::new(
        io::stdout(),
        ratatui::layout::Size::new(size.0, size.1),
        ratatui::layout::Position::ORIGIN,
        size.1.max(1),
        caps,
    )
    .map_err(|error| format!("tui initialization failed: {error}"))?;
    let mut input = TerminalInput::spawn();
    tui.commit(Txn::Frame, view)
        .map_err(|error| format!("tui paint failed: {error}"))?;

    let loop_result: Result<(), String> = async {
        while !view.is_finished() {
            let Some(event) = input.recv().await else {
                break;
            };
            if let UiEvent::Resize { width, height } = event {
                tui.note_resize(width, height);
                guard.set_viewport_bottom_row(height.saturating_sub(1));
                view.invalidate();
                tui.commit(Txn::Frame, view)
                    .map_err(|error| format!("tui paint failed: {error}"))?;
                continue;
            }
            let result = view.handle_event(&event);
            if view.is_finished() {
                break;
            }
            if result.needs_render() || result.is_handled() {
                tui.commit(Txn::Frame, view)
                    .map_err(|error| format!("tui paint failed: {error}"))?;
            }
        }
        Ok(())
    }
    .await;

    let shutdown_result = input.shutdown().await;
    drop(tui);
    // TerminalGuard restores/clears exactly once here on drop.
    drop(guard);
    loop_result?;
    shutdown_result.map_err(|error| format!("terminal input shutdown failed: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::trust::get_project_trust_options;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> UiEvent {
        UiEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl_c() -> UiEvent {
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
    }

    #[test]
    fn options_follow_core_order_with_default_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let options = get_project_trust_options(&cwd, true);
        let labels: Vec<String> = options.into_iter().map(|option| option.label).collect();
        assert_eq!(labels.first().map(String::as_str), Some("Trust"));
        let mut view = TrustPromptView::new("Trust project folder?", &labels);
        assert!(!view.is_finished());
        assert_eq!(view.measure(80) > 0, true);
        let _ = view.handle_event(&key(KeyCode::Enter));
        assert_eq!(view.take_result(), Some(Some("Trust".to_owned())));
        assert_eq!(view.finish_count().load(Ordering::SeqCst), 1);
    }

    #[test]
    fn down_enter_selects_next_label() {
        let labels = vec![
            "Trust".to_owned(),
            "Do not trust".to_owned(),
            "Do not trust (this session only)".to_owned(),
        ];
        let mut view = TrustPromptView::new("prompt", &labels);
        assert_eq!(view.handle_event(&key(KeyCode::Down)), EventResult::Render);
        let _ = view.handle_event(&key(KeyCode::Enter));
        assert_eq!(view.take_result(), Some(Some("Do not trust".to_owned())));
        assert_eq!(view.finish_count().load(Ordering::SeqCst), 1);
    }

    #[test]
    fn escape_cancels_once() {
        let labels = vec!["Trust".to_owned(), "Do not trust".to_owned()];
        let mut view = TrustPromptView::new("prompt", &labels);
        let _ = view.handle_event(&key(KeyCode::Esc));
        assert_eq!(view.take_result(), Some(None));
        let _ = view.handle_event(&key(KeyCode::Esc));
        assert_eq!(view.finish_count().load(Ordering::SeqCst), 1);
        assert!(view.take_result().is_none());
    }

    #[test]
    fn ctrl_c_cancels_once() {
        let labels = vec!["Trust".to_owned()];
        let mut view = TrustPromptView::new("prompt", &labels);
        let _ = view.handle_event(&ctrl_c());
        assert_eq!(view.take_result(), Some(None));
        let _ = view.handle_event(&ctrl_c());
        assert_eq!(view.finish_count().load(Ordering::SeqCst), 1);
    }

    #[test]
    fn noninteractive_adapter_reports_no_ui() {
        let ui = StartupTrustUi::new(false);
        assert!(!ui.has_ui());
    }

    #[test]
    fn hostile_path_cannot_hide_trust_choices() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let hostile_label = "Trust parent folder (proj\u{1b}[2J\u{202e})".to_owned();
        let labels = vec![hostile_label.clone(), "Do not trust".to_owned()];
        let mut hostile_path = String::from("proj");
        hostile_path.push('\n');
        hostile_path.push('\r');
        hostile_path.push_str("\u{1b}[2J");
        hostile_path.push('\u{202e}'); // RLO bidi
        hostile_path.push('\u{2066}'); // LRI
        hostile_path.push_str(&"X".repeat(400));
        let prompt =
            format!("Trust project folder?\n{hostile_path}\n\nThis allows pi to load settings.");
        let mut view = TrustPromptView::new(&prompt, &labels);

        // Tight viewport that a naive multiline title could fully consume.
        let area = Rect::new(0, 0, 48, 10);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);
        let mut plain = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    plain.push_str(cell.symbol());
                }
            }
            plain.push('\n');
        }
        assert!(
            plain.contains("Trust parent folder") && plain.contains("Do not trust"),
            "choices must remain visible; plain={plain:?}"
        );
        assert!(
            !plain.contains('\u{202e}') && !plain.contains('\u{2066}'),
            "bidi controls must be sanitized; plain={plain:?}"
        );

        // The display is sanitized, but selection returns the exact core option label.
        assert!(!view.is_finished());
        let _ = view.handle_event(&key(KeyCode::Enter));
        assert_eq!(view.take_result(), Some(Some(hostile_label)));

        let sanitized = sanitize_trust_path_detail(&hostile_path);
        assert!(!sanitized.contains('\n'));
        assert!(!sanitized.contains('\u{202e}'));
        assert!(sanitized.contains('\u{fffd}') || sanitized.contains('X'));
    }
}
