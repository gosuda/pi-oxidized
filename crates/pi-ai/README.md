# pi-ai

Provider contracts, transports, models, and credentials.

## Workspace topology

`pi-ai` is a **dependency-free** leaf crate — it has no workspace dependencies.
It is depended on by `pi-agent`, `pi-ext`, and `pi`.

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
| `auth` | Credential and authentication types |
| `catalog` | Model catalog |
| `estimate` | Token estimation |
| `lockfile` | Lockfile management |
| `models_store` | Model store |
| `provider` | Provider trait and error/response types |
| `providers` | Concrete provider implementations |
| `simple_options` | Simplified streaming options |
| `types` | Shared type definitions |

## Public re-exports

### `estimate`

| Symbol | Kind |
|---|---|
| `ContextUsageEstimate` | struct |
| `calculate_context_tokens` | fn |
| `estimate_context_tokens` | fn |
| `estimate_message_tokens` | fn |
| `estimate_messages_tokens` | fn |
| `estimate_text_and_image_content_tokens` | fn |
| `estimate_text_tokens` | fn |

### `provider`

| Symbol | Kind |
|---|---|
| `Provider` | trait |
| `ProviderError` | enum |
| `ProviderResponse` | type |
| `StreamOptions` | struct |

### `simple_options`

| Symbol | Kind |
|---|---|
| `AdjustedMaxTokens` | type |
| `CONTEXT_SAFETY_TOKENS` | const |
| `DEFAULT_CACHE_RETENTION` | const |
| `DEFAULT_MAX_RETRY_DELAY_MS` | const |
| `DEFAULT_THINKING_BUDGET_HIGH` | const |
| `DEFAULT_THINKING_BUDGET_LOW` | const |
| `DEFAULT_THINKING_BUDGET_MEDIUM` | const |
| `DEFAULT_THINKING_BUDGET_MINIMAL` | const |
| `SimpleStreamOptions` | struct |
| `ThinkingBudgets` | struct |
| `ThinkingBudgetsResolved` | struct |
| `adjust_max_tokens_for_thinking` | fn |
| `apply_simple_max_tokens_clamp` | fn |
| `apply_thinking_and_context_clamp` | fn |
| `build_base_options` | fn |
| `clamp_max_tokens_to_context` | fn |
| `clamp_reasoning` | fn |
| `default_thinking_budgets` | fn |

### `types`

All items from the `types` module are re-exported via `pub use types::*`.

## Handshake symmetry

The handshake asymmetry (Mode 1 TypeScript-compat hosts validate both
`protocolVersion` and `compatibilityVersion`; Mode 2 lean and Mode 3 native
endpoints validate only `protocolVersion`) is documented in
`docs/extension-compatibility-contract.md`, the single owner doc. This README
references it; other docs point there rather than restating it.
