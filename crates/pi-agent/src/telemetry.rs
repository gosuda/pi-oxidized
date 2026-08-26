//! Vendor-neutral telemetry contracts and reference implementations.
//!
//! Telemetry is deliberately passive: callers own control flow, span failures
//! are contained, and the default context records nothing.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

/// A serializable telemetry attribute value.
#[derive(Clone, Debug, PartialEq)]
pub enum AttributeValue {
    /// UTF-8 string.
    Str(String),
    /// Signed integer.
    Int(i64),
    /// Floating-point number.
    Float(f64),
    /// Boolean.
    Bool(bool),
    /// String list.
    StrList(Vec<String>),
    /// Integer list.
    IntList(Vec<i64>),
    /// Boolean list.
    BoolList(Vec<bool>),
}

/// Deterministically ordered span attributes.
pub type SpanAttributes = BTreeMap<String, AttributeValue>;

/// Inputs used to start a span.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanOptions {
    /// Stable schema span name.
    pub name: String,
    /// Start attributes.
    pub attributes: SpanAttributes,
}

/// Terminal span status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SpanStatus {
    /// The operation did not report an error.
    #[default]
    Ok,
    /// The operation failed.
    Error {
        /// Low-cardinality error class.
        name: Option<String>,
        /// Human-readable error message.
        message: Option<String>,
    },
}

/// Explicit parent capable of starting telemetry spans.
pub trait TelemetryContext: Send + Sync {
    /// Starts a child span. Implementations must not influence agent outcomes.
    fn start_span(&self, options: SpanOptions) -> Box<dyn TelemetrySpan>;
}

/// Mutable span which also serves as a child-span parent.
pub trait TelemetrySpan: TelemetryContext {
    /// Records a named event.
    fn add_event(&self, name: &str, attributes: SpanAttributes);
    /// Merges terminal attributes.
    fn set_attributes(&self, attributes: SpanAttributes);
    /// Replaces the terminal status.
    fn set_status(&self, status: SpanStatus);
}

#[derive(Debug)]
struct NoopSpan;

impl TelemetryContext for NoopSpan {
    fn start_span(&self, _options: SpanOptions) -> Box<dyn TelemetrySpan> {
        Box::new(Self)
    }
}

impl TelemetrySpan for NoopSpan {
    fn add_event(&self, _name: &str, _attributes: SpanAttributes) {}

    fn set_attributes(&self, _attributes: SpanAttributes) {}

    fn set_status(&self, _status: SpanStatus) {}
}

static NOOP: LazyLock<Arc<dyn TelemetryContext>> =
    LazyLock::new(|| Arc::new(NoopSpan) as Arc<dyn TelemetryContext>);

/// Returns the process-wide shared no-op context.
#[must_use]
pub fn noop_context() -> Arc<dyn TelemetryContext> {
    Arc::clone(&NOOP)
}

struct ContainedSpan {
    inner: Option<Box<dyn TelemetrySpan>>,
}

impl ContainedSpan {
    fn new(inner: Box<dyn TelemetrySpan>) -> Self {
        Self { inner: Some(inner) }
    }

    fn inner(&self) -> &dyn TelemetrySpan {
        self.inner.as_deref().unwrap_or(&NoopSpan)
    }
}

impl TelemetryContext for ContainedSpan {
    fn start_span(&self, options: SpanOptions) -> Box<dyn TelemetrySpan> {
        start_span_contained(self.inner(), options)
    }
}

impl TelemetrySpan for ContainedSpan {
    fn add_event(&self, name: &str, attributes: SpanAttributes) {
        contained(|| self.inner().add_event(name, attributes), || ());
    }

    fn set_attributes(&self, attributes: SpanAttributes) {
        contained(|| self.inner().set_attributes(attributes), || ());
    }

    fn set_status(&self, status: SpanStatus) {
        contained(|| self.inner().set_status(status), || ());
    }
}

impl Drop for ContainedSpan {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            let _ = catch_unwind(AssertUnwindSafe(|| drop(inner)));
        }
    }
}

/// Executes passive telemetry code behind a panic boundary.
///
/// The fallback must itself be infallible. Agent behavior must never depend on
/// whether the telemetry implementation succeeds.
pub fn contained<R>(action: impl FnOnce() -> R, fallback: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(action)).unwrap_or_else(|_| fallback())
}

/// Starts a span and degrades a panicking context to a no-op span.
#[must_use]
pub fn start_span_contained<T: TelemetryContext + ?Sized>(
    parent: &T,
    options: SpanOptions,
) -> Box<dyn TelemetrySpan> {
    contained(
        || Box::new(ContainedSpan::new(parent.start_span(options))) as Box<dyn TelemetrySpan>,
        || Box::new(NoopSpan) as Box<dyn TelemetrySpan>,
    )
}

/// Merges span attributes while containing implementation panics.
pub fn set_attributes_contained(span: &dyn TelemetrySpan, attributes: SpanAttributes) {
    contained(|| span.set_attributes(attributes), || ());
}

/// Sets span status while containing implementation panics.
pub fn set_status_contained(span: &dyn TelemetrySpan, status: SpanStatus) {
    contained(|| span.set_status(status), || ());
}

/// Records an event while containing implementation panics.
pub fn add_event_contained(span: &dyn TelemetrySpan, name: &str, attributes: SpanAttributes) {
    contained(|| span.add_event(name, attributes), || ());
}

/// Detached in-memory event snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedEvent {
    /// Event name.
    pub name: String,
    /// Event attributes.
    pub attributes: SpanAttributes,
}

