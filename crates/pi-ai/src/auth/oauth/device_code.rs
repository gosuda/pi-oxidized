//! Generic OAuth device-code polling (RFC 8628).
//!
//! Callers supply a `poll` function that performs one token request. The loop
//! honors `pending`, `slow_down`, `failed`, and `complete`, enforces a minimum
//! interval, and surfaces WSL/VM clock-drift wording after any `slow_down`.

use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::super::error::AuthError;

/// Default poll interval when the authorization server omits `interval`.
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;

/// Floor applied to every computed interval.
pub const MINIMUM_INTERVAL_MS: u64 = 1_000;

/// RFC 8628 §3.5: increase the interval by five seconds on `slow_down`.
pub const SLOW_DOWN_INTERVAL_INCREMENT_MS: u64 = 5_000;

const TIMEOUT_MESSAGE: &str = "Device flow timed out";
const SLOW_DOWN_TIMEOUT_MESSAGE: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";

/// Intermediate or terminal device-code poll outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OAuthDeviceCodePollResult<T> {
    /// Authorization is still pending; keep polling.
    Pending,
    /// Server asked the client to slow down. Optional new interval in seconds.
    SlowDown {
        /// Server-provided minimum interval, when present and finite.
        interval_seconds: Option<u64>,
    },
    /// Terminal failure with a caller-provided message.
    Failed {
        /// Error text raised to the login UI.
        message: String,
    },
    /// Authorization completed.
    Complete {
        /// Provider-specific token payload.
        value: T,
    },
}

/// Clock seam for deterministic tests.
pub trait DeviceCodeClock: Send + Sync {
    /// Current instant used for deadline arithmetic.
    fn now(&self) -> Instant;
}

/// Sleep seam for deterministic tests.
pub trait DeviceCodeSleeper: Send + Sync {
    /// Sleep for `duration`, aborting with [`AuthError::Cancelled`] when cancelled.
    fn sleep(
        &self,
        duration: Duration,
        cancellation: Option<&CancellationToken>,
    ) -> impl Future<Output = Result<(), AuthError>> + Send;
}

/// Wall clock backed by [`Instant::now`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemDeviceCodeClock;

impl DeviceCodeClock for SystemDeviceCodeClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Tokio sleep that observes a [`CancellationToken`].
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioDeviceCodeSleeper;

impl DeviceCodeSleeper for TokioDeviceCodeSleeper {
    async fn sleep(
        &self,
        duration: Duration,
        cancellation: Option<&CancellationToken>,
    ) -> Result<(), AuthError> {
        abortable_sleep(duration, cancellation).await
    }
}

/// Options for [`poll_oauth_device_code_flow`].
pub struct OAuthDeviceCodePollOptions<
    T,
    F,
    Fut,
    C = SystemDeviceCodeClock,
    S = TokioDeviceCodeSleeper,
> where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<OAuthDeviceCodePollResult<T>, AuthError>>,
    C: DeviceCodeClock,
    S: DeviceCodeSleeper,
{
    /// Initial poll interval in seconds (default 5).
    pub interval_seconds: Option<u64>,
    /// Device-code lifetime in seconds; omitted means no deadline.
    pub expires_in_seconds: Option<u64>,
    /// When true, sleep one interval before the first poll (GitHub/xAI).
    pub wait_before_first_poll: bool,
    /// One token/device poll attempt.
    pub poll: F,
    /// Optional cancellation signal.
    pub signal: Option<CancellationToken>,
    /// Clock used for deadline checks.
    pub clock: C,
    /// Sleeper used between polls.
    pub sleeper: S,
    /// Marker so `T` stays tied to the options type without a field.
    pub _marker: std::marker::PhantomData<T>,
}

impl<T, F, Fut> OAuthDeviceCodePollOptions<T, F, Fut, SystemDeviceCodeClock, TokioDeviceCodeSleeper>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<OAuthDeviceCodePollResult<T>, AuthError>>,
{
    /// Build options with system clock/sleeper defaults.
    pub fn new(poll: F) -> Self {
        Self {
            interval_seconds: None,
            expires_in_seconds: None,
            wait_before_first_poll: false,
            poll,
            signal: None,
            clock: SystemDeviceCodeClock,
            sleeper: TokioDeviceCodeSleeper,
            _marker: std::marker::PhantomData,
        }
    }
}

