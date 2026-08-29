# Task 17 Report — VER-ALIGN

- **Issue:** https://github.com/metaphorics/pi-oxidized/issues/145
- **Status:** `BLOCKED`
- **Commit:** `feat(ver-align): pin reference baseline and portable schemas` (single commit on `feat/ver-align-canonical-pin`)

## What changed

1. **Workflow pin** — `.github/workflows/release-verification.yml` checkout `ref` and `rev-parse` assertion now both use `8fa7eebd235355522c8104166b4f1f959b4e2f10`.
2. **Reconstruction comment** — `scripts/reconstruct-provider-data.ts` pin comment names the same canonical SHA.
3. **Schema generator** — `scripts/generate-tool-schemas.ts` selects exactly the seven portable tools (`read`, `bash`, `edit`, `write`, `grep`, `find`, `ls`) from the canonical registry, fails if any required tool is missing, and tolerates reference-only platform tools such as `powershell`. No compatibility branch; schemas emitted only by the generator.
4. **Alignment witness** — `package.json` script `verify:alignment` → `scripts/verification/alignment.ts` (+ `alignment.test.ts`) freezes pin literals, live `.references/pi` HEAD, and portable selection behavior.
5. **Generated fixtures** — ran `bun run scripts/generate-tool-schemas.ts`; wrote the seven `.agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/*.json` files with no hand edits.
6. **Product boundary** — no `crates/*/src` changes.

## Consistency / scope / verification checks

- **Consistency:** Grepped for `4488ad55c18f07ae89a489096c90de8667b3adfb`; remaining mentions are only the witness’s explicit stale-SHA detector. Both workflow sites and the reconstruct comment use the canonical SHA. Generator loops all use `REQUIRED_TOOL_NAMES`.
- **Scope:** Diff limited to owned files listed in the brief (`release-verification.yml`, `reconstruct-provider-data.ts`, `generate-tool-schemas.ts`, `alignment.ts`, `alignment.test.ts`, `package.json`, seven schema fixtures, this report). No product source edits.
- **Verification (owned / focused):**
  - `bun run scripts/generate-tool-schemas.ts` → wrote 7 schemas (powershell not emitted)
  - `bun run verify:alignment` → `ALIGNMENT_WITNESSES_OK`
  - `bun test scripts/verification/alignment.test.ts` → 10 pass / 0 fail
  - `bun run check` → pass after owned undefined-guard fix
- **Verification (brief exact chain):** **FAILED** (see blockers). Did not weaken any gate.

## Blockers (out of VER-ALIGN ownership)

Exact command:

`bun run scripts/generate-tool-schemas.ts && bun run verify:alignment && bun test scripts/verification/alignment.test.ts && cargo nextest run --workspace --all-features && bun run check && bun test scripts packages`

Failed after owned steps on gates that require non-owned edits:

1. **`pi core::tools::bash::tests::schema_matches_typebox_fixture`** — Rust schemars description is still `"Bash command to execute"`; canonical TypeBox fixture is now `"Shell command to execute"`. Fix requires `crates/pi/src/core/tools/bash.rs` (forbidden product boundary).
2. **`scripts/verification/rpc-parity.test.ts`** — authoritative RPC union grew to 33 commands (`clear_queue` uncovered). Fix requires rpc-parity scenario ownership, not VER-ALIGN.
3. **`bun run scripts/reconstruct-provider-data.ts`** — wrapper/catalog mismatch (`baseten`, `qwen-token-plan-individual` present in reference wrappers, absent from committed catalog). Fix requires catalog/product follow-on, not VER-ALIGN.
4. **Additional red noise outside ownership** — `bootstrap_pi_startup_benchmark_guard`, `runtime_feedback_runtime_warning_does_not_exit_bootstrap`, extension-host/foundation prerequisites (missing debug binary / provider manifest). Not repaired here.

## Issue acceptance vs brief verifier

Issue #145 acceptance (pin literals + checkout reproduction + no hand-edited generated artifacts) is met by the owned diff and live `git -C .references/pi rev-parse HEAD` == `8fa7eebd235355522c8104166b4f1f959b4e2f10`.

The brief’s full workspace verifier cannot go green without out-of-scope product/parity repairs. Returning **BLOCKED** rather than editing `crates/*/src`, hand-editing schemas, or skipping assertions.

## Non-goals observed

- Did not close issue #145.
- Did not add aliases, fallbacks, shims, or deferred compatibility paths.
- Did not expand into rpc-parity, provider-catalog, or bash tool product fixes.