/// Detached in-memory span snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedSpan {
    /// Monotonic span id.
    pub id: u64,
    /// Parent id, when the parent is another recorded span.
    pub parent_id: Option<u64>,
    /// Schema span name.
    pub name: String,
    /// Start and terminal attributes.
    pub attributes: SpanAttributes,
    /// Recorded events.
    pub events: Vec<RecordedEvent>,
    /// Current status.
    pub status: SpanStatus,
    /// Whether the span settled.
    pub settled: bool,
    /// Monotonic settlement sequence.
    pub end_sequence: Option<u64>,
}

#[derive(Default)]
struct InMemoryState {
    spans: Vec<RecordedSpan>,
    next_id: u64,
    next_end_sequence: u64,
}

/// Backend-neutral context that records spans in process memory.
#[derive(Clone, Default)]
pub struct InMemoryTelemetryContext {
    state: Arc<Mutex<InMemoryState>>,
}

impl InMemoryTelemetryContext {
    /// Creates an isolated recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns detached snapshots in span-start order.
    #[must_use]
    pub fn spans(&self) -> Vec<RecordedSpan> {
        self.state
            .lock()
            .map_or_else(|_| Vec::new(), |state| state.spans.clone())
    }

    fn start_recorded_span(
        &self,
        options: SpanOptions,
        parent: Option<(u64, Arc<AtomicBool>)>,
    ) -> Option<InMemorySpan> {
        if parent
            .as_ref()
            .is_some_and(|(_, settled)| settled.load(Ordering::Acquire))
        {
            return None;
        }
        let mut state = self.state.lock().ok()?;
        if parent
            .as_ref()
            .is_some_and(|(_, settled)| settled.load(Ordering::Acquire))
        {
            return None;
        }
        state.next_id = state.next_id.saturating_add(1);
        let id = state.next_id;
        let index = state.spans.len();
        state.spans.push(RecordedSpan {
            id,
            parent_id: parent.as_ref().map(|(parent_id, _)| *parent_id),
            name: options.name,
            attributes: options.attributes,
            events: Vec::new(),
            status: SpanStatus::Ok,
            settled: false,
            end_sequence: None,
        });
        Some(InMemorySpan {
            context: self.clone(),
            index,
            id,
            settled: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl TelemetryContext for InMemoryTelemetryContext {
    fn start_span(&self, options: SpanOptions) -> Box<dyn TelemetrySpan> {
        contained(
            || {
                self.start_recorded_span(options, None).map_or_else(
                    || Box::new(NoopSpan) as Box<dyn TelemetrySpan>,
                    |span| Box::new(span),
                )
            },
            || Box::new(NoopSpan) as Box<dyn TelemetrySpan>,
        )
    }
}

struct InMemorySpan {
    context: InMemoryTelemetryContext,
    index: usize,
    id: u64,
    settled: Arc<AtomicBool>,
}

impl InMemorySpan {
    fn mutate(&self, action: impl FnOnce(&mut RecordedSpan)) {
        if self.settled.load(Ordering::Acquire) {
            return;
        }
        let _ = catch_unwind(AssertUnwindSafe(|| {
            if let Ok(mut state) = self.context.state.lock()
                && let Some(span) = state.spans.get_mut(self.index)
                && !span.settled
                && !self.settled.load(Ordering::Acquire)
            {
                action(span);
            }
        }));
    }

    fn settle(&self) {
        if self
            .settled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let _ = catch_unwind(AssertUnwindSafe(|| {
            if let Ok(mut state) = self.context.state.lock() {
                state.next_end_sequence = state.next_end_sequence.saturating_add(1);
                let end_sequence = state.next_end_sequence;
                if let Some(span) = state.spans.get_mut(self.index) {
                    span.settled = true;
                    span.end_sequence = Some(end_sequence);
                }
            }
        }));
    }
}

impl TelemetryContext for InMemorySpan {
    fn start_span(&self, options: SpanOptions) -> Box<dyn TelemetrySpan> {
        contained(
            || {
                self.context
                    .start_recorded_span(options, Some((self.id, Arc::clone(&self.settled))))
                    .map_or_else(
                        || Box::new(NoopSpan) as Box<dyn TelemetrySpan>,
                        |span| Box::new(span),
                    )
            },
            || Box::new(NoopSpan) as Box<dyn TelemetrySpan>,
        )
    }
}

impl TelemetrySpan for InMemorySpan {
    fn add_event(&self, name: &str, attributes: SpanAttributes) {
        self.mutate(|span| {
            span.events.push(RecordedEvent {
                name: name.to_owned(),
                attributes,
            });
        });
    }

    fn set_attributes(&self, attributes: SpanAttributes) {
        self.mutate(|span| span.attributes.extend(attributes));
    }

    fn set_status(&self, status: SpanStatus) {
        self.mutate(|span| span.status = status);
    }
}

impl Drop for InMemorySpan {
    fn drop(&mut self) {
        self.settle();
    }
}

/// Attribute wire type used by schema tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrType {
    /// String.
    Str,
    /// Number.
    Number,
    /// Boolean.
    Bool,
    /// String list.
    StrList,
    /// Number list.
    NumberList,
    /// Boolean list.
    BoolList,
}

/// Expected attribute cardinality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cardinality {
    /// Bounded vocabulary.
    Low,
    /// Invocation- or user-specific values.
    High,
}

/// One pinned schema attribute definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttrDef {
    /// Wire name.
    pub name: &'static str,
    /// Wire type.
    pub ty: AttrType,
    /// Whether the attribute is required at span start.
    pub required: bool,
    /// Closed string vocabulary, or an empty slice.
    pub values: &'static [&'static str],
    /// Cardinality hint.
    pub cardinality: Option<Cardinality>,
}

/// Allowed parent shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParentKind {
    /// Any explicit context.
    Any,
    /// Root or external context.
    RootOrExternal,
    /// One of the named spans.
    Spans(&'static [&'static str]),
}

