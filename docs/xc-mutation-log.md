# XC-CLOSE disposable-worktree mutation log (M1-M22)

Recorded for issue #60 ([XC-CLOSE]) closing issue #19. Every mutation was
applied to production source in the disposable worktree `/tmp/xc-close-wt`
(branch `xc-close-exec`), its named witness was run scoped, the failure was
recorded, the file was reverted (`git checkout --` / backup restore), and
cleanliness was verified (`git status --porcelain` empty + production anchor
restored verbatim) before the witness was re-run green.

- Base tree: `aafca34` on `feat/ver-align-canonical-pin`
- Protocol per mutation: apply -> witness FAIL observed -> revert -> porcelain
  empty -> anchor restored verbatim -> post-revert witness PASS
- Raw driver output: pass one `/tmp/xc-mutations-M1-M22.log` (17/22 direct),
  fix-up pass `/tmp/xc-mutations-fixups.log` (M8, M12, M16, M19, M22
  re-formulated; findings recorded below — the first-pass variants and why they
  were unobservable are part of the evidence)

## Summary: 22/22 mutations observed failing their named witness before revert

| ID | Mutation (corrected formulation) | Named witness (scoped) | Verdict |
|----|----------------------------------|------------------------|---------|
| **M1** | lean-runner hello gates on compatibilityVersion | `lean.test.ts :: matching protocolVersion acks even with a foreign compatibilityVersion` | FAIL observed, revert clean, post-revert PASS |
| **M2** | server validate_hello rejects foreign compatibility_version | `pi-ext :: hello_answers_with_compiled_constants_and_ignores_compatibility` | FAIL observed, revert clean, post-revert PASS |
| **M3** | host.ts drops the compatibilityVersion check | `host.test.ts :: compatibility version mismatch terminates host` | FAIL observed, revert clean, post-revert PASS |
| **M4** | adapters register_tool switched to last-wins | `pi-ext registry_first_registration_wins + pi registry_first_registration_wins_for_duplicates` | FAIL observed, revert clean, post-revert PASS |
| **M5** | build_snapshot strips the command suffix (duplicate cmd twice) | `pi :: command_suffix_disambiguation_observed` | FAIL observed, revert clean, post-revert PASS |
| **M6** | reference runner.ts getShortcuts drops restrictOverride guard | `scripts/verification/xc-matrix.test.ts (reserved_shortcut_guard_present)` | FAIL observed, revert clean, post-revert PASS |
| **M7** | host tool_call drops canonicalJsonEqual input comparison | `endpoint-conformance :: tool_call block with terminate forwards through both modes` | FAIL observed, revert clean, post-revert PASS |
| **M8** | lean input handled short-circuit RESPONSE dropped | `endpoint-conformance :: input handled short-circuit matches across modes` | FAIL observed, revert clean, post-revert PASS |
| **M9** | host removes the specialized before_provider_headers case | `endpoint-conformance :: before_provider_headers in-place mutation and null-delete match` | FAIL observed, revert clean, post-revert PASS |
| **M10** | host tool_call drops the result spread (terminate lost) | `endpoint-conformance :: tool_call block with terminate forwards through both modes` | FAIL observed, revert clean, post-revert PASS |
| **M11** | sanitize_run bypasses the parser (raw run bytes through) | `pi-ext :: hostile_fragments_across_generations_no_leak` | FAIL observed, revert clean, post-revert PASS |
| **M12** | slot-lines clamp application loosened 2x (effective 8192) | `pi-ext :: clamps_line_and_run_counts` | FAIL observed, revert clean, post-revert PASS |
| **M13** | hyperlink scheme filter inverted (javascript: admitted) | `pi-ext :: drops_invalid_hyperlink_keeps_rest` | FAIL observed, revert clean, post-revert PASS |
| **M14** | parser state reused across slot pushes (thread-local) | `pi-ext :: resets_parser_ground_state_across_slots` | FAIL observed, revert clean, post-revert PASS |
| **M15** | terminal-input 4 ms timeout race removed | `scaling.test.ts :: slow handler times out once, disables only itself` | FAIL observed, revert clean, post-revert PASS |
| **M16** | runner.onError crash forwarding dropped (no extensionError) | `acceptance.test.ts :: host forwards crash as nonretryable extensionError` | FAIL observed, revert clean, post-revert PASS |
| **M17** | captureReplacementToken drops the markStale call | `host.test.ts :: per-command replacement staleness suite` | FAIL observed, revert clean, post-revert PASS |
| **M18** | lean handleControlEvent drops requestId extraction | `lean.test.ts :: tool.execute honors tool.cancel with a cancelled error frame` | FAIL observed, revert clean, post-revert PASS |
| **M19** | host resolution falls back to a PATH search | `pi-ext :: m19_no_path_fallback_when_file_exists_on_disk + m19_explicit_none_params_never_discover_stray_executable (both observed failing; m19_env_overrides_never_fall_through_to_path passed under this mutation)` | FAIL observed, revert clean, post-revert PASS |
| **M20** | discovery precedence flipped (package.json over pi-extension.json) | `pi :: m20_pi_extension_json_wins_over_package_json` | FAIL observed, revert clean, post-revert PASS |
| **M21** | classify no longer rejects prebundled .mjs | `pi :: m21_prebundled_mjs_rejected_not_ts_compat` | FAIL observed, revert clean, post-revert PASS |
| **M22** | lean lexical exclusion admits the /compat subpath alias | `lean.test.ts :: excluded-specifier and preload guards agree on aliased typebox and anchored pi-ai` | FAIL observed, revert clean, post-revert PASS |

