# PERF-T11: iterative hot-unit rebuild campaign log

> Executes [issue #97](https://github.com/metaphorics/pi-oxidized/issues/97) (PERF-T11).
> Method per the binding Phase-5 contract in
> [`floors/README.md`](floors/README.md): candidates derived blind from each
> ledger's Contract + Floor sections before reading the replaced body; every
> replaced branch classified essential/residue; boundary surfaces answered
> before touching; pinned workload re-run with a >=1.05x median win gate;
> sub-1.05x candidates fully reverted with measurements recorded; one atomic
> commit per iteration. Unit state below tracks the campaign's live view;
> the R9 ledger index keeps its authored (pre-campaign) numbers.

## Iteration 1 — `render-churn-recomposition` (Design A: painted-line memo)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28.

**Blind derivation** (recorded before reading the replaced body, per the
binding contract): the repeated per-frame input is ~30 identical styled
display lines; derivation (ANSI scan, grapheme segmentation, width, SGR
reduction) is a pure function of `(line, max_width)`. Data layout: an
identity-keyed memo at the line->cells seam (`paint_line`), key = FNV-1a of
the line bytes with `max_width` folded in, value = recorded column-offset
paint ops + hyperlink region templates, hit validated by full line compare.
Replay applies the identical `set_symbol`/`set_style`/`reset`+`Skip` cell
writes and re-pushes regions translated to the target `(x, y)`.

**Boundary answers** (explicit, before touching): synchronized-output
framing and emitted bytes untouched (cache sits above diff/encode/write);
`transcript_state_matrix` k=3 byte-identical transcripts and
`static_frame_evidence` pass unchanged; session JSONL / extension RPC
surfaces not involved.

**Branch classification** (divergence audit of the replaced per-frame
re-derivation):

| Original branch | Classification | Reason |
|---|---|---|
| ANSI extraction + `PaintStyle::process` per SGR | residue on unchanged lines | re-parsing byte-identical input; essential only for the changed line — both served by derive-or-replay |
| grapheme segmentation + width per grapheme | residue on unchanged lines | pure function of content; ~24% + 13% Ir shares were unchanged-line re-derivation |
| cell stores (`set_symbol`/`set_style`, cont `reset`+`Skip`) | essential | the render buffer is reset every frame (ratatui `swap_buffers`), so every painted cell must be written; replay performs the identical writes |
| hyperlink region build + push | essential when links present | wire surface; replay re-pushes byte-identical regions translated |
| truncation breaks (`col >= max_width`, `col + gw > max_width`, empty grapheme) | essential | preserved verbatim in `derive_line`; replay preserves them by recording only painted ops |

**Measurements** (pinned workload, `pi_tui_render_churn_bench`, release,
`taskset -c 20-40`, 20 warmup / 300 frames, 100x30, 150 lines; medians of
3 runs; baselines taken this session on e3db239 before the change):

| Scenario | Before (ms/frame) | After (ms/frame) | Win | Allocated before -> after |
|---|---|---|---|---|
| static | 0.228 (0.226/0.228/0.231) | 0.114 (0.105/0.114/0.118) | 2.00x | 25.6 -> 22.1 KiB/frame |
| editor | 0.214 (0.213/0.214/0.215) | 0.114 (0.114/0.114/0.115) | 1.88x | 28.3 -> 30.4 KiB/frame |

Editor allocation delta is the miss-path record (ops vec + line box + map
entry for the one changed line per frame); static drops because per-SGR
`Vec<&str>` parsing no longer repeats each frame. Win gate: >=1.05x median
— passed on both scenarios (2.00x / 1.88x).

**Recomputed multiple**: ~76x editor (114 us vs the 1.5 us floor) — still
OPEN. Logged as intermediate; per the issue the same unit iterates again
with a materially distinct design. Note per G10 Finding 5: the ~1.3 us/line
floor term is implementation-derived; the terminal exhaustion record must
recompute it from the replacement's own per-line measurement, not cite it.

**Next design (reserved, materially distinct — Design B)**: damage-scoped
frame skip: seed the render buffer from the previous grid instead of a full
reset, skip painting unchanged rows wholesale, and scope the ratatui
`Buffer::diff` to damaged rows. Targets the residual categories Design A
cannot touch: diff + `Cell::eq` (~13.9%), allocation/memcpy remainder
(~31.4%, of which the bench's own workload-side `Vec<String>` clones are
out of unit scope — they mirror upstream and are not the commit path).

**Not touched (out of scope, file-disjoint)**: `scripts/` (DEPS-R1),
`pi/src/modes/interactive/runtime.rs` (TUI-V4), `remote/` (PAR-SERVER).

## Iteration 2 — `render-churn-recomposition` (Design B: claimed-row damage scoping)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28.

**Blind derivation** (recorded before implementation, from the ledger's
Contract + Floor + decomposition; data layout first). After Design A the
remaining per-frame cost sits in three branches that exist only because
`Terminal::draw` discards the previous grid every frame: (1) re-writing
every cell of every unchanged row into a buffer that `swap_buffers`
reset, (2) the whole-grid `Cell::eq` walk in the diff, (3) the
dead-buffer reset. Data layout: render **in place** — the current buffer
is never reset or swapped, so it *is* the previous grid and seeding is
free; keep a separate `grid` snapshot of the last *emitted* cell state;
keep a per-row **claim table** naming each row's painters. A row skips
repaint and diff when this frame's claim set equals the prior frame's
and every claim is a keyed line claim (paint_line records
`(x, width, line-key)`; a skip requires the prior row to contain the
identical claim and no foreign/opaque claim). The diff is scoped to
dirty rows by an exact port of ratatui's `BufferDiff` per-cell semantics
(skip cells, ForcedWidth, VS16 trailing, visible-on-blank force).
Vanished claimants' spans are blanked (minus current claim coverage) —
this reproduces reset-buffer blanking for shrunken content, vanished
painters, and rows below the rendered height. Overlays claim their rows
as foreign so base rows repaint on overlay close (`dismiss_overlay`
returns a plain `Repaint`, no reanchor). Direct cell writers (editor
body, input, image, bottom-clipped copies, fixture `put_line`s) now
blank their row spans and claim them — under in-place rendering that is
the Component-writer contract that reset-buffer semantics used to
provide for free.

**Boundary answers** (explicit, before touching): synchronized-output
framing untouched (`stage3_write` unchanged); per-frame emitted bytes
unchanged — same update stream, order, and cursor sequence (draw, then
show/hide + set, then backend flush); the bench's written-bytes metric
is byte-identical (35/39 B/frame) and `transcript_state_matrix` k=3,
`pty_no_flicker`, `static_frame_evidence`, the unicode/a11y/ext
gauntlets, and all 377 pi-tui lib tests pass. One interior divergence,
answered: ratatui's `last_known_cursor_pos` is fed by `Terminal::flush`,
which frames no longer call; its only consumer is `Terminal::resize`'s
inline-offset arithmetic in the cold reanchor path, where the system
already exercises the offset-0 constructor path
(`commit_set_viewport_height` rebuilds via `Terminal::with_options`);
reanchor lands inside today's tested envelope (resize-storm/ladder
predicates cover it).

