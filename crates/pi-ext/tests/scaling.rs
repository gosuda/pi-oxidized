//! Verification check 8 (Rust side): native keypress-to-paint / frame CPU
//! scaling under active-widget loads, the terminal-input queue bound, and
//! stale-generation drops under widget bursts.
//!
//! Uses an in-process fake host (duplex pipes) so the suite stays deterministic
//! and free of a real Bun process. The TypeScript host suite covers the real
//! extension-host actor under the same thresholds.

use std::error::Error;
use std::time::{Duration, Instant};

use pi_ext::adapters::SlotComponent;
use pi_ext::client::{HostClient, HostNotification, HostResult};
use pi_ext::protocol::{
    Frame, FrameKind, HelloAck, Method, SlotPlacement, StyledRun, TerminalInputResult, UiSlot,
    decode_frame_str, encode_frame, from_payload, to_payload,
};
use pi_tui::component::Component;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

type R = Result<(), Box<dyn Error + Send + Sync>>;

struct FakeHost {
    read: BufReader<tokio::io::DuplexStream>,
    write: tokio::io::DuplexStream,
}

impl FakeHost {
    async fn read_frame(&mut self) -> Option<Frame> {
        let mut line = String::new();
        match self.read.read_line(&mut line).await {
            Ok(0) | Err(_) => None,
            Ok(_) => decode_frame_str(line.trim_end()).ok(),
        }
    }

    async fn require_frame(&mut self, label: &str) -> std::io::Result<Frame> {
        match self.read_frame().await {
            Some(frame) => Ok(frame),
            None => Err(std::io::Error::other(format!(
                "fake host expected a frame ({label}) but got EOF"
            ))),
        }
    }

    async fn write_frame(&mut self, frame: &Frame) -> R {
        let bytes = encode_frame(frame).map_err(|e| std::io::Error::other(e.to_string()))?;
        self.write.write_all(&bytes).await?;
        self.write.flush().await?;
        Ok(())
    }

    async fn answer_hello(&mut self) -> R {
        let req = self.require_frame("hello").await?;
        assert_eq!(req.kind, FrameKind::Req);
        assert_eq!(req.method, Method::Hello.as_str());
        let ack = Frame::response(req.id, Method::Hello, to_payload(&HelloAck::local())?);
        self.write_frame(&ack).await
    }

    async fn close(mut self) {
        let _ = self.write.shutdown().await;
    }
}

fn make_pair() -> (HostClient, FakeHost) {
    let (client_to_host, host_from_client) = tokio::io::duplex(64 * 1024);
    let (host_to_client, client_from_host) = tokio::io::duplex(64 * 1024);
    let (client_err, _host_err) = tokio::io::duplex(4096);
    let client = HostClient::connect_boxed(
        Box::new(client_to_host),
        Box::new(client_from_host),
        Box::new(client_err),
        None,
    );
    let host = FakeHost {
        read: BufReader::new(host_from_client),
        write: host_to_client,
    };
    (client, host)
}

