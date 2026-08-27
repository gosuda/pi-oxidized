# PERF-G10: Audit of the PERF-R9 floor ledgers and cost decompositions

> Resolves [issue #98](https://github.com/metaphorics/pi-oxidized/issues/98) (PERF-G10).
> Audited artifact: `docs/performance/floors/` (12 unit ledgers + README index),
> re-landed on the integration branch by this ticket (see Finding 1).
> Audit date 2026-08-27; audited tree = this branch's HEAD after the re-anchor
> commit; measurement provenance of the ledgers themselves is worktree 48696b5.
> Method: full read of all 13 files; scripted 93-anchor citation census
> (`file:line` -> symbol at tree, both at 48696b5 and at HEAD); independent
> recomputation of every floor sum, multiple, and decomposition reconciliation;
> per-term floor-completeness and contract-sourcing review.

## Verdict

**PASS with findings.** The four audit questions resolve favorably: floors are
complete against their contracts, no ledger understates work to justify a
rebuild or overstates a floor to certify AT-FLOOR (the AT-FLOOR list is empty
and four units hold OPEN fail-closed), every decomposition category carries a
named measurement method and reconciles with its measured total, and no
contract term is sourced from unit internals. Two process defects (Findings 1
and 2) and two floor-term observations (Findings 5 and 6) were found; 1 and 2
are fixed by this ticket's commits; 5 and 6 are recorded so Phase 5 does not
treat those two terms as physical constants. No classification (OPEN vs
AT-FLOOR vs fail-closed) changes under any plausible correction.

## Finding 1 (Important, fixed): the R9 deliverable was never on the integration branch

The PERF-R9 close comment claims delivery at `fbddde2` on
`feat/ver-align-canonical-pin`. In fact the three R9 commits (eb58229,
49595cc, fbddde2) existed only on the unmerged branch `perf-r9-floors`;
`docs/performance/floors/` was absent from the integration tree, and PERF-T11
(the Phase-5 campaign, blocked by this ticket) would have started from a
phantom artifact. Fixed by cherry-picking the three commits onto this branch
(content identical to fbddde2: 13 files), verified by `git cherry` (three `+`
patches, no conflicts, no content drift).

## Finding 2 (Important, fixed): one citation was wrong at authoring time

`stream-frame-pipeline.md` cited the verification provider leg as
"`streamVerification` :225-306, push at :292-299" in
`scripts/verification/extension.ts`. The function definition is at :178 and
the pre-typed `text_delta` push is at :210; lines 292-299 are the
extension-API `replacement.error` region — a different subject. The R9
review-fix commit (49595cc, "resolve review findings on floor sums and
citations") did not catch it. The underlying contract claim is correct
(verified at HEAD: `stream.push({ type: "text_delta", ... })` at :210,
wired as `streamSimple: streamVerification` at :426). Fixed in the re-anchor
commit.

## Finding 3 (Minor, fixed): citation drift across the integration tree

The ledgers were authored against worktree 48696b5; commits landing between
48696b5 and the branch tip moved ~15 anchors by +6..+21 lines (runtime.rs
uniform +15/+21; schedule.rs +12; pi_tool_dispatch_bench.rs +10;
bench-extension-scaling.ts +20; pty.ts -11), and
`crates/pi-agent/src/tools/tool.rs` was renamed to `crates/pi-agent/src/tool.rs`
(line 234 unchanged). Every cited symbol still existed — the census found zero
fabricated citations — but stale anchors mislead a blind Phase-5 derivation.
Fixed by the re-anchor commit: 30 anchors updated, all verified against the
landing tree by scripted check (30/30 resolve, 0 misses). Drift table for the
record:

| Anchor | cited | @48696b5 | @HEAD (re-anchored) |
|---|---|---|---|
| runtime.rs `paint_frame` def | 4527 | 4528 | 4548 |
| runtime.rs `build_root` (call in paint_frame) | 4599 | 4599 | 4551 |
| runtime.rs `run_interactive_mode` | 6589 | 6590 | 6610 |
| runtime.rs `handle_ui_event` | 1962 | 1962 | 1977 |
| runtime.rs `handle_partial_update` | 2181 | 2181 | 2196 |
| runtime.rs `dispatch_action` | 2246 | 2246 | 2261 |
| runtime.rs coalescer arming (partial path) | 4523-4524 | — | 2193 |
| runtime.rs `needs_immediate_repaint` / paint kick | 2157-2159 | — | 2148 / 2174 |
| runtime.rs `InputMapper::map` | 2110 | 2110 | 2126 |
| runtime.rs guard/Tui::new/probe/InteractiveRuntime::new | 6602-6656 | — | 6622/6643/6630/6685 |
| writer.rs `write_stage3_frame` | 544-573 | — | 550-578 |
| schedule.rs `emit_tool_execution_end` | 816 | 816 | 828 |
| schedule.rs `tool_result_message` | 850 | 850 | 847 |
| pi_tool_dispatch_bench.rs sink `emit` | 192 | 192 | 200 |
| pty.ts `writeKeys` | 145 | 145 | 134 |
| bench-extension-scaling.ts measure* | 161/205 | 161/205 | 181/209 |
| host.ts 4 ms race | 1456 | 1456 | 1449 |
| extension.ts streamVerification / push | 225 / 292 | 177 / (wrong) | 178 / 210 |

Anchors not listed were exact at both trees (e.g. sessions/mod.rs:662, 540-542,
445-503, 1505-1523; writer.rs:283-343, 522; backend.rs:275-284;
server.rs:966, 983; schedule.rs:105, 26, 291, 378; model_runtime.rs:359, 435-438,
592-621, 1380; drain.rs:105-118, 146, 220-229; args.rs:148, 278;
bootstrap.rs:446-454, 484-487; core/config.rs:23; session-timing.rs:289-296;
pty_no_flicker.rs:54-66, 236-290; performance.ts:1283-1317, 1492-1524, 1926;
protocol.rs:263, 356, 1904-1910; host.ts:116, 778).

## Question 1 — does every floor include all unavoidable work its contract implies?

Yes, per-ledger review:

- **session-append** (3.73 us): serialize one ~170 B line (achievable
  sonic-rs constant, measured), unique id, timestamp, index insert, one
  `write(2)` at held-open achievable cost. The floor correctly uses the
  *achievable shape* (held-open fd) while the current re-open-per-entry delta
  is booked as addressable overhead in the decomposition — the right direction
  for a rebuild-gating floor. Verified against source: `sync_all` rides only
  the exclusive-create first flush (mod.rs:489-491 inside the `create_new`
  branch), never per-entry appends — the "no per-entry fsync" floor claim is
  source-true, and the strace census (1 openat + 1 write + 1 close per entry)
  corroborates.
- **session-reopen** (0.76 us): warm read + typed parse (measured achievable
  constant) + by-id insert + vec push. The borrowed-typed-entries API forcing
  typed materialization is contract-sourced (get_entries signature + immediate
  payload reads at the cited consumers). Complete.
- **render-churn** (~1.5 us): re-segment/re-wrap the one changed line + damage
  bookkeeping for the rest; unchanged lines excluded with a contract argument
  (wire owes only changed cells; upstream TS skips unchanged frames wholesale —
  external evidence, not internals). See Finding 5 for the per-line constant.
- **terminal-paint** (~0.64 us): changed-cell payload encode + sync wrapper +
  one write(2) at pipe-write cost. Excludes the full-buffer diff scan on the
  same changed-cells contract argument; the exclusion's direction is floor-
  understating (conservative for rebuild justification) and the classification
  survives by a wide margin either way.
- **tool-dispatch** (4.29 us): typed argument parse + event triple + result
  construction + one session append at session-append constants. The worker
  spawn is explicitly booked as overhead (task reuse achievable), not silently
  dropped — correct and stated.
- **startup-version-path** (~0.15 us, CPU currency): argv scan + constant read
  + one write+exit. The wall layer (2814 syscalls, 80 clone3, PTY spawn
  overhead 24.97 ms by subtraction) is decomposed separately and honestly
  labeled; the currency split (CPU multiple vs wall layer) prevents a
  misleading single number.
- **first-frame** (~1.50 ms): probe round trip + construction + first paint.
  See Finding 6 for the construction term.
- **Fail-closed units** (stream-frame-pipeline, extension-rpc-dispatch,
  keypress-dispatch, memory-resource-units): floors stated as class estimates
  only; no verdict rests on them. Their floors err generous (keypress includes
  10-20 us scheduler round-trip terms), i.e. conservative against rebuild
  over-justification.

## Question 2 — understated work (false rebuild justification) or overstated floors (false AT-FLOOR)?

- The **AT-FLOOR list is empty**; there is no false-AT-FLOOR certification to
  find. The fail-closed rule is applied in the correct direction four times:
  unmeasured or noise-rejected units (stream-frame-pipeline, extension-rpc,
  keypress at rs 27%, memory) hold OPEN with named measurement prerequisites —
  an untrusted measurement can never prove AT-FLOOR.
- Robustness of every OPEN multiple against floor error: no plausible floor
  revision flips any classification. session-append 4.91x would need the floor
  to more than double (to >9.17 us) to drop under 2x — impossible given the
  decomposition itself shows >=3.73 us of unavoidable serialize+write work;
  session-reopen 7.79x would need a ~4x floor revision; tool-dispatch 5.62x
  needs >12.06 us vs a decomposition showing >=4.29 us forced; render-churn
  stays >70x even if the per-line constant doubles; startup (~2480x) and
  first-frame (162x) are orders clear. The two generous floor terms
  (Findings 5 and 6) only shrink multiples that remain far above 2x.
- No decomposition category quietly disappears work into "residual" to inflate
  a multiple: every residual is the *closing* subtraction term, small or
  explicitly named (stream-frame-pipeline's unattributed residual is named,
  not estimated, and drives its fail-closed state rather than a verdict).

## Question 3 — does every decomposition category carry a method and reconcile?

All six decomposed units reconcile to their measured totals (recomputed
independently):

| Ledger | Sum of categories | Measured | Method words present |
|---|---|---|---|
| session-append | 2.063+3.363+6.87+1.28+1.03+3.73 = 18.336 | 18.337 us/entry | subtraction, floorkit, profiler attribution ✓ |
| session-reopen | 2.57+1.14+1.60+0.46+0.18 = 5.95 | 5.952 us/entry | profiler attribution + closing subtraction ✓ |
| render-churn | 50.9+27.6+24.8+29.5+8.3+5.5+65.4 = 212.0 | 212 us/frame | profiler attribution + residual subtraction ✓ |
| tool-dispatch | 5.43+8.10+3.55+2.22+4.82 = 24.12 | 24.12 us/call | floorkit+strace, profiler attribution, subtraction ✓ |
| startup-version | 54+132+26+160+<4 ~= 372 (CPU) + separate 14.7 ms wall layer | 0.37 ms CPU / 15.1 ms wall | profiler attribution, subtraction, strace census ✓ |
| first-frame | 157.2+58.3+18.9+9.2 = 243.6 | 243.61 ms | strace -T census, callgrind, subtraction ✓ |

Share arithmetic spot-checks reproduce: append serde pipeline
(112.6+75.0+6.4+7.7)/379.4 = 53.2% ✓ applied to the correct 12.91 us CPU
remainder (18.337 - 2.063 - 3.363) ✓; has_assistant 37.5/379.4 = 9.9% ✓;
churn Ir shares 24/13/11.7/13.9/3.9/2.6 sum 69.1 with residual 30.9% = 65.5
(listed 65.4, rounding) ✓; dispatch Ir 5.274 G/21k = 251.1 kIr/call at the
10.6 kIr/us calibration = 24.1 us corroborating the 24.12 wall median ✓;
first-frame 200.6 M Ir / 10.6 kIr/us = 18.9 ms ✓, rustls 20.2/200.6 = 10.1% ✓;
version loader 14.4% of 3.95 M Ir = 53.7 us ✓. Terminal-paint's amortization
is honest and shown: 212 us x <=32 paints / 256 frames = 26.5 us/frame, paint
share ~18% of the churn frame from the lane-7 callgrind, giving 4.8-5.5 us
amortized vs the 0.64 us floor (7.5-8.6x; ledger quotes the low end).
Units without a trusted total assert **no** categories (extension-rpc,
keypress, memory) — the discipline holds.

## Question 4 — is any contract term sourced from the unit's internals?

No. Every Contract section traces to signatures, call sites, tests, or harness
observables, all verified to exist at HEAD:

- Signatures: `append_message` (mod.rs:662), `open` (mod.rs:1214),
  `get_entries` (mod.rs:634), `serve_io` (server.rs:966), `execute_tool_calls`
  (schedule.rs:105), `commit`->`commit_frame` (writer.rs:283-343).
- Tests: `append_prefix_stability` (mod.rs:1767),
  `failed_append_does_not_advance_tree` (mod.rs:1792),
  `deferred_write_until_first_assistant` (mod.rs:1685), reopen-after-move /
  branch-from-reopened-leaf (mod.rs:1707-1760), sync-balance and
  probe-before-sync (pty_no_flicker.rs:54-66, 236-290), tool-dispatch protocol
  (pi_tool_dispatch_bench.rs), hello handshake (server.rs:4044, 4085).
- Call sites / harness observables: `persist_message_end`
  (agent_session/persistence.rs:87), `flush_pending_bash_messages`
  (bash.rs:242), `run_agent_loop` dispatch (run.rs:164-172), paint/coalescer
  call sites in runtime.rs, performance.ts frameObservation (:1026, keyed on
  the SYNC_END-terminated chunk), session-timing sha-prefix stability
  (:289-296).

Two boundary cases examined and passed: (a) terminal-paint's "one complete
write transaction per frame" cites stage3_write internals but the *term* is
sourced from the harness observable that keys on it — the internal cite is
locating, not justifying; (b) keypress's scheduler round-trip floor term is a
physical-constant class (context-switch cost), observed via strace, not a
behavioral property of the unit's own code.

## Finding 5 (recorded, no action): render-churn's per-line floor constant is implementation-derived

The ~1.3 us/line term is computed as whole-frame Ir / line count
(2.106 M Ir / ~151 lines at 10.6 kIr/us) — i.e. the *current* implementation's
average per-line cost, not an independently measured segmentation bandwidth.
Direction: floor-overstating (shrinks the multiple). At ~141x the OPEN
classification is unaffected (even doubling the constant leaves ~70x). Phase 5
should not cite this term as a physical constant when re-deriving the floor
after a rebuild; recompute it from the replacement's own per-line measurement.

## Finding 6 (recorded, no action): first-frame's construction term carries no measured constant

The ~0.5 ms "config + 10-adapter registry + Tui construction at achievable
cost" term is an engineering estimate — unlike every other floor term it has
no floorkit/serial/callgrind constant behind it. Direction:
floor-overstating. At ~162.4x the classification is unaffected. The probe
round-trip (~1 ms) and first-paint (0.64 us) terms are properly sourced. If a
Phase-5 iteration attacks construction, its first step is measuring this term.

## Gate decision

PASS. The ledger set is fit to bind Phase 5 (PERF-T11): contracts are
caller-sourced and citation-verified at the landing tree, floors are complete
and direction-conservative, decompositions reconcile exactly, no false
AT-FLOOR exists, and the fail-closed units name their measurement
prerequisites. PERF-T11's entry condition (this audit) is satisfied.
