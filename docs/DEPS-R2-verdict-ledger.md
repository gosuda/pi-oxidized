# DEPS-R2 verdict ledger

Per-change record of every shipped-exposure classification made under the DEPS-R2
remediation runbook (`docs/DEPS-R2-remediation-runbook.md`, issue #128). This is the
DEPS-R2 view of the campaign's invariant ledger required by EXT-23: every shipped-input
change commit carries either seven-target evidence or a complete E1–E4 exemption bundle,
and the corresponding row here is the citation. Verdicts are **per change** — no package
carries a permanent class label; a package reclassified after a runtime import or field
move immediately takes its new verdict.

## Row format

Rows are emitted by the checker itself and pasted verbatim:

```
bun run verify:dependency-exposure classify --subject <kind:name> \
  --reference scripts/verification/fixtures/dependency-exposure/reference --emit-ledger-row
```

| Column | Meaning |
|---|---|
| `head` | Commit the classification ran against. For sanity rows this is the reference capture head (the classification describes the captured baseline); for live remediation rows, the post-fix commit the verdict was recorded on. |
| `date` | Reference capture date (the baseline the verdict was decided against). |
| `subject` | `npm:<name>` / `crate:<name>` / `tool:rust-toolchain\|bun-runtime\|bun-bundler`. |
| `class` | `S` (full seven-target post-audit) or `E` (complete E1–E4 bundle; only the lane is skippable). |
| `checks` | `E1..E4` statuses; any `fail` **or** `undecidable` forces `S`. |

Every live row must be accompanied by an entry in the records list below (advisory/yank
citation, gates actually run, commit SHA of the remediation). A row without its record
entry is an audit failure at DEPS-D1.

## Ledger

| head | date | subject | class | checks |
|---|---|---|---|---|
| b90362dc | 2026-08-26 | npm:typebox | S | E1:fail E2:fail E3:pass E4:pass |
| b90362dc | 2026-08-26 | npm:@types/bun | E | E1:pass E2:pass E3:pass E4:pass |
| b90362dc | 2026-08-26 | tool:bun-runtime | S | E1:pass E2:fail E3:fail E4:fail |

## Records

- **b90362dc / npm:typebox — sanity, not a remediation.** Known-member anchor from the
  checker `self-check` at DEPS-R2 landing: production-field position
  (`packages/extension-host/package.json` `dependencies`, pre and post) and bundled into
  the shipped sidecar (metafile inputs under `.references/pi/node_modules/typebox/`).
  Any future typebox remediation is Class S: full seven-target lane including both musl
  per-artifact proofs.
- **b90362dc / npm:@types/bun — sanity, its recorded verdict.** Complete E1–E4 bundle:
  devDependencies-only across all three surfaces, zero of the 2493 metafile inputs, no
  shipped-byte-producing invocation, none of the staged inputs. A future @types/bun bump
  may skip only the seven-target lane; lockfile law, advisory scans, and SBOM diff still
  apply. (In the scheduled Bin M epoch it nonetheless keeps its full seven-target gate —
  zero scheduled epoch member is pre-classified exempt; Class E exists only for
  execution-time out-of-band/lifecycle changes carrying this evidence bundle.)
- **b90362dc / tool:bun-runtime — sanity.** Bun embedded-runtime bumps change the
  compiled sidecar bytes and stage the runtime into the runtime-bundle archive: Class S
  by definition.