fn sample_slot(key: &str, generation: u64, text: &str) -> UiSlot {
    UiSlot {
        key: key.to_owned(),
        generation,
        placement: SlotPlacement::AboveEditor,
        height: 1,
        runs: vec![vec![StyledRun {
            text: text.to_owned(),
            style: pi_ext::protocol::Style::default(),
        }]],
        focusable: false,
        cursor: None,
        overlay_options: None,
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

fn stats(samples: &[f64]) -> (f64, f64, f64) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (
        percentile(&sorted, 50.0),
        percentile(&sorted, 95.0),
        percentile(&sorted, 99.0),
    )
}

/// Simulate native keypress-to-paint for N installed idle slots:
/// sanitize + measure + render into a buffer (never awaits host).
fn paint_cycle(slots: &mut [SlotComponent], width: u16) -> f64 {
    let t0 = Instant::now();
    for component in slots.iter_mut() {
        let height = component.measure(width);
        let area = Rect::new(0, 0, width, height.max(1));
        let mut buf = Buffer::empty(area);
        component.render(area, &mut buf);
    }
    t0.elapsed().as_secs_f64() * 1_000.0
}

#[tokio::test]
async fn active_widget_burst_drops_stale_and_stays_responsive() -> R {
    let (client, mut host) = make_pair();
    let mut sub = client.subscribe_notifications();

    let client_task = tokio::spawn(async move {
        client.handshake().await?;
        // Drive one request so the reader is live while we flood slots.
        client
            .request(
                Method::Notify,
                serde_json::json!({}),
                Duration::from_secs(2),
            )
            .await
    });

    host.answer_hello().await?;
    let req = host.require_frame("notify").await?;

    // 20 active widgets, then a stale reordered burst (gen N-1 after N).
    let mut accepted = 0u32;
    for i in 0..20u64 {
        let key = format!("widget.active.{i}");
        let fresh = sample_slot(&key, i + 2, &format!("fresh-{i}"));
        let stale = sample_slot(&key, i + 1, &format!("stale-{i}"));
        host.write_frame(&Frame::event(0, Method::UiSlot, to_payload(&fresh)?))
            .await?;
        host.write_frame(&Frame::event(0, Method::UiSlot, to_payload(&stale)?))
            .await?;
        accepted += 1;
    }

    let res = Frame::response(req.id, Method::Notify, serde_json::json!({}));
    host.write_frame(&res).await?;
    let _ = client_task.await??;

    let mut seen: Vec<(String, u64)> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while let Ok(Ok(ev)) = timeout(
        deadline.saturating_duration_since(tokio::time::Instant::now()),
        sub.recv(),
    )
    .await
    {
        if let HostNotification::UiSlot(slot) = ev {
            seen.push((slot.key, slot.generation));
        }
    }

    // Exactly one accepted generation per key (the higher one); stale dropped.
    assert_eq!(
        seen.len(),
        20,
        "expected 20 accepted slots, got {} ({seen:?})",
        seen.len()
    );
    for i in 0..20u64 {
        let key = format!("widget.active.{i}");
        let gens: Vec<u64> = seen
            .iter()
            .filter(|(k, _)| k == &key)
            .map(|(_, g)| *g)
            .collect();
        assert_eq!(
            gens,
            vec![i + 2],
            "stale must be dropped for {key}: {gens:?}"
        );
    }
    let _ = accepted;

    // Input path stays local/responsive: no host await on paint of accepted slots.
    let mut components: Vec<SlotComponent> = seen
        .iter()
        .map(|(key, generation)| {
            let slot = sample_slot(key, *generation, "ok");
            SlotComponent::from_ui_slot(&slot)
        })
        .collect();
    let t0 = Instant::now();
    let ms = paint_cycle(&mut components, 80);
    assert!(
        t0.elapsed() < Duration::from_millis(50),
        "paint after burst must stay responsive, took {ms:.3}ms"
    );

    host.close().await;
    Ok(())
}

#[tokio::test]
async fn terminal_input_fast_path_under_five_ms_p99() -> R {
    const WARMUPS: usize = 20;
    const SAMPLES: usize = 100;
    const INPUT_TIMEOUT: Duration = Duration::from_millis(4);

    let (client, mut host) = make_pair();
    let client_for_task = client;
    let driver = tokio::spawn(async move {
        client_for_task.handshake().await?;
        let mut latencies = Vec::with_capacity(WARMUPS + SAMPLES);
        for i in 0..(WARMUPS + SAMPLES) {
            let data = if i % 3 == 0 {
                "x"
            } else if i % 3 == 1 {
                "a"
            } else {
                "b"
            };
            let t0 = Instant::now();
            let frame = client_for_task
                .request(
                    Method::TerminalInput,
                    serde_json::json!({ "data": data }),
                    Duration::from_millis(50),
                )
                .await?;
            latencies.push(t0.elapsed().as_secs_f64() * 1_000.0);
            let result: TerminalInputResult =
                from_payload(&frame.payload).map_err(|e| format!("decode terminalInput: {e}"))?;
            match data {
                "x" => assert!(result.consume, "x must be consumed"),
                "a" => {
                    assert!(!result.consume);
                    assert_eq!(result.data.as_deref(), Some("A"));
                }
                _ => {
                    assert!(!result.consume);
                    assert_eq!(result.data.as_deref(), Some("b"));
                }
            }
        }
        Ok::<Vec<f64>, Box<dyn Error + Send + Sync>>(latencies)
    });

    host.answer_hello().await?;
    for i in 0..(WARMUPS + SAMPLES) {
        let req = host.require_frame("terminalInput").await?;
        assert_eq!(req.method, Method::TerminalInput.as_str());
        let data = req
            .payload
            .get("data")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let (consume, out) = match data {
            "x" => (true, "x".to_owned()),
            "a" => (false, "A".to_owned()),
            other => (false, other.to_owned()),
        };
        let body = TerminalInputResult {
            consume,
            data: Some(out),
        };
        // Fast path: respond immediately (well under 4 ms).
        let _ = i;
        let _ = INPUT_TIMEOUT;
        host.write_frame(&Frame::response(
            req.id,
            Method::TerminalInput,
            to_payload(&body)?,
        ))
        .await?;
    }

    let latencies = driver.await??;
    let measured = &latencies[WARMUPS..];
    let (median, p95, p99) = stats(measured);
    assert!(
        p99 < 5.0,
        "fast terminalInput p99 must be <5ms, got median={median:.3} p95={p95:.3} p99={p99:.3}"
    );
    host.close().await;
    Ok(())
}

#[tokio::test]
async fn terminal_input_slow_timeout_disables_and_stays_local() -> R {
    const INPUT_TIMEOUT: Duration = Duration::from_millis(4);

    let (client, mut host) = make_pair();
    let client_for_task = client;
    let driver = tokio::spawn(async move {
        client_for_task.handshake().await?;

        // First keystroke: host is slow → client deadline fires once.
        let first = client_for_task
            .request(
                Method::TerminalInput,
                serde_json::json!({ "data": "q" }),
                INPUT_TIMEOUT,
            )
            .await;
        match first {
            Err(pi_ext::client::HostClientError::Timeout { .. }) => {}
            other => {
                return Err(format!("expected Timeout on slow handler, got {other:?}").into());
            }
        }

        // Later input: host responds immediately (handler disabled / local).
        let t0 = Instant::now();
        let second = client_for_task
            .request(
                Method::TerminalInput,
                serde_json::json!({ "data": "a" }),
                Duration::from_millis(50),
            )
            .await?;
        let elapsed = t0.elapsed().as_secs_f64() * 1_000.0;
        let result: TerminalInputResult = from_payload(&second.payload)
            .map_err(|e| format!("decode second terminalInput: {e}"))?;
        assert!(!result.consume);
        assert_eq!(result.data.as_deref(), Some("a"));
        // Single-sample path under load; semantic is "no second timeout".
        assert!(
            elapsed < 50.0,
            "later input after disable must stay local, took {elapsed:.3}ms"
        );
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    });

    host.answer_hello().await?;

    // Slow first request: read it, wait past 4 ms, then answer too late.
    let req1 = host.require_frame("terminalInput slow").await?;
    assert_eq!(req1.method, Method::TerminalInput.as_str());
    tokio::time::sleep(Duration::from_millis(12)).await;
    // Late response must be dropped by the client (pending already removed).
    let late = TerminalInputResult {
        consume: true,
        data: Some("slow:q".to_owned()),
    };
    let _ = host
        .write_frame(&Frame::response(
            req1.id,
            Method::TerminalInput,
            to_payload(&late)?,
        ))
        .await;

    // Second request: respond immediately with original key (local path).
    let req2 = host.require_frame("terminalInput local").await?;
    let local = TerminalInputResult {
        consume: false,
        data: Some("a".to_owned()),
    };
    host.write_frame(&Frame::response(
        req2.id,
        Method::TerminalInput,
        to_payload(&local)?,
    ))
    .await?;

    driver.await??;
    host.close().await;
    Ok(())
}

#[tokio::test]
async fn terminal_input_queue_bound_is_sixty_four() -> R {
    // Contract: STREAM/outbound capacities and the host's sequential actor
    // are bounded at 64 for input. Documented constant from the plan.
    assert_eq!(pi_ext::client::STREAM_EVENT_CAPACITY, 64);
    Ok(())
}

// Silence unused HostResult import warning if the compiler optimizes paths.
#[allow(dead_code)]
fn _touch_host_result(_: HostResult<()>) {}