/// One pinned span definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanDef {
    /// Stable span name.
    pub name: &'static str,
    /// Allowed parents.
    pub parents: ParentKind,
    /// Start attributes.
    pub start: &'static [AttrDef],
    /// End attributes.
    pub end: &'static [AttrDef],
    /// Whether status defaults to ok.
    pub status_default_ok: bool,
}

/// Versioned telemetry schema.
#[derive(Debug, PartialEq, Eq)]
pub struct TelemetrySchema {
    /// Schema version.
    pub version: u32,
    /// Span definitions.
    pub spans: &'static [SpanDef],
}

const NONE: &[&str] = &[];
const AI_OPERATIONS: &[&str] = &[
    "stream",
    "fetch_deferred",
    "cancel_deferred",
    "generate_images",
];
const STOP_REASONS: &[&str] = &["stop", "length", "tool_use", "error", "aborted", "deferred"];
const RUN_OUTCOMES: &[&str] = &["completed", "aborted", "failed", "suspended"];
const COMPACTION_OUTCOMES: &[&str] = &["completed", "declined", "aborted", "failed"];
const STEP_OUTCOMES: &[&str] = &[
    "succeeded",
    "retry",
    "failed",
    "aborted",
    "deferred",
    "overflow",
];
const HOOK_NAMES: &[&str] = &[
    "before_run",
    "before_resume",
    "before_run_end",
    "transform_context",
    "before_request",
    "before_payload",
    "after_response",
    "before_tool",
    "after_tool",
    "before_compaction",
    "before_navigation",
];
const EVENT_TYPES: &[&str] = &[
    "run_start",
    "run_resume",
    "run_suspend",
    "run_abort",
    "run_end",
    "fault",
    "handler_error",
    "turn_start",
    "turn_end",
    "retry_scheduled",
    "retry_start",
    "retry_end",
    "message_start",
    "message_update",
    "message_end",
    "tool_start",
    "tool_update",
    "tool_end",
    "entry_added",
    "write_pending",
    "queue_update",
    "fact_update",
    "config_update",
    "compaction_start",
    "compaction_end",
    "navigation_start",
    "navigation_end",
    "lane_created",
    "usage",
];

const fn attr(
    name: &'static str,
    ty: AttrType,
    required: bool,
    values: &'static [&'static str],
    cardinality: Option<Cardinality>,
) -> AttrDef {
    AttrDef {
        name,
        ty,
        required,
        values,
        cardinality,
    }
}

const AI_REQUEST_START: &[AttrDef] = &[
    attr("pi.ai.operation", AttrType::Str, true, AI_OPERATIONS, None),
    attr("pi.ai.provider", AttrType::Str, true, NONE, None),
    attr("pi.ai.model", AttrType::Str, true, NONE, None),
    attr("pi.ai.api", AttrType::Str, true, NONE, None),
    attr("pi.ai.streaming", AttrType::Bool, true, NONE, None),
    attr("pi.ai.deferred", AttrType::Bool, false, NONE, None),
];
const AI_REQUEST_END: &[AttrDef] = &[
    attr("pi.ai.response.model", AttrType::Str, false, NONE, None),
    attr(
        "pi.ai.response.id",
        AttrType::Str,
        false,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.ai.response.stop_reason",
        AttrType::Str,
        false,
        STOP_REASONS,
        None,
    ),
    attr(
        "pi.ai.http.status_code",
        AttrType::Number,
        false,
        NONE,
        None,
    ),
    attr(
        "pi.ai.usage.input_tokens",
        AttrType::Number,
        false,
        NONE,
        None,
    ),
    attr(
        "pi.ai.usage.output_tokens",
        AttrType::Number,
        false,
        NONE,
        None,
    ),
    attr(
        "pi.ai.usage.cache_read_tokens",
        AttrType::Number,
        false,
        NONE,
        None,
    ),
    attr(
        "pi.ai.usage.cache_write_tokens",
        AttrType::Number,
        false,
        NONE,
        None,
    ),
    attr(
        "pi.ai.usage.reasoning_tokens",
        AttrType::Number,
        false,
        NONE,
        None,
    ),
    attr(
        "pi.ai.usage.total_tokens",
        AttrType::Number,
        false,
        NONE,
        None,
    ),
    attr("pi.ai.usage.cost", AttrType::Number, false, NONE, None),
    attr(
        "pi.ai.stream.chunk_count",
        AttrType::Number,
        false,
        NONE,
        None,
    ),
    attr(
        "pi.ai.stream.time_to_first_chunk_ms",
        AttrType::Number,
        false,
        NONE,
        None,
    ),
    attr(
        "pi.ai.error.type",
        AttrType::Str,
        false,
        NONE,
        Some(Cardinality::Low),
    ),
];
const AI_SPANS: &[SpanDef] = &[SpanDef {
    name: "pi.ai.request",
    parents: ParentKind::Any,
    start: AI_REQUEST_START,
    end: AI_REQUEST_END,
    status_default_ok: true,
}];

/// Pinned AI request telemetry schema.
pub static AI_TELEMETRY_SCHEMA: TelemetrySchema = TelemetrySchema {
    version: 1,
    spans: AI_SPANS,
};

