# Task 3 report

Status: DONE

Base: `f26b1507c0c83c4cfb2501dbf7d67464686a0f9e`

## Findings

### 1. Aggregate hook transport failure

The finding is invalid for every concrete `HostExtensionRunner` hook. Each implementation catches an in-flight `hook_request`/request transport error, reports it through `report_host_error`, and returns the hook's identity/default `Ok`, so the aggregate `?` cannot receive a concrete endpoint transport failure and later siblings remain live:

- `emit`: `crates/pi/src/core/extension_host.rs:2126-2162` (error isolation at 2157-2160)
- `emit_message_update_delta`: `crates/pi/src/core/extension_host.rs:2165-2200` (2195-2198)
- `emit_message_end`: `crates/pi/src/core/extension_host.rs:2203-2244` (2239-2242)
- `emit_tool_call`: `crates/pi/src/core/extension_host.rs:2247-2288` (2283-2286)
- `emit_tool_result`: `crates/pi/src/core/extension_host.rs:2291-2339` (2334-2337)
- `emit_input`: `crates/pi/src/core/extension_host.rs:2342-2387` (2382-2385)
- `emit_before_agent_start`: `crates/pi/src/core/extension_host.rs:2390-2427` (2422-2425)
- `emit_resources_discover`: `crates/pi/src/core/extension_host.rs:2430-2472` (2468-2470)
- `execute_command`: `crates/pi/src/core/extension_host.rs:2491-2521` (2518-2520)

No speculative production change was made for this finding.

### 2. Provider transaction on failed reload

Added the crate-private, non-publishing `ModelRuntime::validate_provider_registration` seam and made normal registration use the same validator (`crates/pi/src/core/model_runtime.rs:683-713`). Reload now validates every first-owned replacement provider before reading or mutating the old generation's provider state (`crates/pi/src/core/extension_runtime_set.rs:907-917`, `1724-1740`). Validation failure stops/reaps the unpublished replacement and returns before old configs, stream adapters, generation, routes, slots, or endpoints are changed. The existing post-validation rollback remains as a defensive guard (`extension_runtime_set.rs:917-927`).

The focused regression test at `extension_runtime_set.rs:2646-2748` verifies that invalid replacement validation performs zero provider-map mutations, publishes neither replacement provider, preserves the old model and generation, and keeps the old endpoint hook live.

### 3. Lifecycle restoration after failed concrete reload

`AgentSession::reload` now emits `session_start{reload}` to the still-current old generation before returning a concrete restart error (`crates/pi/src/core/agent_session/extension.rs:361-367`). The success path retains its single emission at lines 373-375, so success does not emit twice.

The focused concrete-host regression test at `extension.rs:1430-1459` observes exactly one `session_shutdown{reload}` and one restorative `session_start{reload}`, then confirms the old runtime remains active.

## Verification

Executed exactly:

`cargo fmt --all --check && cargo test -p pi --lib extension_runtime_set --locked && cargo test -p pi --lib agent_session::extension --locked && cargo clippy -p pi --lib --tests --locked -- -D warnings`

Result: PASS.

- `extension_runtime_set`: 17 passed, 0 failed
- `agent_session::extension`: 16 passed, 0 failed
- clippy: `OK`

## Self-review

Reviewed the full diff from the required base with `git diff --check`, diff stat, and the complete diff for all three allowed source files. The change remains limited to provider prevalidation, failed-reload lifecycle restoration, and focused test support. No product API or compatibility shim was added. No concerns remain.
