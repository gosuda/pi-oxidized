# TUI-V6: accessibility evidence record

- **Issue:** [#72][issue-72] — `TUI-V6`
- **Track:** issue #25 §4 P5 accessibility lane (automated three-invariant
  lane on every Tier N row; manual Orca/VoiceOver sign-off over the
  eight-scenario script, or an explicitly flagged degraded-verdict
  limitation row)
- **Policy inputs:** TUI-G1 reduced-motion decision
  (`docs/TUI-G1-reduced-motion-policy.md`) — the three invariants never
  require a motion gate; TUI-G2 hardware-cursor decision — the invariants
  are cursor-position-agnostic.

[issue-72]: https://github.com/metaphorics/pi-oxidized/issues/72

## 1. Automated lane: the three invariants over canonical content

Implementation: `crates/pi-tui/tests/transcript_a11y_invariants.rs` driving
the stepped fixture `pi_tui_a11y_fixture`
(`crates/pi-tui/src/bin/pi_tui_a11y_fixture.rs`) through the real
`Tui`/`Loader`/probe/guard pipeline under the schema-v1 PTY harness
(scenario `fixture-a11y-gauntlet`). The invariants are computed over the
canonical settled content (`CanonicalEvent::Snapshot` lines) of the
recorded artifact, never over timing:

1. **Notice persistence.** The transient notice text (product
   `push_notice("export", …)` payload shape: railed `[export]` label plus
   text) must be present in at least one settled frame. The fixture
   renders the notice in its first two settled frames whose non-notice
   content differs (`NOTICE-TICK 1`), so persistence is canonical content
   across a scripted content change. Scope note: the fixture draws the
   notice line itself (no product `push_notice` code path runs under the
   PTY harness), so this lane proves the invariant's detection surface,
   not the product notice code.
2. **Static sufficiency.** Every spinner-status frame, meaning a settled
   frame carrying a line that ends with the product `status_message`
   cancel-hint suffix (` to cancel`), must carry the kind label, the
   elapsed counter (`{N}s`), and the cancel hint. The fixture renders the
   real `Loader` (frame 0 pinned; the invariants never require a motion
   gate, per TUI-G1) with the product status shapes `Working… {4,5,6}s`,
   `Retrying… {2,3}s`, `Compacting context… {7,8}s`, each with the cancel
   hint derived from the keybinding registry (`key_text("app.interrupt")`,
   rebind-proof; the pi-tui default registry leaves this binding unbound,
   so the rendered hint is the bare suffix, and the hint slot itself is
   the invariant).
3. **Anti-chatter.** Within one settled stage (the maximal run of canonical
   events between two `Input` boundaries), an identical announcement string
   (trimmed non-empty snapshot lines joined by newline: what a screen
   reader would voice for the frame) may occupy at most one settled frame
   consecutively; repeats are counted over logical sequence numbers.
   Identical announcements separated by a content change inside the stage
   are allowed (A→B→A passes); identical announcements across a stage
   boundary are never compared, because the boundary is itself a content
   change.

Teeth: each invariant has synthetic negative probes
(`notice_persistence_fails_without_a_notice_frame`,
`static_sufficiency_fails_on_missing_elapsed_or_kind`,
`anti_chatter_fails_on_repeated_announcement_inside_a_stage`) proving the
checker fails on the mutated shapes (missing notice frame, spinner line
whose elapsed counter is missing while the kind label stays valid,
spinner line without kind label, identical announcement held across two
consecutive settled frames of one stage) and passes the boundary cases
(stage-split repeats, post-content-change repeats, non-spinner frames
ignored; the cancel hint itself is guaranteed by the spinner-line
classification). Scope note on anti-chatter: the step-gated corpus
records exactly one settled frame per stage (deterministic settle at real
input boundaries), so the cross-frame comparison inside a stage is
exercised by the synthetic probes, while the corpus proves the pass path
and the input-boundary stage grouping over real transcripts. The verdict
record discloses this shape (`corpusShape`).

### Timing quarantine (measured fields)

The 2s urgency window is asserted only as a measured field against the
timing envelope, with a pinned tolerance; it can never alter canonical
content or the digest (the timing envelope is excluded from
`digest_canonical` by construction):

