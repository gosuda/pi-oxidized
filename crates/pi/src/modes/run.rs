//! Mode dispatch: route a resolved [`AppMode`] to the right runner, with
//! signal handling and guaranteed `dispose`.
//!
//! The concrete RPC server (`run_rpc_mode`), interactive TUI, and the live
//! print-mode event wiring are owned by sibling slices and are still landing.
//! To stay compilable and unit-testable today, [`run_mode_default`] dispatches
//! through an injected [`ModeDispatch`] trait: each method corresponds to one
//! application mode and returns an exit code. The integrator (Main) wires the
//! real implementations once they land; tests inject fakes.
//!
//! # Signal handling
//!
//! [`run_mode_with_codes`] races the dispatched mode future against SIGINT /
//! SIGTERM / SIGHUP handlers. A shared first-wins sender ensures exactly one
//! signal's exit code reaches the dispatcher; the others are dropped. When a
//! signal arrives first, the mode future is dropped (cancelled) and the
//! canonical signal exit code is returned.
//!
//! # Dispose guarantee
//!
//! `runtime.dispose()` runs on every exit path (mode completion, signal,
//! error) because `run_mode_with_codes` awaits it explicitly before
//! returning. No `Drop`-based `block_on`: the caller's tokio runtime owns the
//! lifecycle.
//!
//! # Signal exit codes (match `modes/print-mode.ts` + `modes/rpc/rpc-mode.ts`)
//!
//! | signal / condition      | exit | notes                       |
//! |-------------------------|------|-----------------------------|
//! | stdin EOF (RPC)         | 0    | clean shutdown (mode path)  |
//! | SIGINT / SIGTERM        | 143  | 128 + 15                    |
//! | SIGHUP (unix)           | 129  | 128 + 1                     |
//! | normal completion       | mode-determined                 |

use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::sync::oneshot;

use crate::cli::bootstrap::{AppMode, Dispatched};
use crate::core::agent_session_runtime::AgentSessionRuntime;
use crate::core::output_guard;

/// Dispatcher injected by the integrator.
///
/// Each method receives the resolved [`Dispatched`] (mode, runtime handle,
/// initial message, follow-ups, migrations) and returns the process exit code
/// for that mode. Implementations are expected to bind extensions, subscribe
/// session events, drive the mode loop, and return the final exit code.
///
/// All methods take `&self` so a single dispatcher can be reused across
/// `/reload` cycles (each reload rebuilds the runtime but not the dispatcher).
pub trait ModeDispatch: Send + Sync {
    /// Run the interactive TUI mode.
    ///
    /// # Errors
    /// Implementation-defined; the error is surfaced on stderr and the
    /// dispatcher falls back to exit 1.
    fn run_interactive(
        &self,
        dispatched: Dispatched,
        runtime: Arc<AgentSessionRuntime>,
    ) -> BoxFuture<'_, Result<u8, String>>;

    /// Run the text or JSON print mode.
    ///
    /// `mode` is guaranteed to be [`AppMode::Print`] or [`AppMode::Json`].
    ///
    /// # Errors
    /// Implementation-defined.
    fn run_print(
        &self,
        dispatched: Dispatched,
        runtime: Arc<AgentSessionRuntime>,
    ) -> BoxFuture<'_, Result<u8, String>>;

    /// Run the headless RPC server. Returns when stdin EOF arrives or a
    /// signal cancels the run.
    ///
    /// # Errors
    /// Implementation-defined.
    fn run_rpc(
        &self,
        dispatched: Dispatched,
        runtime: Arc<AgentSessionRuntime>,
    ) -> BoxFuture<'_, Result<u8, String>>;
}

/// Run the resolved mode to completion with canonical signal handling.
///
/// Installs signal handlers, dispatches to the right [`ModeDispatch`] method,
/// guarantees `dispose`, and returns a process [`ExitCode`]. The dispatch
/// future is raced against the signal handlers; a signal cancels the mode and
/// returns its exit code.
pub async fn run_mode_default(dispatched: Dispatched, handler: &dyn ModeDispatch) -> ExitCode {
    run_mode_with_codes(dispatched, handler, SignalCodes::default()).await
}

