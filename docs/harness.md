# Agent harness

Ported from `.references/pi/packages/agent/docs/harness.md` at pin `8fa7eebd`.
The reference is an implementation specification for a durable `AgentHarness`
runtime; that runtime is not ported. This page documents the surface the
`pi-agent` crate ships today, with every Rust symbol below verified against
`crates/pi-agent/src/lib.rs`. Claims are bound to
[evidence/harness.json](evidence/harness.json); the unported harness internals
are listed under "Pending port surface" rather than described as working.

## Crate surface

`pi-agent` owns the agent turn loop, the steering and follow-up queues, tool
scheduling, events, and the telemetry contracts. The public re-export surface
of the crate:

```rust
use pi_agent::{
    Agent, AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentOptions,
    PendingMessageQueue, QueueMode, RunIo, run_agent_loop, run_agent_loop_continue,
};
<!-- doc-c:fence=harness.01 -->
```

Every path in that `use` list is a public re-export declared in
`crates/pi-agent/src/lib.rs`. The crate also re-exports `pi_ai`, the provider
crate the agent contracts build on.

## Stateful agent

`Agent` wraps the low-level loop with one-active-run semantics. It owns the
transcript, queues, partial-message watch, and idle notification, and
guarantees exactly one `agent_end` event per run:

```rust
use std::sync::Arc;
use pi_agent::{Agent, AgentOptions, AgentMessage, user_text};

// provider: an Arc<dyn pi_ai::Provider> resolved from the model registry.
let mut options = AgentOptions::new(Arc::clone(&provider));
options.system_prompt = "You are a coding agent.".to_owned();
options.messages = vec![user_text("Review the diff in src/", [])];

let agent = Agent::new(options);
let events = agent.subscribe();
agent.prompt(vec![user_text("Start.", [])]).await?;
let transcript: Vec<AgentMessage> = agent.transcript();
<!-- doc-c:fence=harness.02 -->
```

The lifecycle surface: `prompt` starts a run and awaits it, `continue_run`
continues from the transcript tail, `abort` cancels the active run without
clearing queues, `wait_for_idle` blocks until no run is active, and `reset`
aborts, waits, then clears transcript and queues. Steering and follow-up
messages are enqueued through `steer` and `follow_up` into
`PendingMessageQueue`s whose drain mode is read and set with
`steering_mode`/`set_steering_mode` and `follow_up_mode`/`set_follow_up_mode`
(`QueueMode` selects one-at-a-time or all).

## Low-level turn loop

Below the stateful wrapper, `run_agent_loop` runs one prompt turn and
`run_agent_loop_continue` resumes from an existing context. Both are async and
return the messages the invocation produced:

```rust
use pi_agent::{AgentContext, AgentLoopConfig, AgentMessage, RunIo, run_agent_loop};

// io bundles the event sink, the provider reference, and the per-run partial
// assistant watch; cancel is the run's cancellation token.
let new_messages: Vec<AgentMessage> =
    run_agent_loop(prompts, context, config, io, cancel).await?;
<!-- doc-c:fence=harness.03 -->
```

`continue_run` at the `Agent` layer mirrors the reference `continue` contract:
an assistant-tailed transcript drains queued steering and follow-up messages as
a new prompt, while a user or tool-result tail enters the continuation loop
directly.

## Events, tools, and telemetry

`AgentEvent` is the wire enum consumed by the session, UI, and extension layers:
`agent_start`/`agent_end`, `turn_start`/`turn_end`,
`message_start`/`message_update`/`message_end`, and
`tool_execution_start`/`tool_execution_update`/`tool_execution_end`, with field
names matching the TypeScript contract. Subscriptions are bounded:
`Agent::subscribe()` returns an `AgentEventSubscription` and
`subscribe_extension()` an extension-scoped one.

Tools implement the `AgentTool` trait (`name`, `description`, and execution);
`AgentToolResult` carries content plus structured details, and
`to_pi_tool` projects a tool into the provider wire shape. Tool batches run
under `ToolExecutionMode` (sequential, or preflight-sequential then parallel)
with a bounded `MAX_PARALLEL_TOOL_CALLS`.

The crate's telemetry contracts (span schemas, the no-op and in-memory
reference contexts, and `Agent::telemetry()`) are covered in
[telemetry-schema.md](telemetry-schema.md); the crate entrypoints are surveyed
in [sdk.md](sdk.md). Agent loop behavior is exercised end to end by the e2e
smoke harness, whose passing run this tree binds in the manifest below.

## Pending port surface

- the durable harness runtime: lanes, operations, the `op.meta`/`op.state`
  register program counter, and crash recovery (unported-feature)
- the `Storage` transaction layer with register namespaces and the Memory,
  JSONL, and SQLite backends, including writer leases and snapshot compaction
  (unported-feature)
- the hook interception pipeline (`before_run`, `before_tool`,
  `before_request`, `after_response`, and friends) (unported-feature)
- compaction and navigation operations and the effect-sandwich replay policy
  for unsafe tool calls (unported-feature)
- the partitioned Postgres retention sketch (Part 6 of the reference)
  (unported-feature)