/// Poll until complete, failed, cancelled, or timed out.
///
/// # Errors
///
/// Returns [`AuthError::Cancelled`] on abort, [`AuthError::Message`] for poll
/// failures/timeouts (including the WSL `slow_down` timeout wording), or any
/// error raised by the caller-supplied `poll` function.
pub async fn poll_oauth_device_code_flow<T, F, Fut, C, S>(
    mut options: OAuthDeviceCodePollOptions<T, F, Fut, C, S>,
) -> Result<T, AuthError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<OAuthDeviceCodePollResult<T>, AuthError>>,
    C: DeviceCodeClock,
    S: DeviceCodeSleeper,
{
    let start = options.clock.now();
    let deadline = match options.expires_in_seconds {
        Some(seconds) => Some(
            start
                .checked_add(Duration::from_secs(seconds))
                .ok_or_else(|| AuthError::message(TIMEOUT_MESSAGE))?,
        ),
        None => None,
    };

    let mut interval_ms = MINIMUM_INTERVAL_MS.max(
        options
            .interval_seconds
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS)
            .saturating_mul(1000),
    );

    let mut slow_down_responses = 0_u32;

    if options.wait_before_first_poll {
        let remaining = remaining_ms(options.clock.now(), deadline);
        if remaining > 0 {
            options
                .sleeper
                .sleep(
                    Duration::from_millis(interval_ms.min(remaining)),
                    options.signal.as_ref(),
                )
                .await?;
        }
    }

    loop {
        let now = options.clock.now();
        if deadline.is_some_and(|deadline| now >= deadline) {
            break;
        }
        if options
            .signal
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(AuthError::Cancelled);
        }

        // Race each in-flight poll against cancellation so a hung token request
        // cannot outlive an abort. Lifetime is enforced between polls/sleeps,
        // matching the TypeScript device-code loop (and preserving WSL wording).
        let poll_result = run_poll_cancellable((options.poll)(), options.signal.as_ref()).await?;

        match poll_result {
            OAuthDeviceCodePollResult::Complete { value } => return Ok(value),
            OAuthDeviceCodePollResult::Failed { message } => {
                return Err(AuthError::message(message));
            }
            OAuthDeviceCodePollResult::Pending => {}
            OAuthDeviceCodePollResult::SlowDown { interval_seconds } => {
                slow_down_responses = slow_down_responses.saturating_add(1);
                interval_ms = match interval_seconds {
                    Some(seconds) if seconds > 0 => {
                        MINIMUM_INTERVAL_MS.max(seconds.saturating_mul(1000))
                    }
                    _ => MINIMUM_INTERVAL_MS
                        .max(interval_ms.saturating_add(SLOW_DOWN_INTERVAL_INCREMENT_MS)),
                };
            }
        }

        let remaining = remaining_ms(options.clock.now(), deadline);
        if remaining == 0 {
            break;
        }

        options
            .sleeper
            .sleep(
                Duration::from_millis(interval_ms.min(remaining)),
                options.signal.as_ref(),
            )
            .await?;
    }

    if slow_down_responses > 0 {
        Err(AuthError::message(SLOW_DOWN_TIMEOUT_MESSAGE))
    } else {
        Err(AuthError::message(TIMEOUT_MESSAGE))
    }
}
async fn run_poll_cancellable<T, Fut>(
    poll: Fut,
    cancellation: Option<&CancellationToken>,
) -> Result<OAuthDeviceCodePollResult<T>, AuthError>
where
    Fut: Future<Output = Result<OAuthDeviceCodePollResult<T>, AuthError>>,
{
    if let Some(signal) = cancellation {
        tokio::select! {
            () = signal.cancelled() => Err(AuthError::Cancelled),
            result = poll => result,
        }
    } else {
        poll.await
    }
}

/// Sleep that fails with [`AuthError::Cancelled`] when the token fires.
///
/// # Errors
///
/// Returns [`AuthError::Cancelled`] if cancellation is already set or fires
/// during the sleep.
pub async fn abortable_sleep(
    duration: Duration,
    cancellation: Option<&CancellationToken>,
) -> Result<(), AuthError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(AuthError::Cancelled);
    }
    if let Some(signal) = cancellation {
        tokio::select! {
            () = signal.cancelled() => Err(AuthError::Cancelled),
            () = tokio::time::sleep(duration) => Ok(()),
        }
    } else {
        tokio::time::sleep(duration).await;
        Ok(())
    }
}