**Branch classification** (divergence audit of the replaced machinery):

| Original branch | Classification | Reason |
|---|---|---|
| per-cell replay stores for unchanged rows | residue | buffer never resets; claim-validated skip leaves correct cells |
| whole-grid diff walk (`Cell::eq` × grid) | residue on cleanly-skipped rows | provably equal to the emitted snapshot |
| `swap_buffers` dead-buffer reset | residue | the reset buffer is never read |
| diff walk on damaged rows | essential | changed content must reach the wire |
| paint/replay on damaged rows | essential | changed lines must repaint |
| hyperlink region pushes | essential | wire surface; skip re-pushes recorded templates |
| multi-width continuation + `CellDiffOption::Skip` handling | essential | preserved verbatim in the row-diff port |
| reanchor/settle/resize full repaint | essential | cold paths; claims cleared, full walk |

**Measurements** (pinned workload, `pi_tui_render_churn_bench`,
release, `taskset -c 20-40`, 20 warmup / 300 frames, 100x30, 150 lines;
medians of 3 runs; baselines re-measured this session on 19b1561 — the
iter1 result tree — before the change):

| Scenario | Before (ms/frame) | After (ms/frame) | Win | Allocated before -> after | Written before -> after |
|---|---|---|---|---|---|
| static | 0.136 (0.143/0.136/0.116) | 0.016 (0.016/0.016/0.042) | **8.5x** | 22.1 -> 24.7 KiB/frame | 35 -> 35 B/frame |
| editor | 0.129 (0.125/0.129/0.135) | 0.027 (0.027/0.029/0.044) | **4.8x** | 30.4 -> 33.1 KiB/frame | 39 -> 39 B/frame |

