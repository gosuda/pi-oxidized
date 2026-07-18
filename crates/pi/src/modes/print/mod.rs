//! Print (single-shot) mode: send prompts, render output, exit.
//!
//! Ports `.references/pi/packages/coding-agent/src/modes/print-mode.ts` into
//! three independent pieces so the runtime binding is deferred:
//!
//! - [`input`] assembles the initial prompt from piped stdin, `@file`
//!   arguments, and CLI messages.
//! - [`text`] drains an [`AgentSessionEvent`] stream into final stdout text.
//! - [`json`] emits the session header followed by a lossless JSONL event
//!   stream.
//!
//! Both renderers are generic over a [`PrintSink`] (protocol stdout + product
//! stderr) and a [`Stream`] of events, so tests inject in-memory buffers and
//! the real process binds [`OutputGuardSink`] + the live session subscription.

pub mod input;
pub mod json;
pub mod text;

pub use input::{
    PrintInputError, ProcessFileOptions, ProcessedFiles, PromptSource, build_initial_message,
    process_file_arguments, read_piped_stdin,
};
pub use json::{render_json, render_json_event, render_json_events, render_json_header};
pub use text::{TextOutcome, TextRenderer, render_text};

use std::io;
use std::sync::{Arc, Mutex};

use futures::Stream;
use pi_ai::ImageContent;

use crate::core::agent_session::AgentSessionEvent;
use crate::core::output_guard::{ProductOutput, flush_raw_stdout, write_raw_stdout};
use crate::core::sessions::SessionHeader;

/// Output mode for print (single-shot) runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrintOutput {
    /// Write the final assistant text only; errors go to stderr.
    Text,
    /// Write the session header + every event as JSONL.
    Json,
}

impl PrintOutput {
    /// Returns `true` for the JSON output mode.
    #[must_use]
    pub const fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

/// Options for [`run_print_mode`], mirroring TypeScript `PrintModeOptions`.
#[derive(Clone, Debug)]
pub struct PrintModeOptions {
    /// Output mode: text or JSON.
    pub mode: PrintOutput,
    /// Additional prompts sent after the initial message.
    pub messages: Vec<String>,
    /// First prompt (may carry `@file` content). `None` skips the initial call.
    pub initial_message: Option<String>,
    /// Images attached to the initial prompt.
    pub initial_images: Vec<ImageContent>,
}

impl PrintModeOptions {
    /// Build options for a given output mode with no prompts.
    #[must_use]
    pub fn new(mode: PrintOutput) -> Self {
        Self {
            mode,
            messages: Vec::new(),
            initial_message: None,
            initial_images: Vec::new(),
        }
    }
}

/// Sink abstraction for print-mode output.
///
/// [`write_stdout`](Self::write_stdout) targets the protocol stdout sink
/// (the [`OutputGuard`](crate::core::output_guard) raw queue in production);
/// [`write_stderr`](Self::write_stderr) targets product stderr. Both provide
/// FIFO ordering and backpressure. Implementations must be cheap to borrow
/// (`&self`) since renderers hold the borrow across an entire event drain.
pub trait PrintSink {
    /// Append `text` to the protocol stdout sink.
    fn write_stdout(&self, text: &str) -> impl Future<Output = io::Result<()>> + Send;
    /// Append `text` to product stderr.
    fn write_stderr(&self, text: &str) -> impl Future<Output = io::Result<()>> + Send;
    /// Flush all pending stdout writes.
    fn flush(&self) -> impl Future<Output = io::Result<()>> + Send;
}

/// [`OutputGuard`](crate::core::output_guard)-backed sink for the real process.
///
/// Stdout writes go through the bounded raw-stdout queue (FIFO + backpressure);
/// stderr writes go through [`ProductOutput`], which routes to stderr while
/// stdout is taken over for protocol frames.
#[derive(Clone, Copy, Debug)]
pub struct OutputGuardSink;

impl PrintSink for OutputGuardSink {
    async fn write_stdout(&self, text: &str) -> io::Result<()> {
        write_raw_stdout(text).await.map_err(io::Error::other)
    }