const OP_RUN_START: &[AttrDef] = &[
    attr(
        "pi.session.id",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.lane.name",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.operation.id",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr("pi.operation.recovery", AttrType::Bool, true, NONE, None),
    attr("pi.operation.kind", AttrType::Str, true, &["run"], None),
];
const OP_COMPACTION_START: &[AttrDef] = &[
    attr(
        "pi.session.id",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.lane.name",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.operation.id",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr("pi.operation.recovery", AttrType::Bool, true, NONE, None),
    attr(
        "pi.operation.kind",
        AttrType::Str,
        true,
        &["compaction"],
        None,
    ),
];
const OP_NAVIGATION_START: &[AttrDef] = &[
    attr(
        "pi.session.id",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.lane.name",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.operation.id",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr("pi.operation.recovery", AttrType::Bool, true, NONE, None),
    attr(
        "pi.operation.kind",
        AttrType::Str,
        true,
        &["navigation"],
        None,
    ),
];
const RUN_END: &[AttrDef] = &[
    attr(
        "pi.operation.outcome",
        AttrType::Str,
        false,
        RUN_OUTCOMES,
        None,
    ),
    attr(
        "pi.error.code",
        AttrType::Str,
        false,
        NONE,
        Some(Cardinality::Low),
    ),
    attr(
        "pi.error.type",
        AttrType::Str,
        false,
        NONE,
        Some(Cardinality::Low),
    ),
];
const COMPACTION_END: &[AttrDef] = &[
    attr(
        "pi.operation.outcome",
        AttrType::Str,
        false,
        COMPACTION_OUTCOMES,
        None,
    ),
    attr(
        "pi.error.code",
        AttrType::Str,
        false,
        NONE,
        Some(Cardinality::Low),
    ),
    attr(
        "pi.error.type",
        AttrType::Str,
        false,
        NONE,
        Some(Cardinality::Low),
    ),
];
const CHECKPOINT_START: &[AttrDef] = &[
    attr(
        "pi.lane.name",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.operation.id",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.checkpoint.kind",
        AttrType::Str,
        true,
        &["normal", "failure_drain", "abort_reconcile"],
        None,
    ),
];
const TURN_START: &[AttrDef] = &[
    attr(
        "pi.lane.name",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.operation.id",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.turn.id",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
];
const STEP_START: &[AttrDef] = &[
    attr(
        "pi.lane.name",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.operation.id",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.step.kind",
        AttrType::Str,
        true,
        &["assistant", "compaction", "branch_summary"],
        None,
    ),
    attr("pi.step.attempt", AttrType::Number, true, NONE, None),
    attr(
        "pi.compaction.reason",
        AttrType::Str,
        false,
        &["manual", "threshold", "overflow"],
        None,
    ),
];
const STEP_END: &[AttrDef] = &[attr(
    "pi.step.outcome",
    AttrType::Str,
    false,
    STEP_OUTCOMES,
    None,
)];
const TOOL_START: &[AttrDef] = &[
    attr(
        "pi.lane.name",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.operation.id",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.turn.id",
        AttrType::Str,
        false,
        NONE,
        Some(Cardinality::High),
    ),
    attr("pi.tool.name", AttrType::Str, true, NONE, None),
    attr(
        "pi.tool.call_id",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.tool.replay",
        AttrType::Str,
        true,
        &["never", "safe"],
        None,
    ),
    attr("pi.tool.recovery", AttrType::Bool, true, NONE, None),
];
const TOOL_END: &[AttrDef] = &[attr("pi.tool.is_error", AttrType::Bool, false, NONE, None)];
const HOOK_START: &[AttrDef] = &[
    attr(
        "pi.lane.name",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.operation.id",
        AttrType::Str,
        false,
        NONE,
        Some(Cardinality::High),
    ),
    attr("pi.hook.name", AttrType::Str, true, HOOK_NAMES, None),
    attr("pi.hook.registration_id", AttrType::Str, false, NONE, None),
];
const HOOK_END: &[AttrDef] = &[attr(
    "pi.hook.outcome",
    AttrType::Str,
    false,
    &["completed", "skipped", "blocked", "failed"],
    None,
)];
const SLEEP_START: &[AttrDef] = &[
    attr(
        "pi.operation.id",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr("pi.sleep.delay_ms", AttrType::Number, true, NONE, None),
];
const SLEEP_END: &[AttrDef] = &[attr(
    "pi.sleep.outcome",
    AttrType::Str,
    false,
    &["elapsed", "aborted"],
    None,
)];
const EVENT_HANDLER_START: &[AttrDef] = &[
    attr(
        "pi.event.type",
        AttrType::Str,
        true,
        EVENT_TYPES,
        Some(Cardinality::Low),
    ),
    attr(
        "pi.lane.name",
        AttrType::Str,
        false,
        NONE,
        Some(Cardinality::High),
    ),
];
const SESSION_WRITE_START: &[AttrDef] = &[
    attr(
        "pi.lane.name",
        AttrType::Str,
        true,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.operation.id",
        AttrType::Str,
        false,
        NONE,
        Some(Cardinality::High),
    ),
    attr(
        "pi.session.mutation",
        AttrType::Str,
        true,
        &["entry", "record", "lane", "fact"],
        None,
    ),
    attr("pi.session.item_type", AttrType::Str, false, NONE, None),
];
const SESSION_WRITE_END: &[AttrDef] =
    &[attr("pi.session.seq", AttrType::Number, false, NONE, None)];
const EMPTY: &[AttrDef] = &[];

const HARNESS_SPANS: &[SpanDef] = &[
    SpanDef {
        name: "pi.harness.run",
        parents: ParentKind::RootOrExternal,
        start: OP_RUN_START,
        end: RUN_END,
        status_default_ok: true,
    },
    SpanDef {
        name: "pi.harness.compaction",
        parents: ParentKind::RootOrExternal,
        start: OP_COMPACTION_START,
        end: COMPACTION_END,
        status_default_ok: true,
    },
    SpanDef {
        name: "pi.harness.navigation",
        parents: ParentKind::RootOrExternal,
        start: OP_NAVIGATION_START,
        end: COMPACTION_END,
        status_default_ok: true,
    },
    SpanDef {
        name: "pi.harness.checkpoint",
        parents: ParentKind::Spans(&["pi.harness.run"]),
        start: CHECKPOINT_START,
        end: EMPTY,
        status_default_ok: true,
    },
    SpanDef {
        name: "pi.harness.turn",
        parents: ParentKind::Spans(&["pi.harness.run"]),
        start: TURN_START,
        end: EMPTY,
        status_default_ok: true,
    },
    SpanDef {
        name: "pi.harness.step",
        parents: ParentKind::Spans(&[
            "pi.harness.turn",
            "pi.harness.checkpoint",
            "pi.harness.compaction",
            "pi.harness.navigation",
        ]),
        start: STEP_START,
        end: STEP_END,
        status_default_ok: true,
    },
    SpanDef {
        name: "pi.harness.tool",
        parents: ParentKind::Spans(&["pi.harness.turn", "pi.harness.run"]),
        start: TOOL_START,
        end: TOOL_END,
        status_default_ok: true,
    },
    SpanDef {
        name: "pi.harness.hook",
        parents: ParentKind::Any,
        start: HOOK_START,
        end: HOOK_END,
        status_default_ok: true,
    },
    SpanDef {
        name: "pi.harness.sleep",
        parents: ParentKind::Spans(&["pi.harness.step", "pi.harness.run"]),
        start: SLEEP_START,
        end: SLEEP_END,
        status_default_ok: true,
    },
    SpanDef {
        name: "pi.harness.event_handler",
        parents: ParentKind::Any,
        start: EVENT_HANDLER_START,
        end: EMPTY,
        status_default_ok: true,
    },
    SpanDef {
        name: "pi.session.write",
        parents: ParentKind::Any,
        start: SESSION_WRITE_START,
        end: SESSION_WRITE_END,
        status_default_ok: true,
    },
];

/// Pinned harness telemetry schema.
pub static HARNESS_TELEMETRY_SCHEMA: TelemetrySchema = TelemetrySchema {
    version: 1,
    spans: HARNESS_SPANS,
};

/// Combined agent-owned schema vocabulary.
pub static AGENT_TELEMETRY_SCHEMAS: &[&TelemetrySchema] =
    &[&AI_TELEMETRY_SCHEMA, &HARNESS_TELEMETRY_SCHEMA];

/// AI provider operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AiOperation {
    /// Streaming generation.
    Stream,
    /// Deferred-result fetch.
    FetchDeferred,
    /// Deferred-result cancellation.
    CancelDeferred,
    /// Image generation.
    GenerateImages,
}

impl AiOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::FetchDeferred => "fetch_deferred",
            Self::CancelDeferred => "cancel_deferred",
            Self::GenerateImages => "generate_images",
        }
    }
}

/// Required AI request start attributes.
pub struct AiRequestStart {
    /// Provider operation.
    pub operation: AiOperation,
    /// Provider id.
    pub provider: String,
    /// Requested model id.
    pub model: String,
    /// Provider API id.
    pub api: String,
    /// Whether the operation streams.
    pub streaming: bool,
    /// Deferred execution marker.
    pub deferred: Option<bool>,
}

/// Required run start attributes.
pub struct HarnessRunStart {
    /// Session id.
    pub session_id: String,
    /// Lane name.
    pub lane_name: String,
    /// Operation id.
    pub operation_id: String,
    /// Recovery invocation marker.
    pub recovery: bool,
}

/// Required turn start attributes.
pub struct HarnessTurnStart {
    /// Lane name.
    pub lane_name: String,
    /// Operation id.
    pub operation_id: String,
    /// Invocation-local turn id.
    pub turn_id: String,
}

/// Tool replay policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolReplay {
    /// Never replay.
    Never,
    /// Safe to replay.
    Safe,
}