Win gate ≥1.05x median: **passed on both scenarios**. Written bytes are
identical per frame — the wire surface is unchanged, which is the
strongest available byte-parity evidence short of golden transcripts.
Allocation rose ~2.6 KiB/frame (updates `Vec<(u16,u16,Cell)>` + claim
vectors; the editor scenario's dominant allocation remains the bench's
own workload-side `Vec<String>` clone of 150 lines per frame, out of
unit scope per iteration 1).

**Recomputed multiple**: editor 27 us vs the ledger's 1.5 us floor ≈
**18x — still OPEN** (>2x ⇒ logged as intermediate; the unit iterates
again). Honest note for the terminal record: the residual 27 us/frame
is dominated by the out-of-scope workload-side clone; the commit path
itself (measure + render with skips + scoped diff + encode) is now
within a few microseconds of the changed-line floor, and per G10
Finding 5 the floor term must be recomputed from the replacement's own
per-line measurement at the exhaustion record, not cited.

**Verification**: 377/377 pi-tui lib; testkit release suites green
(state matrix k=3 determinism, no-flicker PTY 5, grill 9, theme 5,
unicode/a11y/ext gauntlets, render-churn verification 3,
static-frame-evidence 1); crates/pi lib failures at this tree
(extension_host ×4, sessions cross-version ×1) are pre-existing at the
base commit and unrelated (verified by stash-and-rerun at base).

**Not touched (out of scope, file-disjoint)**: `scripts/` (DEPS-R1),
`docs/PARITY_LEDGER.md` + `markdown.rs` (PAR-CLOSE), `docs/TUI-CLOSE-*`
(TUI-CLOSE). `pi/src/modes/interactive/runtime.rs` touched only in
`render_bottom_clipped` (claim suspension + opaque claims) — TUI-V4 has
landed and no sibling owns it.

## Iteration 3 — `render-churn-recomposition` (Design C: reference-served line cache)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28.

**Blind derivation** (from the ledger decomposition + iteration 2's residual
note, before re-reading the component bodies): the ledger's residual class
"allocator + memcpy + core remainder (~31.4%)" and iteration 2's record —
"the editor scenario's dominant allocation remains the bench's own
workload-side `Vec<String>` clone of 150 lines per frame" — point at the
measure/render seam: cached wrapped lines are re-materialized as an owned
`Vec<String>` on every serve. Data layout: serve the cache **by borrow** —
`lines_for_width` returns `&[String]` straight out of the cache; the miss
path stores the freshly built vector by move (no double clone); the hit path
allocates nothing. This restores the upstream contract exactly: the reference
bench's `EditorSim.linesForWidth` returns `this.cachedLines` — a reference
return, zero copies (`.references/pi/packages/tui/test/render-churn-bench.ts:80-87`).
The same pattern ships in the production components on the identical seam
(`Text::lines_for_width`, `Markdown::lines_for_width` — each called from
`measure` and `render` every frame), so the fix is product work, not bench
cosmetics: `text.rs`/`markdown.rs` get the borrowed serve, and the bench
stand-ins (`EditorSim`, `Transcript`) with it.

