# Pi Documentation (Rust port)

This is the user documentation for the Rust port of pi, enumerated from the
reference corpus at pin `8fa7eebd235355522c8104166b4f1f959b4e2f10`. The topic
list below is verified against that corpus by `bun run scripts/verification/docs-evidence.ts`;
the count is computed at checker time, never hardcoded.

Every shipped topic carries an evidence manifest at `docs/evidence/<topic>.json`
binding its behavioral claims to transcript artifacts produced by the current
toolchain (executed `--help` snapshots of `target/release/pi`, session-interop
fixtures, rpc-parity transcripts, and e2e-smoke runs). Topics that cannot yet
be proven sit on the pending list below — they are never described in
aspirational prose.

## Topics

<!-- doc-c:index-shipped-begin -->
- [Quickstart](quickstart.md) — install, authenticate, first session. [evidence](evidence/quickstart.json)
- [Using Pi](usage.md) — CLI reference and command surface. [evidence](evidence/usage.json)
- [Providers](providers.md) — provider environment variables. [evidence](evidence/providers.json)
- [Security](security.md) — project trust and tool gating. [evidence](evidence/security.json)
- [Settings](settings.md) — config directories and the config TUI. [evidence](evidence/settings.json)
- [Keybindings](keybindings.md) — shortcut surface exercised by e2e. [evidence](evidence/keybindings.json)
- [Sessions](sessions.md) — session flags and storage. [evidence](evidence/sessions.json)
- [Compaction](compaction.md) — compaction and steering in e2e. [evidence](evidence/compaction.json)
- [Extensions](extensions.md) — extension loading and host entrypoints. [evidence](evidence/extensions.json)
- [Skills](skills.md) — skill discovery flags. [evidence](evidence/skills.json)
- [Prompt templates](prompt-templates.md) — template discovery flags. [evidence](evidence/prompt-templates.json)
- [Themes](themes.md) — theme discovery flags. [evidence](evidence/themes.json)
- [Pi packages](packages.md) — install/remove/update/list/config commands. [evidence](evidence/packages.json)
- [Models](models.md) — model selection flags and thinking levels. [evidence](evidence/models.json)
- [Custom providers](custom-provider.md) — registering a provider from an extension. [evidence](evidence/custom-provider.json)
- [SDK](sdk.md) — Rust crate entrypoints pi, pi-agent, pi-ai. [evidence](evidence/sdk.json)
- [RPC](rpc.md) — headless RPC mode. [evidence](evidence/rpc.json)
- [JSON mode](json.md) — JSONL event output. [evidence](evidence/json.json)
- [Environment variables](environment-variables.md) — executed environment table. [evidence](evidence/environment-variables.json)
- [Session format](session-format.md) — JSONL session formats v1–v3. [evidence](evidence/session-format.json)
- [Shell aliases](shell-aliases.md) — alias recipes over executed flags. [evidence](evidence/shell-aliases.json)
- [Development](development.md) — building and verifying this port. [evidence](evidence/development.json)
- [Agent harness](harness.md) — the pi-agent crate surface. [evidence](evidence/harness.json)
- [Search tools](search.md) — built-in read-only tools. [evidence](evidence/search.json)
- [Telemetry](telemetry-schema.md) — telemetry flag and schema status. [evidence](evidence/telemetry-schema.json)
<!-- doc-c:index-shipped-end -->

## Pending topics

These corpus topics are not yet shippable with evidence. They carry no ported
page until their blocker lands; the checker rejects any attempt to register
them earlier.

<!-- doc-c:index-pending-begin -->
- containerization — sandboxing backends are not ported — unported-feature
- llama-cpp — the /llama router integration is not ported — unported-feature
- terminal-setup — terminal-visual topic; sidecars must carry the landed transcript schema — TUI-CLOSE
- termux — terminal-visual topic; sidecars must carry the landed transcript schema — TUI-CLOSE
- tmux — terminal-visual topic; sidecars must carry the landed transcript schema — TUI-CLOSE
- tui — terminal-visual topic; sidecars must carry the landed transcript schema — TUI-CLOSE
- windows — terminal-visual topic; sidecars must carry the landed transcript schema — TUI-CLOSE
<!-- doc-c:index-pending-end -->

## Evidence program

See [evidence.md](evidence.md) for the doc-evidence ledger that mechanically
verifies this tree, and [compatibility.md](compatibility.md) for the generated
version-pin matrix.
