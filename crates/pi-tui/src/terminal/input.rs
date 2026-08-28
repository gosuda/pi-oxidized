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
    pub fn deferred() -> Self {
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
    pub fn start(&mut self) {
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
    /// Interleaved keystrokes are reinjected through the resume path.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if pause/resume fails or writing the query fails.
    /// On write failure the stream is still resumed (best-effort empty reinject)
    /// so the input task is never left paused.
    pub async fn requery_background<W: Write>(&self, output: &mut W) -> io::Result<Option<bool>> {
        self.pause().await?;
        match probe_background(output) {
            Ok((dark, reinject)) => {
                self.resume(reinject).await?;
                Ok(dark)
            }
            Err(error) => {
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
    input_task_with_factory(tx, control_rx, EventStream::new).await;
}

async fn input_task_with_factory<S, F>(
    tx: mpsc::UnboundedSender<UiEvent>,
    mut control_rx: mpsc::UnboundedReceiver<InputControl>,
    mut make_stream: F,
) where
    S: Stream<Item = io::Result<Event>> + Unpin,
    F: FnMut() -> S,
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
            InputWake::Event(Some(Err(_))) => {
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
        Event::Mouse(_) => None,
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
    }
    #[tokio::test]
    async fn pause_resume_acknowledges_and_reinjects() -> io::Result<()> {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        tokio::spawn(input_task_with_factory(events_tx, control_rx, || {
            futures::stream::pending::<io::Result<Event>>()
        }));
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
    async fn requery_background_resume_reinjects_from_chunks() -> io::Result<()> {
        // Exercise the pause → reinject → resume control path that requery_background
        // uses; classification itself is unit-tested via probe_background_from_chunks.
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        tokio::spawn(input_task_with_factory(events_tx, control_rx, || {
            futures::stream::pending::<io::Result<Event>>()
        }));
        let (unused_tx, _unused_rx) = mpsc::unbounded_channel();
        let mut input = TerminalInput {
            tx: unused_tx,
            rx: events_rx,
            control_tx,
            control_rx: None,
        };

        input.pause().await?;
        let (dark, reinject) = crate::terminal::probe::probe_background_from_chunks([
            b"z".as_slice(),
            b"\x1b]11;#ffffff\x07".as_slice(),
        ]);
        assert_eq!(dark, Some(false));
        input.resume(reinject).await?;
        assert!(matches!(
            input.recv().await,
            Some(UiEvent::Key(k)) if k.code == KeyCode::Char('z')
        ));
        input.shutdown();
        Ok(())
    }
}