fn remaining_ms(now: Instant, deadline: Option<Instant>) -> u64 {
    match deadline {
        Some(deadline) if deadline > now => {
            u64::try_from(deadline.duration_since(now).as_millis()).unwrap_or(u64::MAX)
        }
        Some(_) => 0,
        None => u64::MAX,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use super::*;

    type TestResult = Result<(), String>;

    fn err(msg: impl Into<String>) -> String {
        msg.into()
    }

    fn expect_err<T, E>(result: Result<T, E>, label: &str) -> Result<E, String> {
        match result {
            Ok(_) => Err(err(label)),
            Err(error) => Ok(error),
        }
    }

    #[derive(Clone)]
    struct FakeClock {
        base: Instant,
        now_ms: Arc<AtomicU64>,
    }

    impl FakeClock {
        fn new(start_ms: u64) -> Self {
            Self {
                base: Instant::now(),
                now_ms: Arc::new(AtomicU64::new(start_ms)),
            }
        }

        fn advance(&self, ms: u64) {
            self.now_ms.fetch_add(ms, Ordering::SeqCst);
        }
    }

    impl DeviceCodeClock for FakeClock {
        fn now(&self) -> Instant {
            self.base + Duration::from_millis(self.now_ms.load(Ordering::SeqCst))
        }
    }

    #[derive(Clone)]
    struct RecordingSleeper {
        sleeps_ms: Arc<AtomicU64>,
        last_sleep_ms: Arc<AtomicU64>,
        clock: FakeClock,
    }

    impl RecordingSleeper {
        fn new(clock: FakeClock) -> Self {
            Self {
                sleeps_ms: Arc::new(AtomicU64::new(0)),
                last_sleep_ms: Arc::new(AtomicU64::new(0)),
                clock,
            }
        }
    }

    impl DeviceCodeSleeper for RecordingSleeper {
        async fn sleep(
            &self,
            duration: Duration,
            cancellation: Option<&CancellationToken>,
        ) -> Result<(), AuthError> {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                return Err(AuthError::Cancelled);
            }
            let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
            self.sleeps_ms.fetch_add(ms, Ordering::SeqCst);
            self.last_sleep_ms.store(ms, Ordering::SeqCst);
            self.clock.advance(ms);
            Ok(())
        }
    }

    #[tokio::test]
    async fn pending_then_complete() -> TestResult {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_poll = Arc::clone(&calls);
        let clock = FakeClock::new(0);
        let sleeper = RecordingSleeper::new(clock.clone());
        let value = poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
            interval_seconds: Some(1),
            expires_in_seconds: Some(30),
            wait_before_first_poll: false,
            poll: move || {
                let calls = Arc::clone(&calls_poll);
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    if n == 1 {
                        Ok(OAuthDeviceCodePollResult::Pending)
                    } else {
                        Ok(OAuthDeviceCodePollResult::Complete { value: "ok" })
                    }
                }
            },
            signal: None,
            clock,
            sleeper,
            _marker: std::marker::PhantomData,
        })
        .await
        .map_err(|e| err(e.to_string()))?;
        assert_eq!(value, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[tokio::test]
    async fn slow_down_grows_interval_by_five_seconds() -> TestResult {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_poll = Arc::clone(&calls);
        let clock = FakeClock::new(0);
        let sleeper = RecordingSleeper::new(clock.clone());
        let last = Arc::clone(&sleeper.last_sleep_ms);

        let err_value = expect_err(
            poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
                interval_seconds: Some(1),
                expires_in_seconds: Some(12),
                wait_before_first_poll: false,
                poll: move || {
                    let calls = Arc::clone(&calls_poll);
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                        if n == 1 {
                            Ok(OAuthDeviceCodePollResult::<()>::SlowDown {
                                interval_seconds: None,
                            })
                        } else {
                            Ok(OAuthDeviceCodePollResult::Pending)
                        }
                    }
                },
                signal: None,
                clock,
                sleeper,
                _marker: std::marker::PhantomData,
            })
            .await,
            "timeout",
        )?;

        // After slow_down without server interval: 1s -> 6s.
        assert!(last.load(Ordering::SeqCst) >= 6_000);
        assert!(err_value.to_string().contains("clock drift in WSL"));
        Ok(())
    }

    #[tokio::test]
    async fn slow_down_uses_server_interval_when_positive() -> TestResult {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_poll = Arc::clone(&calls);
        let clock = FakeClock::new(0);
        let sleeper = RecordingSleeper::new(clock.clone());
        let last = Arc::clone(&sleeper.last_sleep_ms);

        poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
            interval_seconds: Some(1),
            expires_in_seconds: Some(20),
            wait_before_first_poll: false,
            poll: move || {
                let calls = Arc::clone(&calls_poll);
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    if n == 1 {
                        Ok(OAuthDeviceCodePollResult::SlowDown {
                            interval_seconds: Some(9),
                        })
                    } else {
                        Ok(OAuthDeviceCodePollResult::Complete { value: () })
                    }
                }
            },
            signal: None,
            clock,
            sleeper,
            _marker: std::marker::PhantomData,
        })
        .await
        .map_err(|e| err(e.to_string()))?;

        assert_eq!(last.load(Ordering::SeqCst), 9_000);
        Ok(())
    }

    #[tokio::test]
    async fn cancel_maps_to_login_cancelled() -> TestResult {
        let token = CancellationToken::new();
        token.cancel();
        let clock = FakeClock::new(0);
        let err_value = expect_err(
            poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
                interval_seconds: Some(1),
                expires_in_seconds: Some(10),
                wait_before_first_poll: false,
                poll: || async { Ok(OAuthDeviceCodePollResult::<()>::Pending) },
                signal: Some(token),
                clock: clock.clone(),
                sleeper: RecordingSleeper::new(clock),
                _marker: std::marker::PhantomData,
            })
            .await,
            "cancelled",
        )?;
        assert!(matches!(err_value, AuthError::Cancelled));
        assert_eq!(err_value.to_string(), "Login cancelled");
        Ok(())
    }

    #[tokio::test]
    async fn cancel_aborts_in_flight_poll() -> TestResult {
        let token = CancellationToken::new();
        let cancel = token.clone();
        let clock = FakeClock::new(0);
        let handle = tokio::spawn(async move {
            poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
                interval_seconds: Some(1),
                expires_in_seconds: Some(30),
                wait_before_first_poll: false,
                poll: || async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(OAuthDeviceCodePollResult::Complete { value: () })
                },
                signal: Some(cancel),
                clock: clock.clone(),
                sleeper: RecordingSleeper::new(clock),
                _marker: std::marker::PhantomData,
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
        let joined = handle.await.map_err(|e| err(e.to_string()))?;
        let err_value = expect_err(joined, "cancelled")?;
        assert!(matches!(err_value, AuthError::Cancelled));
        Ok(())
    }

    #[tokio::test]
    async fn timeout_without_slow_down_uses_plain_message() -> TestResult {
        let clock = FakeClock::new(0);
        let sleeper = RecordingSleeper::new(clock.clone());
        let err_value = expect_err(
            poll_oauth_device_code_flow(OAuthDeviceCodePollOptions {
                interval_seconds: Some(1),
                expires_in_seconds: Some(1),
                wait_before_first_poll: false,
                poll: || async { Ok(OAuthDeviceCodePollResult::<()>::Pending) },
                signal: None,
                clock,
                sleeper,
                _marker: std::marker::PhantomData,
            })
            .await,
            "timeout",
        )?;
        assert_eq!(err_value.to_string(), TIMEOUT_MESSAGE);
        Ok(())
    }

    #[tokio::test]
    async fn abortable_sleep_observes_cancel() -> TestResult {
        let token = CancellationToken::new();
        let cancel = token.clone();
        let handle =
            tokio::spawn(
                async move { abortable_sleep(Duration::from_secs(30), Some(&cancel)).await },
            );
        tokio::time::sleep(Duration::from_millis(10)).await;
        token.cancel();
        let joined = handle.await.map_err(|e| err(e.to_string()))?;
        let err_value = expect_err(joined, "cancelled")?;
        assert!(matches!(err_value, AuthError::Cancelled));
        Ok(())
    }
}