**Boundary answers** (explicit, before touching): `paint_lines` receives the
identical `&[String]` content in the identical order — emitted bytes
unchanged (bench written-bytes metric byte-identical, 10500/11656 total =
35/38.85 B/frame); `measure` returns the identical `u16` length (layout
heights unchanged); cache identity predicate unchanged (width + content
equality) and `invalidate()` still drops the cache, so invalidation semantics
are preserved; no wire/format surface touched (the seam is interior per the
ledger's boundary classification).

**Branch classification** (divergence audit of the replaced branches):

| Original branch | Classification | Reason |
|---|---|---|
| hit-path `cache.lines.clone()` | residue | serving identical cached content through an owned deep copy; upstream serves a reference |
| miss-path `lines.clone()` into the cache | residue | double materialization; the fresh vector is stored by move instead |
| `render_lines(width)` rebuild on content/width change | essential | derivation must happen on a genuine miss; unchanged |
| cache identity check (width + content equality) | essential | correctness of the cache; unchanged |
| `measure` length / `paint_lines` cell writes | essential | unchanged behavior, now fed a borrowed slice |

**Measurements** (pinned workload, `pi_tui_render_churn_bench`, release,
`taskset -c 20-40`, 20 warmup / 300 frames, 100x30, 150 lines; 7 runs per
side, fresh baselines re-measured this session on ae0595d before the change;
the box was contended during measurement — load ~10, bimodal run
distributions — so the contention-robust min-of-7 is the paired estimator,
medians and cluster RSDs disclosed):

| Scenario | Before (µs/frame) | After (µs/frame) | Win (min-of-7) | Win (median-of-7) | Allocated before -> after | Written before -> after |
|---|---|---|---|---|---|---|
| static | 14.3 (min) / 36.8 (median) | 8.2 (min) / 23.3 (median) | 1.75x | 1.58x | 24.7 -> 5.4 KiB/frame | 10500 -> 10500 B |
| editor | 29.0 (min) / 41.8 (median) | 19.2 (min) / 35.7 (median) | 1.51x | 1.17x | 33.1 -> 13.4 KiB/frame | 11656 -> 11656 B |

Clean-cluster RSDs: baseline static ~4.7%, editor ~8.4%; after static ~6.5%,
editor ~10.7% — all < 20%. Win gate >=1.05x median: **passed on both
scenarios under both estimators**. Written bytes are byte-identical — the
wire surface is unchanged.

**Recomputed multiple**: editor 19.2 us vs the ledger's 1.5 us floor ≈
**12.8x — still OPEN** (>2x ⇒ logged as intermediate; the unit iterates
again). Per G10 Finding 5 the terminal exhaustion record must recompute the
floor from the replacement's own per-line measurement, not cite the
implementation-derived 1.3 us/line term.

**Next design (reserved, materially distinct — Design D)**: the remaining
static-scenario allocation (5.4 KiB/frame) is the bench root's visible-window
materialization — `visible: Vec<String> = all_lines[start..].iter().cloned()
.collect()` per frame — a deep copy where the upstream `ScrollView` window is
a shallow `.slice()` of references. Design D serves the window as a borrow
(`&all_lines[start..]`), restoring upstream workload fidelity; the residual
after that is measured for the E1-E4 exhaustion record.

**Verification**: 395/395 pi-tui lib tests + integration suites green
(render-churn verification 3 — including editor>static allocation ordering,
which still holds: 13.4 > 5.4 KiB/frame; no-flicker PTY 5; state matrix;
theme 5; static-frame-evidence 1); `cargo clippy -p pi-tui --all-targets
--release --locked` clean on the changed files (the one new `needless_borrow`
the change introduced was fixed in-commit; remaining clippy warnings are
pre-existing in untouched files).

**Not touched (out of scope, file-disjoint)**: `scripts/` (DEPS lane),
`docs/TUI-CLOSE-*` (TUI-CLOSE), `packages/extension-host` (XC-CLOSE).
`pi/src/modes/interactive/runtime.rs` untouched this iteration (its
transcript path renders through the already-fixed `Text`/`Markdown` seam).
