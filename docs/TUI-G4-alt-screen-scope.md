# Alt-Screen and Scroll-View Scope — TUI-G4 Decision Record (#35)

Status: Ratified (decision-only; no source changes by this task)
Issue: [TUI-G4 #35](https://github.com/metaphorics/pi-oxidized/issues/35)
Stable ID: `TUI-G4`
Blocked by: [Terminal interaction audit #25](https://github.com/metaphorics/pi-oxidized/issues/25); [rail-only doctrine #40](https://github.com/metaphorics/pi-oxidized/issues/40)

## Decision

`TuiAltScreen` is a deferred-by-design roadmap surface. It is not part of the Rust-port completion scope in issue #12.

The current interactive product remains an inline terminal event stream with rail-only transcript blocks. Pulling an alternate-screen application into this campaign would change the screen model, viewport ownership, input dispatch, and lifecycle instead of polishing the existing architecture. Issue #25 makes the current screen structure inviolable and routes these changes out of the polish track. Issue #40 assigns the alt-screen and scroll-view domain to this decision.

## Affordance classification

| Missing affordance | Classification | Boundary |
|---|---|---|
| Fullscreen scroll-view | Deferred-by-design roadmap | Requires alternate-screen ownership, an internal scroll model, and a separate viewport lifecycle. |
| Mouse capture | Deferred-by-design roadmap | Depends on the alternate-screen interaction model and adds pointer dispatch that the current inline flow does not own. |
| Search overlay | Deferred-by-design roadmap | Depends on retained scroll-view content, overlay focus, and search navigation semantics. |
| Flash confirmations | Deferred-by-design roadmap | Belongs to an alternate-screen notification layer; the current product keeps confirmations in the inline transcript and existing overlays. |
| Alt-screen prompt navigation | Deferred-by-design roadmap | The inline editor already browses retained prompt history. This missing affordance is a separate full-screen prompt list, focus model, and navigation UI tied to the alternate-screen surface. |

All five classifications are final for issue #12. None is an untracked parity gap. A later roadmap proposal must treat them as one screen-model unit rather than importing one affordance into the inline TUI as a special case.

## Evidence and guardrail

- The published terminal plan in issue #25 lists alt-screen, scroll-view, and mouse implementation as out of scope and reserves TUI-G4 for this scope decision.
- The rail-only decision record assigns the alt-screen and scroll-view surface to TUI-G4 and keeps `scrollbarThumb` terminal-inert until such a surface exists.
- The current `crates/pi` and `crates/pi-tui` source trees contain no `EnterAlternateScreen`, `LeaveAlternateScreen`, mouse-capture command, search-overlay type, scroll-view type, flash-confirmation surface, or full-screen prompt-navigation surface.
- `crates/pi-tui/src/editor_support/history.rs` already owns retained prompt history and `History::navigate`; `crates/pi-tui/src/components/editor/mod.rs` dispatches inline editor Up/Down keys into it. This record defers only the missing alt-screen UI and semantics, not the existing inline history browser.

This record changes no Rust source, public terminal API, key binding, viewport policy, or theme schema.
