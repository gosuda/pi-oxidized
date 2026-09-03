//! JSON renderer: emit a session header followed by a lossless JSONL event
//! stream.
//!
//! Mirrors the json-mode branch of `.references/pi-2.0/packages/coding-agent/src/
//! modes/print-mode.ts`: the session header (if any) is written first, then
//! every [`AgentSessionEvent`] is serialized with [`serialize_json_line`] (raw
//! U+2028/U+2029, LF-terminated). Writes go through a [`PrintSink`] so the
//! [`OutputGuard`](crate::core::output_guard) raw-stdout queue provides FIFO
//! ordering and backpressure; no `println!` touches stdout while the guard is
//! active.

use std::io;

use futures::Stream;
use futures::StreamExt;
use serde::Serialize;

use super::PrintSink;
use crate::core::agent_session::AgentSessionEvent;
use crate::core::sessions::SessionHeader;
use crate::modes::rpc::serialize_json_line;

/// Write the session header as the first JSONL record (when present).
///
/// # Errors
///
/// Propagates sink write or serialization failures.
pub async fn render_json_header<K>(header: Option<&SessionHeader>, sink: &K) -> io::Result<()>
where
    K: PrintSink,
{
    let Some(header) = header else {
        return Ok(());
    };
    let line = serialize_line(header)?;
    sink.write_stdout(&line).await
}

/// Write one event as a JSONL record.
///
/// # Errors
///
/// Propagates sink write or serialization failures.
pub async fn render_json_event<K>(event: &AgentSessionEvent, sink: &K) -> io::Result<()>
where
    K: PrintSink,
{
    let line = serialize_line(event)?;
    sink.write_stdout(&line).await
}

/// Render a stream of session events as JSONL lines.
///
/// # Errors
///
/// Propagates the first sink write or serialization failure.
pub async fn render_json_events<S, K>(events: S, sink: &K) -> io::Result<()>
where
    S: Stream<Item = AgentSessionEvent> + Send + Unpin,
    K: PrintSink,
{
    let mut events = events;
    while let Some(event) = events.next().await {
        render_json_event(&event, sink).await?;
    }
    sink.flush().await
}

/// Render an optional session header followed by a stream of session events.
///
/// # Errors
///
/// Propagates the first sink write or serialization failure.
pub async fn render_json<S, K>(
    header: Option<&SessionHeader>,
    events: S,
    sink: &K,
) -> io::Result<()>
where
    S: Stream<Item = AgentSessionEvent> + Send + Unpin,
    K: PrintSink,
{
    render_json_header(header, sink).await?;
    render_json_events(events, sink).await
}

fn serialize_line<T: Serialize>(value: &T) -> io::Result<String> {
    serialize_json_line(value).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::print::BufferSink;
    use futures::stream;
    use pi_agent::user_text;
    use serde_json::Value;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn parse_lines(text: &str) -> Result<Vec<Value>, serde_json::Error> {
        text.lines()
            .filter(|line| !line.is_empty())
            .map(serde_json::from_str)
            .collect()
    }

    #[tokio::test]
    async fn json_emits_header_then_events() -> TestResult {
        let header = SessionHeader::new("id-1", "2024-01-01T00:00:00.000Z", "/cwd", None);
        let events = vec![
            AgentSessionEvent::AgentStart,
            AgentSessionEvent::AgentSettled,
        ];
        let sink = BufferSink::default();
        render_json(Some(&header), stream::iter(events), &sink).await?;

        let stdout = sink.stdout_string();
        let records = parse_lines(&stdout)?;
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["type"], "session");
        assert_eq!(records[0]["id"], "id-1");
        assert_eq!(records[1]["type"], "agent_start");
        assert_eq!(records[2]["type"], "agent_settled");
        assert!(sink.stderr_string().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn json_without_header_emits_events_only() -> TestResult {
        let events = vec![AgentSessionEvent::AgentStart];
        let sink = BufferSink::default();
        render_json(None, stream::iter(events), &sink).await?;
        let records = parse_lines(&sink.stdout_string())?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["type"], "agent_start");
        Ok(())
    }

    #[tokio::test]
    async fn json_each_line_parses_and_preserves_event() -> TestResult {
        let event = AgentSessionEvent::QueueUpdate {
            steering: vec!["steer-msg".into()],
            follow_up: vec!["follow-msg".into()],
        };
        let sink = BufferSink::default();
        render_json_event(&event, &sink).await?;
        let line = sink.stdout_string();
        let value: Value = serde_json::from_str(line.trim_end())?;
        assert_eq!(value["type"], "queue_update");
        assert_eq!(value["steering"], Value::from(["steer-msg"]));
        assert_eq!(value["followUp"], Value::from(["follow-msg"]));
        Ok(())
    }

    #[tokio::test]
    async fn json_preserves_camel_case_payload_fields() -> TestResult {
        let event = AgentSessionEvent::AgentEnd {
            messages: vec![user_text("hi", std::iter::empty())],
            will_retry: true,
        };
        let sink = BufferSink::default();
        render_json_event(&event, &sink).await?;
        let value: Value = serde_json::from_str(sink.stdout_string().trim_end())?;
        assert_eq!(value["type"], "agent_end");
        assert_eq!(value["willRetry"], true);
        assert!(value["messages"].is_array());
        Ok(())
    }

    #[tokio::test]
    async fn json_empty_event_stream_writes_header_only() -> TestResult {
        let header = SessionHeader::new("id-2", "2024-01-01T00:00:00.000Z", "/cwd", None);
        let sink = BufferSink::default();
        render_json(Some(&header), stream::empty(), &sink).await?;
        let records = parse_lines(&sink.stdout_string())?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["type"], "session");
        Ok(())
    }

    #[tokio::test]
    async fn json_lines_are_lf_terminated() -> TestResult {
        let events = vec![
            AgentSessionEvent::AgentStart,
            AgentSessionEvent::AgentSettled,
        ];
        let sink = BufferSink::default();
        render_json_events(stream::iter(events), &sink).await?;
        let stdout = sink.stdout_string();
        assert!(stdout.ends_with('\n'));
        assert!(!stdout.contains("\r\n"), "no CRLF in JSONL output");
        Ok(())
    }
}
