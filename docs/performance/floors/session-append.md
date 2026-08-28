# Floor ledger: session JSONL append (serialization + file append)

Owning R2 hot rows (lane 9): *JSONL entry serialization*, *File append (bytes written)*;
also carries the *session append* leg of lanes 3 and 8. State: **AT-FLOOR (terminal — maintained assistant-present state at iteration 21; 5.275 µs/entry vs 3.735 µs floor ⇒ 1.41x).**

## Contract (from call sites, tests, signatures — never internals)

- `SessionManager::append_message(&mut self, message: &AgentMessage) -> Result<String, SessionError>` — crates/pi/src/core/sessions/mod.rs:662. Owes its callers: a returned unique entry id, tree linkage to the current leaf, and durability of the entry as one complete JSONL line.
- Per-entry wire obligations (callers + tests): unique 8-hex id (`next_id`, collision-checked, mod.rs:540-542); ISO timestamp; parentId linkage; camelCase typed JSON line (entries.rs serde).
- `append_prefix_stability` (mod.rs:1767): existing file bytes are byte-stable across appends — each entry must land as exactly one complete trailing line.
- `failed_append_does_not_advance_tree` (mod.rs:1792): a failed append must roll back entry, leaf, and index — the append path owes atomicity at entry granularity.
- `deferred_write_until_first_assistant` (mod.rs:1685): no file before the first assistant message; after the header flush, every entry is appended individually (`persist_entry_at` mod.rs:445-503, `append_line` mod.rs:1505-1523).
- Streaming consumers append per message end (crates/pi/src/core/agent_session/persistence.rs, `persist_message_end`) and per bash result (agent_session/bash.rs, `flush_pending_bash_messages`) — per-entry latency is on the interactive path.

Boundary classification: the JSONL line is the **session JSONL v3 on-disk wire format**
(boundary; e.g. upstream TS reopen interop, session-interop harness). The append
machinery above the line format is **interior** (all callers in-tree). Unresolved
channels: none — the file consumer set is enumerable (reopen, branch, export_html,
session_transfer).

## Floor (computed)

Per entry, the contract forces: serialize one ~170 B line, generate one unique id and
one timestamp, one index insert, and at least one `write(2)` of the line at achievable
append cost on this filesystem (page-cache append; no per-entry fsync is forced —
durability contract is line-atomicity, `sync_all` rides only the first-flush rewrite,
mod.rs:489-491).

Arithmetic (constants measured 2026-08-27, see index):

```
serialize 170 B typed line   276.2 ns   (sonic-rs typed serialize, achievable)
unique id (PRNG, no syscall)    ~20 ns
timestamp (vDSO clock_gettime)  ~25 ns
index insert (HashMap)          ~50 ns
write(2) 170 B, held-open zfs 3363.4 ns   (floorkit)
                            -------------
floor                       3734.6 ns ~= 3.73 us/entry
```

## Measured cost

Fresh run 2026-08-27: `session-timing --mode append --entries 5000` => median
91.686 ms => **18.337 us/entry** (rs 5.32%, passes noise gate; taskset -c 20-40,
worktree 48696b5). R8 recorded 116.42 ms warm (23.3 us/entry) on the unpinned machine;
the fresh number is the decomposition anchor.

**Multiple = 18.337 / 3.735 = 4.91x => OPEN.**

*Superseded by PERF-T11: terminal AT-FLOOR at 1.41x (iteration 21; 5.275 µs/entry
vs 3.735 µs floor, maintained assistant-present state). See
[t11-iterations.md](../t11-iterations.md), iteration 21.*

## Cost decomposition (sums to 18.34 us/entry)

| Category | Cost | Method |
|---|---|---|
| openat+close per append (file re-opened per entry, mod.rs:1510-1513) | 2.063 us | subtraction: floorkit open+write+close 5.426 us minus held-open write 3.363 us |
| write(2) 170 B, zfs append | 3.363 us | floorkit held-open measurement |
| entry serialization + message build (serde to_value/from_value/to_string pipeline) | 6.87 us | profiler attribution: callgrind Ir share 53.2% (entries.rs 112.6 M + message.rs 75.0 M + serde ser 6.4 M + memcpy 7.7 M of 379.4 M unit Ir, bench sha2 excluded) applied to the 12.91 us CPU remainder |
| `has_assistant` scan + iteration (mod.rs:453-457; O(entries) per append => O(n^2) per session) | 1.28 us | profiler attribution: filter_map/iter/non_null adapters 37.5 M Ir = 9.9% of unit Ir; grows quadratically with entry count |
| allocator (malloc/free) | 1.03 us | profiler attribution: 8.0% of unit Ir |
| id + timestamp + leaf/bookkeeping remainder | 3.73 us | subtraction (residual closes the sum) |

## Addressable-overhead notes for Phase 5 (from the decomposition, not the internals)

Hold the append fd open (removes 2.06 us), single-pass typed serialization (targets the
6.87 us Value pipeline), maintain the assistant-present flag instead of rescanning
(removes the quadratic term). Boundary: the JSONL line bytes themselves are wire
format — any rewrite must keep `append_prefix_stability` and the interop reopen green.