| Measured field | Nominal | Tolerance | Source |
|---|---|---|---|
| `noticeUrgencyWindowMs` | 2000 | observed at most 10 000 ms (the settle-abort ceiling); a value beyond the band is a measurement-channel failure, never a content verdict | wall span between the two notice-carrying settled frames, from the settle wall offsets (dominated by the 120 ms settle quiet window) |

The observed value is recorded per row in
`target/verification/tui-transcripts/<row>/a11y-gauntlet/verdict.json`
under `measuredFields` with verdict `tolerated`. Scope note: the fixture
drives no urgency timer (the product renders notices as persistent
transcript rows; the 2 s figure is the quality bar's nominal reference,
not a product constant), so this measured field proves the quarantine
channel itself (timing-envelope-only, tolerance-banded, digest-neutral),
not product urgency behavior.

## 2. Automated lane evidence

- **Local gnu-x64 row: PASS.** `cargo test -p pi-tui --features testkit
  --test transcript_a11y_invariants` is green: 5/5 tests (corpus plus four
  negative probes), k=3 byte-identical canonical bytes and digest
  (`sha256:a933e672011c46f9be9eafcf1704404e9a3c902ea6061056905f64b7787d7593`),
  validator-clean artifacts in memory and as a serialized round-trip.
  Ten settled frames, one per scripted stage: notice, notice-tick-1,
  working-4s, working-5s, working-6s, retry-2s, retry-3s, compaction-7s,
  compaction-8s, DONE. Seven spinner-status frames all carry kind +
  elapsed + cancel hint; the notice text is present in the two notice
  frames; zero repeated announcements. Per-row verdict at
  `target/verification/tui-transcripts/local/a11y-gauntlet/verdict.json`
  with all three invariants `pass` and the measured field `tolerated`
  (observed 123 ms against the 2 000 ms nominal inside the pinned band).
- **Tier N five-runner evidence: PENDING**, identical to the other
  corpora: the lane is row-parameterized
  (`PI_TUI_TIER_ROW=tier-n/<row>@<image>`; local runs structurally cannot
  claim Tier N) and lands with the standing Tier-N CI evidence wired by
  REL-T4 #108, documented PENDING in `docs/tui-transcript-schema-v1.md`
  §9.

## 3. Manual lane: eight-scenario script

The sign-off script for both named screen readers (Orca on Ubuntu 24.04
AT-SPI terminal; VoiceOver on macOS Terminal.app), per issue #25 P5:

| # | Scenario | Pass criterion (binary) |
|---|---|---|
| 1 | Cold start | First paint announces the model/header identity once; no mid-boot chatter |
| 2 | First-run wizard | Each wizard step announced; selection state reachable and readable by keyboard |
| 3 | `/login` | Provider selector options announced; completion notice announced |
| 4 | Streaming | Spinner status announced with kind + elapsed + cancel hint; streamed deltas announced without repeating unchanged content |
| 5 | Interrupt | Aborting… status announced; post-interrupt state announced once |
| 6 | Error | Error notice announced and persistent (present in a settled frame) |
| 7 | Resize | No announcement storm during resize; settled state re-announced once after settle |
| 8 | Selector | List options + exit hint announced; selection moves announce the newly selected item only |

Required artifacts per run: speech log (AT-SPI debug log / VoiceOver
caption transcript), paired schema-v1 transcript, binary per-scenario
verdicts, and the named sign-off owner.

## 4. Manual lane: degraded-verdict limitation row

| Limitation | Flag |
|---|---|
| `limitation:manual-screen-reader-signoff-not-executed-headless-host`: the Orca/VoiceOver eight-scenario sign-off has not been executed. This execution host is a headless Linux build machine (no AT-SPI desktop session; `DISPLAY`/`WAYLAND_DISPLAY` unset) and no macOS host is attached, so no speech log, per-scenario verdict, or named owner exists yet. | **DEGRADED VERDICT, explicit**, per the ticket's own closure rule (the manual lane cannot be scheduled before close, so TUI-V6 closes with an explicitly flagged degraded-verdict limitation row) |

The manual lane remains executable exactly as scripted in §3 on any
desktop-equipped host; this record is the flagged limitation row that
issue #25 §4 P5 and TUI-G1's sign-off traceability require, not a
rewording or weakening of the protocol. The automated lane (§1, §2) is
unaffected and green on the local row.
