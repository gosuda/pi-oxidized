//! Sole owner of the Crossterm `EventStream`.

use std::io::{self, Write};

use crossterm::event::{Event, EventStream};
use futures::{Stream, StreamExt};
use tokio::sync::{mpsc, oneshot};

use crate::component::UiEvent;
use crate::terminal::probe::probe_background;

/// Handle used by the UI loop to receive mapped terminal events.
#[derive(Debug)]
pub struct TerminalInput {
    tx: mpsc::UnboundedSender<UiEvent>,
    rx: mpsc::UnboundedReceiver<UiEvent>,
    control_tx: mpsc::UnboundedSender<InputControl>,
    /// Held back by [`TerminalInput::deferred`] until [`TerminalInput::start`]
    /// spawns the reader task.
    control_rx: Option<mpsc::UnboundedReceiver<InputControl>>,
}

/// Control messages for pausing the `EventStream` around probes.
#[derive(Debug)]
enum InputControl {
    Pause {
        acknowledged: oneshot::Sender<()>,
    },
    Resume {
        reinject: Vec<UiEvent>,
        acknowledged: oneshot::Sender<()>,
    },
    Shutdown,
}
enum InputWake {
    Control(Option<InputControl>),
    Event(Option<std::io::Result<Event>>),
}

impl TerminalInput {
    /// Spawn the sole `EventStream` owner task.
    ///
    /// Only one instance should exist for the process while interactive.
    #[must_use]
    pub fn spawn() -> Self {
        let mut input = Self::deferred();
        input.start();
        input
    }

    /// Create the input handle WITHOUT spawning the `EventStream` reader.
    ///
    /// Startup calls this while the capability probe still owns stdin (its
    /// collector may run on a blocking thread during first-frame painting);
    /// [`TerminalInput::start`] spawns the reader once stdin ownership is
    /// back with this handle. [`Self::pause`] and [`Self::resume`] must not
    /// be called before `start`.
    #[must_use]
    pub(crate) fn deferred() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx,
            control_tx,
            control_rx: Some(control_rx),
        }
    }

    /// Spawn the reader task for a [`TerminalInput::deferred`] handle.
    ///
    /// No-op when the reader is already running. Only one reader may exist
    /// for the process while interactive; call only once nothing else reads
    /// stdin (after the probe collector joined).
    pub(crate) fn start(&mut self) {
        if let Some(control_rx) = self.control_rx.take() {
            tokio::spawn(input_task(self.tx.clone(), control_rx));
        }
    }

    /// Create an input handle backed by a pre-built channel (tests / mocks).
    #[must_use]
    pub fn mock(rx: mpsc::UnboundedReceiver<UiEvent>) -> Self {
        let (tx, _tx_rx) = mpsc::unbounded_channel();
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx,
            control_tx,
            control_rx: None,
        }
    }

    /// Receive the next UI event.
    pub async fn recv(&mut self) -> Option<UiEvent> {
        self.rx.recv().await
    }

    /// Non-blocking poll of the next UI event.
    pub fn try_recv(&mut self) -> Option<UiEvent> {
        self.rx.try_recv().ok()
    }

    /// Borrow the receiver for `tokio::select!`.
    pub fn receiver_mut(&mut self) -> &mut mpsc::UnboundedReceiver<UiEvent> {
        &mut self.rx
    }

    /// Pause the `EventStream` so a probe session can own stdin reads.
    ///
    /// Returns only after the input task has dropped its stream.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the input task has stopped.
    pub async fn pause(&self) -> io::Result<()> {
        let (acknowledged, received) = oneshot::channel();
        self.control_tx
            .send(InputControl::Pause { acknowledged })
            .map_err(|_| io::Error::other("terminal input task stopped"))?;
        received
            .await
            .map_err(|_| io::Error::other("terminal input pause was not acknowledged"))
    }

    /// Resume the `EventStream`, reinjecting synthetic events first.
    ///
    /// Returns only after all events are re-injected and a new stream exists.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the input task has stopped.
    pub async fn resume(&self, reinject: Vec<UiEvent>) -> io::Result<()> {
        let (acknowledged, received) = oneshot::channel();
        self.control_tx
            .send(InputControl::Resume {
                reinject,
                acknowledged,
            })
            .map_err(|_| io::Error::other("terminal input task stopped"))?;
        received
            .await
            .map_err(|_| io::Error::other("terminal input resume was not acknowledged"))
    }

    /// Pause the event stream, emit OSC 11, classify the background, resume.
    ///
    /// Returns `Ok(Some(dark))` when OSC 11 classified a polarity, or `Ok(None)`
    /// on timeout / no-TTY / unparseable reply (caller keeps its prior value).
    /// The requery drives the SAME persistent reader the stream uses; keys
    /// typed during the requery (and any pending CSI/UTF-8/paste state) stay
    /// queued in the shared parser and are delivered after resume.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if pause/resume fails or writing the query fails.
    /// On write failure the stream is still resumed (empty reinject) so the
    /// input task is never left paused. A latched reply protocol error is
    /// surfaced to the caller AFTER in-place recovery, so the session stays
    /// usable.
    pub async fn requery_background<W: Write>(&self, output: &mut W) -> io::Result<Option<bool>> {
        self.pause().await?;
        match probe_background(output) {
            Ok(dark) => {
                self.resume(Vec::new()).await?;
                Ok(dark)
            }
            Err(error) => {
                if crossterm::event::reply::is_protocol_error(&error) {
                    // Defined recovery: clear the latch so the resumed stream
                    // starts healthy; the error still reaches the caller.
                    crossterm::event::reply::recover_protocol_error();
                }
                let _ = self.resume(Vec::new()).await;
                Err(error)
            }
        }
    }

    /// Request task shutdown.
    pub fn shutdown(&self) {
        let _ = self.control_tx.send(InputControl::Shutdown);
    }
}