/// Exit codes for each signal condition.
#[derive(Clone, Copy, Debug)]
pub struct SignalCodes {
    /// SIGINT (Ctrl+C) / SIGTERM.
    pub sigterm: u8,
    /// SIGHUP (Unix only; ignored on Windows).
    pub sighup: Option<u8>,
}

impl Default for SignalCodes {
    fn default() -> Self {
        Self {
            sigterm: defaults::SIGTERM,
            #[cfg(unix)]
            sighup: Some(defaults::SIGHUP),
            #[cfg(not(unix))]
            sighup: None,
        }
    }
}

/// Canonical exit codes matching the TypeScript reference.
pub mod defaults {
    /// Exit code for RPC stdin EOF.
    pub const STDIN_EOF: u8 = 0;
    /// Exit code for SIGTERM / SIGINT.
    pub const SIGTERM: u8 = 143;
    /// Exit code for SIGHUP on unix.
    pub const SIGHUP: u8 = 129;
}

/// Run with explicit signal codes. Exposed for tests that want to observe
/// the signal-race machinery.
pub async fn run_mode_with_codes(
    dispatched: Dispatched,
    handler: &dyn ModeDispatch,
    codes: SignalCodes,
) -> ExitCode {
    let mode = dispatched.mode;
    let runtime = dispatched.handle.runtime.clone();

    let signal = SignalRelay::install(codes);
    let signal_rx = signal.take_rx();

    let mode_fut = match mode {
        AppMode::Interactive => handler
            .run_interactive(dispatched, Arc::clone(&runtime))
            .boxed(),
        AppMode::Print | AppMode::Json => {
            handler.run_print(dispatched, Arc::clone(&runtime)).boxed()
        }
        AppMode::Rpc => handler.run_rpc(dispatched, Arc::clone(&runtime)).boxed(),
    };

    let result = tokio::select! {
        biased;
        code = async {
            match signal_rx {
                Some(rx) => rx.await.unwrap_or(defaults::STDIN_EOF),
                None => defaults::STDIN_EOF,
            }
        } => Ok(code),
        outcome = mode_fut => outcome,
    };

    signal.cancel().await;

    // Restore stdout (print/rpc took it over).
    if !mode.is_interactive() {
        output_guard::restore_stdout();
    }

    // Dispose before returning the exit code.
    runtime.dispose().await;

    let exit_code = match result {
        Ok(code) => code,
        Err(message) => {
            output_guard::ProductOutput::writeln(&format!("Error: {message}"));
            1
        }
    };
    ExitCode::from(exit_code)
}

/// One-shot relay shared across all signal handlers. The first handler to
/// call [`SignalRelayHandle::fire`] wins; subsequent calls are dropped.
struct SignalRelay {
    /// `Some` until the first signal fires; `None` afterwards.
    sender: Mutex<Option<oneshot::Sender<u8>>>,
    /// Cancellation token for the background handler tasks.
    cancel: tokio_util::sync::CancellationToken,
    /// Receiver. Wrapped in a Mutex so `rx()` can take it exactly once.
    receiver: Mutex<Option<oneshot::Receiver<u8>>>,
}

/// Handle returned to the caller so it can await the signal and later cancel
/// the handlers.
pub struct SignalRelayHandle {
    relay: Arc<SignalRelay>,
}