impl ToolReplay {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Safe => "safe",
        }
    }
}

/// Required tool start attributes.
pub struct HarnessToolStart {
    /// Lane name.
    pub lane_name: String,
    /// Operation id.
    pub operation_id: String,
    /// Optional live turn id.
    pub turn_id: Option<String>,
    /// Tool name.
    pub tool_name: String,
    /// Provider tool-call id.
    pub call_id: String,
    /// Replay policy.
    pub replay: ToolReplay,
    /// Recovery execution marker.
    pub recovery: bool,
}

/// Required compaction start attributes.
pub struct HarnessCompactionStart {
    /// Session id.
    pub session_id: String,
    /// Lane name.
    pub lane_name: String,
    /// Operation id.
    pub operation_id: String,
    /// Recovery invocation marker.
    pub recovery: bool,
}

/// Retryable step kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// Assistant generation.
    Assistant,
    /// Context compaction.
    Compaction,
    /// Branch summary.
    BranchSummary,
}

impl StepKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Assistant => "assistant",
            Self::Compaction => "compaction",
            Self::BranchSummary => "branch_summary",
        }
    }
}

/// Compaction trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionReason {
    /// User request.
    Manual,
    /// Token threshold.
    Threshold,
    /// Context overflow.
    Overflow,
}

impl CompactionReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Threshold => "threshold",
            Self::Overflow => "overflow",
        }
    }
}

/// Required compaction-step start attributes.
pub struct HarnessStepStart {
    /// Lane name.
    pub lane_name: String,
    /// Operation id.
    pub operation_id: String,
    /// Step kind.
    pub kind: StepKind,
    /// One-based attempt.
    pub attempt: u32,
    /// Optional compaction trigger.
    pub compaction_reason: Option<CompactionReason>,
}

fn insert(attrs: &mut SpanAttributes, name: &str, value: impl Into<AttributeValue>) {
    attrs.insert(name.to_owned(), value.into());
}