    async fn write_stderr(&self, text: &str) -> io::Result<()> {
        ProductOutput::write(text);
        Ok(())
    }

    async fn flush(&self) -> io::Result<()> {
        flush_raw_stdout().await.map_err(io::Error::other)
    }
}

/// In-memory sink for deterministic tests.
///
/// Captures stdout and stderr as bytes behind a shared
/// [`Arc`]`<`[`Mutex`]`>`, so clones observe the same accumulated output.
#[derive(Clone, Default)]
pub struct BufferSink {
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl BufferSink {
    /// Create an empty buffer sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the captured stdout bytes as a UTF-8 string (lossy).
    #[must_use]
    pub fn stdout_string(&self) -> String {
        String::from_utf8_lossy(&lock_buffer(&self.stdout)).into_owned()
    }

    /// Return the captured stderr bytes as a UTF-8 string (lossy).
    #[must_use]
    pub fn stderr_string(&self) -> String {
        String::from_utf8_lossy(&lock_buffer(&self.stderr)).into_owned()
    }
}

/// Lock a buffer, recovering from poison so a panicking writer never deadlocks
/// the sink (matches `output_guard`'s `into_inner` recovery).
fn lock_buffer(buf: &Mutex<Vec<u8>>) -> std::sync::MutexGuard<'_, Vec<u8>> {
    buf.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl PrintSink for BufferSink {
    async fn write_stdout(&self, text: &str) -> io::Result<()> {
        lock_buffer(&self.stdout).extend_from_slice(text.as_bytes());
        Ok(())
    }

    async fn write_stderr(&self, text: &str) -> io::Result<()> {
        lock_buffer(&self.stderr).extend_from_slice(text.as_bytes());
        Ok(())
    }

    async fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

/// For JSON mode the session header is written first. The renderer and prompt
/// driver then run concurrently. As soon as the prompt driver settles,
/// `finish_events` must close or unsubscribe the event producer; the renderer
/// is still awaited so it drains every event already delivered before success
/// and terminates after an early prompt failure.
///
/// Signal handling, extension binding, and disposal remain the caller's
/// responsibility (the live runtime host owns them); this function is the
/// rendering + prompt-driving core so it stays unit-testable with injected
/// streams and sinks.
///
/// # Errors
///
/// Propagates the first sink write, serialization, or prompt-driver failure.
pub async fn run_print_mode<S, F, Fut, C, K>(
    options: &PrintModeOptions,
    header: Option<&SessionHeader>,
    events: S,
    drive_prompts: F,
    finish_events: C,
    sink: &K,
) -> io::Result<i32>
where
    S: Stream<Item = AgentSessionEvent> + Send + Unpin,
    F: FnOnce() -> Fut,
    Fut: Future<Output = io::Result<()>>,
    C: FnOnce(),
    K: PrintSink,
{
    if options.mode.is_json() {
        json::render_json_header(header, sink).await?;
    }

    let render = async {
        match options.mode {
            PrintOutput::Text => text::render_text(events, sink).await,
            PrintOutput::Json => {
                json::render_json_events(events, sink).await?;
                Ok(0)
            }
        }
    };
    tokio::pin!(render);
    let prompts = drive_prompts();
    tokio::pin!(prompts);
    let mut finish_events = Some(finish_events);

    let (render_result, prompt_result) = tokio::select! {
        prompt_result = &mut prompts => {
            if let Some(finish) = finish_events.take() {
                finish();
            }
            (render.await, prompt_result)
        }
        render_result = &mut render => {
            let prompt_result = prompts.await;
            if let Some(finish) = finish_events.take() {
                finish();
            }
            (render_result, prompt_result)
        }
    };
    prompt_result?;
    let exit_code = render_result?;

    sink.flush().await?;
    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use pi_agent::AgentMessage;
    use pi_ai::{AssistantContent, AssistantMessage, Message, StopReason, TextContent};
    use std::sync::atomic::{AtomicBool, Ordering};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn assistant(text: &str, reason: StopReason) -> AgentMessage {
        let mut msg = AssistantMessage::new("api", "provider", "model", 2);
        if !text.is_empty() {
            msg.content
                .push(AssistantContent::Text(TextContent::new(text)));
        }
        msg.stop_reason = reason;
        AgentMessage::Llm(Box::new(Message::Assistant(msg)))
    }

    #[tokio::test]
    async fn run_print_mode_text_drives_prompts_and_renders() -> TestResult {
        let final_msg = assistant("answer", StopReason::Stop);
        let events = vec![AgentSessionEvent::AgentEnd {
            messages: vec![final_msg],
            will_retry: false,
        }];
        let options = PrintModeOptions::new(PrintOutput::Text);
        let sink = BufferSink::default();

        let prompt_called = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&prompt_called);
        let code = run_print_mode(
            &options,
            None,
            stream::iter(events),
            move || {
                let flag = Arc::clone(&flag);
                async move {
                    flag.store(true, Ordering::SeqCst);
                    Ok(())
                }
            },
            || {},
            &sink,
        )
        .await?;

        assert_eq!(code, 0);
        assert_eq!(sink.stdout_string(), "answer\n");
        assert!(prompt_called.load(Ordering::SeqCst));
        Ok(())
    }

    #[tokio::test]
    async fn run_print_mode_json_writes_header_events_exit_zero() -> TestResult {
        let header = SessionHeader::new("sid", "2024-01-01T00:00:00.000Z", "/cwd", None);
        let events = vec![
            AgentSessionEvent::AgentStart,
            AgentSessionEvent::AgentSettled,
        ];
        let options = PrintModeOptions::new(PrintOutput::Json);
        let sink = BufferSink::default();

        let code = run_print_mode(
            &options,
            Some(&header),
            stream::iter(events),
            || async { Ok(()) },
            || {},
            &sink,
        )
        .await?;

        assert_eq!(code, 0);
        let stdout = sink.stdout_string();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("\"type\":\"session\""));
        assert!(lines[1].contains("\"agent_start\""));
        assert!(lines[2].contains("\"agent_settled\""));
        Ok(())
    }

