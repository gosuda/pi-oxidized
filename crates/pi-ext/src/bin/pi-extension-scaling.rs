//! Machine-readable production `serve_io` sampler for extension scaling.

use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pi_ext::protocol::{
    COMPATIBILITY_VERSION, ErrorPayload, Frame, FrameKind, Hello, HelloAck, Method,
    PROTOCOL_VERSION, TerminalInputResult, decode_frame_str, encode_frame, from_payload,
    to_payload,
};
use pi_ext::server::{
    EXTENSIONS_LOAD_METHOD, ExtensionFault, NativeEventSink, NativeExtension,
    NativeExtensionContext, NativeFuture, RegistrySnapshot, ServerConfig, ServerError, ToolCall,
    ToolUpdateSink, serve_io,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

const ENTRYPOINT: &str = "pi_ext::server::serve_io";
const FRAME_CODEC: &str = "pi_ext::protocol::{encode_frame,decode_frame_str}";
const CORPUS_IDENTITY: &str = "extension-scaling-terminal-input-v1";
const DIGEST_ALGORITHM: &str = "fnv1a64";
const MEASURED_ROUNDS: usize = 9;
const WARMUPS: usize = 30;
const SAMPLES: usize = 10_000;
const FAST_STREAM_SAMPLES: usize = 10_000;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

type AnyError = Box<dyn Error + Send + Sync>;
type Result<T> = std::result::Result<T, AnyError>;
type ServerHandle = tokio::task::JoinHandle<std::result::Result<(), ServerError>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadProfile {
    Zero,
    Idle100,
    Active20,
    Fast,
    Slow,
}

impl LoadProfile {
    const fn scenario(self) -> &'static str {
        match self {
            Self::Zero => "zero",
            Self::Idle100 => "idle100",
            Self::Active20 => "active20",
            Self::Fast => "fastTerminalInput",
            Self::Slow => "slowTerminalInput",
        }
    }

    const fn extension_count(self) -> u64 {
        match self {
            Self::Zero => 0,
            Self::Idle100 => 100,
            Self::Active20 => 20,
            Self::Fast => 1,
            Self::Slow => 2,
        }
    }

    const fn has_session_start(self) -> bool {
        matches!(self, Self::Active20 | Self::Fast | Self::Slow)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TerminalInputMode {
    PassThrough,
    Fast,
    SlowThenFast,
}

impl TerminalInputMode {
    const fn label(self) -> &'static str {
        match self {
            Self::PassThrough => "passThrough",
            Self::Fast => "fast",
            Self::SlowThenFast => "slowThenFast",
        }
    }
}

struct ScalingAdapter {
    profile: LoadProfile,
    terminal_mode: TerminalInputMode,
    terminal_call_count: AtomicUsize,
}

impl ScalingAdapter {
    const fn new(profile: LoadProfile, terminal_mode: TerminalInputMode) -> Self {
        Self {
            profile,
            terminal_mode,
            terminal_call_count: AtomicUsize::new(0),
        }
    }
}

impl NativeExtension for ScalingAdapter {
    fn snapshot(&self) -> RegistrySnapshot {
        RegistrySnapshot {
            tools: Vec::new(),
            commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            renderers: Vec::new(),
            providers: Vec::new(),
            handlers: if self.profile.has_session_start() {
                vec!["session_start".to_owned()]
            } else {
                Vec::new()
            },
            terminal_input: !matches!(self.terminal_mode, TerminalInputMode::PassThrough),
            extensions: self.profile.extension_count(),
            errors: Vec::new(),
        }
    }

    fn prepare_tool(
        &self,
        _context: std::sync::Arc<NativeExtensionContext>,
        _name: String,
        args: Value,
    ) -> NativeFuture<std::result::Result<Value, ExtensionFault>> {
        Box::pin(async move { Ok(args) })
    }

    fn validate_tool(
        &self,
        _context: std::sync::Arc<NativeExtensionContext>,
        _name: String,
        args: Value,
        _tool_call_id: Option<String>,
    ) -> NativeFuture<std::result::Result<Value, ExtensionFault>> {
        Box::pin(async move { Ok(args) })
    }

    fn execute_tool(
        &self,
        _context: std::sync::Arc<NativeExtensionContext>,
        call: ToolCall,
        updates: ToolUpdateSink,
        cancel: CancellationToken,
    ) -> NativeFuture<std::result::Result<Value, ExtensionFault>> {
        let _ = (updates, cancel);
        Box::pin(async move {
            Err(ExtensionFault::not_found(format!(
                "tool not found: {}",
                call.name
            )))
        })
    }

    fn handle_terminal_input(
        &self,
        _context: std::sync::Arc<NativeExtensionContext>,
        data: String,
    ) -> NativeFuture<std::result::Result<TerminalInputResult, ExtensionFault>> {
        let is_slow = self.terminal_mode == TerminalInputMode::SlowThenFast
            && self.terminal_call_count.fetch_add(1, Ordering::SeqCst) == 0;
        let mode = self.terminal_mode;
        Box::pin(async move {
            if is_slow {
                std::future::pending::<()>().await;
                unreachable!()
            }
            match mode {
                TerminalInputMode::PassThrough => Ok(TerminalInputResult {
                    consume: false,
                    data: Some(data),
                }),
                TerminalInputMode::Fast | TerminalInputMode::SlowThenFast if data == "x" => {
                    Ok(TerminalInputResult {
                        consume: true,
                        data: Some(data),
                    })
                }
                TerminalInputMode::Fast | TerminalInputMode::SlowThenFast => {
                    let rewritten = if data.len() == 1 {
                        data.to_uppercase()
                    } else {
                        data
                    };
                    Ok(TerminalInputResult {
                        consume: false,
                        data: Some(rewritten),
                    })
                }
            }
        })
    }

    fn on_lifecycle(
        &self,
        _context: std::sync::Arc<NativeExtensionContext>,
        event_type: String,
        _payload: Value,
        events: NativeEventSink,
    ) -> NativeFuture<std::result::Result<Value, ExtensionFault>> {
        let widget_count = if self.profile == LoadProfile::Active20 {
            20
        } else {
            0
        };
        Box::pin(async move {
            for index in 0..widget_count {
                let key = format!("widget.active.{index}");
                let _ = events
                    .send(
                        Method::UiSlot.as_str(),
                        json!({
                            "key": key,
                            "generation": 1,
                            "placement": "aboveEditor",
                            "height": 1,
                            "runs": [[{ "text": format!("widget-{index}"), "style": {} }]],
                        }),
                    )
                    .await;
            }
            Ok(json!({ "seen": event_type }))
        })
    }
}

struct RawPeer {
    write: tokio::io::DuplexStream,
    read: BufReader<tokio::io::DuplexStream>,
    pending_events: Vec<Frame>,
}

impl RawPeer {
    async fn send(&mut self, frame: &Frame) -> Result<()> {
        let bytes = encode_frame(frame).map_err(|error| io_error(error.to_string()))?;
        self.write.write_all(&bytes).await?;
        self.write.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Frame> {
        let mut line = String::new();
        let bytes = tokio::time::timeout(IO_TIMEOUT, self.read.read_line(&mut line))
            .await
            .map_err(|_| io_error("serve_io response timed out"))??;
        if bytes == 0 {
            return Err(io_error("serve_io closed before sending a response").into());
        }
        decode_frame_str(line.trim_end()).map_err(|error| io_error(error.to_string()).into())
    }

    async fn recv_with_id(&mut self, expected_id: u64) -> Result<Frame> {
        loop {
            let frame = self.recv().await?;
            if frame.id == expected_id {
                return Ok(frame);
            }
            if frame.kind != FrameKind::Event {
                return Err(io_error(format!(
                    "expected response id {expected_id}, received {}",
                    frame.id
                ))
                .into());
            }
            self.pending_events.push(frame);
        }
    }

    async fn request(&mut self, id: u64, method: &str, payload: Value) -> Result<Frame> {
        self.send(&Frame {
            id,
            kind: FrameKind::Req,
            method: method.to_owned(),
            payload,
        })
        .await?;
        self.recv_with_id(id).await
    }

    async fn terminal_input(&mut self, id: u64, data: &str) -> Result<Frame> {
        self.request(id, Method::TerminalInput.as_str(), json!({ "data": data }))
            .await
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProtocolProvenance {
    compiled_protocol_version: u32,
    compiled_compatibility_version: &'static str,
    observed_protocol_version: u32,
    observed_compatibility_version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CorpusProvenance {
    identity: &'static str,
    digest_algorithm: &'static str,
    digest: String,
    measured_rounds: usize,
    warmups_per_scenario: usize,
    samples_per_scenario: usize,
    fast_stream_samples: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Provenance {
    entrypoint: &'static str,
    frame_codec: &'static str,
    protocol: ProtocolProvenance,
    corpus: CorpusProvenance,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScenarioReport {
    scenario: &'static str,
    extension_count: u64,
    terminal_input_mode: &'static str,
    requests_per_sample: usize,
    normalized_samples_ms: Vec<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    timeout_samples_ms: Vec<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    locality_samples_ms: Vec<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Correctness {
    hello_ack_observed: bool,
    id_correlation: bool,
    deterministic_payloads: bool,
    active_widget_keys: usize,
    slow_timeout_code: &'static str,
    slow_timeout_retryable: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SamplerReport {
    schema_version: u32,
    provenance: Provenance,
    scenarios: Vec<ScenarioReport>,
    correctness: Correctness,
    pass: bool,
    failures: Vec<String>,
}

fn io_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

fn corpus_data(index: usize) -> &'static str {
    match index % 3 {
        0 => "x",
        1 => "a",
        _ => "b",
    }
}

struct CorpusHasher {
    hash: u64,
}

impl CorpusHasher {
    fn seeded() -> Self {
        let mut hasher = Self {
            hash: 0xcbf29ce484222325,
        };
        hasher.field("identity", CORPUS_IDENTITY.as_bytes());
        hasher.number("measuredRounds", MEASURED_ROUNDS);
        hasher.number("warmupsPerScenario", WARMUPS);
        hasher.number("samplesPerScenario", SAMPLES);
        hasher.number("fastStreamSamples", FAST_STREAM_SAMPLES);
        hasher
    }

    fn record_request(
        &mut self,
        profile: LoadProfile,
        mode: TerminalInputMode,
        round: usize,
        phase: &'static str,
        id: u64,
        data: &str,
    ) {
        self.field("scenario", profile.scenario().as_bytes());
        self.field("mode", mode.label().as_bytes());
        self.number("round", round);
        self.field("phase", phase.as_bytes());
        self.field("requestId", &id.to_le_bytes());
        self.field("data", data.as_bytes());
    }

    fn number(&mut self, name: &'static str, value: usize) {
        let value = u64::try_from(value).expect("corpus values fit in u64");
        self.field(name, &value.to_le_bytes());
    }

    fn field(&mut self, name: &'static str, value: &[u8]) {
        self.bytes(name.as_bytes());
        self.bytes(value);
    }

    fn bytes(&mut self, value: &[u8]) {
        let length = u64::try_from(value.len()).expect("corpus fields fit in u64");
        self.update(&length.to_le_bytes());
        self.update(value);
    }

    fn update(&mut self, value: &[u8]) {
        for &byte in value {
            self.hash ^= u64::from(byte);
            self.hash = self.hash.wrapping_mul(0x100000001b3);
        }
    }

    fn digest(&self) -> String {
        format!("{:016x}", self.hash)
    }
}

async fn recorded_terminal_input(
    peer: &mut RawPeer,
    corpus: &mut CorpusHasher,
    profile: LoadProfile,
    mode: TerminalInputMode,
    round: usize,
    phase: &'static str,
    id: u64,
    data: &str,
) -> Result<Frame> {
    corpus.record_request(profile, mode, round, phase, id, data);
    peer.terminal_input(id, data).await
}

fn spawn_server(profile: LoadProfile, mode: TerminalInputMode) -> (RawPeer, ServerHandle) {
    let (client_tx, server_rx) = tokio::io::duplex(64 * 1024);
    let (server_tx, client_rx) = tokio::io::duplex(64 * 1024);
    let adapter = ScalingAdapter::new(profile, mode);
    let handle = tokio::spawn(async move {
        serve_io(server_rx, server_tx, adapter, ServerConfig::default()).await
    });
    (
        RawPeer {
            write: client_tx,
            read: BufReader::new(client_rx),
            pending_events: Vec::new(),
        },
        handle,
    )
}

fn expect_frame(frame: &Frame, id: u64, kind: FrameKind, method: &str) -> Result<()> {
    if frame.id != id || frame.kind != kind || frame.method != method {
        return Err(io_error(format!(
            "unexpected frame: id={} kind={} method={} (wanted id={id} kind={kind} method={method})",
            frame.id, frame.kind, frame.method
        ))
        .into());
    }
    Ok(())
}

async fn open_scenario(
    profile: LoadProfile,
    mode: TerminalInputMode,
) -> Result<(RawPeer, ServerHandle, HelloAck)> {
    let (mut peer, handle) = spawn_server(profile, mode);
    let hello = peer
        .request(
            1,
            Method::Hello.as_str(),
            to_payload(&Hello::local()).map_err(|error| io_error(error.to_string()))?,
        )
        .await?;
    expect_frame(&hello, 1, FrameKind::Res, Method::Hello.as_str())?;
    let ack = from_payload::<HelloAck>(&hello.payload)?;
    if ack != HelloAck::local() {
        return Err(
            io_error("serve_io hello acknowledgment did not match compiled constants").into(),
        );
    }

    let loaded = peer
        .request(
            2,
            EXTENSIONS_LOAD_METHOD,
            json!({
                "extensionPaths": [],
                "cwd": "/extension-scaling",
                "projectTrusted": false,
            }),
        )
        .await?;
    expect_frame(&loaded, 2, FrameKind::Res, EXTENSIONS_LOAD_METHOD)?;
    let snapshot = from_payload::<RegistrySnapshot>(&loaded.payload)?;
    if snapshot.extensions != profile.extension_count() {
        return Err(io_error(format!(
            "{} reported {} extensions, expected {}",
            profile.scenario(),
            snapshot.extensions,
            profile.extension_count()
        ))
        .into());
    }

    if profile.has_session_start() {
        let started = peer
            .request(
                3,
                "session_start",
                json!({ "type": "session_start", "reason": "startup" }),
            )
            .await?;
        expect_frame(&started, 3, FrameKind::Res, "session_start")?;
    }

    if profile == LoadProfile::Active20 {
        let keys = peer
            .pending_events
            .iter()
            .filter(|frame| frame.method == Method::UiSlot.as_str())
            .filter_map(|frame| frame.payload.get("key").and_then(Value::as_str))
            .collect::<BTreeSet<_>>();
        if keys.len() != 20 {
            return Err(io_error(format!(
                "active20 emitted {} unique widget keys, expected 20",
                keys.len()
            ))
            .into());
        }
    }

    Ok((peer, handle, ack))
}

async fn finish_scenario(peer: RawPeer, handle: ServerHandle) -> Result<()> {
    drop(peer);
    let joined = tokio::time::timeout(IO_TIMEOUT, handle)
        .await
        .map_err(|_| io_error("serve_io did not stop after peer EOF"))?;
    joined.map_err(|error| io_error(format!("serve_io task failed: {error}")))??;
    Ok(())
}

fn validate_terminal_result(
    frame: &Frame,
    id: u64,
    data: &str,
    mode: TerminalInputMode,
) -> Result<()> {
    expect_frame(frame, id, FrameKind::Res, Method::TerminalInput.as_str())?;
    let actual = from_payload::<TerminalInputResult>(&frame.payload)?;
    let expected = match mode {
        TerminalInputMode::PassThrough => TerminalInputResult {
            consume: false,
            data: Some(data.to_owned()),
        },
        TerminalInputMode::Fast | TerminalInputMode::SlowThenFast if data == "x" => {
            TerminalInputResult {
                consume: true,
                data: Some(data.to_owned()),
            }
        }
        TerminalInputMode::Fast | TerminalInputMode::SlowThenFast => TerminalInputResult {
            consume: false,
            data: Some(data.to_uppercase()),
        },
    };
    if actual != expected {
        return Err(io_error(format!(
            "terminalInput id {id} returned {actual:?}, expected {expected:?}"
        ))
        .into());
    }
    Ok(())
}

fn request_id(start: u64, index: usize) -> Result<u64> {
    Ok(start + u64::try_from(index)?)
}

async fn measure_regular_scenario(
    profile: LoadProfile,
    mode: TerminalInputMode,
    requests_per_sample: usize,
    corpus: &mut CorpusHasher,
) -> Result<(ScenarioReport, HelloAck)> {
    let mut normalized_samples_ms = Vec::with_capacity(MEASURED_ROUNDS);
    let mut observed_ack = None;

    for round in 0..MEASURED_ROUNDS {
        let (mut peer, handle, ack) = open_scenario(profile, mode).await?;
        if observed_ack
            .as_ref()
            .is_some_and(|observed| observed != &ack)
        {
            return Err(io_error("serve_io hello acknowledgment changed between rounds").into());
        }
        observed_ack = Some(ack);

        for index in 0..WARMUPS {
            let data = if mode == TerminalInputMode::Fast {
                corpus_data(index)
            } else {
                "k"
            };
            let id = request_id(100, index)?;
            let frame = recorded_terminal_input(
                &mut peer, corpus, profile, mode, round, "warmup", id, data,
            )
            .await?;
            validate_terminal_result(&frame, id, data, mode)?;
        }

        let started = Instant::now();
        for index in 0..requests_per_sample {
            let data = if mode == TerminalInputMode::Fast {
                corpus_data(index)
            } else {
                "k"
            };
            let id = request_id(
                if mode == TerminalInputMode::Fast {
                    300
                } else {
                    200
                },
                index,
            )?;
            let frame = recorded_terminal_input(
                &mut peer, corpus, profile, mode, round, "measured", id, data,
            )
            .await?;
            validate_terminal_result(&frame, id, data, mode)?;
        }
        let request_count = f64::from(u32::try_from(requests_per_sample)?);
        normalized_samples_ms.push(started.elapsed().as_secs_f64() * 1_000.0 / request_count);
        finish_scenario(peer, handle).await?;
    }

    Ok((
        ScenarioReport {
            scenario: profile.scenario(),
            extension_count: profile.extension_count(),
            terminal_input_mode: mode.label(),
            requests_per_sample,
            normalized_samples_ms,
            timeout_samples_ms: Vec::new(),
            locality_samples_ms: Vec::new(),
        },
        observed_ack.ok_or_else(|| io_error("no serve_io rounds were measured"))?,
    ))
}

async fn measure_slow_scenario(corpus: &mut CorpusHasher) -> Result<(ScenarioReport, HelloAck)> {
    let mut normalized_samples_ms = Vec::with_capacity(MEASURED_ROUNDS);
    let mut timeout_samples_ms = Vec::with_capacity(MEASURED_ROUNDS);
    let mut locality_samples_ms = Vec::with_capacity(MEASURED_ROUNDS);
    let mut observed_ack = None;

    for round in 0..MEASURED_ROUNDS {
        let (mut peer, handle, ack) =
            open_scenario(LoadProfile::Slow, TerminalInputMode::SlowThenFast).await?;
        if observed_ack
            .as_ref()
            .is_some_and(|observed| observed != &ack)
        {
            return Err(io_error("serve_io hello acknowledgment changed between rounds").into());
        }
        observed_ack = Some(ack);

        let timeout_started = Instant::now();
        let timed_out = recorded_terminal_input(
            &mut peer,
            corpus,
            LoadProfile::Slow,
            TerminalInputMode::SlowThenFast,
            round,
            "timeout",
            10,
            "q",
        )
        .await?;
        timeout_samples_ms.push(timeout_started.elapsed().as_secs_f64() * 1_000.0);
        expect_frame(
            &timed_out,
            10,
            FrameKind::Error,
            Method::TerminalInput.as_str(),
        )?;
        let timeout = from_payload::<ErrorPayload>(&timed_out.payload)?;
        if timeout.code != "timeout" || timeout.retryable {
            return Err(io_error(format!(
                "slow terminalInput returned code={} retryable={}, expected timeout/non-retryable",
                timeout.code, timeout.retryable
            ))
            .into());
        }

        let locality_started = Instant::now();
        let local = recorded_terminal_input(
            &mut peer,
            corpus,
            LoadProfile::Slow,
            TerminalInputMode::SlowThenFast,
            round,
            "locality",
            11,
            "a",
        )
        .await?;
        locality_samples_ms.push(locality_started.elapsed().as_secs_f64() * 1_000.0);
        validate_terminal_result(&local, 11, "a", TerminalInputMode::SlowThenFast)?;

        for index in 0..WARMUPS {
            let data = corpus_data(index);
            let id = request_id(100, index)?;
            let frame = recorded_terminal_input(
                &mut peer,
                corpus,
                LoadProfile::Slow,
                TerminalInputMode::SlowThenFast,
                round,
                "warmup",
                id,
                data,
            )
            .await?;
            validate_terminal_result(&frame, id, data, TerminalInputMode::SlowThenFast)?;
        }

        let started = Instant::now();
        for index in 0..SAMPLES {
            let data = corpus_data(index);
            let id = request_id(200, index)?;
            let frame = recorded_terminal_input(
                &mut peer,
                corpus,
                LoadProfile::Slow,
                TerminalInputMode::SlowThenFast,
                round,
                "measured",
                id,
                data,
            )
            .await?;
            validate_terminal_result(&frame, id, data, TerminalInputMode::SlowThenFast)?;
        }
        normalized_samples_ms
            .push(started.elapsed().as_secs_f64() * 1_000.0 / f64::from(u32::try_from(SAMPLES)?));
        finish_scenario(peer, handle).await?;
    }

    Ok((
        ScenarioReport {
            scenario: LoadProfile::Slow.scenario(),
            extension_count: LoadProfile::Slow.extension_count(),
            terminal_input_mode: TerminalInputMode::SlowThenFast.label(),
            requests_per_sample: SAMPLES,
            normalized_samples_ms,
            timeout_samples_ms,
            locality_samples_ms,
        },
        observed_ack.ok_or_else(|| io_error("no serve_io slow rounds were measured"))?,
    ))
}

async fn sample() -> Result<SamplerReport> {
    let mut corpus = CorpusHasher::seeded();
    let (zero, zero_ack) = measure_regular_scenario(
        LoadProfile::Zero,
        TerminalInputMode::PassThrough,
        SAMPLES,
        &mut corpus,
    )
    .await?;
    let (idle, idle_ack) = measure_regular_scenario(
        LoadProfile::Idle100,
        TerminalInputMode::PassThrough,
        SAMPLES,
        &mut corpus,
    )
    .await?;
    let (active, active_ack) = measure_regular_scenario(
        LoadProfile::Active20,
        TerminalInputMode::PassThrough,
        SAMPLES,
        &mut corpus,
    )
    .await?;
    let (fast, fast_ack) = measure_regular_scenario(
        LoadProfile::Fast,
        TerminalInputMode::Fast,
        FAST_STREAM_SAMPLES,
        &mut corpus,
    )
    .await?;
    let (slow, slow_ack) = measure_slow_scenario(&mut corpus).await?;

    for ack in [&idle_ack, &active_ack, &fast_ack, &slow_ack] {
        if ack != &zero_ack {
            return Err(
                io_error("serve_io scenarios observed different protocol provenance").into(),
            );
        }
    }

    Ok(SamplerReport {
        schema_version: 1,
        provenance: Provenance {
            entrypoint: ENTRYPOINT,
            frame_codec: FRAME_CODEC,
            protocol: ProtocolProvenance {
                compiled_protocol_version: PROTOCOL_VERSION,
                compiled_compatibility_version: COMPATIBILITY_VERSION,
                observed_protocol_version: zero_ack.protocol_version,
                observed_compatibility_version: zero_ack.compatibility_version,
            },
            corpus: CorpusProvenance {
                identity: CORPUS_IDENTITY,
                digest_algorithm: DIGEST_ALGORITHM,
                digest: corpus.digest(),
                measured_rounds: MEASURED_ROUNDS,
                warmups_per_scenario: WARMUPS,
                samples_per_scenario: SAMPLES,
                fast_stream_samples: FAST_STREAM_SAMPLES,
            },
        },
        scenarios: vec![zero, idle, active, fast, slow],
        correctness: Correctness {
            hello_ack_observed: true,
            id_correlation: true,
            deterministic_payloads: true,
            active_widget_keys: 20,
            slow_timeout_code: "timeout",
            slow_timeout_retryable: false,
        },
        pass: true,
        failures: Vec::new(),
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.as_slice() != ["--json"] {
        eprintln!("usage: pi-extension-scaling --json");
        return ExitCode::from(2);
    }

    match sample().await {
        Ok(report) => match serde_json::to_string(&report) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("pi-extension-scaling: failed to serialize report: {error}");
                ExitCode::from(1)
            }
        },
        Err(error) => {
            eprintln!("pi-extension-scaling: {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_names_production_entrypoint() {
        assert_eq!(ENTRYPOINT, "pi_ext::server::serve_io");
        assert_eq!(
            FRAME_CODEC,
            "pi_ext::protocol::{encode_frame,decode_frame_str}"
        );
    }

    #[test]
    fn corpus_field_framing_is_stable() {
        let mut corpus = CorpusHasher::seeded();
        corpus.record_request(
            LoadProfile::Fast,
            TerminalInputMode::Fast,
            2,
            "measured",
            302,
            "b",
        );
        assert_eq!(corpus.digest(), "ba47553fc7e516e9");

        let mut different_framing = CorpusHasher::seeded();
        different_framing.record_request(
            LoadProfile::Fast,
            TerminalInputMode::Fast,
            2,
            "measure",
            302,
            "db",
        );
        assert_ne!(corpus.digest(), different_framing.digest());
        assert_eq!(CORPUS_IDENTITY, "extension-scaling-terminal-input-v1");
    }

    #[test]
    fn scenarios_cover_expected_extension_counts() {
        assert_eq!(LoadProfile::Zero.extension_count(), 0);
        assert_eq!(LoadProfile::Idle100.extension_count(), 100);
        assert_eq!(LoadProfile::Active20.extension_count(), 20);
        assert_eq!(LoadProfile::Fast.extension_count(), 1);
        assert_eq!(LoadProfile::Slow.extension_count(), 2);
    }
}