impl From<String> for AttributeValue {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}
impl From<&str> for AttributeValue {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}
impl From<bool> for AttributeValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}
impl From<i64> for AttributeValue {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}
impl From<f64> for AttributeValue {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

/// Losslessly converts a `u64` when possible, saturating only schema transport.
#[must_use]
pub fn number(value: u64) -> AttributeValue {
    AttributeValue::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

/// Starts a typed AI request span.
#[must_use]
pub fn start_ai_request_span<T: TelemetryContext + ?Sized>(
    parent: &T,
    start: AiRequestStart,
) -> Box<dyn TelemetrySpan> {
    let mut attributes = SpanAttributes::new();
    insert(&mut attributes, "pi.ai.operation", start.operation.as_str());
    insert(&mut attributes, "pi.ai.provider", start.provider);
    insert(&mut attributes, "pi.ai.model", start.model);
    insert(&mut attributes, "pi.ai.api", start.api);
    insert(&mut attributes, "pi.ai.streaming", start.streaming);
    if let Some(deferred) = start.deferred {
        insert(&mut attributes, "pi.ai.deferred", deferred);
    }
    start_span_contained(
        parent,
        SpanOptions {
            name: "pi.ai.request".to_owned(),
            attributes,
        },
    )
}

/// Starts a typed harness run span.
#[must_use]
pub fn start_harness_run_span<T: TelemetryContext + ?Sized>(
    parent: &T,
    start: HarnessRunStart,
) -> Box<dyn TelemetrySpan> {
    let mut attributes = operation_attributes(
        start.session_id,
        start.lane_name,
        start.operation_id,
        start.recovery,
    );
    insert(&mut attributes, "pi.operation.kind", "run");
    start_span_contained(
        parent,
        SpanOptions {
            name: "pi.harness.run".to_owned(),
            attributes,
        },
    )
}

/// Starts a typed harness turn span.
#[must_use]
pub fn start_harness_turn_span<T: TelemetryContext + ?Sized>(
    parent: &T,
    start: HarnessTurnStart,
) -> Box<dyn TelemetrySpan> {
    let mut attributes = SpanAttributes::new();
    insert(&mut attributes, "pi.lane.name", start.lane_name);
    insert(&mut attributes, "pi.operation.id", start.operation_id);
    insert(&mut attributes, "pi.turn.id", start.turn_id);
    start_span_contained(
        parent,
        SpanOptions {
            name: "pi.harness.turn".to_owned(),
            attributes,
        },
    )
}

/// Starts a typed harness tool span.
#[must_use]
pub fn start_harness_tool_span<T: TelemetryContext + ?Sized>(
    parent: &T,
    start: HarnessToolStart,
) -> Box<dyn TelemetrySpan> {
    let mut attributes = SpanAttributes::new();
    insert(&mut attributes, "pi.lane.name", start.lane_name);
    insert(&mut attributes, "pi.operation.id", start.operation_id);
    if let Some(turn_id) = start.turn_id {
        insert(&mut attributes, "pi.turn.id", turn_id);
    }
    insert(&mut attributes, "pi.tool.name", start.tool_name);
    insert(&mut attributes, "pi.tool.call_id", start.call_id);
    insert(&mut attributes, "pi.tool.replay", start.replay.as_str());
    insert(&mut attributes, "pi.tool.recovery", start.recovery);
    start_span_contained(
        parent,
        SpanOptions {
            name: "pi.harness.tool".to_owned(),
            attributes,
        },
    )
}

/// Starts a typed harness compaction span.
#[must_use]
pub fn start_harness_compaction_span<T: TelemetryContext + ?Sized>(
    parent: &T,
    start: HarnessCompactionStart,
) -> Box<dyn TelemetrySpan> {
    let mut attributes = operation_attributes(
        start.session_id,
        start.lane_name,
        start.operation_id,
        start.recovery,
    );
    insert(&mut attributes, "pi.operation.kind", "compaction");
    start_span_contained(
        parent,
        SpanOptions {
            name: "pi.harness.compaction".to_owned(),
            attributes,
        },
    )
}

/// Starts a typed harness retry-step span.
#[must_use]
pub fn start_harness_step_span<T: TelemetryContext + ?Sized>(
    parent: &T,
    start: HarnessStepStart,
) -> Box<dyn TelemetrySpan> {
    let mut attributes = SpanAttributes::new();
    insert(&mut attributes, "pi.lane.name", start.lane_name);
    insert(&mut attributes, "pi.operation.id", start.operation_id);
    insert(&mut attributes, "pi.step.kind", start.kind.as_str());
    insert(&mut attributes, "pi.step.attempt", i64::from(start.attempt));
    if let Some(reason) = start.compaction_reason {
        insert(&mut attributes, "pi.compaction.reason", reason.as_str());
    }
    start_span_contained(
        parent,
        SpanOptions {
            name: "pi.harness.step".to_owned(),
            attributes,
        },
    )
}

fn operation_attributes(
    session_id: String,
    lane_name: String,
    operation_id: String,
    recovery: bool,
) -> SpanAttributes {
    let mut attributes = SpanAttributes::new();
    insert(&mut attributes, "pi.session.id", session_id);
    insert(&mut attributes, "pi.lane.name", lane_name);
    insert(&mut attributes, "pi.operation.id", operation_id);
    insert(&mut attributes, "pi.operation.recovery", recovery);
    attributes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_context_is_shared_and_inert() {
        let left = noop_context();
        let right = noop_context();
        assert!(Arc::ptr_eq(&left, &right));
        let span = start_span_contained(
            left.as_ref(),
            SpanOptions {
                name: "test".to_owned(),
                attributes: SpanAttributes::new(),
            },
        );
        set_attributes_contained(
            span.as_ref(),
            BTreeMap::from([("x".to_owned(), 1_i64.into())]),
        );
        set_status_contained(
            span.as_ref(),
            SpanStatus::Error {
                name: None,
                message: None,
            },
        );
    }

    #[test]
    fn in_memory_records_nesting_and_settles_once() {
        let context = InMemoryTelemetryContext::new();
        let parent = context
            .start_recorded_span(
                SpanOptions {
                    name: "parent".to_owned(),
                    attributes: SpanAttributes::new(),
                },
                None,
            )
            .expect("parent");
        let child = parent.start_span(SpanOptions {
            name: "child".to_owned(),
            attributes: SpanAttributes::new(),
        });
        drop(child);
        parent.settle();
        parent.settle();
        let spans = context.spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].parent_id, Some(spans[0].id));
        assert_eq!(spans[1].end_sequence, Some(1));
        assert_eq!(spans[0].end_sequence, Some(2));
    }

