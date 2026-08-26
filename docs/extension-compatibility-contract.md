# Extension Compatibility Contract (XC-1)

**Stable ID:** `XC-1` — **Issue:** [#52](https://github.com/metaphorics/pi-oxidized/issues/52)

This hand-written contract doc enumerates every settled serialized-and-observable
TypeScript extension boundary of the pi Rust port. It is not a generator-owned
artifact: it is authored and maintained by hand, and every rule below carries a
`path::symbol` witness (`witness:`) that resolves to an existing type, constant,
function, or struct in the cited file at the cited location. Nothing in this
document changes protocol, fixtures, scripts, `.references`, `.outline`, or the
parity ledger; those surfaces are cited read-only (see [Ownership](#ownership)).

The compatibility target is the reference TypeScript packages under
`.references/pi/packages/{ai,agent,coding-agent,tui}`. Serialized names and
behavior are the contract; JavaScript object order and implementation identity
are explicitly **not** (see [Non-contracts](#non-contracts)).

---

## 1. Versioned constants and envelope

| Constant | Value | Witness |
| --- | --- | --- |
| `PROTOCOL_VERSION` | `1` | `packages/pi-tui-protocol/src/types.ts::PROTOCOL_VERSION` (line 10); mirrored `crates/pi-ext/src/protocol.rs::PROTOCOL_VERSION` (line 257), asserted equal in `pi-ext` unit test at `protocol.rs` line 2138 |
| `COMPATIBILITY_VERSION` | `"0.80.10"` | `packages/pi-tui-protocol/src/types.ts::COMPATIBILITY_VERSION` (line 13); `packages/extension-host/src/version.ts::COMPATIBILITY_VERSION`; mirrored `crates/pi-ext/src/protocol.rs::COMPATIBILITY_VERSION` (line 260), asserted at `protocol.rs` line 2139 |
| `MAX_FRAME_BYTES` | `8 * 1024 * 1024` | `packages/pi-tui-protocol/src/types.ts::MAX_FRAME_BYTES` (line 16); mirrored `crates/pi-ext/src/protocol.rs::MAX_FRAME_BYTES` (line 263), asserted at `protocol.rs` line 2140 |

One frame is a single JSON object on one line, at most `MAX_FRAME_BYTES` UTF-8
bytes excluding the trailing newline; the Rust boundary enforces the cap when
serializing and when scanning inbound frames
(`crates/pi-ext/src/protocol.rs::MAX_FRAME_BYTES` usages at lines 1908, 1931, 1948).
Frame kinds are `req | res | event | error` with an id-correlated `FrameId`
(`packages/pi-tui-protocol/src/types.ts::FrameKind`, `::Frame`, `::FrameId`).

## 2. Method registry

The allowlisted bridge and host-control method set is fixed and ordered.

- TypeScript allowlist: `packages/pi-tui-protocol/src/types.ts::METHODS` (line 45) —
  `hello, toolUpdate, providerEvent, uiSlot, disposeSlot, extensionError, select,
  confirm, input, editor, notify, terminalInput, flags.set, shortcut.execute,
  uiEvent, measure, render`. `::isMethod` (line 68) rejects anything not on the
  allowlist.
- Rust typed mirror: `crates/pi-ext/src/protocol.rs::Method::ALL` (lines
  410-426) contains fifteen enum variants. `flags.set` and `shortcut.execute`
  remain open Rust method constants (`::FLAGS_SET_METHOD` and
  `::SHORTCUT_EXECUTE_METHOD`, lines 21-25) and are handled outside the enum.
  Together, the Rust enum and open constants recognize the same seventeen
  serialized names as the TypeScript allowlist. Their representation and
  internal order are not observable compatibility properties.
- Additional open control methods ride the JSONL envelope:
  `providers.update`, `session.*`, `ui.*`, and `theme.*`. The host publishes the live provider registry through `providers.update`
  (`packages/extension-host/src/host.ts::PROVIDERS_UPDATE_METHOD`; emitted by
  `::emitProvidersUpdate` as
  `{ method: "providers.update", payload: { providers } }`).

Lifecycle event discriminants reuse the exact `type` strings from the reference
extension API and are carried as open method strings on a `Frame`
(`crates/pi-ext/src/protocol.rs` module doc, lines 367-369).

## 3. Three-mode handshake asymmetry

Handshake requires `hello` as the very first frame; any other first method is a
terminating error in both the host and the lean runner.

- **Mode 1 (bundled TypeScript-compat host):** validates BOTH `protocolVersion`
  and `compatibilityVersion`; either mismatch terminates the host before any
  registration runs. `witness: packages/extension-host/src/host.ts::handleHelloFrame`
  (checks `remoteProtocol !== PROTOCOL_VERSION` and
  `remoteCompat !== COMPATIBILITY_VERSION`, terminating on mismatch; acknowledges
  with both versions on the `helloAck`).
- **Mode 2 (lean)** and **Mode 3 (native):** validate `protocolVersion` ONLY and
  ignore `compatibilityVersion` — lean and native endpoints do not expose the
  pinned TypeScript runtime, so requiring its compatibility version would reject
  valid endpoints.
  `witness: packages/extension-host/src/lean-runner.ts` module doc (lines 23-25)
  and `::handleHelloFrame` (lines 975-985, "validate `protocolVersion` ONLY");
  doctrine line 10 of `AGENTS.md` ("Preserve the handshake asymmetry").

The asymmetry is a settled compatibility property, not an oversight (doctrine
line `AGENTS.md` line 10).

## 4. Host resolution and discovery/packaging precedence

The TypeScript extension host is resolved ONLY from `PI_EXTENSION_HOST` or
sibling packaged assets; never from `PATH` and never by falling back to another
executable. This prevents accidental or attacker-controlled host selection.

`witness: AGENTS.md` doctrine line 11 ("Resolve the TypeScript extension host
only from `PI_EXTENSION_HOST` or sibling packaged assets; never search `PATH`...").

Extension/import resolution in the host uses jiti with a dual strategy
(`packages/extension-host/src/virtual-modules.ts`):

- **Compiled sidecar:** reference packages are statically imported and served via
  `::getVirtualModules` through jiti `virtualModules`, so the shipped binary needs
  no reference sources on disk.
- **Source mode (`bun test` / fixtures):** `::getExtensionAliases` maps every
  specifier to the pinned reference source for fresh per-load evaluation.

The `CODING` full package index (`pi-coding-agent-full`) resolves to the coding-agent
FULL index in source mode and to the bundled ext capture in compiled mode
(`packages/extension-host/src/virtual-modules.ts` module doc).

## 5. Mode 1 virtual-module alias import surface

In Mode 1 the following specifiers are importable by extensions, resolved either
to bundled sidecar instances (compiled) or to pinned reference source (source
mode). Both strategies yield identical surfaces
(`packages/extension-host/src/virtual-modules.ts::getVirtualModules` and
`::getExtensionAliases`):

| Extension-visible specifier | Resolves to |
| --- | --- |
| `@earendil-works/pi-coding-agent` | coding-agent full package index / bundled ext capture |
| `@earendil-works/pi-agent-core` | agent package index |
| `@earendil-works/pi-tui` | tui package index |
| `@earendil-works/pi-ai` | `ai` **`./compat`** entry |
| `@earendil-works/pi-ai/compat` | `ai` `./compat` entry |
| `@earendil-works/pi-ai/oauth` | `ai` `./oauth` entry |
| `@earendil-works/pi-ai/providers/all` | `ai` `./providers/all` entry |
| `@mariozechner/*` (each of the above) | the same `@earendil-works/*` counterpart (legacy alias) |
| `typebox`, `typebox/compile`, `typebox/value` | modern `typebox` build |
| `@sinclair/typebox*` (each of the above) | the same modern `typebox` counterpart |

`@earendil-works/pi-ai` deliberately maps to the `./compat` entry — the legacy
global provider-registry surface is served through this alias.

**A8 disposition (parity-blocked):** the upstream `./compat` legacy global
provider registry is owned by the parity plan and is **parity-blocked**, meaning
it cannot advance without its evidence checklist (upstream export map, upstream
source surface, downstream importer corpus, extension-host routing, and
executable negative witnesses).
`witness: docs/PARITY_LEDGER.md::row A8` — "Upstream ./compat legacy global
provider registry", owner `pi-ai`, status **parity-blocked**, module
`auth/config_value.rs or deletion ledger`. The host routes this surface through
the `@earendil-works/pi-ai/*` aliases above
(`packages/extension-host/src/virtual-modules.ts` lines 49-50, 86-87) and consumes
the legacy registry via `validateToolArguments` from
`@earendil-works/pi-ai/compat`
(`packages/extension-host/src/host.ts` import, module doc region).

## 6. Registration conflict matrix

Two modes register extensions into distinct registries with observable
conflict-resolution differences; this asymmetry is a settled compat boundary.
The mapped surfaces are tools, commands, flags, shortcuts, providers, renderers,
and hooks.

| Rule | Surface | Mode 1 (host) | Mode 2 (lean runner) |
| --- | --- | --- | --- |
| 1 | tools | **first registration per name wins** in extension load order | **first registration per name wins** |
| 2 | commands | duplicates remain observable and receive unique invocation names (`name:1`, `name:2`, …) | **first registration per name wins** |
| 3 | flags | **first registration per name wins** | **first registration per name wins** |
| 4 | shortcuts | the last extension registration wins unless a built-in binding has `restrictOverride: true`, which cannot be replaced | **last registration per key wins**; lean has no built-in override restriction |
| 5 | providers | a valid re-registration merges its defined fields over the prior registration; invalid input throws before changing stored state | **first registration per name wins** |

Witnesses:

- Mode 1 tools and flags use guarded insertion in
  `.references/pi/packages/coding-agent/src/core/extensions/runner.ts::getAllRegisteredTools`
  and `::getFlags` (lines 450-483). Commands use
  `::resolveRegisteredCommands` (lines 603-636), which retains collisions and
  assigns unique invocation names.
- Mode 1 shortcut resolution is defined by
  `.references/pi/packages/coding-agent/src/core/extensions/runner.ts::getShortcuts`
  (lines 494-536): restricted built-ins are skipped and later extension
  registrations replace earlier ones. Host dispatch walks extensions from the
  last index in `packages/extension-host/src/host.ts::handleShortcutExecute`
  (lines 860-866), so the same later-registration rule is observable over JSONL.
- Mode 1 provider re-registration is defined by
  `.references/pi/packages/coding-agent/src/core/model-runtime.ts::registerProvider`
  (lines 742-777): validation runs first, then defined fields merge over the
  previous entry.
- Mode 2 registration binds first-wins for tools, commands, flags, and providers.
  It stores shortcuts in registration order, then
  `::handleShortcutExecute` scans from the last entry backward, so the last
  registration for a key wins (lines 1514-1528).

Hooks and renderers are adjacent registry behavior, not extra conflict-matrix
rules. Mode 1 renderers are deduplicated by `type:name`, first seen wins. Mode 2
hooks fan out: every handler for a discriminant is appended and fired. The host
and lean snapshots expose only handlers present in the canonical set below.

## 7. Canonical 33-hook classification

The canonical lifecycle event set is exactly the following 33 discriminants, in
this exact order, and they are the byte-diff-equal of BOTH
`ALL_EVENT_TYPES` (`packages/extension-host/src/host.ts`, lines 71-105) and
`LEAN_EVENT_TYPES` (`packages/extension-host/src/lean-api.ts`, lines 18-52). The
host classifies a registered handler as a `handlers` item in the registry
snapshot only for discriminants in this set
(`packages/extension-host/src/host.ts::buildRegistrySnapshot`, `handlers` field,
`ALL_EVENT_TYPES.filter(...)`); the lean runner does the same over
`LEAN_EVENT_TYPES` (`packages/extension-host/src/lean-runner.ts`, `LEAN_EVENT_TYPES.filter`,
line 1195).

```text
project_trust
resources_discover
session_start
session_info_changed
session_before_switch
session_before_fork
session_before_compact
session_compact
session_shutdown
session_before_tree
session_tree
context
before_provider_request
before_provider_headers
after_provider_response
before_agent_start
agent_start
agent_end
agent_settled
turn_start
turn_end
message_start
message_update
message_end
tool_execution_start
tool_execution_update
tool_execution_end
model_select
thinking_level_select
tool_call
tool_result
user_bash
input
```

Both arrays are identical and both end at `input`. Any boundary that adds or
removes a discriminant MUST update both arrays in lockstep and re-verify parity
(see [Ownership](#ownership)).

## 8. Tools, commands, flags, shortcuts, providers wire shapes

The full registry snapshot (`RegistrySnapshotWire` consumed by Rust
`HostExtensionRunner::load`) is produced by
`packages/extension-host/src/host.ts::buildRegistrySnapshot` (lines 2078-2171):

- `tools`: `{ name, label, description, parameters, executionMode? }` from
  `runner.getAllRegisteredTools()` definitions.
- `commands`: `{ name (invocationName), description, source (path), sourceInfo }`
  from `runner.getRegisteredCommands()`.
- `shortcuts`: `{ key, description, extensionPath }` per registered extension.
- `flags`: `{ name, description, type, extensionPath, default?, value? }` from
  `runner.getFlags()` plus effective values from `runner.getFlagValues()`.
- `renderers`: `{ type: "message" | "widget", name }`, deduplicated.
- `providers`: entries from `::buildProviderSnapshot` (lines 2059-2077):
  `{ name, streamSimple (boolean), baseUrl?, api?, displayName?, apiKey?, headers?,
  authHeader?, models? }` — matching `SessionToolWire` / `SessionCommandInfoWire`
  / `ProvidersUpdate` mirror fields on the Rust side
  (`crates/pi-ext/src/protocol.rs` typed wire structs, session-action and theme
  open-method sections at lines 1339, 1429).
- `handlers`: the canonical 33 discriminants with at least one registered handler.
- `terminalInput`: boolean, whether an active terminal-input handler exists.

## 9. UI slots and sanitization clamps

TypeScript is NOT the trust boundary for extension-rendered UI. Every inbound
`UiSlot` passes through `pi_ext::sanitize` and only `SanitizedSlot` is rendered;
raw extension bytes are never painted, because extensions can split terminal
control sequences across runs or generations.

`witness: AGENTS.md` doctrine line 9 ("Treat Rust as the trust boundary for
extension-rendered UI: pass every inbound `UiSlot` through `pi_ext::sanitize` and
render only `SanitizedSlot`...").

ANSI sanitizer clamps (TypeScript fallback used by the host for its inbound ANSI
lines; the Rust boundary is authoritative):
`packages/extension-host/src/sanitize.ts`:

- `MAX_HYPERLINK_ID_BYTES = 128` (line 21) — a hyperlink id longer than 128 bytes
  drops the hyperlink (line 455).
- `MAX_HYPERLINK_URI_BYTES = 2048` (line 23) — a uri longer than 2048 bytes drops
  the hyperlink (line 459).
- Everything not on the SGR/color/OSC-8/printable allowlist is silently dropped
  (`sanitize.ts` module doc, lines 2-8; "only hyperlinks are allowlisted", line 430).

The UI slot wire surfaces (`UiSlot`, `SanitizedSlot`, `SlotPlacement`,
`OverlayOptions`, `measure`/`render` method payloads) are defined in
`packages/pi-tui-protocol/src/types.ts` and mirrored in
`crates/pi-ext/src/protocol.rs` with generation-gated measure/render.

## 10. Deadlines, cancellation, error isolation, stale guard

- **Mutable hook deadline:** lifecycle hooks must respond within **30 s**.
  `witness: packages/extension-host/src/host.ts::EXTENSION_HOOK_TIMEOUT_MS =
  30_000` (line 108).
- **Terminal-input deadline:** terminal `onTerminalInput` consume/rewrite must
  respond within **4 ms**, through the sequential input actor.
  `witness: packages/extension-host/src/host.ts::EXTENSION_INPUT_TIMEOUT_MS = 4`
  (line 116), `::EXTENSION_INPUT_QUEUE_CAPACITY = 64` (line 118).
- **Shortcut cancellation and single-flight:** each shortcut execution receives
  its own `AbortController`. Host disposal aborts the active controller. A
  second dispatch for the same key returns `{ handled: true }` without starting
  another invocation and without aborting the active one.
  `witness: packages/extension-host/src/host.ts::handleShortcutExecute`
  (single-flight return at lines 873-877; controller and signal at lines
  878-899) and host disposal (line 3153).
- **Error isolation:** a detached shortcut failure emits a per-extension
  `extensionError` notification and does not stop the host; aborted or disposed
  shortcut executions suppress that notification
  (`host.ts::handleShortcutExecute`, lines 898-910). Ordinary lifecycle handler
  failures are isolated inside the runner, emit `extensionError`, and allow the
  remaining handlers and correlated request to continue. Only an error that
  escapes lifecycle request processing returns a correlated non-retryable
  `extension_error` response (`host.ts::handleLifecycleHook`, lines 1052-1058;
  `lean-runner.ts::runHooks`, lines 1583-1600).
- **Stale-command-context guard:** a captured `pi`/command context is invalid
  after `newSession`/`fork`/`switchSession`/`reload`; the marker message must be
  surfaced verbatim on misuse.
  `witness: packages/extension-host/src/host.ts::STALE_COMMAND_CONTEXT_MESSAGE`
  (line 112).

## 11. Explicit non-contracts

The following are explicitly NOT compatibility surfaces and may differ without
violating this contract:

- JavaScript object property order and implementation identity. Compatibility is
  serialized names and behavior only.
  `witness: AGENTS.md` doctrine line 7 ("match serialized names and behavior, not
  JavaScript object order or implementation identity").
- Package-shape parity: "Package-shape parity is not a goal."
  `witness: docs/PARITY_LEDGER.md` intro paragraph (line 3).
- Non-runtime evals, example extensions, build machinery, install machinery, and
  test machinery (excluded from the parity ledger's frozen behavior set,
  `docs/PARITY_LEDGER.md` line 3).
- Lean/native validation of `compatibilityVersion` (deliberately absent — see
  [Handshake asymmetry](#3-three-mode-handshake-asymmetry)).

## 12. Ownership

This plan (stable ID `XC-1` / issue #52) **solely owns** the declared
mirror/fixture/mutation-checker surfaces and the A8 audit-record slot:

- The mirror/fixture/mutation-checker surfaces:
  `packages/pi-tui-protocol/tests/fixtures/frames.jsonl` (the shared cross-language
  wire witness that cross-locks `Method`, `ALL_EVENT_TYPES`, and the lean lists),
  and the mutation checker `scripts/verification/parity.ts` (with
  `compat-matrix.json`), which verify the mirror cannot become a competing
  protocol authority.
- The A8 audit-record slot: `docs/PARITY_LEDGER.md` row A8 ("Upstream ./compat
  legacy global provider registry"), status **parity-blocked**.

These surfaces are **consumed read-only** everywhere else in the repository;
protocol files (`packages/pi-tui-protocol/src/types.ts`,
`crates/pi-ext/src/protocol.rs`), fixtures, verification scripts, `.references`,
`.outline`, and `PARITY_LEDGER.md` are cited as witnesses here, never edited by
this contract. The extension-parity responsibility for `protocol.rs`, TypeScript
`METHODS`, and `frames.jsonl` lockstep is `extension-plan-owned`
(`docs/PARITY_LEDGER.md` row E1, status `extension-plan-owned`), and `sync-docs`
evidence boundaries (AR10) apply: regenerable fixtures are regenerated only via
their owning scripts; this hand-written contract doc is not regenerable.