# Task 8 review fix

## Outcome
DONE

## Base
`1ebea90`

## Commit
`test(pi): prove reload rejection transaction`

## Fix
- Added a private `#[cfg(test)]` prepared-generation seam. Production reloads still use the ordinary builder.
- Drove injected native-only and invalidated compatible replacements through `restart_and_rewire`.
- Asserted rejection preserves generation 1 and the old provider map/epoch, and reaps every rejected replacement.
- Asserted native-only rejection occurs before `flags.set`; invalidation reaches prepared flag sync but loses no provider/publication state.
- Extended the active-compat-with-inactive-sibling checks for both prepared-native and invalidation cells.

## Coverage manifest
Updated `.outline/sdd/task-8-report.md`: 30 covered, 0 gaps, 0 deferred.

## Self-review
- Checked the new tests against the complete review finding and Task 8 matrix.
- Confirmed the injected path calls the real `restart_and_rewire` transaction and the seam is compiled only in tests.
- Confirmed prepared admission precedes flags, provider mutation, and publication; invalidation preserves the old provider map and reaps the replacement.

## Verification
Passed exactly:

```text
cargo fmt --all --check && cargo test -p pi --lib extension_runtime_set --locked && cargo clippy -p pi --lib --tests --locked -- -D warnings
```

Focused tests: 27 passed.
