# Floor ledger: render-churn recomposition + layout (per-frame view rebuild)

Owning R2 hot rows (lane 7): *Component tree recomposition*, *Layout calculation*.
State: **OPEN**, ~141x floor — the largest sustained multiple on the hot list.

## Contract (from call sites, tests, signatures — never internals)

- One frame of the churn workload (crates/pi-tui/src/bin/pi_tui_render_churn_bench.rs:327-344) = `frame(i, root)` + `tui.commit(Txn::Frame, root)` through the production `Tui::commit` -> `commit_frame` path (crates/pi-tui/src/terminal/writer.rs:283-343): the root is measured, rendered into the cell buffer, differenced, encoded, and written. The bench's NullWriter discards bytes and counts them (bench.rs:49-73).
- Editor scenario: exactly one character is appended per frame (bench.rs:397-399); static scenario: no mutation (bench.rs:395). Viewport 100x30, transcript 150 styled ANSI lines, 20 warmups, 300 frames (bench.rs:40-45) — mirrors the upstream parameters (`.references/pi/packages/tui/test/render-churn-bench.ts`).
- Production consumers of the same path: `paint_frame` (crates/pi/src/modes/interactive/runtime.rs:4527-4528) -> `build_root` (runtime.rs:4599) on every repaint; the interactive runtime owes a correct grid for the current view state and only-changed-cells bytes on the wire.
- Behavior tests pinning the render path: pi-tui transcript/state-matrix fixtures (crates/pi-tui/tests/transcript_state_matrix.rs, static_frame_evidence.rs) assert grid-level output correctness, not frame cost.

Boundary classification: the emitted escape-byte stream is the terminal wire surface
(**boundary** — synchronized-output framing, backend.rs:275-284). The measure/render/
diff machinery above the wire is **interior**. Unresolved channels: none.

## Floor (computed)

Contract-forced per editor frame: re-wrap/re-segment only the changed line (one char
appended), update its cells, track which cells changed, emit the changed bytes.

```
re-segment + re-width + re-wrap one ~100-col styled line   ~1.3 us
   (measured: whole-frame 2.106 M Ir over ~151 lines + chrome
    => ~14 kIr/line => ~1.3 us/line at 10.6 kIr/us)
damage bookkeeping for the remaining lines (unchanged)      ~0.2 us
                                                        ---------
floor                                                   ~1.5 us/frame
```

The unchanged 150 lines are *not* contract-forced work: the terminal state after the
frame differs only in the mutated line's cells, and the wire owes only changed-cell
bytes. (The upstream TS implementation skips unchanged frames wholesale in the static
scenario, confirming skip-ability is a property of the contract, not of our internals.)

## Measured cost

Fresh run 2026-08-27 (attribution build): editor **0.214 ms/frame** (212 us), static
0.209 ms/frame; 28.3 KiB allocated and **39 B written** per editor frame. R8 trusted
baseline: editor 0.212 ms/frame Rust vs 0.243 ms TS (paired, PASS).

**Multiple = 212 / 1.5 ~= 141x => OPEN.** Both implementations sit ~2 orders above the
changed-line floor — the full-tree recompute is a shared design property, and the
ledger records it as the campaign's largest sustained target.

## Cost decomposition (sums to 212 us/frame; callgrind Ir shares, 1.348 G Ir / 640 frames)

| Category | Cost | Method |
|---|---|---|
| grapheme segmentation of all lines (unicode-segmentation Graphemes family, ~24%) | 50.9 us | profiler attribution |
| width computation (pi_tui text/width + unicode-width tables, ~13%) | 27.6 us | profiler attribution |
| paint_line styled-cell production (pi_tui components/util, ~11.7%) | 24.8 us | profiler attribution |
| ratatui buffer diff + Cell equality (BufferDiff + Cell::eq, ~13.9%) | 29.5 us | profiler attribution |
| Cell::set_symbol + compact_str stores (~3.9%) | 8.3 us | profiler attribution |
| ANSI escape extraction (pi_tui text/ansi, ~2.6%) | 5.5 us | profiler attribution |
| allocator + memcpy + core remainder (~31.4%) | 65.4 us | subtraction (residual closes the sum) |

## Addressable-overhead notes for Phase 5

The decomposition says per-frame cost is re-derivation of unchanged content: cached
segmentation/width per unchanged line, damage-scoped diff, and allocation reuse
(28.3 KiB/frame) are the candidate directions. The static-vs-editor delta (209 vs 214
us) proves the current cost is frame-shape, not mutation-size. Boundary: emitted bytes
for a given tree state must remain byte-identical (transcript fixtures pin them).