    #[tokio::test]
    async fn run_print_mode_text_error_exit_one() -> TestResult {
        let mut msg = AssistantMessage::new("api", "provider", "model", 2);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("boom".into());
        let events = vec![AgentSessionEvent::AgentEnd {
            messages: vec![AgentMessage::Llm(Box::new(Message::Assistant(msg)))],
            will_retry: false,
        }];
        let options = PrintModeOptions::new(PrintOutput::Text);
        let sink = BufferSink::default();

        let code = run_print_mode(
            &options,
            None,
            stream::iter(events),
            || async { Ok(()) },
            || {},
            &sink,
        )
        .await?;

        assert_eq!(code, 1);
        assert_eq!(sink.stderr_string(), "boom\n");
        assert!(sink.stdout_string().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn prompt_failure_before_events_closes_renderer_and_returns_error() -> TestResult {
        let options = PrintModeOptions::new(PrintOutput::Text);
        let sink = BufferSink::default();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let events = Box::pin(stream::unfold(event_rx, |mut receiver| async move {
            receiver.recv().await.map(|event| (event, receiver))
        }));

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            run_print_mode(
                &options,
                None,
                events,
                || async { Err(io::Error::other("preflight auth failed")) },
                move || drop(event_tx),
                &sink,
            ),
        )
        .await;

        let error = match result {
            Ok(Err(error)) => error,
            Ok(Ok(exit_code)) => {
                return Err(format!(
                    "prompt failure must be preserved, but print mode returned exit code {exit_code}"
                )
                .into());
            }
            Err(error) => {
                return Err(
                    format!("print mode must not hang after prompt setup fails: {error}").into(),
                );
            }
        };
        assert!(error.to_string().contains("preflight auth failed"));
        Ok(())
    }

    #[tokio::test]
    async fn buffer_sink_appends_in_order() -> TestResult {
        let sink = BufferSink::default();
        sink.write_stdout("a").await?;
        sink.write_stdout("b").await?;
        sink.write_stderr("e").await?;
        assert_eq!(sink.stdout_string(), "ab");
        assert_eq!(sink.stderr_string(), "e");
        Ok(())
    }
}