    #[test]
    fn settled_span_rejects_mutations_and_children() {
        let context = InMemoryTelemetryContext::new();
        let span = context
            .start_recorded_span(
                SpanOptions {
                    name: "parent".to_owned(),
                    attributes: SpanAttributes::new(),
                },
                None,
            )
            .expect("span");
        span.settle();
        span.set_attributes(BTreeMap::from([("late".to_owned(), true.into())]));
        let child = span.start_span(SpanOptions {
            name: "child".to_owned(),
            attributes: SpanAttributes::new(),
        });
        drop(child);
        let spans = context.spans();
        assert_eq!(spans.len(), 1);
        assert!(!spans[0].attributes.contains_key("late"));
    }

    struct PanickingContext;
    impl TelemetryContext for PanickingContext {
        fn start_span(&self, _options: SpanOptions) -> Box<dyn TelemetrySpan> {
            panic!("telemetry panic")
        }
    }

    struct DropPanickingSpan;

    impl TelemetryContext for DropPanickingSpan {
        fn start_span(&self, _options: SpanOptions) -> Box<dyn TelemetrySpan> {
            Box::new(NoopSpan)
        }
    }

    impl TelemetrySpan for DropPanickingSpan {
        fn add_event(&self, _name: &str, _attributes: SpanAttributes) {
            panic!("event panic")
        }

        fn set_attributes(&self, _attributes: SpanAttributes) {
            panic!("attribute panic")
        }

        fn set_status(&self, _status: SpanStatus) {
            panic!("status panic")
        }
    }

    impl Drop for DropPanickingSpan {
        fn drop(&mut self) {
            panic!("drop panic")
        }
    }

    struct DropPanickingContext;

    impl TelemetryContext for DropPanickingContext {
        fn start_span(&self, _options: SpanOptions) -> Box<dyn TelemetrySpan> {
            Box::new(DropPanickingSpan)
        }
    }

    #[test]
    fn hostile_span_methods_and_drop_are_contained() {
        let span = start_span_contained(
            &DropPanickingContext,
            SpanOptions {
                name: "test".to_owned(),
                attributes: SpanAttributes::new(),
            },
        );
        set_attributes_contained(span.as_ref(), SpanAttributes::new());
        set_status_contained(span.as_ref(), SpanStatus::Ok);
        add_event_contained(span.as_ref(), "event", SpanAttributes::new());
        drop(span);
    }
    #[test]
    fn panicking_context_degrades_to_noop() {
        let span = start_span_contained(
            &PanickingContext,
            SpanOptions {
                name: "test".to_owned(),
                attributes: SpanAttributes::new(),
            },
        );
        set_attributes_contained(span.as_ref(), SpanAttributes::new());
    }

