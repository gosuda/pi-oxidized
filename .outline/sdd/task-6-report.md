# Task 6 report — full cross-binary RPC parity replay

## Status

**GREEN — raw-event cross-binary RPC parity achieved.** The replay executes all 31 authoritative RPC commands over 41 dependency-valid steps. Rust and source-pinned TypeScript each emitted 142 records; all 142 records are identical after normalizing only generated IDs, timestamps/elapsed values, and temporary/repository paths. Both processes exited 0.

Final evidence: `target/verification/rpc-parity/run-1785095001026/result.json` (`equal: true`). No full compatibility matrix was run.

## Review findings closed

### 1. Raw streaming events

Removed streaming-event collapse from the parity normalizer and its tests. Every `message_update` and `tool_execution_update` is now retained and compared one-for-one, including partial payload shape.

Rust's bash collector emitted a duplicate final partial after the throttle had already flushed, producing three updates where TypeScript produced two. It also represented streamed empty details as null/absent where TypeScript emits `{}`. The bash boundary now omits the duplicate final send, preserves initial-details omission, and emits `{}` on the streamed partial. `bash_stream_updates_match_source_event_shape` asserts the exact two-update sequence and details shape.

### 2. Extension command provenance

Removed slash-command path-prefix provenance guessing. `AgentSession` now caches the `ResourceLoader` extension snapshot's resolved `SourceInfo` by configured and resolved path, refreshes it with resource snapshots, and uses that metadata for path-bearing extension commands. Synthetic provenance remains only for inline and pathless registrations.

`extension_command_source_info_preserves_loader_metadata` covers CLI, user/project local settings, user/project auto discovery, package origin, inline, and pathless sources.

### 3. Canonical user-message persistence

Moved text-only user-message canonicalization to shared `AgentMessage` serialization. On-disk session JSONL now stores text as `[{[0m"type":"text","text":"..."}]`; mixed text/image blocks pass through unchanged. The RPC-only recursive rewrite was removed.

The focused message and `SessionManager` tests cover canonical serialization, Rust persist/reload, source-shaped TypeScript fixture loading, mixed-content preservation, and the absence of unrelated recursive RPC rewriting.

### 4. Replacement state and API-key policy

Added `get_state` immediately after `clone` and immediately after `new_session`, before switching away. The scenario now has 41 steps without dropping or excluding any authoritative command.

The replacement factory now retains the parsed invocation model policy, resolves it against each replacement runtime, preserves saved-session pre-provisioning, and applies the invocation API key to the final resolved model. This removed the fresh-session `--api-key requires a model` diagnostic and prevents a new session from falling back to the synthetic unknown model. Saved-session model restoration remains intact.

The first raw replay with the new probe exposed the unknown-model defect at normalized record 134. After fixing replacement model resolution, the replay became GREEN at 142/142 records.

## Commits

Original Task 6 sequence:

- `1ac2ea8` — `fix(session): do not invoke extension shutdown handler on session replacement`
- `6fc5f6e` — `fix(rpc): match upstream wire events, command source info, and extension load order`
- `716f1cf` — `test(rpc): replay full cross-binary parity matrix`
- `05e500c` — `fix(rpc): preserve upstream extension command order`
- `563b6c6` — `fix(rpc): canonicalize user message wire content`
- `ec5d4b5` — `fix(agent): omit absent tool result details`
- `4485483` — `fix(edit): match upstream trailing newline diff`
- `6cc1ce3` — `fix(session): retain bash results in live history`
- `615a952` — `fix(session): persist source-pinned model entries`
- `a81ee09` — `fix(session): restore CLI runtime policy after fork`
- `821c960` — `fix(compaction): wrap Ok(None) manual boundary with 'Compaction failed: ' prefix`

Task 6 review closure:

- `209f2ab` — `fix(session): persist canonical user text blocks`
- `e6ffd96` — `fix(rpc): preserve resolved command source metadata`
- `fe1e75a` — `fix(session): apply API key after replacement model resolution`
- `e95824e` — `fix(rpc): preserve and match raw streaming updates`
- `25b6609` — `test(rpc): inspect replacement session state`
- `c83d6e7` — `style(task6): apply Rust formatting`
- `07c369e` — `test(rpc): type raw parity fixtures`
- `1fa4751` — `fix(rpc): tighten command provenance plumbing`
- `3959055` — `fix(session): resolve invocation model on replacement`

## Verification

- `cargo test -p pi --lib modes::rpc --locked` — 102 passed.
- `cargo test -p pi --lib core::sessions --locked` — 59 passed, 1 ignored.
- `cargo test -p pi --lib cli::entry::tests::replacement_ --locked` — 3 passed after the final replacement fix.
- `cargo test -p pi --lib bash_stream_updates_match_source_event_shape --locked` — 1 passed.
- `cargo test -p pi --lib extension_command_source_info_preserves_loader_metadata --locked` — 1 passed.
- `cargo test -p pi-agent --lib message::tests --locked` — 6 passed.
- `bun test packages/extension-host/tests` — 168 passed.
- `bun test scripts/verification/rpc-parity.test.ts` — 9 passed, 25 assertions.
- `cargo build -p pi --release --locked` — passed; the release binary was rebuilt before the final replay.
- `bun run scripts/verification/rpc-parity.ts` — 31 commands / 41 steps, 142 Rust records / 142 TypeScript records, all normalized records identical, both processes exit 0.
- `cargo fmt --all --check` — passed.
- `cargo clippy -p pi -p pi-agent --all-targets --locked -- -D warnings` — passed.
- `bun run check` — passed.

## Normalization and coverage boundary

The harness derives command coverage from the authoritative source-pinned TypeScript `RpcCommand` union and fails for missing or extra scenario variants. It normalizes only generated UUID/entry IDs with reference-preserving first-seen mappings, timestamps/elapsed values, and run-specific temporary/repository paths. It does not collapse, sort, omit, or exclude streaming events, payload fields, commands, model/provider values, message content, or errors.