impl SignalRelayHandle {
    /// Take the receiver out of the handle so it can be awaited independently.
    /// Returns `None` if already taken.
    fn take_rx(&self) -> Option<oneshot::Receiver<u8>> {
        self.relay
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Cancel the background handler tasks (best-effort).
    async fn cancel(&self) {
        self.relay.cancel.cancel();
        // Give the handler tasks a chance to observe cancellation.
        tokio::task::yield_now().await;
    }
}

impl SignalRelay {
    /// Install the signal handlers and return a handle.
    fn install(codes: SignalCodes) -> SignalRelayHandle {
        let (tx, rx) = oneshot::channel::<u8>();
        let relay = Arc::new(SignalRelay {
            sender: Mutex::new(Some(tx)),
            cancel: tokio_util::sync::CancellationToken::new(),
            receiver: Mutex::new(Some(rx)),
        });

        // SIGINT (Ctrl+C) — treated like SIGTERM. Always installed (cross-platform).
        let int_relay = Arc::clone(&relay);
        let int_cancel = relay.cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                () = int_cancel.cancelled() => {}
                res = tokio::signal::ctrl_c() => {
                    if let Ok(()) = res {
                        fire(&int_relay, codes.sigterm);
                    }
                }
            }
        });

        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            // SIGTERM.
            if let Ok(mut stream) = signal(SignalKind::terminate()) {
                let term_relay = Arc::clone(&relay);
                let term_cancel = relay.cancel.clone();
                let term_code = codes.sigterm;
                tokio::spawn(async move {
                    tokio::select! {
                        biased;
                        () = term_cancel.cancelled() => {}
                        _ = stream.recv() => {
                            fire(&term_relay, term_code);
                        }
                    }
                });
            }
            // SIGHUP.
            if let Some(hup_code) = codes.sighup
                && let Ok(mut stream) = signal(SignalKind::hangup())
            {
                let hup_relay = Arc::clone(&relay);
                let hup_cancel = relay.cancel.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        biased;
                        () = hup_cancel.cancelled() => {}
                        _ = stream.recv() => {
                            fire(&hup_relay, hup_code);
                        }
                    }
                });
            }
        }

        #[cfg(not(unix))]
        {
            let _ = codes;
        }

        SignalRelayHandle { relay }
    }
}

/// Fire the relay with `code` if no signal has fired yet. First-wins.
fn fire(relay: &Arc<SignalRelay>, code: u8) {
    let sender = {
        let mut guard = relay
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.take()
    };
    if let Some(tx) = sender {
        let _ = tx.send(code);
    }
}

// ---------------------------------------------------------------------------
// Concrete print-mode binding (live AgentSessionRuntime)
// ---------------------------------------------------------------------------

use crate::core::agent_session::AgentSessionEvent;
use crate::core::agent_session::prompt::PromptOptions;
use crate::modes::print::{OutputGuardSink, PrintModeOptions, PrintOutput, run_print_mode};
use tokio::sync::mpsc;