    #[test]
    fn schemas_pin_ai_and_all_harness_spans() {
        assert_eq!(AI_TELEMETRY_SCHEMA.version, 1);
        assert_eq!(AI_TELEMETRY_SCHEMA.spans[0].name, "pi.ai.request");
        assert_eq!(HARNESS_TELEMETRY_SCHEMA.version, 1);
        assert_eq!(HARNESS_TELEMETRY_SCHEMA.spans.len(), 11);
        let names: Vec<_> = HARNESS_TELEMETRY_SCHEMA
            .spans
            .iter()
            .map(|span| span.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "pi.harness.run",
                "pi.harness.compaction",
                "pi.harness.navigation",
                "pi.harness.checkpoint",
                "pi.harness.turn",
                "pi.harness.step",
                "pi.harness.tool",
                "pi.harness.hook",
                "pi.harness.sleep",
                "pi.harness.event_handler",
                "pi.session.write",
            ]
        );
    }

    #[test]
    fn schemas_pin_attribute_names_and_value_enums() {
        let ai = &AI_TELEMETRY_SCHEMA.spans[0];
        assert_eq!(
            ai.start
                .iter()
                .map(|attribute| attribute.name)
                .collect::<Vec<_>>(),
            vec![
                "pi.ai.operation",
                "pi.ai.provider",
                "pi.ai.model",
                "pi.ai.api",
                "pi.ai.streaming",
                "pi.ai.deferred",
            ]
        );
        assert_eq!(
            ai.end
                .iter()
                .map(|attribute| attribute.name)
                .collect::<Vec<_>>(),
            vec![
                "pi.ai.response.model",
                "pi.ai.response.id",
                "pi.ai.response.stop_reason",
                "pi.ai.http.status_code",
                "pi.ai.usage.input_tokens",
                "pi.ai.usage.output_tokens",
                "pi.ai.usage.cache_read_tokens",
                "pi.ai.usage.cache_write_tokens",
                "pi.ai.usage.reasoning_tokens",
                "pi.ai.usage.total_tokens",
                "pi.ai.usage.cost",
                "pi.ai.stream.chunk_count",
                "pi.ai.stream.time_to_first_chunk_ms",
                "pi.ai.error.type",
            ]
        );
        assert_eq!(ai.start[0].values, AI_OPERATIONS);
        assert_eq!(ai.end[2].values, STOP_REASONS);

        let expected = [
            (
                "pi.harness.run",
                vec![
                    "pi.session.id",
                    "pi.lane.name",
                    "pi.operation.id",
                    "pi.operation.recovery",
                    "pi.operation.kind",
                ],
                vec!["pi.operation.outcome", "pi.error.code", "pi.error.type"],
            ),
            (
                "pi.harness.compaction",
                vec![
                    "pi.session.id",
                    "pi.lane.name",
                    "pi.operation.id",
                    "pi.operation.recovery",
                    "pi.operation.kind",
                ],
                vec!["pi.operation.outcome", "pi.error.code", "pi.error.type"],
            ),
            (
                "pi.harness.navigation",
                vec![
                    "pi.session.id",
                    "pi.lane.name",
                    "pi.operation.id",
                    "pi.operation.recovery",
                    "pi.operation.kind",
                ],
                vec!["pi.operation.outcome", "pi.error.code", "pi.error.type"],
            ),
            (
                "pi.harness.checkpoint",
                vec!["pi.lane.name", "pi.operation.id", "pi.checkpoint.kind"],
                vec![],
            ),
            (
                "pi.harness.turn",
                vec!["pi.lane.name", "pi.operation.id", "pi.turn.id"],
                vec![],
            ),
            (
                "pi.harness.step",
                vec![
                    "pi.lane.name",
                    "pi.operation.id",
                    "pi.step.kind",
                    "pi.step.attempt",
                    "pi.compaction.reason",
                ],
                vec!["pi.step.outcome"],
            ),
            (
                "pi.harness.tool",
                vec![
                    "pi.lane.name",
                    "pi.operation.id",
                    "pi.turn.id",
                    "pi.tool.name",
                    "pi.tool.call_id",
                    "pi.tool.replay",
                    "pi.tool.recovery",
                ],
                vec!["pi.tool.is_error"],
            ),
            (
                "pi.harness.hook",
                vec![
                    "pi.lane.name",
                    "pi.operation.id",
                    "pi.hook.name",
                    "pi.hook.registration_id",
                ],
                vec!["pi.hook.outcome"],
            ),
            (
                "pi.harness.sleep",
                vec!["pi.operation.id", "pi.sleep.delay_ms"],
                vec!["pi.sleep.outcome"],
            ),
            (
                "pi.harness.event_handler",
                vec!["pi.event.type", "pi.lane.name"],
                vec![],
            ),
            (
                "pi.session.write",
                vec![
                    "pi.lane.name",
                    "pi.operation.id",
                    "pi.session.mutation",
                    "pi.session.item_type",
                ],
                vec!["pi.session.seq"],
            ),
        ];
        for (span, (name, start, end)) in HARNESS_TELEMETRY_SCHEMA.spans.iter().zip(expected) {
            assert_eq!(span.name, name);
            assert_eq!(
                span.start
                    .iter()
                    .map(|attribute| attribute.name)
                    .collect::<Vec<_>>(),
                start
            );
            assert_eq!(
                span.end
                    .iter()
                    .map(|attribute| attribute.name)
                    .collect::<Vec<_>>(),
                end
            );
        }
        assert_eq!(
            HARNESS_TELEMETRY_SCHEMA.spans[0].end[0].values,
            RUN_OUTCOMES
        );
        assert_eq!(
            HARNESS_TELEMETRY_SCHEMA.spans[5].start[2].values,
            &["assistant", "compaction", "branch_summary"]
        );
        assert_eq!(
            HARNESS_TELEMETRY_SCHEMA.spans[5].end[0].values,
            STEP_OUTCOMES
        );
        assert_eq!(
            HARNESS_TELEMETRY_SCHEMA.spans[7].start[2].values,
            HOOK_NAMES
        );
        assert_eq!(
            HARNESS_TELEMETRY_SCHEMA.spans[9].start[0].values,
            EVENT_TYPES
        );
    }

    #[test]
    fn typed_starters_emit_pinned_keys() {
        let context = InMemoryTelemetryContext::new();
        let run = start_harness_run_span(
            &context,
            HarnessRunStart {
                session_id: "s".to_owned(),
                lane_name: "main".to_owned(),
                operation_id: "op-1".to_owned(),
                recovery: false,
            },
        );
        let turn = start_harness_turn_span(
            run.as_ref(),
            HarnessTurnStart {
                lane_name: "main".to_owned(),
                operation_id: "op-1".to_owned(),
                turn_id: "turn-1".to_owned(),
            },
        );
        drop(turn);
        drop(run);
        let spans = context.spans();
        assert_eq!(
            spans[0].attributes.get("pi.operation.kind"),
            Some(&AttributeValue::Str("run".to_owned()))
        );
        assert_eq!(
            spans[1].attributes.get("pi.turn.id"),
            Some(&AttributeValue::Str("turn-1".to_owned()))
        );
    }

    #[test]
    fn every_agent_loop_config_literal_carries_telemetry() {
        let sources = [
            include_str!("agent.rs"),
            include_str!("config.rs"),
            include_str!("run.rs"),
            include_str!("schedule.rs"),
            include_str!("../../pi/src/core/agent_session/mod.rs"),
        ];
        let mut literals = Vec::new();
        for source in sources {
            let mut offset = 0;
            while let Some(found) = source[offset..].find("AgentLoopConfig {") {
                let start = offset + found;
                let prefix = &source[..start];
                offset = start + "AgentLoopConfig {".len();
                if prefix.ends_with("struct ")
                    || prefix.ends_with("impl ")
                    || prefix.trim_end().ends_with("->")
                {
                    continue;
                }
                let mut depth = 1_u32;
                let mut end = offset;
                for (index, byte) in source[offset..].bytes().enumerate() {
                    match byte {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = offset + index + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                literals.push(&source[start..end]);
                offset = end;
            }
        }
        assert_eq!(literals.len(), 5);
        for (i, literal) in literals.iter().enumerate() {
            if !literal.contains("telemetry:") {
                panic!("literal {i} missing telemetry:\n{literal}");
            }
        }
    }
}
