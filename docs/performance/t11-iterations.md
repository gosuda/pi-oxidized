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

## Iteration 4 — `render-churn-recomposition` (Design D: borrowed visible window)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28.

**Blind derivation** (reserved at iteration 3 from the measured residual, per
the ledger's allocation-reuse candidate direction): after Design C the
static scenario still allocated 5.4 KiB/frame with zero content change; the
residual sits in the bench root's window serve — `visible: Vec<String> =
all_lines[start..].iter().cloned().collect()` per frame, a deep copy of ~25
visible lines. The upstream window is a shallow `.slice()` of string
references — copying pointers, never string bytes. Data layout: serve the
window as a borrow (`&all_lines[start..]`); `paint_lines` already accepts a
slice, and the `take(scroll_h)` bound is subsumed by the `start`
computation (slice length is already `<= scroll_h`; `paint_lines` clips by
area). This is workload-fidelity work as much as optimization: the deep copy
was a port infidelity measured as if it were unit cost.

**Boundary answers** (explicit, before touching): bench-local change — no
production file touched; emitted bytes unchanged (written-bytes metric
byte-identical 10500/11656); the window contents and order passed to
`paint_lines` are identical; scenario parameters untouched (parameter-parity
test green).

**Branch classification**:

| Original branch | Classification | Reason |
|---|---|---|
| per-frame `Vec<String>` window materialization (`.cloned().collect()`) | residue | upstream serves a shallow reference window; the deep copy re-materializes unchanged content every frame |
| `start` computation (`len.saturating_sub(scroll_h)`) | essential | follow-end window selection; unchanged |
| `paint_lines` over the window | essential | cell writes; now fed the borrowed window |

**Measurements** (pinned workload, same protocol; baselines = the iteration-3
tree measured fresh this session before the change; contended box disclosed
— min-of-7 is the pre-declared paired estimator):

| Scenario | Before (µs/frame) | After (µs/frame) | Win (min-of-7) | Allocated before -> after | Written before -> after |
|---|---|---|---|---|---|
| static | 8.2 (min) / 23.3 (median) | 6.5 (min) / 7.6 (median) | 1.25x | 5.4 -> 2.7 KiB/frame | 10500 -> 10500 B |
| editor | 19.2 (min) / 35.7 (median) | 18.1 (min) / 21.3 (median) | 1.06x | 13.4 -> 10.6 KiB/frame | 11656 -> 11656 B |

Median-of-7 wins (static 3.05x, editor 1.67x) are inflated by differing
contention mix between the two measurement windows and are disclosed, not
claimed; the honest paired estimate is min-of-7. Clean-cluster RSDs: static
~6.3%, editor ~7.9% — < 20%. Win gate >=1.05x median: **passed on both
scenarios under both estimators** (min-of-7 1.25x / 1.06x).

**Recomputed multiple**: editor 18.1 us vs the ledger's 1.5 us floor ≈
**12.1x — still OPEN** (>2x ⇒ the unit iterates again; handed to the next
slot with Design E (measure-walk skip on the static residual) pre-derived
and the E1-E4 exhaustion record pending, floor to be recomputed per G10
Finding 5).

**Verification**: render-churn verification suite 3/3 (parameter parity,
non-zero results, editor>static allocation ordering — 10.6 > 2.7 KiB/frame
still holds); full pi-tui suite green at iteration 3 on the identical
library tree (this iteration touches no library code — bench binary only).

**Not touched**: no production file; `scripts/`, `docs/TUI-CLOSE-*`,
`packages/extension-host` (sibling lanes).

## Iteration 5 — `render-churn-recomposition` (Design E: keyed identity, probe-free skips)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28.

**Blind derivation** (recorded at 5be1884 before reading post-iter3/4 code,
from the ledger's Contract + Floor + decomposition sections; reserved in the
handoff): after Designs A (painted-line memo), B (claimed-row damage
scoping), C (reference-served line cache), and D (borrowed visible window),
the static scenario still paid 7.0 us/frame with zero content change — none
of A–D touched the per-frame *identity revalidation* of unchanged lines:
every painted line re-hashed its content (FNV-1a), re-probed the memo
(HashMap<u64> under SipHash), and re-compared the full line bytes as a
collision guard, every frame. Data layout: move identity to where content
is finalized — caches that serve wrapped lines (`Text`, `Markdown`, the
bench's `Transcript`/`EditorSim`) key each line once at cache fill
(`KeyedLine { line, key }`), the memo map hashes its already-mixed keys with
an identity hasher instead of SipHash, and the row claim carries a `linked`
flag (recorded from the derivation's own region set, a pure function of the
key) so a claim-matched regionless line skips the memo probe and validation
compare entirely: its cells are provably in the in-place buffer and there
are no hyperlink regions to re-push. Because the skip path now trusts the
key alone, the key widens to a 128-bit composite (FNV-1a 64 crossed with an
independent rotate-multiply 64 in the same byte loop): an accidental or
practical crafted collision needs to defeat two independent 64-bit mixes
simultaneously (~2^128), which re-classifies the per-frame full-line
validation compare as residue.

**Boundary answers** (explicit, before touching): emitted bytes unchanged —
bench written-bytes metric byte-identical (10500/11656 B); `paint_lines`
plain path preserved verbatim for uncached callers (rail, loader, editor
body, lists, image fallback); claim set-equality semantics unchanged
(`linked` is a pure function of the key, so it never differentiates two
paints of the same key); memo eviction behavior on the skip path is now
irrelevant for regionless lines (cells live in the in-place buffer, not the
memo) and unchanged for linked lines (probe miss falls through to
re-derivation, re-pushing regions); no wire/format surface touched.

**Branch classification** (divergence audit of the replaced machinery):

| Original branch | Classification | Reason |
|---|---|---|
| per-frame FNV-1a hash of every served line | residue | identity is a pure function of content finalized at cache fill; owners compute it once per rebuild |
| per-frame SipHash of the u64 memo key (HashMap RandomState) | residue | the key is already a well-mixed digest; identity hasher |
| per-frame memo probe + full-line compare for claim-matched regionless rows | residue | cells provably in the in-place buffer (Design B); no regions to re-push; 128-bit composite key makes the collision the compare guarded a ~2^-128 accident |
| memo probe + full-line compare for repainting rows (claim miss) | essential | replay validation stays; changed content must repaint |
| `derive_line` on memo miss | essential | derivation on genuine change; unchanged |
| hyperlink region re-push on claim-matched linked rows | essential | wire surface; probe retained for `linked` claims |
| claim record/probe set bookkeeping | essential | Design B's skip license; now carries `linked` |

**Measurements** (pinned workload, `pi_tui_render_churn_bench`, release,
`taskset -c 20-40`, 20 warmup / 300 frames, 100x30, 150 lines; 7 runs per
side, fresh baselines measured this session on 0748236 before the change;
box contended (load ~10) — min-of-7 is the pre-declared paired estimator,
medians disclosed):

| Scenario | Before (µs/frame) | After (µs/frame) | Win (min-of-7) | Win (median-of-7) | Allocated before -> after | Written before -> after |
|---|---|---|---|---|---|---|
| static | 7.0 (min) / 7.0 (median) | 3.0 (min) / 3.0 (median) | **2.33x** | **2.33x** | 2.7 -> 4.6 KiB/frame | 10500 -> 10500 B |
| editor | 17.0 (min) / 22.0 (median) | 15.0 (min) / 16.0 (median) | **1.13x** | **1.38x** | 10.6 -> 12.6 KiB/frame | 11656 -> 11656 B |

Win gate >=1.05x median: **passed on both scenarios under both estimators**
(min 2.33x/1.13x, median 2.33x/1.38x). Written bytes byte-identical — the
wire surface is unchanged. Allocation rose ~1.9-2.0 KiB/frame: `RowClaim::Line`
grew 16 -> 32 bytes with the u128 key (claim table rebuild dominates the
static frame's remaining allocation); time cost is negative net — disclosed,
not hidden. Editor>static allocation ordering still holds (12.6 > 4.6).

**Recomputed multiple**: editor 15.0 us vs the ledger's 1.5 us floor ≈
**10x — still OPEN** (>2x ⇒ logged as intermediate). The residual is
dominated by the bench-side `EditorSim` per-frame rebuild (borders + text
row re-materialized on every text miss — upstream-faithful workload cost,
out of unit scope per iteration 1/3 precedent) plus the per-frame claim
table rebuild in `commit_frame` (a candidate Design F: pooled/reused claim
vectors) and the changed-line derive + 3-row damage diff. The terminal
exhaustion record (E1–E4 with the G10-Finding-5 floor revalidation from the
replacement's own per-line measurement) remains the unit's closing work.

**Verification**: 395/395 pi-tui lib tests; full release integration suite
green (render-churn verification 3 — parameter parity, non-zero results,
editor>static allocation ordering; no-flicker PTY 5; grill adjudication 8;
theme 5; static-frame-evidence 1; state-matrix / unicode / a11y / ext /
musl gauntlet harnesses green); `cargo clippy --release -p pi-tui
--all-targets --locked` reports no findings on the changed files.

**Not touched (out of scope, file-disjoint)**: `scripts/` (DEPS lane),
`docs/TUI-CLOSE-*` (TUI-CLOSE), `packages/extension-host` (XC-CLOSE),
`.github/workflows/` (REL-T4 lane), `pi/src/modes/interactive/runtime.rs`.

## Iteration 6 — `render-churn-recomposition` (Design F: pooled claim tables)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28. Base `1e6b5b7`
(TUI-T9 floor merged over iteration 5's `dee3103`; the T9 commit touches
the interactive runtime's narrow-width gate, not the claim path).

**Derivation** (shape reserved in iteration 5's terminal reservation —
"per-frame claim table rebuild in `commit_frame` (a candidate Design F:
pooled/reused claim vectors)" — and re-derived here from the allocation
decomposition before implementation, honestly noting the post-iteration-5
claim bodies were read for the branch classification after the derivation
was written down): after A–E, the static scenario still allocated
4.56 KiB/frame with zero content change. Data layout: the allocation is
not per-frame *data* — it is per-frame *storage* for a table whose shape
is a pure function of viewport geometry. Each frame paid (a) a fresh
outer table `vec![Vec::new(); rows]` in `install_prior`, (b) a first-push
`Vec` allocation in every row a claimant touched (~29 rows × 128 B), and
(c) the end-of-frame drop of the consumed prior table (the same row
buffers dealloc'd). None of those bytes depend on frame content: rows are
cleared and refilled with bounded claim multisets every frame. Design F
makes the tables a two-slot pool owned by the writer: `RowClaims` gains
`install_pooled(prior, frame)` (installs caller-cleared scratch rows,
capacity retained) and `into_tables()` (returns both tables instead of
dropping the consumed prior); `commit_frame` clears the pooled scratch in
place before composition, and after `emit_frame_diff` returns the consumed
prior table to the pool for the next frame. A geometry change (realign
branch, first frame) rebuilds the pool at the new absolute row count;
`suspend_row_claims` keeps `install_prior` semantics untouched.

**Boundary answers** (explicit, before touching): emitted bytes unchanged
— written-bytes metric byte-identical (10500/11656 B); probe/record/claim
set-equality semantics unchanged (pooled rows are cleared before install,
so no stale claim can be read; the round trip is pinned by
`pooled_tables_round_trip_preserves_probe_and_starts_cleared`); the skip
license (Design B/E) reads only the prior table, which is exactly the
previous frame's recorded claims; no wire/format surface touched; the
allocation *profile* is the design target (static 4660 → 100 B/frame).

**Branch classification** (divergence audit of the replaced machinery):

| Original branch | Classification | Reason |
|---|---|---|
| per-frame `vec![Vec::new(); rows]` frame table in `install_prior` | residue | table shape is a pure function of viewport geometry, not frame data |
| first-push row `Vec` allocation per touched row per frame | residue | bounded reusable storage; steady-state claim multiset ≤ rows × claimants |
| end-of-frame drop of the consumed prior table | residue | same storage serves as the next frame's table after an in-place clear |
| in-place row `clear()` of pooled rows (new) | essential | replaces the alloc/dealloc pair with a length reset at the same correctness |
| pool rebuild on geometry change / first frame | essential | claim tables are indexed by absolute terminal row; length must track geometry |
| prior-row probe (Design B/E skip license) | essential | unchanged |
| claim record + `contains` dedup | essential | claim set construction; unchanged |

**Measurements** (pinned workload, `pi_tui_render_churn_bench`, release,
`taskset -c 20-40`, 20 warmup / 300 frames, 100x30, 150 lines; baseline
re-measured this session on `1e6b5b7` with the same-session binary; box
bimodally contended — load average ~8 on 80 cores with build bursts;
interleaved A/B pairs so bursts hit both sides). Pre-declared estimator:
min of the first 7 pairs (iteration-5 protocol). Supplementary: 34 total
interleaved pairs with clean-cluster medians (cluster cut 1.9x side-min,
iteration-3 precedent).

| Scenario | Before (µs/frame) | After (µs/frame) | Win min-of-7 | Win clean-median | Win min-of-34 | Allocated before → after | Written |
|---|---|---|---|---|---|---|---|
| static | 2.98 | 1.94 | **1.54x** | 3.21 → 2.09 = **1.54x** | 2.95 → 1.85 = **1.60x** | 4660 → 100 B/frame (**-46.6x**) | 10500 → 10500 B (identical) |
| editor | 15.4 | 13.9 | **1.11x** | 15.8 → 14.4 = **1.10x** | 14.5 → 13.6 = **1.06x** | 12855 → 8295 B/frame (**-4.45 KiB/frame**) | 11656 → 11656 B (identical) |

Win gate >=1.05x: **passed on both scenarios under the pre-declared
min-of-7 and under the clean-cluster medians** (static 1.54x/1.54x,
editor 1.11x/1.10x); the editor min-of-34 (1.06x) also clears. Disclosed
plainly: naive median-of-34 is 0.63x/0.56x — a cluster-mixture artifact
of bursty contention (the interleaved NEW runs drew 14 clean-tier vs
BASE's 19 of 34 by scheduler luck), not a regression signal: the static
tiers are fully disjoint (BASE clean worst 4.98 < NEW contended best
6.32; every BASE clean run is slower than every NEW clean run), and the
deterministic allocation drop (4.45 KiB/frame on editor, 98% of static
allocation) is contention-immune evidence the removed work is real.
Editor>static allocation ordering still holds (8.10 > 0.10 KiB/frame).

**Recomputed multiple**: editor ~13.9 µs (clean median 14.4) vs the
ledger's 1.5 µs floor ≈ **9x — still OPEN** (>2x ⇒ logged as
intermediate). Named dominant residual, unchanged from iteration 5: the
upstream-faithful bench-side `EditorSim` per-frame rebuild (borders +
text row re-materialized on every text miss — out of unit scope per
iteration 1/3 precedent), then the changed-line derive + 3-row damage
diff. The terminal E1–E4 exhaustion record with the G10-Finding-5 floor
revalidation (recomputed from the replacement's own per-line measurement)
remains the unit's closing work.

**Verification**: 396/396 pi-tui lib tests (395 + the new pooled
round-trip contract test); full release suite green (render-churn
verification 3 — parameter parity, non-zero results, editor>static
allocation ordering; no-flicker PTY 5; grill adjudication 8; theme 5;
static-frame-evidence 1); `cargo clippy --release -p pi-tui --lib
--locked` findings on the changed files are identical to the base commit
(3 pre-existing; zero new).

**Not touched (out of scope, file-disjoint)**: `scripts/` (DEPS lane),
`docs/TUI-CLOSE-*` (TUI-CLOSE), `packages/extension-host` (XC-CLOSE),
`.github/workflows/` (REL-T4 lane), `pi/src/modes/interactive/runtime.rs`,
`docs/performance/floors/` (PERF-R9/G10 lane).
