# Rust Port Parity Ledger

This ledger freezes behavior-level parity against canonical reference `.references/pi-2.0` at commit `853a80d26c90a14c1886f0ebb8ffaae133ca2185` as the canonical behavior authority. It maps every shipped runtime capability to exactly one deep-module owner in the fixed five-crate workspace. Package-shape parity is not a goal. Non-runtime evals, example extensions, build machinery, install machinery, and test machinery are excluded.

Statuses are evidence-bearing: `landed` names an existing owner seam; `folded` records intentional consolidation into one owner; `planned` pins a future owner without claiming implementation; `host-owned` remains in the TypeScript extension host; `dev-only` is test support, not product code; `parity-blocked` cannot advance without its checklist; `witnessed` records that a parity-blocked checklist has been satisfied with executable negative witnesses and the ruling is recorded; and `extension-plan-owned` is verified by the extension compatibility plan rather than this ledger.

## Capability ledger

| ID | Capability | Owner | Module | Seam | Status | Evidence or contract |
| --- | --- | --- | --- | --- | --- | --- |
| A1 | Unified Provider trait and types | pi-ai | provider.rs, types.rs | Provider | landed | Vendor wire shapes remain behind one provider contract. |
| A2 | Ten provider transports and registry | pi-ai | providers/, registry.rs | provider registry | landed | Ten LLM transports plus OpenRouter images at the canonical .references/pi-2.0 tree at 853a80d26c90a14c1886f0ebb8ffaae133ca2185. |
| A3 | Model catalog, models-store, and cross-process lockfile | pi-ai | catalog/, models_store.rs, lockfile.rs | catalog merge | landed | Discovery and persistence share one catalog owner. |
| A4 | Parameter normalization | pi-ai | simple_options.rs | normalized stream options | landed | Thinking budgets and token clamps are provider-neutral. |
| A5 | Token estimation | pi-ai | estimate.rs | token estimate | landed | Shared overflow arithmetic has one owner. |
| A6 | Provider streaming event surface | pi-ai | types.rs, providers/*/stream_state.rs | provider event stream | landed | Provider events remain distinct from the agent lifecycle bus. |
| A7 | Credential store, native OAuth, and environment-key resolution | pi-ai | auth/ | auth resolver | landed | PKCE, device code, and config-value resolution share one parser and command cache. |
| A8 | Upstream ./compat legacy global provider registry | pi-ai | deletion ledger | compatibility audit | witnessed | Ruling: delete-not-port. All five checklist items witnessed green by scripts/verification/compat-audit.ts (issue #59): upstream export map (17 re-exports, 18 direct exports, 2 side effects); upstream source surface (compat.ts in .references/pi-2.0 at canonical 853a80d26c90a14c1886f0ebb8ffaae133ca2185); downstream importer corpus (all importers TS-side-runtime in ai/agent/coding-agent src/test/examples, zero Rust-surface consumers); extension-host routing (Mode 1 alias serves JS bundle's ./compat, not a Rust port); executable negative witnesses (no pi_ai::compat or mod compat in any Rust source). Env-key resolution already in A7 auth/env_keys.rs; Model.compat already adapter-local in types.rs. |
| A9 | Standalone pi-ai binary, the upstream OAuth-CLI surface | pi-ai | crate-local [[bin]] | CLI process contract | landed | PAR-CLI (#51) landed the pi-ai [[bin]] at src/cli.rs (commit ed6b302): unknown command/provider exit 1 with typed errors; metadata witness proves zero imports from pi, pi-agent, pi-ext, or pi-tui and zero new workspace edges. |
| A10 | Bun OAuth helpers | pi-ext | TypeScript host bundle | Bun runtime boundary | host-owned | Native OAuth remains A7; no Rust module is added. |
| A11 | Transport retry, diagnostics, and validation | pi-ai | providers/shared | ProviderError | landed | Session retry policy remains product-owned. |
| G1 | Agent loop control | pi-agent | run.rs, agent.rs | AgentLoopConfig | landed | Terminal events and queue stages stay behind run_agent_loop. |
| G2 | Harness and loop hooks | pi-agent | config.rs | AgentLoopConfig hooks | landed | Extensions and product code inject behavior only through the hook seam. |
| G3 | Prompt templates, system prompt, and skills formatting | pi | core/resources/prompts.rs, core/system_prompt.rs, core/resources/skills.rs | assembled prompt | folded | Agent-harness duplicates fold into the product owner. |
| G4 | Compaction and branch summarization | pi | core/compaction/ | injected summarization future | folded | Product tool serialization stays out of pi-agent. |
| G5 | Tool execution and scheduling | pi-agent | tool.rs, schedule.rs, queue.rs, drain.rs | AgentTool | landed | Execution, cancellation, updates, and parallel batches share one engine seam. |
| G6 | Execution environment | pi-agent | tool.rs plus pi concrete tools | tool execution contract | folded | One native environment exists; no shallow ExecutionEnv wrapper is introduced. |
| G7 | Agent-harness built-in tool copies | pi | core/tools/ | product tool registry | folded | Duplicated harness tools fold into C5. |
| G8 | Session persistence including sqlite-node | pi | core/sessions/, core/agent_session/persistence.rs, core/migrations.rs | session store | folded | JSONL, memory, and SQLite persistence have one product consumer and owner. |
| G9 | Vendor-neutral telemetry schema and span context | pi-agent | telemetry.rs | TelemetryContext | landed | PAR-TEL (#71) landed pi-agent::telemetry with one injected no-fail TelemetryContext (e91ee94); all six AgentLoopConfig literal sites carry telemetry (enumeration witness every_agent_loop_config_literal_carries_telemetry, count = 6); compaction spans emit through the same injected context. Exporter adapters remain in pi; install-telemetry gating remains C18. |
| G10 | Session-testing utilities | pi-agent | test support | dev-only support | dev-only | This surface never ships as product code. |
| G11 | Message, event, and state model | pi-agent | message.rs, event.rs, state.rs, bus.rs | lifecycle event bus | landed | Agent lifecycle events remain distinct from provider stream events. |
| T1 | Differential rendering engine | pi-tui | frame.rs, terminal/ | frame buffer | landed | Rendering remains product-agnostic. PAR-PTY-GRILL verified: host-tier PTY evidence proves zero full-screen clears, row-local erase with immediate reflow, and continuous content across resizes (crates/pi-tui/tests/pty_grill_adjudication.rs). |
| T2 | Terminal state management | pi-tui | terminal/, link.rs, keys.rs | Terminal capability | landed | Raw mode, alternate screen, ANSI, OSC, and keyboard protocols share one owner. pi-tui owns terminal capability detection and environment overrides: `TerminalCapabilityOverrides` and `detect_with_overrides` parse `PI_HYPERLINKS` and `PI_TRUE_COLOR` (`1`, `0`, `auto`) before automatic detection. PAR-PTY-GRILL verified: probes precede synchronized output, balanced sync markers, restoration on exit, Kitty keyboard flag toggles (crates/pi-tui/tests/pty_grill_adjudication.rs). |
| T3 | Terminal image rendering | pi-tui | image.rs, terminal/caps.rs | ImageProtocol | landed | Kitty, iTerm2, and fallback selection stay behind terminal capabilities. pi-tui owns image protocol detection and the `PI_IMAGE_PROTOCOL` environment override (`kitty`, `iterm2`, `none`, `0`, `auto`) via `ImageProtocolOverride` and `detect_with_overrides`. PAR-PTY-GRILL verified: Kitty/iTerm2 encoders produce correct sequences (unit), no raw image bytes on PTY wire (host-tier negative witness, crates/pi-tui/tests/pty_grill_adjudication.rs). |
| T4 | LaTeX math rendering | pi-tui | components/markdown.rs, text/latex.rs | markdown math path | landed | PAR-MATH (#37) re-landed per strategy (docs/PAR-MATH-latex-strategy.md): stage 1 layout engine at text/latex.rs (0c27a40) with entry-exact command tables and canonical .references/pi-2.0 corpus expectations byte-for-byte; stage 2 markdown integration wires the rendered-vs-fallback contract — block `$$…$$`/`\[…]`, inline `$$…$$`/`\(…\)`/single `$…$` under the four rejection rules, code-span/fence exclusion, escaped-dollar handling, and MarkdownOptions.render_latex gate. Unsupported input falls back to raw source. The PAR-PTY-GRILL open gap (math events dropped, no fallback) is superseded; see docs/PAR-PTY-GRILL-verdict.md T4 re-adjudication. |
| T5 | Markdown rendering | pi-tui | components/markdown.rs, text/ | styled text | landed | Width, wrapping, and ANSI handling are framework concerns. |
| T6 | Layout and widgets | pi-tui | component.rs, components/, layout.rs, overlay.rs, focus.rs | Component and UiEvent | landed | The event set remains closed and product-agnostic. |
| T7 | Keybindings manager and key parsing | pi-tui | keybindings.rs, keys.rs | parsed key event | landed | Product default bindings remain in pi. |
| T8 | Fuzzy matching | pi-tui | fuzzy.rs | matcher | landed | Product-independent matching stays in the TUI crate. |
| T9 | Terminal interfaces | pi-tui | terminal/ | Terminal | landed | Callers never emit terminal escape bytes directly. PAR-PTY-GRILL verified: sole stdout owner after probes, transaction markers present, probe batch emitted through terminal interface (crates/pi-tui/tests/pty_grill_adjudication.rs). |
| E1 | Extension JSONL protocol authority | pi-ext | protocol.rs | versioned JSONL frames | extension-plan-owned | The mirror/fixture lockstep witness for protocol.rs, TypeScript METHODS, and frames.jsonl is extension-plan-owned and consumed read-only here. |
| E2 | Extension host lifecycle | pi-ext | host.rs, client.rs, server.rs | HostClient | landed | Source-pinned spawn, handshake, bounded channels, and shutdown share one owner. |
| E3 | Extension bridges | pi-ext | adapters.rs, sanitize.rs | AgentTool, Provider, and Component adapters | landed | Host JSON shapes terminate at the three downstream trait adapters. |
| E4 | Extension-side OAuth and JavaScript helpers | pi-ext | TypeScript host bundle | Bun runtime boundary | host-owned | This is the extension half of the A10/A7 split. |
| C1 | CLI arguments and bootstrap | pi | cli/ | command entry | landed | Product startup has one owner. |
| C2 | Interactive mode | pi | modes/interactive/ | interactive runtime | landed | Terminal product composition stays above pi-tui. |
| C3 | Print mode | pi | modes/print/ | print runtime | landed | Noninteractive product behavior stays in pi. |
| C4 | JSONL RPC mode and rpc-entry | pi | modes/rpc/ | RPC frames | landed | This is distinct from the remote session stack R1-R4. |
| C5 | Built-in tools | pi | core/tools/ | tool registry | landed | Read, write, edit, bash, find, grep, ls, mutation queue, truncation, and path utilities share one owner. |
| C6 | Extension product wiring | pi | core/extension_host.rs, extension_runtime_set/, extension_manifest.rs | ExtensionRunner | landed | AgentSession never depends on pi-ext directly. |
| C7 | Settings | pi | core/settings.rs | settings manager | landed | Product configuration remains product-owned. pi owns the settings/runtime adapter for terminal capability overrides: `SettingsManager::get_terminal_capability_overrides()` returns `pi_tui::terminal::TerminalCapabilityOverrides`, JSON settings (`terminal.hyperlinks`, `terminal.images`, `terminal.trueColor`) override `PI_*` environment values, which override terminal detection. Runtime reload replaces only images, hyperlinks, and true_color, preserving sync_output, keyboard_protocol, cell, and dark_background. |
| C8 | Project trust | pi | core/trust.rs | trust store | landed | Trust decisions do not leak into lower crates. |
| C9 | Session manager and tree navigation | pi | core/sessions/, core/agent_session/tree.rs | session manager | landed | Product sessions stay in pi. |
| C10 | Resource and skill loader | pi | core/resources/ | resource loader | landed | Product discovery and formatting remain together. |
| C11 | Image resize and conversion | pi | image integration | product image action | landed | Terminal display selection remains T3. |
| C12 | Themes | pi | modes/interactive/theme.rs, core/resources/themes.rs | product theme | landed | Theme choice stays above the product-agnostic TUI. |
| C13 | Clipboard | pi | interactive product action | clipboard action | planned | Generic OSC52 emission, when added, belongs to pi-tui terminal capabilities. |
| C14 | Interactive components and UI hooks | pi | modes/interactive/ | product UI composition | landed | Model selectors, tool views, headers, footers, timelines, and extension slots compose in the product. |
| C15 | AgentSession and runtime | pi | core/agent_session/ | AgentSession | landed | Product session orchestration stays above pi-agent. |
| C16 | Export HTML | pi | core/export_html/ | export action | landed | Transcript export is a product surface. |
| C17 | Package manager, update, migrations, share, and transfer | pi | core/package_manager/, core/update/, core/migrations.rs, core/share.rs, core/session_transfer.rs | product lifecycle operations | landed | Distribution and migration policy stay in pi. |
| C18 | Model runtime, provider attribution, install-telemetry gating, and auth guidance | pi | core/model_resolver/, core/model_runtime/, core/provider_attribution.rs, core/settings.rs | product model runtime | landed | PI_TELEMETRY and install telemetry are not the G9 vendor-neutral schema. |
| R1 | Remote codec and framing | pi | remote/codec.rs, remote/framing.rs | transport-neutral bytes | landed | PAR-CODEC (#31, commit 8917d54): codec-layer errors only; 20 remote_codec_golden tests green; encode/decode byte-identical roundtrips for all message types; absence witness proves no R3/R4 symbols (ByteTransport, EndpointSpec, client taxonomy). Portable neutral surface compiles on every target. |
| R2 | Remote schemas | pi | remote/schemas.rs | transport-neutral schema | landed | PAR-CODEC (#31, commit 8917d54): schema types share the codec layer's transport-neutral bytes; 20 remote_codec_golden tests cover the schema-annotated roundtrips. Portable neutral surface compiles on every target. |
| R3 | Remote client and ByteTransport | pi | remote/client.rs, remote/transport/ | ByteTransport | landed | PAR-CLIENT (#33, SHA a0bca74, review fix 38897ce): two adapters (in-memory + unix), five-class error taxonomy pinned by test, detach/reattach and mid-request disconnect typed; 40/40 remote tests green. Unix adapter is #[cfg(unix)]; the in-memory adapter and client are portable; Unix endpoints on non-Unix return typed EndpointSpecError::UnsupportedOnPlatform. Windows x86_64-pc-windows-msvc compile probe green. |
| R4 | Remote multi-session server | pi | remote/server.rs | portable server and listener preset | landed | Unix listener preset is #[cfg(unix)]; the transport-neutral server is portable; the merge-blocking Windows-target compile check covers the transport-neutral surface. |

## Deletion ledger

Finalized by PAR-CLOSE (#39). Each entry names the deleted surface, the ruling, and the executable evidence that keeps it deleted.

| Deleted surface | Ruling | Evidence IDs |
| --- | --- | --- |
| `crates/pi/src/core/config_value.rs` (179-line wrapper) plus its `pub mod config_value;` declaration | delete-not-port (AR6): one parser and one process-wide command cache, both owned by `crates/pi-ai/src/auth/config_value.rs`; inline ruling recorded at the canonical import in `crates/pi/src/core/model_runtime.rs` | PAR-COMPAT-DISPO (#45) commits `fc8958c`, `24647d9`, `f88e862`, `3deed0c`; witness 6 `config-value-single-owner` in `scripts/verification/compat-audit.ts` (runs under `bun test scripts` and `bun run verify:compatibility`); deletion entry carried in the `compat-audit-witness` row of `scripts/verification/compat-matrix.json`; `cargo check -p pi --locked` green post-deletion |
| Upstream `./compat` legacy global provider registry (A8, 17 re-exports, 18 direct exports, 2 side effects) | delete-not-port: env-key resolution already A7 (`auth/env_keys.rs`), `Model.compat` already adapter-local (`types.rs`), extension host serves the JS bundle's own `./compat` | PAR-COMPAT-AUDIT (#59) commit `d8dfb5d`; five checklist witnesses green via `scripts/verification/compat-audit.ts` (issues #59, #45); executable negative witnesses: no `pi_ai::compat` and no `mod compat` in any Rust source |

## Pinned workspace contract

The workspace contains exactly `pi`, `pi-agent`, `pi-ai`, `pi-ext`, and `pi-tui`. Its complete internal edge set is `pi-agent -> pi-ai`, `pi-ext -> pi-ai`, `pi-ext -> pi-agent`, `pi-ext -> pi-tui`, and `pi -> pi-ai`, `pi -> pi-agent`, `pi -> pi-ext`, `pi -> pi-tui`. `pi-ai` and `pi-tui` have no workspace dependencies. `pi-agent` must not import `pi_ext` or `pi_tui`; `pi-ext` must not import `pi`.

The shared arbitration oracle is exactly six `AgentLoopConfig` literal sites: `crates/pi-agent/src/agent.rs:65-93`, `crates/pi-agent/src/config.rs:373-409`, `crates/pi-agent/src/run.rs:944-971`, `crates/pi-agent/src/schedule.rs:902-929`, `crates/pi/src/core/agent_session/mod.rs:473-501`, and `crates/pi-agent/src/bin/pi_agent_stream_frame_bench.rs:267-295`.

## Graduated parity-ticket DAG

`blocked_by` contains stable IDs only. The external `XC-2` row is included so every dependency resolves inside this witness table.

| Stable ID | Kind | blocked_by |
| --- | --- | --- |
| PAR-LEDGER | task | — |
| PAR-TEL | task | PAR-LEDGER |
| PAR-CLI-PROTO | prototype | PAR-LEDGER |
| PAR-CLI | task | PAR-CLI-PROTO |
| PAR-WIRE | research | PAR-LEDGER |
| PAR-CODEC | task | PAR-WIRE |
| PAR-CLIENT | task | PAR-CODEC |
| PAR-SERVER | task | PAR-CLIENT |
| PAR-COMPAT-AUDIT | grilling | PAR-LEDGER |
| PAR-COMPAT-DISPO | task | PAR-COMPAT-AUDIT |
| PAR-MATH-RESEARCH | research | PAR-LEDGER |
| PAR-MATH | task | PAR-MATH-RESEARCH |
| PAR-FOLD | task | PAR-TEL, PAR-CLI, PAR-COMPAT-DISPO |
| PAR-PTY-GRILL | grilling | PAR-LEDGER, PAR-MATH |
| XC-2 | external | — |
| PAR-CLOSE | task | PAR-FOLD, PAR-CLIENT, PAR-SERVER, PAR-COMPAT-AUDIT, PAR-COMPAT-DISPO, PAR-PTY-GRILL, XC-2 |

## Boundary ratification and downstream handoff

Ratified by PAR-CLOSE (#39): parity = all nine runtime packages' capabilities at behavior level, folded at deep-module interfaces — every upstream runtime capability maps to exactly one owner seam in the fixed five-crate workspace above. The A8 adjudication (method: five-witness compatibility audit against the canonical .references/pi-2.0 tree at 853a80d26c90a14c1886f0ebb8ffaae133ca2185; result: delete-not-port with the surviving config-value surface owned by A7 auth) and the non-runtime exclusion enumeration (evals, example extensions, build machinery, install machinery, test machinery; package-shape parity is not a goal) are recorded above. This ledger is now frozen as the owner table consumed by documentation-sync, maximum-performance, seven-platform-release, and extension-compatibility.
