# Task 8: Exhaustive reload admission

## Outcome
DONE

## Commit
`fix(pi): keep native runtime startup-only` — `1ebea90`

## Admission predicates
- Current: `Generation::has_one_active_compat_endpoint`, reached through `PublishedRuntimeState::reloadable`.
- Prepared: `Generation::is_single_compat_replacement`, checked after an all-or-nothing build and before flag sync, provider validation, provider registration, or generation publication.
- Final provider replacement and generation publication share the state lock. An invalidation that wins this lock prevents any provider mutation.

## Reload admission matrix

`C1` stale; `C2` zero active; `C3` one active `TsCompat`; `C4` one active `Native`; `C5` more than one active; `C6` one active `TsCompat` plus inactive siblings.

`P1` build failure; `P2` one `TsCompat`; `P3` one `Native`; `P4` more than one endpoint; `P5` invalidation before publication.

| Current \ Prepared | P1 build failure | P2 one `TsCompat` | P3 one `Native` | P4 multi-endpoint | P5 invalidation |
|---|---|---|---|---|---|
| C1 stale | **Covered** — `current_reload_admission_covers_every_active_endpoint_class` calls `restart_and_rewire`; current gate rejects before build, generation, or provider mutation. | **Covered** — same executable current gate check. | **Covered** — same executable current gate check. | **Covered** — same executable current gate check. | **Covered** — same executable current gate check. |
| C2 zero active | **Covered** — `current_reload_admission_covers_every_active_endpoint_class` rejects through `restart_and_rewire`, asserting generation 1 and unchanged provider epoch. | **Covered** — same executable current gate check. | **Covered** — same executable current gate check. | **Covered** — same executable current gate check. | **Covered** — same executable current gate check. |
| C3 one active `TsCompat` | **Covered** — `runtime_set_failed_replacement_keeps_old_generation_published`. | **Covered** — `single_endpoint_publication_remains_reloadable`. | **Covered** — `prepared_native_reload_rejects_before_mutating_published_state` injects one `Native` generation into `restart_and_rewire`; it observes no `flags.set`, unchanged generation/provider epoch/map, and replacement reaping. | **Covered** — `native_manifest_builds_multi_endpoint_replacement_without_publication` asserts rejected restart, unchanged provider epoch/generation, and old endpoint remains live. | **Covered** — `invalidation_before_publication_keeps_provider_map_and_reaps_replacement` invalidates a prepared compatible replacement in `restart_and_rewire` before publication, then observes the old provider map/epoch and replacement reap. |
| C4 one active `Native` | **Covered** — `current_reload_admission_covers_every_active_endpoint_class` rejects through `restart_and_rewire`, asserting generation 1 and unchanged provider epoch. | **Covered** — same executable current gate check. | **Covered** — same executable current gate check. | **Covered** — same executable current gate check. | **Covered** — same executable current gate check. |
| C5 more than one active | **Covered** — `current_reload_admission_covers_every_active_endpoint_class` rejects through `restart_and_rewire`, asserting generation 1 and unchanged provider epoch. | **Covered** — same executable current gate check. | **Covered** — same executable current gate check. | **Covered** — same executable current gate check. | **Covered** — same executable current gate check. |
| C6 one active `TsCompat` + inactive sibling | **Covered** — `current_reload_admission_covers_every_active_endpoint_class` admits C6 before its direct P3/P5 transaction checks. | **Covered** — `current_reload_admission_covers_every_active_endpoint_class` admits C6; `single_endpoint_publication_remains_reloadable` covers P2. | **Covered** — `current_reload_admission_covers_every_active_endpoint_class` injects P3 into C6 through `restart_and_rewire`, asserting unchanged generation/provider epoch and replacement reap. | **Covered** — `native_manifest_builds_multi_endpoint_replacement_without_publication` rejects P4 before publication. | **Covered** — `current_reload_admission_covers_every_active_endpoint_class` injects P5 into C6 through `restart_and_rewire`, asserting `flags.set` reached the prepared compat runner, unchanged generation/provider epoch, and replacement reap. |

`P3` is exercised both as a direct predicate and as a native-only injected prepared generation at the production transaction boundary. The native-manifest integration case continues to exercise planner-produced `P4`.

## Tally
- Covered: 30
- Gap: 0
- Deferred: 0

## Self-review
- Reviewed the diff against `task-8-brief.md` and `task-8-review-findings.json` after implementation.
- Confirmed the test-only seam is absent from production builds; without it, `restart_and_rewire` calls the ordinary generation builder.
- Independently reviewed the transaction ordering: prepared admission precedes flags/providers/publication; invalidation before the state lock leaves the old provider map published; every rejected injected replacement is stopped and reaped.

## Verification
Passed exactly:

```text
cargo fmt --all --check && cargo test -p pi --lib extension_runtime_set --locked && cargo clippy -p pi --lib --tests --locked -- -D warnings
```

`cargo test` reported 27 passing focused tests.