## Fix-up findings (recorded honestly, part of the evidence)

Five first-pass formulations were unobservable by their named runtime witness
and were re-formulated; both passes are recorded:

- **M8** dropping the runHooks `return false` PASSED the witness: the
  conformance fixture registers a single input handler, so hook-chain
  short-circuit is unobservable there (that variant is proven statically by
  `xc-dispatch.test.ts::M8`). The observable contract is the handled
  short-circuit response, mutated in the fix-up.
- **M12** changing `MAX_SLOT_LINES` 4096 -> 8192 PASSED the witness: the
  witness builds its input from the constant symbolically, so a constant-only
  change is tautologically consistent. The load-bearing property (clamp
  applied at the cap) was mutated at the application seam (effective 8192);
  the constant value itself is pinned by the contract document row (§9).
- **M16** rethrowing in `handleLifecycleHook`'s catch PASSED the witness:
  handler throws are isolated inside the reference `ExtensionRunner`, so the
  host catch never fires for them (that catch is proven statically by
  `xc-deadline.test.ts::M16`). The runtime-observable isolation is the
  `runner.onError` -> `extensionError` forwarding, mutated in the fix-up.
- **M19** first-pass anchor was ambiguous (production + test match arm);
  re-anchored on the `resolve_with_fallback` tail.
- **M22** the /compat exclusion row lives in `excluded-specifier and preload
  guards agree on aliased typebox and anchored pi-ai` (asserting
  `findExcludedImport` and the preload fixture regex agree), not in `detects
  the compat graph`; witness name corrected.

## Final consolidated witness sweep (clean tree aafca34, post mutation log)

Run 2026-08-27T17:08:27Z:

- `cargo test -p pi-ext --all-targets --locked`: 194 pass, 0 fail (177 + 7 + 10)
- `bun test packages/pi-tui-protocol/tests`: 26 pass, 0 fail
- `bun test packages/extension-host/tests`: 297 pass, 0 fail
- `bun test scripts/verification/xc-{handshake,dispatch,matrix,deadline}.test.ts`: 72 pass, 0 fail

## Zero-skipped-witness-rows mechanical check

Section-level coverage over all 16 sections of
`docs/extension-compatibility-contract.md`: every rule section carries a
`witness:`/`mutation:` reference (§2 method registry, §7 canonical 33-hook
classification, and §8 wire shapes gained explicit witness references in this
commit); every rule table with a Witness column has a filled cell in every
data row (§1 constants, §3.1 handshake matrix, §7.1 dispatch lattice,
exemption table); no TODO/TBD/unwitnessed-rule/deferred markers. Result:
**PASS - zero skipped witness rows**.