async fn input_task(
    tx: mpsc::UnboundedSender<UiEvent>,
    control_rx: mpsc::UnboundedReceiver<InputControl>,
) {
    input_task_with_factory(
        tx,
        control_rx,
        EventStream::new,
        crossterm::event::reply::recover_protocol_error,
    )
    .await;
}

async fn input_task_with_factory<S, F, R>(
    tx: mpsc::UnboundedSender<UiEvent>,
    mut control_rx: mpsc::UnboundedReceiver<InputControl>,
    mut make_stream: F,
    mut recover_protocol_error: R,
) where
    S: Stream<Item = io::Result<Event>> + Unpin,
    F: FnMut() -> S,
    R: FnMut() -> bool,
{
    let mut paused = false;
    let mut stream = Some(make_stream());
    loop {
        if paused {
            match control_rx.recv().await {
                Some(InputControl::Resume {
                    reinject,
                    acknowledged,
                }) => {
                    // Defined recovery: a latched reply protocol error (from
                    // a recognized malformed or oversized reply) clears here
                    // so the recreated stream starts healthy.
                    let _ = recover_protocol_error();
                    stream = Some(make_stream());
                    for event in reinject {
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                    paused = false;
                    let _ = acknowledged.send(());
                }
                Some(InputControl::Shutdown) | None => return,
                Some(InputControl::Pause { acknowledged }) => {
                    let _ = acknowledged.send(());
                }
            }
            continue;
        }

        let Some(active_stream) = stream.as_mut() else {
            return;
        };
        let wake = tokio::select! {
            control = control_rx.recv() => InputWake::Control(control),
            event = active_stream.next() => InputWake::Event(event),
        };
        match wake {
            InputWake::Control(Some(InputControl::Pause { acknowledged })) => {
                // Drop the only `EventStream` before acknowledging the probe.
                stream = None;
                paused = true;
                let _ = acknowledged.send(());
            }
            InputWake::Control(Some(InputControl::Resume {
                reinject,
                acknowledged,
            })) => {
                for event in reinject {
                    if tx.send(event).is_err() {
                        return;
                    }
                }
                let _ = acknowledged.send(());
            }
            InputWake::Control(Some(InputControl::Shutdown) | None) | InputWake::Event(None) => {
                return;
            }
            InputWake::Event(Some(Ok(event))) => {
                if let Some(ui) = map_event(event)
                    && tx.send(ui).is_err()
                {
                    return;
                }
            }
            InputWake::Event(Some(Err(error))) => {
                if crossterm::event::reply::is_protocol_error(&error) {
                    // Recognized malformed or oversized reply framing: drop the
                    // current stream, clear the latch, and recreate the stream
                    // in-task so decoding resumes immediately. The defined
                    // Pause/Resume path remains available for probes.
                    drop(stream.take());
                    let _ = recover_protocol_error();
                    stream = Some(make_stream());
                }
                // Transient read errors are ignored; EOF ends the task.
            }
        }
    }
}

/// Map a Crossterm event into the closed [`UiEvent`] set.
#[must_use]
pub fn map_event(event: Event) -> Option<UiEvent> {
    match event {
        Event::Key(key) => Some(UiEvent::Key(key)),
        Event::Paste(text) => Some(UiEvent::Paste(normalize_paste(&text))),
        Event::FocusGained => Some(UiEvent::FocusGained),
        Event::FocusLost => Some(UiEvent::FocusLost),
        Event::Resize(width, height) => Some(UiEvent::Resize { width, height }),
        Event::Mouse(mouse) => Some(UiEvent::Mouse(mouse)),
    }
}

fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Blocking helper that maps a single polled event (tests / non-async paths).
///
/// # Errors
///
/// Returns an I/O error when polling or reading the terminal event fails.
pub fn try_map_next() -> io::Result<Option<UiEvent>> {
    if crossterm::event::poll(std::time::Duration::from_millis(0))? {
        Ok(map_event(crossterm::event::read()?))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    #[test]
    fn maps_key_paste_focus_resize() {
        let key = Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(matches!(map_event(key), Some(UiEvent::Key(_))));
        assert_eq!(
            map_event(Event::Paste("a\r\nb".into())),
            Some(UiEvent::Paste("a\nb".into()))
        );
        assert_eq!(map_event(Event::FocusGained), Some(UiEvent::FocusGained));
        assert_eq!(map_event(Event::FocusLost), Some(UiEvent::FocusLost));
        assert_eq!(
            map_event(Event::Resize(80, 24)),
            Some(UiEvent::Resize {
                width: 80,
                height: 24
            })
        );
        let mouse = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 7,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(map_event(mouse), Some(UiEvent::Mouse(_))));
    }
    #[tokio::test]
    async fn pause_resume_acknowledges_and_reinjects() -> io::Result<()> {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        tokio::spawn(input_task_with_factory(
            events_tx,
            control_rx,
            || futures::stream::pending::<io::Result<Event>>(),
            || false,
        ));
        let (unused_tx, _unused_rx) = mpsc::unbounded_channel();
        let mut input = TerminalInput {
            tx: unused_tx,
            rx: events_rx,
            control_tx,
            control_rx: None,
        };

        input.pause().await?;
        input.resume(vec![UiEvent::FocusGained]).await?;
        assert_eq!(input.recv().await, Some(UiEvent::FocusGained));
        input.shutdown();
        Ok(())
    }

    #[tokio::test]
    async fn protocol_error_recovers_in_task_and_keeps_stream_live() -> io::Result<()> {
        // Use crossterm's real latched-error message shape. The injected
        // operation below models the parser latch and makes task recovery
        // observable without requiring a live terminal.
        let protocol_message =
            "crossterm reply protocol error: malformed OSC 11 reply framing (unterminated ST)";
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory_calls = std::sync::Arc::clone(&calls);
        let recovery_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recovery_successes =
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recovery_latched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let recovery_calls_for_task = std::sync::Arc::clone(&recovery_calls);
        let recovery_successes_for_task = std::sync::Arc::clone(&recovery_successes);
        let recovery_latched_for_task = std::sync::Arc::clone(&recovery_latched);
        tokio::spawn(input_task_with_factory(
            events_tx,
            control_rx,
            move || {
                let n = factory_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let stream: std::pin::Pin<
                    Box<dyn futures::Stream<Item = io::Result<Event>> + Send>,
                > = match n {
                    0 => Box::pin(futures::stream::iter(vec![Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        protocol_message,
                    ))])),
                    1 => Box::pin(
                        futures::stream::iter(vec![
                            Ok(Event::FocusGained),
                            Ok(Event::Resize(80, 24)),
                        ])
                        .chain(futures::stream::pending()),
                    ),
                    _ => Box::pin(futures::stream::pending::<io::Result<Event>>()),
                };
                stream
            },
            move || {
                recovery_calls_for_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let recovered =
                    recovery_latched_for_task.swap(false, std::sync::atomic::Ordering::SeqCst);
                if recovered {
                    recovery_successes_for_task
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                recovered
            },
        ));
        let (unused_tx, _unused_rx) = mpsc::unbounded_channel();
        let mut input = TerminalInput {
            tx: unused_tx,
            rx: events_rx,
            control_tx,
            control_rx: None,
        };

        // The task recovers in-task and delivers the next events without a
        // Resume control.
        assert_eq!(input.recv().await, Some(UiEvent::FocusGained));
        assert_eq!(
            input.recv().await,
            Some(UiEvent::Resize {
                width: 80,
                height: 24,
            })
        );
        assert_eq!(
            recovery_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the protocol error must invoke recovery before replacement events",
        );
        assert_eq!(
            recovery_successes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "recovery must report that it cleared the latched protocol error",
        );

        // The defined Pause/Resume path still works after in-task recovery.
        input.pause().await?;
        input.resume(vec![UiEvent::FocusGained]).await?;
        assert_eq!(input.recv().await, Some(UiEvent::FocusGained));
        assert!(
            calls.load(std::sync::atomic::Ordering::SeqCst) >= 3,
            "error, recovery, and resume must each recreate the stream"
        );
        input.shutdown();
        Ok(())
    }
}