/// Run print mode (text or JSON) against a live [`AgentSession`].
///
/// This is the concrete binding that connects the bootstrap's [`Dispatched`]
/// (runtime + initial message + follow-ups) to the generic
/// [`run_print_mode`] renderer:
///
/// 1. Subscribes to `AgentSessionEvent` via an unbounded channel bridge.
/// 2. Reads the session header from the session manager (for JSON mode).
/// 3. Drives prompts with `PromptOptions { source: "print"|"json" }`.
/// 4. Calls `run_print_mode` with `OutputGuardSink`.
/// 5. Returns the exit code (0 on success, 1 on error stop reason).
///
/// # Errors
///
/// Returns an error when prompting the session or rendering print output fails.
pub async fn run_print_session(
    dispatched: Dispatched,
    runtime: Arc<AgentSessionRuntime>,
) -> Result<u8, String> {
    let print_output = if dispatched.mode.is_json() {
        PrintOutput::Json
    } else {
        PrintOutput::Text
    };
    let session = runtime.session();

    // Bind extensions before subscribing/prompting: emits the stored
    // session_start{startup} and runs bind-time resource discovery
    // (upstream print-mode parity). Bind errors are non-fatal.
    let _ = session
        .bind_extensions(crate::core::agent_session::ExtensionBindings {
            mode: Some(if print_output.is_json() {
                crate::core::agent_session::ExtensionMode::Json
            } else {
                crate::core::agent_session::ExtensionMode::Print
            }),
            ..Default::default()
        })
        .await;

    // Session header (JSON mode only).
    let header = if print_output.is_json() {
        let sm = session.session_manager();
        let sm_guard = sm.lock().await;
        let sm = sm_guard;
        sm.get_header().cloned()
    } else {
        None
    };

    // Subscribe to events via channel bridge.
    let (event_tx, event_rx) = mpsc::unbounded_channel::<AgentSessionEvent>();
    let unsubscribe = session.subscribe(move |event: &AgentSessionEvent| {
        let _ = event_tx.send(event.clone());
    });

    let source_str = if print_output.is_json() {
        "json"
    } else {
        "print"
    };
    let source_string = source_str.to_owned();
    let initial_images = dispatched.initial_images.clone();
    let initial_message = dispatched.initial_message.clone();
    let remaining_messages = dispatched.remaining_messages.clone();
    let session_for_prompts = Arc::clone(&session);

    let prompt_driver = move || async move {
        if let Some(initial) = initial_message.as_deref() {
            let opts = PromptOptions {
                images: initial_images.clone(),
                source: Some(source_string.clone()),
                ..PromptOptions::default()
            };
            if let Err(err) = session_for_prompts.prompt(initial, opts).await {
                return Err(std::io::Error::other(format!("{err}")));
            }
        }
        for msg in &remaining_messages {
            let opts = PromptOptions {
                source: Some(source_string.clone()),
                ..PromptOptions::default()
            };
            if let Err(err) = session_for_prompts.prompt(msg, opts).await {
                return Err(std::io::Error::other(format!("{err}")));
            }
        }
        Ok(())
    };

    let options = PrintModeOptions {
        mode: print_output,
        messages: Vec::new(),
        initial_message: dispatched.initial_message.clone(),
        initial_images: dispatched.initial_images.clone(),
    };

    // Box::pin the unfold stream to satisfy the `Unpin` bound on
    // `run_print_mode`'s `S` type parameter.
    let event_stream = Box::pin(futures::stream::unfold(event_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }));

    let exit_code = run_print_mode(
        &options,
        header.as_ref(),
        event_stream,
        prompt_driver,
        unsubscribe,
        &OutputGuardSink,
    )
    .await
    .map_err(|e| format!("{e}"))?;

    Ok(u8::try_from(exit_code).unwrap_or(1))
}

/// Runner for an injected application mode.
pub type ModeRunner = dyn Fn(Dispatched, Arc<AgentSessionRuntime>) -> BoxFuture<'static, Result<u8, String>>
    + Send
    + Sync;

/// Default dispatcher: concrete print/json mode, injectable RPC/interactive.
///
/// Print and JSON modes are wired directly to [`run_print_session`]. RPC and
/// interactive modes accept closures that the integrator provides — these are
/// real dependency-injection points, not stubs.
pub struct DefaultDispatcher {
    /// RPC mode runner.
    pub rpc: Option<Arc<ModeRunner>>,
    /// Interactive mode runner.
    pub interactive: Option<Arc<ModeRunner>>,
}

impl DefaultDispatcher {
    /// Create with print/json wired; RPC and interactive injected.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rpc: None,
            interactive: None,
        }
    }

    /// Set the RPC mode runner.
    #[must_use]
    pub fn with_rpc<F>(mut self, f: F) -> Self
    where
        F: Fn(Dispatched, Arc<AgentSessionRuntime>) -> BoxFuture<'static, Result<u8, String>>
            + Send
            + Sync
            + 'static,
    {
        self.rpc = Some(Arc::new(f));
        self
    }

    /// Set the interactive mode runner.
    #[must_use]
    pub fn with_interactive<F>(mut self, f: F) -> Self
    where
        F: Fn(Dispatched, Arc<AgentSessionRuntime>) -> BoxFuture<'static, Result<u8, String>>
            + Send
            + Sync
            + 'static,
    {
        self.interactive = Some(Arc::new(f));
        self
    }
}

