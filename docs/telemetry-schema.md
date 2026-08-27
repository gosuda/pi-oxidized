# Telemetry

Ported from `.references/pi/packages/agent/docs/telemetry-schema.md` at pin
`8fa7eebd`. The reference page is generated attribute tables for the
`pi.ai.request` and `pi.harness.*` span schemas; the generated tables are not
ported. What this build proves today is the install-telemetry override and the
pinned schema constants of the `pi-agent` crate, bound to
[evidence/telemetry-schema.json](evidence/telemetry-schema.json).

## Install telemetry override

The executed `--help` snapshot documents the `PI_TELEMETRY` environment
variable:

```text
pi - AI coding assistant with read, bash, edit, write tools

Usage:
  pi [options] [@files...] [messages...]

Commands:
  pi install <source> [-l]     Install extension source and add to settings
  pi remove <source> [-l]      Remove extension source from settings
  pi uninstall <source> [-l]   Alias for remove
  pi update [source|self|pi]   Update pi, extensions, or model catalogs
  pi list                      List installed extensions from settings
  pi config [-l]               Open TUI to enable/disable package resources (Tab switches scope)
  pi <command> --help          Show help for install/remove/uninstall/update/list/config

Options:
  --provider <name>              Provider name (default: google)
  --model <pattern>              Model pattern or ID (supports "provider/id" and optional ":<thinking>")
  --api-key <key>                API key (defaults to env vars)
  --system-prompt <text>         System prompt (default: coding assistant prompt)
  --append-system-prompt <text>  Append text or file contents to the system prompt (can be used multiple times)
  --mode <mode>                  Output mode: text (default), json, or rpc
<!-- doc-c:fence=telemetry-schema.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

Set `PI_TELEMETRY` to `1`, `true`, or `yes` to force install telemetry on, or
to `0`, `false`, or `no` to force it off; when unset, the setting keeps its
default. The variable sits in the same environment block as `PI_OFFLINE` and
the other `PI_*` variables surveyed in
[environment-variables.md](environment-variables.md).

## Crate telemetry contracts

The `pi-agent` crate pins the span-name vocabulary as compile-time schema
constants: `AI_TELEMETRY_SCHEMA` (version 1, the `pi.ai.request` span) and
`HARNESS_TELEMETRY_SCHEMA` (version 1, eleven `pi.harness.*` and
`pi.session.write` spans: run, compaction, navigation, checkpoint, turn, step,
tool, hook, sleep, event_handler, and session write), combined in
`AGENT_TELEMETRY_SCHEMAS`. Typed start helpers (`start_ai_request_span`,
`start_harness_run_span`, and siblings) build the required start attributes.

Telemetry is passive by design: callers own control flow, span failures are
contained behind a panic boundary, and the default context records nothing.

```rust
use pi_agent::{
    AGENT_TELEMETRY_SCHEMAS, AttributeValue, InMemoryTelemetryContext, SpanOptions,
    TelemetryContext, noop_context,
};

// The shared no-op context records nothing.
let quiet = noop_context();
drop(quiet);

// An in-memory recorder captures detached span snapshots.
let recorder = InMemoryTelemetryContext::new();
let span = recorder.start_span(SpanOptions {
    name: "pi.ai.request".to_owned(),
    attributes: [(
        "pi.ai.provider".to_owned(),
        AttributeValue::Str("google".to_owned()),
    )]
    .into(),
});
drop(span);
let settled_spans = recorder.spans();

// The pinned vocabulary, versioned at compile time.
for schema in AGENT_TELEMETRY_SCHEMAS {
    let _ = (schema.version, schema.spans.len());
}
<!-- doc-c:fence=telemetry-schema.02 -->
```

Attributes carried in spans are ids, names, counts, durations, statuses, and
usage figures; prompts, completions, tool arguments and results, and provider
payloads never enter telemetry attributes. `Agent::telemetry()` hands back the
context configured for the run; the harness surface it plugs into is described
in [harness.md](harness.md).

## Pending port surface

- the generated per-span attribute tables for `pi.ai.request`, the
  `pi.harness.*` family, and `pi.session.write` (unported-feature)
- the `generate-telemetry-docs.ts` generator that produced the reference
  tables (unported-feature)
- backend exporters beyond the no-op and in-memory reference contexts
  (unported-feature)
