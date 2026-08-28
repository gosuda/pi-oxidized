# Floor ledger: session JSONL reopen (scan/parse + state reconstruction)

Owning R2 hot rows (lane 9): *JSONL scan/parse on reopen*, *Session/tree state
reconstruction*. State: **AT-FLOOR (terminal — single-pass open at iteration 14; 1.506 µs/entry vs 0.764 µs floor ⇒ 1.97x).**

## Contract (from call sites, tests, signatures — never internals)

- `SessionManager::open(path, dir, agent_dir) -> Result<SessionManager, SessionError>` — crates/pi/src/core/sessions/mod.rs:1214 (entry via `set_session_file` mod.rs:282-334). Owes callers: full typed entry list, by-id index, parent/child tree linkage, labels, and leaf — reconstructed from the file alone; the file is not modified (sha-prefix stability asserted by the lane harness, session-timing.rs:289-296).
- `get_entries() -> Vec<&SessionEntry>` (mod.rs:634) returns borrowed *typed* entries — typed availability is owed before the first borrow, with no fallibility; consumers (transcript render mod.rs:644+, export, branching mod.rs:1753+) read message payloads immediately.
- Round-trip: `migrate_values_to_current` + `file_entry_from_value` per line (entries.rs); migration rewrites only when a migration fired (mod.rs:318-325).
- Tests: reopen-after-move appends land in the moved file (mod.rs:1707-1730); branch-from-reopened-leaf targets the reopened dir (mod.rs:1732-1760).

Boundary classification: reading is **interior** to the process, but the *format* is
the JSONL v3 wire format (**boundary**) — reopen must accept every file the append path
and the upstream TS implementation produce. Unresolved channels: none (file set is
enumerable; no reflection/string dispatch on the load path).

## Floor (computed)

Per entry the contract forces: read the bytes, parse the line into the typed entry
(owned fields; the borrowed-ref API forces typed materialization by first use), insert
into by_id, push into the entry vector.

```
read 170 B (warm page cache)        10 ns   (floorkit readback)
typed parse, owned fields         683.8 ns   (sonic-rs typed parse, achievable constant)
by_id insert                         ~50 ns
Vec push (amortized)                 ~20 ns
                                   ---------
floor                              763.8 ns ~= 0.76 us/entry
```

## Measured cost

Fresh run 2026-08-27: `session-timing --mode reopen --entries 5000` => median
29.758 ms => **5.952 us/entry** (rs 5.65%, passes noise gate). R8 recorded no trusted
reopen cell (TS reopen noisy at all counts); this fresh Rust-side distribution is the
decomposition anchor and is recorded as a single-implementation measurement, not a
paired claim.

**Multiple = 5.952 / 0.764 = 7.79x => OPEN.**

## Cost decomposition (sums to 5.95 us/entry)

| Category | Cost | Method |
|---|---|---|
| read + serde_json parse + Value tree build/drop (parse_str 11.0 M Ir, StrRead/MapAccess family, Value drop glue) | 2.57 us | profiler attribution: ~45% of 213 M unit Ir (bench sha2 84.3 M excluded) applied to 5.72 us CPU |
| typed conversion (entries.rs `file_entry_from_value` + message.rs) | 1.14 us | profiler attribution: ~20% of unit Ir |
| allocation (malloc/free family 59.2 M Ir) | 1.60 us | profiler attribution: ~28% of unit Ir |
| by_id index + leaf/labels rebuild | 0.46 us | profiler attribution: ~8% of unit Ir |
| syscalls (read path) + residual | 0.18 us | subtraction (closes the sum) |

## Addressable-overhead notes for Phase 5

The dominant term is the serde_json `Value` round-trip (parse to Value, convert to
typed): a direct typed parse (the 683.8 ns achievable constant) plus arena allocation
targets ~3 us/entry of the decomposition. Boundary: byte-identical acceptance of
existing v3 files is non-negotiable (reopen of upstream-written files must stay green).
