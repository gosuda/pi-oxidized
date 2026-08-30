# TUI-G1: reduced-motion and spinner opt-out policy (decision record)

- **Issue:** [#49][issue-49] — `TUI-G1` (routed decision: settings category)
- **Decision type:** recorded decision, not implementation
- **Deliverable:** this document and the matching commit, and only these
- **Decision:** Option (b), match upstream's no-gate behavior as a deliberate,
  explicit parity deviation.

[issue-49]: https://github.com/metaphorics/pi-oxidized/issues/49

## Selected option

Retain upstream's no-preference-gate spinner behavior. The default braille
`Loader` animates at its fixed cadence. Callers can supply a one-frame static
indicator or an empty hidden indicator, but no reduced-motion preference,
environment setting, or terminal capability selects those forms.

Current seams:

- Reference, `.references/pi-2.0/packages/coding-agent/src/modes/interactive/components/status-indicator.ts` (at `853a80d26c90a14c1886f0ebb8ffaae133ca2185`):
  the default status indicator uses the animated `Loader` frames. The underlying
  loader also accepts caller-supplied static or empty frame lists. A search of
  the upstream interactive mode finds no reduced-motion or motion-gate setting.
- Loader and static-frame seam, `crates/pi-tui/src/components/loader.rs`:
  `DEFAULT_LOADER_FRAMES` (braille frames) and `DEFAULT_INTERVAL_MS = 80`, driven
  only by the product's external `advance(Instant)` ticks, no internal timer and
  no gate. `crates/pi/src/modes/interactive/status.rs` build_status constructs
  `Loader::new(…, None)` and pins `set_frame_index(status.frame)`; the static
  frame (kind label + elapsed counter) is a P4 prototype shape, not a gate.
- Spinner clock seam, `crates/pi/src/modes/interactive/runtime.rs`:
  `SPINNER_TICK = 80 ms`, advanced through `arm_spinner_deadline` /
  `tick_status_indicator` / `reconcile_spinner_clock`.

## Rationale (one line)

Adding an env/settings gate would create a new persistence and motion-control
surface while the canonical reference has none; keeping upstream's no-gate
behavior honors issue #25's settled rule, *"reference parity is the default and
every deviation is an explicit, recorded decision"*, and the reduced-motion gap
is owned by TUI-T11 [#78][issue-78] as its one implementation against this
decision.

## Invariant and sign-off traceability (issue #25)

This decision is made against the issue #25 P2/P5 accessibility contract.

- Automated invariants (P2, TUI-P2). The three accessibility invariants over
  canonical ordering, notice persistence, static sufficiency, and anti-chatter,
  never require a motion gate. The static frame (kind label + elapsed counter,
  no frame animation, tick-repaint suppression) is a P4 prototype shape, not a
  gate, and survives under parity.
- Manual sign-off protocol (P5). The named manual Orca (Ubuntu AT-SPI terminal)
  and VoiceOver (Terminal.app) sign-off uses an eight-scenario script with
  speech logs and binary per-scenario verdicts. This record does not define the
  scenario contents or add a spinner-specific pass condition. TUI-T11 #78 must
  satisfy that protocol if it changes motion behavior.

## Rejected alternatives

- Option (a), env/setting opt-out rendering the static frame. Rejected. A new
  settings.rs-adjacent persistence key and an env var are exactly the
  "persistence/settings" surface the issue #25 classifier routes to decision
  tickets and that #78 alone may land (zero-settings-diff boundary).
- Option (c), terminal motion gate. Rejected. Upstream exposes no such gate (no
  `prefers-reduced-motion` or equivalent in the reference); adding one is new
  product surface with no parity analogue, out of scope for a polish ticket.

## Ownership boundary

- No settings and no Rust source change lands under TUI-G1. This ticket changes
  exactly one file, `docs/TUI-G1-reduced-motion-policy.md`.
- TUI-T11 [#78][issue-78] owns all implementation of any future reduced-motion
  mechanism, gated additionally on the TUI-P4 [#84][issue-84] prototype evidence.
- TUI-P4 [#84][issue-84] consumes this decision as a READ input on its path; it
  may prototype the static frame against this record but does not own the
  policy, which remains solely with #78.

[issue-78]: https://github.com/metaphorics/pi-oxidized/issues/78
[issue-84]: https://github.com/metaphorics/pi-oxidized/issues/84