impl Default for DefaultDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ModeDispatch for DefaultDispatcher {
    fn run_interactive(
        &self,
        dispatched: Dispatched,
        runtime: Arc<AgentSessionRuntime>,
    ) -> BoxFuture<'_, Result<u8, String>> {
        match &self.interactive {
            Some(runner) => runner(dispatched, runtime),
            None => Box::pin(async move {
                // Interactive TUI requires a terminal; in headless contexts
                // we cannot launch it. Surface the condition explicitly.
                Err("interactive mode requires a TTY terminal".to_owned())
            }),
        }
    }

    fn run_print(
        &self,
        dispatched: Dispatched,
        runtime: Arc<AgentSessionRuntime>,
    ) -> BoxFuture<'_, Result<u8, String>> {
        Box::pin(async move { run_print_session(dispatched, runtime).await })
    }

    fn run_rpc(
        &self,
        dispatched: Dispatched,
        runtime: Arc<AgentSessionRuntime>,
    ) -> BoxFuture<'_, Result<u8, String>> {
        match &self.rpc {
            Some(runner) => runner(dispatched, runtime),
            None => {
                Box::pin(async move { Err("rpc mode requires the RPC server runner".to_owned()) })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Fake dispatcher recording which mode ran.
    #[derive(Default)]
    struct FakeDispatcher {
        print_code: StdMutex<Option<u8>>,
        calls: StdMutex<Vec<&'static str>>,
    }

    impl ModeDispatch for Arc<FakeDispatcher> {
        fn run_interactive(
            &self,
            _dispatched: Dispatched,
            _runtime: Arc<AgentSessionRuntime>,
        ) -> BoxFuture<'_, Result<u8, String>> {
            let result = self
                .calls
                .lock()
                .map_err(|error| format!("record interactive call: {error}"))
                .map(|mut calls| calls.push("interactive"));
            Box::pin(async move {
                result?;
                Ok(0)
            })
        }
        fn run_print(
            &self,
            _dispatched: Dispatched,
            _runtime: Arc<AgentSessionRuntime>,
        ) -> BoxFuture<'_, Result<u8, String>> {
            let result = self
                .calls
                .lock()
                .map_err(|error| format!("record print call: {error}"))
                .and_then(|mut calls| {
                    calls.push("print");
                    self.print_code
                        .lock()
                        .map_err(|error| format!("read print exit code: {error}"))
                        .map(|code| code.unwrap_or(0))
                });
            Box::pin(async move { result })
        }
        fn run_rpc(
            &self,
            _dispatched: Dispatched,
            _runtime: Arc<AgentSessionRuntime>,
        ) -> BoxFuture<'_, Result<u8, String>> {
            let result = self
                .calls
                .lock()
                .map_err(|error| format!("record RPC call: {error}"))
                .map(|mut calls| calls.push("rpc"));
            Box::pin(async move {
                result?;
                Ok(0)
            })
        }
    }

    #[test]
    fn defaults_match_reference() {
        assert_eq!(defaults::STDIN_EOF, 0);
        assert_eq!(defaults::SIGTERM, 143);
        assert_eq!(defaults::SIGHUP, 129);
    }

    #[test]
    fn signal_codes_default() {
        let codes = SignalCodes::default();
        assert_eq!(codes.sigterm, 143);
        #[cfg(unix)]
        assert_eq!(codes.sighup, Some(129));
    }

    #[test]
    fn fire_is_first_wins() -> Result<(), String> {
        let (tx, _rx) = oneshot::channel::<u8>();
        let relay = Arc::new(SignalRelay {
            sender: Mutex::new(Some(tx)),
            cancel: tokio_util::sync::CancellationToken::new(),
            receiver: Mutex::new(None),
        });
        fire(&relay, 143);
        // Sender is now taken.
        assert!(
            relay
                .sender
                .lock()
                .map_err(|error| format!("inspect signal sender: {error}"))?
                .is_none()
        );
        // Second fire is a no-op.
        fire(&relay, 129);
        assert!(
            relay
                .sender
                .lock()
                .map_err(|error| format!("inspect signal sender after second fire: {error}"))?
                .is_none()
        );
        Ok(())
    }
}
