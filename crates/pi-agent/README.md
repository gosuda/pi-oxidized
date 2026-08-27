# pi-agent

Agent turn loop, queues, tool scheduling, and events.

## Workspace topology

`pi-agent` depends on `pi-ai` only. It is depended on by `pi-ext` and `pi`.

```
pi-ai  (no workspace deps)
  ↑
pi-agent → pi-ai
pi-ext   → {pi-ai, pi-agent, pi-tui}
pi       → {pi-ai, pi-agent, pi-ext, pi-tui}
```

The full topology is owned by the root `AGENTS.md` and generated from workspace
`Cargo.toml` edges so this README and `AGENTS.md` share one source.

## Public modules

| Module | Description |
|---|---|
| `agent` | Agent and AgentOptions |
| `bus` | Event bus and subscriptions |
| `config` | Agent loop configuration and hooks |
| `drain` | Provider drain |
| `error` | Agent loop and tool errors |
| `event` | Agent events |
| `message` | Agent messages |
| `queue` | Pending message queue |
| `run` | Agent loop runner |
| `schedule` | Tool call scheduling |
| `state` | Agent state and snapshots |
| `telemetry` | Telemetry context and spans |
| `tool` | Agent tool trait and results |

## Public re-exports

### `agent`

| Symbol | Kind |
|---|---|
| `Agent` | struct |
| `AgentOptions` | struct |

### `bus`

| Symbol | Kind |
|---|---|
| `AGENT_EVENT_CAPACITY` | const |
| `AgentEventSink` | type |
| `AgentEventSubscription` | type |
| `EXTENSION_EVENT_CAPACITY` | const |
| `EventSink` | type |
| `ExtensionEvent` | type |
| `ExtensionSubscription` | type |

### `config`

| Symbol | Kind |
|---|---|
| `AfterToolCall` | type |
| `AfterToolCallContext` | type |
| `AfterToolCallResult` | type |
| `AgentContext` | type |
| `AgentLoopConfig` | struct |
| `AgentLoopTurnUpdate` | type |
| `BeforeToolCall` | type |
| `BeforeToolCallContext` | type |
| `BeforeToolCallResult` | type |
| `ConvertToLlm` | type |
| `GetApiKey` | type |
| `GetMessages` | type |
| `PrepareNextTurn` | type |
| `PrepareNextTurnContext` | type |
| `ShouldStopAfterTurn` | type |
| `ShouldStopAfterTurnContext` | type |
| `TransformContext` | type |
| `build_stream_options` | fn |
| `default_convert_to_llm_hook` | fn |

### `drain`

| Symbol | Kind |
|---|---|
| `DRAIN_EVENT_CAPACITY` | const |
| `DrainItem` | type |
| `ProviderDrain` | type |

### `error`

| Symbol | Kind |
|---|---|
| `AgentLoopError` | enum |
| `ToolError` | enum |

### `event`

| Symbol | Kind |
|---|---|
| `AgentEvent` | enum |

### `message`

| Symbol | Kind |
|---|---|
| `AgentMessage` | enum |
| `CustomAgentMessage` | type |
| `default_convert_to_llm` | fn |
| `now_millis` | fn |
| `user_text` | fn |

### `queue`

| Symbol | Kind |
|---|---|
| `PendingMessageQueue` | struct |
| `QueueMode` | enum |

### `run`

| Symbol | Kind |
|---|---|
| `RunIo` | struct |
| `run_agent_loop` | fn |
| `run_agent_loop_continue` | fn |

### `schedule`

| Symbol | Kind |
|---|---|
| `EmitAgentEvent` | type |
| `ExecutedToolCallBatch` | type |
| `MAX_PARALLEL_TOOL_CALLS` | const |
| `PARALLEL_TOOL_UPDATE_CAPACITY` | const |
| `execute_tool_calls` | fn |
| `fail_tool_calls_from_truncated_message` | fn |
| `should_terminate_tool_batch` | fn |

### `state`

| Symbol | Kind |
|---|---|
| `AgentState` | type |
| `AgentStateSnapshot` | type |

### `telemetry`

| Symbol | Kind |
|---|---|
| `AGENT_TELEMETRY_SCHEMAS` | const |
| `AI_TELEMETRY_SCHEMA` | const |
| `AttributeValue` | type |
| `HARNESS_TELEMETRY_SCHEMA` | const |
| `InMemoryTelemetryContext` | type |
| `RecordedEvent` | type |
| `RecordedSpan` | type |
| `SpanAttributes` | type |
| `SpanOptions` | type |
| `SpanStatus` | type |
| `TelemetryContext` | trait |
| `TelemetrySchema` | type |
| `TelemetrySpan` | type |
| `noop_context` | fn |

### `tool`

| Symbol | Kind |
|---|---|
| `AgentTool` | trait |
| `AgentToolResult` | type |
| `ToolExecutionMode` | type |
| `ToolUpdates` | type |
| `error_tool_result` | fn |
| `to_pi_tool` | fn |

### Re-export

`pi_ai` is re-exported from this crate via `pub use pi_ai`.

## Handshake symmetry

The handshake asymmetry is documented in
`docs/extension-compatibility-contract.md`, the single owner doc. This README
references it; other docs point there rather than restating it.
