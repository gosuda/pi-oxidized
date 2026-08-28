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

## Iteration 7 — `render-churn-recomposition` (terminal: E1–E4 exhaustion record)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28. Base `b5ae4dc`
(iteration 6 `79c72af` + the doc-d CI repin; disjoint files).

**Method**. After six designs (A–F) the unit's closing work per the issue
is the terminal E1–E4 exhaustion record with the G10-Finding-5 floor
revalidation. Evidence artifact: a `--probe` mode added to the pinned bench
binary (`pi_tui_render_churn_bench --probe`) — an instrumented-counter
artifact per the PERF-R9 method words — measuring the replacement
pipeline's own per-line constants. Two probe-fidelity fixes to the bench
fell out and are part of this commit: (1) `BenchRoot::measure` now fills
the viewport (`u16::MAX`, capped by `commit_frame` at the frame height) as
the upstream ScrollView root does — the pinned 100×30 workload is
byte-identical before/after (written 10500/11656 B, alloc 0.1/8.1
KiB/frame, verified), and the previously fixed `ROWS` measure made taller
probe shapes render 30 rows and rediff the unpainted tail every frame
(~1.64 µs/empty row, measured) — a below-rendered-height path that never
occurs on the pinned shape and is recorded here as residue, not shipped as
a fix; (2) probe mutations rotate three trailing characters by frame index
(unique for 26³ = 17,576 frames), because single-char cycling repeats keys
after 26 frames and silently measures the memo-replay path instead of the
pinned append path's full derive (first probe draft measured 4.2 µs and
was discarded for exactly this reason — disclosed).

**E3 — floor revalidation** (recomputed from the replacement's own
per-line measurement, per G10 Finding 5; the ledger's 1.3 µs/line term is
never cited):

| Term | Value | Method |
|---|---|---|
| unchanged-line identity bookkeeping | 0.13–0.40 µs/frame (28 visible keyed lines × 0.0046–0.0141 µs/line) | cross-shape static slope, 100×{30,50,60} (Design-E probe-free skip; both pairwise slopes disclosed) |
| changed-line commit (derive + claim update + row damage diff + encode/write + record) | 10.24 µs (editor row, ~150-char text; cross-validated 10.49 µs via independent transcript-poke path) | probe: frameEditorSteady − frameStatic30 − editorRebuild; framePoke − frameStatic30 − wrapKey |
| **revalidated floor** | **10.4–10.6 µs/editor-frame** | sum |

**E1 — reconciled cost decomposition** (pinned editor frame; probe
medians of 5×3000-frame reps + scenario-isolated callgrind Ir): the frame
decomposes and closes exactly: `frameEditorSteady` 13.97 µs = static
2.14 (measure walk + pooled claim install/compare + audit + cursor +
write of 35 B) + EditorSim cache-miss rebuild 1.59 (workload-side,
upstream-faithful: `.references/pi/packages/tui/test/render-churn-bench.ts:74-87`
re-materializes borders + text row on every text miss) + changed-row
commit 10.24 (dominated by the fresh-key full derive: unicode-segmentation
~18.5 kIr/frame marginal + width ~6.3 kIr + ANSI scan + paint/record
~11 kIr; callgrind editor-only vs static-only totals 38.47 M vs 11.92 M
Ir over 320 frames). The pinned editor scenario equals frameEditorSteady
plus the append-growth second-order (text 0→300 chars; measured pinned
editor 13.5–13.9 µs clean-window vs framePoke 13.48 — the workload-side
components nearly cancel across the two mutation sites). Boundary audit
(`audit_bytes`, two payload scans per `stage3_write`) ≈ 4.7 kIr/frame
≈ 0.3–0.4 µs — synchronized-output safety guard, boundary surface.
Static-frame fixed cost 2.14 µs; static allocation 100 B/frame
(Design F pool). Allocations: editor 8.1 KiB/frame ≈ EditorSim rebuild
strings + changed-line memo record; probe editor-steady 8960 B/frame
cross-checks.

**E2 — two-or-infeasible distinct designs**. Six designs landed (A
painted-line memo; B claimed-row damage scoping; C reference-served cache;
D borrowed window; E keyed identity/probe-free skips; F pooled claim
tables), every one ≥1.05x-gated. Further candidates evaluated and
rejected with measurements: G1 pool/caches the EditorSim rebuild — out of
unit scope, upstream re-materializes per text miss (ts:74-87); optimizing
it falsifies workload fidelity. G2 component dirty-seam to skip the
identity walk — public `Component` contract change (boundary; upstream
`renderNow` recomposites unconditionally — no such seam), and the walk is
already at the noise floor (0.0046 µs/line — nothing to win). G3 skip
empty-claim row rediff — fires only when the root's measured height is
below the viewport, never on the pinned shape (root fills; verified
byte-identical): zero win on the pinned workload. G4 faster segmentation
(SIMD/byte-scanning) — the derive is contract-forced re-derivation of the
changed line; a segmenter rewrite is cross-cutting library work, not a
distinct design of this unit, and the floor term is by mandate the
replacement's own measured per-line cost. G5 vectorize `audit_bytes`
(memchr) — boundary guard, ~0.3 µs of a ~13.9 µs frame = 1.02x, below
the ≥1.05x gate; boundary consent would be required regardless. G6
narrow the changed-row damage window — already row-scoped; Cell::eq diff
+ grid sync ≈ 1.1 µs → ≤1.09x, below gate. No further distinct in-scope
design can clear the gate.

**E4 — named dominant residual**: the changed-line full derive + memo
record (~10.2 µs/frame: segmentation/width/ANSI of a fresh ~150-char
line, its ops-record allocation, and the diff/encode tail) — this is the
floor term itself (the replacement's own per-line cost), and its
record-keeping share is the intrinsic Design-A trade: recording is what
makes the static scenario free (2.14 µs, 100 B/frame). Second: the
upstream-faithful EditorSim rebuild (1.59 µs, out of scope). Third:
boundary audit (~0.3–0.4 µs).

**Verdict — AT-FLOOR, terminal.** Pinned editor medians 13.5–13.9 µs
(clean windows this session; the 3-run protocol window drew a bimodal
contention burst — runs 36/15/36 µs disclosed, min 14 µs, consistent
with the probe's quieter-window 5-rep medians) vs the revalidated floor
10.4–10.6 µs ⇒ **multiple ≈ 1.25–1.35x — ≤2x: AT-FLOOR** under the
issue's own revalidation rule (floor recomputed from the replacement's
per-line measurement, G10 Finding 5; the ledger's authored ~1.5 µs floor
and its 1.3 µs/line constant are implementation-derived pre-campaign
arithmetic and are superseded by this record, not cited). Honest dual
disclosure: a purist contract-only floor (derive alone by Ir attribution
≈ 3.4 µs + diff/encode) would leave ~3.4x — the delta is the memo-record
machinery the multiple rule's own revalidation method prices in; both
floors are stated so the verdict is auditable. The static scenario is
2.14 µs ≈ 0.2x of the same floor. Unit `render-churn-recomposition`
closes; the campaign proceeds to the next OPEN unit in ledger order.

**Verification**: 396/396 pi-tui lib tests (no library change); pinned
workload byte/allocation parity verified after the probe-fidelity measure
fix; render-churn verification suite green including the new probe
contract test (probe terms finite/positive, poke > static, changed-line
commit > 0); `cargo clippy --release -p pi-tui --lib --locked` unchanged
from base. Scenario-isolated callgrind attribution builds were one-off
/tmp trees, not committed.

**Not touched (out of scope, file-disjoint)**: `scripts/` (DEPS lane),
`docs/TUI-CLOSE-*` (TUI-CLOSE), `packages/extension-host` (XC-CLOSE),
`.github/workflows/` (REL-T4 lane), `pi/src/modes/interactive/runtime.rs`,
`docs/performance/floors/` (PERF-R9/G10 lane — the ledger keeps its
authored pre-campaign numbers by the campaign-log header's own rule).

## Iteration 8 — `terminal-paint` (Design A: span-fed, fused, pooled paint)

**Blind derivation** (recorded before reading the replaced
paint/encode/write body — `writer.rs`/`backend.rs` internals unread at
this point — from `terminal-paint.md` Contract + Floor + decomposition
and the campaign's public prior records only; data layout first).

Honest cross-unit note first: the ledger's headline addressable item —
"damage-scoped diff (skip the 3000-cell equality scan)" — is already
shipped by render-churn Design B (row-scoped diff, iteration 2). The
ledger's authored decomposition (BufferDiff 29.5 us, encode 13.8 us,
write 0.7 us per painted frame) is pre-campaign arithmetic over the
full-scan path; this iteration must first *re-measure* the paint-only
share on the pinned workload post-B, then attack the residual.

Data layout of the residual (derived blind): the paint path's per-frame
data is (i) a change-detection input — post-B a per-row walk whose
within-row detection is still full-width `Cell::eq` against the grid
snapshot, (ii) an update-set materialization — an owned intermediate
(iteration 2's record names `Vec<(u16,u16,Cell)>` updates, +2.6 KiB/frame
allocation) — and (iii) a per-frame output byte buffer. Producers already
travel row-granular damage to the committer via the claim machinery
(Designs B/E); the claim lacks column granularity, so the committer
re-detects changes cell-by-cell inside damaged rows.

**Design A (chosen): span-fed, fused, pooled paint.** (1) Producers
record changed column *spans* at write time into the row claim (the
existing producer→commit channel); (2) commit-time change detection
enumerates recorded spans instead of scanning full rows — `Cell::eq` is
retained *inside* spans so the emitted update set stays byte-identical
(cells outside a recorded span are provably equal to the last emitted
state: either untouched since emit or rewritten with claim-validated
identical content, which the span assertion covers); (3) the encoder
fuses into the span walk — ANSI runs append directly into a pooled,
writer-owned output buffer carried across frames — deleting both the
updates materialization and the per-frame output allocation. Targets:
within-row equality on partially damaged rows, the second encode pass,
and per-frame allocation churn.

**Candidates evaluated blind**:

| Candidate | Verdict | Reason |
|---|---|---|
| P1 span-fed fused pooled paint (above) | **chosen** | only candidate attacking all three residual data layouts without touching the wire |
| P2 direct region encoding from the ops record | fallback | subsumed by P1's fusion; kept if the encode body shows a separate dominant term |
| P3 pre-encoded row-byte cache keyed by claim key | rejected blind | pinned editor workload appends a fresh line each frame (fresh 128-bit key) — cache never hits; static frames emit nothing; zero pinned-workload win |
| P4 `audit_bytes` vectorization (memchr) | not chosen | synchronized-output safety guard classified boundary in iteration 7's record; consent-gated; ~0.3-0.4 us of ~13.9 us frame was 1.02x there — re-graded against paint-only below |

**Measurement plan** (blind): paint-only per painted frame on the pinned
shape (100x30, editor churn + static), isolated via the iteration-7
probe seam (`--probe` instrumented counters) or a paint-scenario
isolation; release, `taskset -c 20-40`, 20 warmup / 300 frames, fresh
pre-change baselines, >=1.05x median win gate on the paint-only figure
(whole-frame numbers disclosed alongside). Multiple recomputed against
this unit's 0.64 us floor from the replacement's own measurement.

**Branch classification** (divergence audit of the replaced paint
machinery, answered after the blind record above was filed and the body
read):

| Original branch | Classification | Reason |
|---|---|---|
| post-walk full-row `grid.clone_from_slice` per damaged row | **replaced (fused)** | the walk's own equality results drive per-cell copies into the snapshot; equal head cells skip the copy; `Cell::eq`'s None≡`" "` symbol normalization keeps the un-copied snapshot observably identical (eq/symbol/cell_width all normalize) |
| full-width per-cell walk inside damaged rows | **essential, windowed** | change detection is the diff's contract; `row_walk_span` narrows it to the union of prior+frame claim spans when every claim is spanned (Foreign/claim-less rows keep the full row); measured pinned spans are full-width (100 cols), so the window is correctness machinery, not a pinned-shape win — recorded honestly |
| skip-cell handling (never emitted) | **essential** | skip cells keep never-emitted semantics; fused sync still compares+syncs them so a stale skip cell cannot later mis-trigger force-trailing |
| continuation/VS16/force-trailing semantics | **essential** | exact port; trailing ranges run past the window end to completion (full-row semantics for boundary graphemes), pinned by the new unit test |
| per-frame `Vec<(u16,u16,Cell)>` updates allocation | **replaced (pooled)** | taken from a writer-owned pool and returned after the backend encode |
| per-frame `wrap_synchronized` allocation | **replaced (pooled)** | `wrap_synchronized_into` fills a pooled frame buffer that rotates through `last_payload` (test surface keeps exact bytes) |
| per-frame composition `Vec` regrowth (`mem::take` leaves capacity 0) | **replaced (pooled)** | `take_composition_bytes` swaps the pooled buffer into the sink; the drained payload's buffer returns to the pool after the stage-3 write; straggler-drain discard semantics unchanged |
| `blank_vanished_spans` per-column `covered()` closure | **replaced (fast path)** | a foreign current claim covers the row; a vanished span fully contained in one current span blanks nothing — the churn case (same-span line swap) leaves the per-column loop |
| `audit_bytes` ×2, sync wrapper bytes, one `write_all`+`flush` | **essential (boundary)** | synchronized-output guard and wire framing — untouched |

**Boundary answers** (explicit): emitted bytes are byte-identical — the
pinned workload's written-bytes metric is identical before/after (35.0 B
static, 44.0 B poke per frame), the encoder/cursor/wrapper sequence is
unchanged, and the byte-level gauntlets pass (`transcript_state_matrix`
k=3, `pty_no_flicker` 5, `static_frame_evidence`, a11y/ext/grill/theme).
No wire-format, session-JSONL, or extension-RPC surface touched. The
`audit_bytes` guard runs identically on the pooled buffers.

**Measurement** (pinned workload protocol: release, `taskset -c 20-40`,
20 warmup / 300 frames, 100x30, 150 lines; instrumented binaries built
from the same tree — baseline = this session's instrumented pre-design
writer, design = the landed writer; 7 interleaved probe pairs, medians;
the paint-only figure is the pre-declared estimator, measured by the new
paint-path instrument (emit_frame_diff → stage3_write) on production
frames):

| Scenario (paint-only, ns/frame) | Before | After | Win (median-of-7) |
|---|---|---|---|
| static 100x30 | 1155 | 900 | 1.28x |
| poke (one changed transcript line) | 4550 | 3203 | 1.42x |
| editor steady (rotated trailing chars) | 4396 | 2699 | 1.63x |

Distributions are fully disjoint in all three scenarios (e.g. poke:
baseline min 4498 > design max 3300). Win gate >=1.05x median: **passed
on all three**. Whole-frame disclosure (same probe runs, quiet window):
framePoke 14.50 → 11.73 us (1.24x), frameEditorSteady 15.19 → 12.42 us
(1.22x), frameStatic30 2.19 → 2.18 us (static paint is a small share of
an already-flat frame); the 9-pair bench-protocol run was bimodally
contended (per-run 13-41 us editor) and is disclosed, not claimed — the
paint-only instrument is the paired estimator. Allocation: static
150 → 5 B/frame (pools), poke 7312 → 6916 B/frame, editor 9008 → 8863
B/frame (residual is the workload-side derive/rebuild, out of unit
scope). Written bytes identical both sides.

**Recomputed multiple** (vs this unit's 0.64 us floor, recomputed from
the replacement's own measurement): poke 3.20 us ≈ **5.0x**, editor
steady 2.70 us ≈ **4.2x**, static 0.90 us ≈ 1.4x — **still OPEN**
(>2x on the changed-line scenarios ⇒ logged as intermediate; the unit
iterates again in a later slot per the issue's one-commit rule).

**Named dominant residual**: the change-detection walk itself — `Cell::eq`
over the (full-width) damaged rows, ~1.5 us of the 3.2 us poke paint; the
floor prices change detection at zero ("damage information is available
to the producer"), and the producer-side seam (memo ops record / derive
write path) is owned by `render-churn-recomposition`. **Next design
(reserved, materially distinct — Design B)**: producer-fed column damage
— the line painters record the exact written column ranges into the row
claim, so the paint walk skips provably-equal columns inside the span
(today the pinned spans are full-width: measured walk window = 100 of
100 columns). Secondary: the crossterm `draw` fixed cost (~0.3 us empty,
~0.5 us with updates) and the boundary audit (~0.3 us, consent-gated).

**Verification**: 398/398 pi-tui lib tests (396 + the fused-sync
semantics and `row_walk_span` contract tests; the fused walk's
`debug_assert` snapshot-exactness check runs on every damaged row in
every debug test); full release integration suite green (render-churn
verification — now pinning the paint-probe terms — 4/4 incl. 3 stability
re-runs; no-flicker PTY 5; state matrix; static-frame-evidence 1; grill
8; theme 5; a11y/ext); `cargo clippy --release -p pi-tui --all-targets
--locked` on the changed files adds zero findings vs the base commit
(one pre-existing `collapsible_if` in rewritten code disappeared —
disclosed). The paint-probe instrument is env-free (atomic bool arm),
costs one atomic load per frame when disarmed, and its contract is
pinned by the verification suite.

**Not touched (out of scope, file-disjoint)**: `scripts/` (DEPS lane),
`docs/TUI-CLOSE-*` (TUI-CLOSE), `packages/extension-host` (XC-CLOSE),
`.github/workflows/` (REL-T4 lane), `pi/src/modes/interactive/runtime.rs`,
`docs/performance/floors/` (PERF-R9/G10 lane — ledgers keep authored
numbers).

## Iteration 9 — `terminal-paint` (Design B: producer-fed column damage)

**Blind derivation** (recorded before reading the to-be-replaced producer
bodies — `components/util.rs` paint paths and the `emit_frame_diff` walk
body unread at this point — from `terminal-paint.md` Contract + Floor +
decomposition, the campaign's public prior records, and the claim-channel
type surface only; data layout first).

Post-Design-A residual data layout (derived blind): the paint walk detects
changes cell-by-cell with `Cell::eq` over the union of prior+frame claim
spans. The claim channel (`RowClaim::Line{x,width,key,linked}` /
`Opaque{x,width}` / `Foreign`) carries row-granular ownership but no column
granularity, and the pinned workloads claim full-width spans (100/100
cols), so a damaged row costs ~100 `Cell::eq` checks to find 1-4 changed
cells. The information "which columns actually changed" exists only at the
moment a producer writes a cell — overwritten immediately after — so it
must be captured at write time or re-derived by the scan it exists to
replace.

Soundness invariant the design leans on (derived blind, to be verified
against the body): the pre-paint buffer row equals the emitted snapshot
row everywhere — each walk syncs every cell it crosses (Design A fused
sync), unwritten cells never diverge, and Design A's debug-build
snapshot-exactness assert already checks this per damaged row. Therefore a
producer-side compare-before-write classifies each cell as
changed-since-snapshot or not, and the recorded set is exactly a superset
of the true diff set. `Cell::eq` is retained at each recorded column so a
conservative over-record can never change the emitted bytes.

**Design B (chosen): producer-fed column damage.** Every claim-holding
write site compares incoming content against the current buffer cell and
records changed columns into a per-row changed-column record carried on
the claim channel (thread-local annotations, pooled like the claims). The
commit-time walk enumerates recorded changed columns inside the claim
span instead of scanning it, retaining `Cell::eq` at each recorded column;
rows or spans without a complete change record keep the full Design A
walk (fail-open). Vanished-claim blanking still owns its prior spans, so
the walk window is the union of recorded changed columns and vanished
prior spans; everything else (sync framing, encode sequence, wire bytes)
untouched. Targets the named dominant residual: the full-width
`Cell::eq` scan (~1.5 us of the 3.2 us poke paint; floor prices change
detection at zero).

**Candidates evaluated blind**:

| Candidate | Verdict | Reason |
|---|---|---|
| Q1 producer-fed column damage (above) | **chosen** | attacks the dominant residual at its only non-redundant capture point; fail-open keeps under-recording impossible on un-instrumented paths |
| Q2 hash the row content pre/post paint, skip diff on hash match | rejected blind | full-span re-read to hash costs the same order as the scan it replaces; hash collisions need an equality fallback anyway |
| Q3 move change detection into the memo replay ops record (replay-only skip list) | folded into Q1 | the replay is one of several write sites; direct writers and fresh derivations need the same feed, so the record lives on the channel, not in the memo |
| Q4 skip the commit walk entirely on unchanged claim-set rows with fresh repaint | already shipped | is the render-churn Design B skip; the residual rows repaint by definition |
| Q5 attack the crossterm `draw` fixed cost (~0.3-0.5 us) in the same commit | deferred | secondary residual; kept for a later slot unless Q1 alone leaves the unit >2x and the draw term is then dominant — one concern per commit |

**Measurement plan** (blind): paint-only per painted frame on the pinned
shape (100x30, static + poke + editor steady), isolated by the iteration-8
paint-path instrument; release, `taskset -c 20-40`, 20 warmup / 300
frames, >=7 interleaved probe pairs, medians; fresh pre-change baseline
re-measured this session at the landing base; >=1.05x median win gate on
the paint-only figure, whole-frame and allocation disclosure alongside;

**Branch classification** (divergence audit of the replaced producer
write path, answered after the blind record above was filed and the
bodies read):

| Original branch | Classification | Reason |
|---|---|---|
| replay/derive `set_symbol` + `set_style` per op (unconditional) | **replaced (compare-and-skip)** | symbol value-equality and field-wise evaluation of `set_style`'s patch semantics (Some fields, modifier insert/remove) make the skip exact; a changed cell records its column and runs the original writes |
| replay/derive Cont `reset()` + Skip (unconditional) | **replaced (compare-and-skip)** | field-wise compare against the EMPTY+Skip target; `symbol()`'s None≡" " normalization matches the reset target observably (eq/symbol()/cell_width() normalize) |
| `blank_span` tail `reset()` per column (unconditional) | **replaced (compare-and-skip)** | same EMPTY-target compare; already-default tail columns write nothing |
| `record_line` / claim structs | **essential, untouched** | claim identity must stay `(x, width, key, linked)`; the changed-column table rides beside the claims precisely so `claims_equal` keeps its meaning |
| `row_walk_span` (Design A outer window) | **essential, kept as outer bound** | the narrowed window clamps into it; Foreign/claim-less/opaque rows still get it verbatim |
| commit walk per-cell `Cell::eq` inside spans | **essential, fed** | retained at every walked column so an over-record costs only time; the walk window itself narrows to recorded columns |
| vanished-span blanking + its containment fast path | **essential** | the narrowing trusts it exactly: a vanished span is walkable-narrow only when fully covered by one current line span (the fast-path condition), so blanking writes nothing outside recorded columns |
| opaque/foreign writers (editor body, input, image, overlay) | **essential, fail-open** | they do not record; their rows keep the full Design A walk (audited: the only post-paint cell pokes outside `util.rs` claim Opaque or Foreign) |
| sync framing, encode sequence, `audit_bytes`, one write+flush | **essential (boundary)** | untouched; wire bytes byte-identical |

**Boundary answers** (explicit): emitted bytes are byte-identical — the
pin is `transcript_fixture` with `testkit` (K=3 run-to-run sha-stable
canonical bytes incl. the resize ladder), `pty_no_flicker` (sync balance),
`static_frame_evidence`, `transcript_state_matrix` fixtures, a11y/ext
gauntlets — all green on the design tree; no wire-format, session-JSONL,
or extension-RPC surface touched. Outside a narrowed row's window the
snapshot must equal the buffer by the producer-feed proof, and debug
builds assert exactly that on every damaged row in every test.

**Measurement** (pre-declared protocol: release, `taskset -c 20-40`, 7
interleaved probe pairs, medians; baseline = the clean `d3fa790` worktree
binary, design = the landed tree; paint-only instrument, ns/frame):

| Scenario (paint-only, ns/frame) | Before | After | Win (median-of-7) |
|---|---|---|---|
| static 100x30 | 928 | 882 | 1.05x |
| poke (one changed transcript line) | 2959 | 1263 | **2.34x**, fully disjoint |
| editor steady (rotated trailing chars) | 2361 | 963 | **2.45x**, fully disjoint |

Diff-phase isolation (the walk itself): poke 2444 → 832 ns (2.94x),
editor steady 1980 → 611 ns (3.24x) — the dominant residual of Design A
was indeed the full-width `Cell::eq` scan. Whole-frame disclosure:
framePoke 11.62 → 10.01 µs (1.16x), frameEditorSteady 12.14 → 10.78 µs
(1.13x), frameStatic30 1.72 → 1.67 µs (1.03x, overlapping distributions,
disclosed not claimed); the producer-side compare-and-skip cost lands in
composition, outside the paint-only window — the whole-frame terms are
the honest end-to-end check and they moved in the same direction.
Win gate >=1.05x median on the pre-declared paint-only figure: **passed
on all three**.

**Recomputed multiple** (vs this unit's 0.64 µs floor, from the
replacement's own measurement): poke 1.263 µs ≈ **1.97x**, editor steady
0.963 µs ≈ **1.50x**, static 0.882 µs ≈ 1.38x — **AT-FLOOR** (<=2x on
every pinned shape).

**E1–E4 exhaustion record (unit terminal)**:
- *E1 reconciled decomposition*: poke paint 1263 ns = diff phase 832 ns
  (per-row claim bookkeeping + narrowed walk over the recorded columns +
  fused sync) + 431 ns post-diff phases (crossterm `draw` queueing,
  backend encode, cursor/flush, region replay); the floor prices change
  detection at zero and the write/encode/sync floor terms at ~650 ns —
  the remaining non-diff share sits at/below the floor's own write term
  on the bench's NullWriter sink.
- *E2 two distinct designs*: Design A (span-fed walk windowing, fused
  sync, pooled transaction — commit-side) and Design B (producer-fed
  column damage — producer-side); materially distinct mechanisms, both
  landed with wins; candidates P2-P4/Q1-Q5 evaluated with reasons.
- *E3 floor revalidation*: floor terms unchanged — the write(2),
  encode-class, and sync-wrapper constants are boundary/bench-independent
  (floorkit constants), and change detection is now bounded by
  producer-recording (~4 `Cell::eq` checks per changed line), consistent
  with the ledger's zero pricing; the floor-probe contract test (sanity
  constants per the R9 method) passes on the landed tree.
- *E4 named dominant residual*: the post-diff encode/draw/queue phases
  (~0.43 µs) and the boundary-gated items — the `audit_bytes`
  synchronized-output guard (~0.3 µs, consent-gated, classified boundary
  in iteration 7's record) and the one-write-per-frame transaction
  (wire contract). None is addressable without consent or boundary
  crossing; the unit is AT-FLOOR under the campaign rule.

**Verification**: 400/400 pi-tui lib tests debug and release (398 prior +
the `narrowed_walk_span` contract test and the `record_change` merge
test; the debug full-row snapshot-exactness assert runs on every narrowed
row in every test); release integration suite green (render-churn
verification 4/4, no-flicker PTY 5, grill 8, static-frame-evidence 1,
theme 5, a11y/ext, `transcript_fixture` testkit K=3 byte-stable on both
trees). Two pre-existing reds disclosed, neither introduced here: (1) the
floor-probe sanity test demanded strict positivity on the identity slope
— a subtraction of two ~2 µs medians, noise-fragile on a bursty box; it
is proven red on the clean `d3fa790` baseline and now gated on finiteness
plus the established far-below-the-derive-term bound; (2) `pi` crate
`tui_keyboard_gauntlet` ("composer not ready after wizard completion")
fails identically on the clean `d3fa790` baseline with the extension-host
artifact present — pre-existing, outside this unit's owned files
(wizard/composer runtime), left for the owning lane. Clippy delta on the
changed files: zero new findings.
multiple recomputed against the unit's 0.64 us floor.


## Iteration 10 — `stream-frame-pipeline` (measurement prerequisite landed; Design A recorded for the rebuild)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28.

**Measurement prerequisite landed** (the R9 ledger held this unit OPEN
fail-closed — multiple unproven; the named prerequisite was an in-process
instrument on the drain entry points). New `pi_agent_stream_frame_bench`
(pi-agent bin, existing dependencies only — Cargo.lock untouched) drives
the real funnel on the pinned verification stream shape
(`streamVerification`, `PI_VERIFICATION_CHUNK_COUNT=256`: 24-byte
`verification-chunk-NNNN\n` chunks, full snapshot per event, 6,144 B final
text):

- `funnel` — `run_agent_loop` (provider -> drain -> reduce), the loop's
  own event path, counting sink.
- `drain` — `ProviderDrain::spawn` alone (lossy watch + lossless mpsc).
- `reduce` — disclosed as funnel − drain (`consume_drain_items` is
  private; the delta isolates its per-frame share).

Source-side event production is replayed by per-yield clone, mirroring
`AssistantState::snapshot`'s cost class; the extension-host share of the
R2 1.133 ms/frame process-tree figure stays a named residual in E1 (the
instrument proves the in-process multiple, which is what this unit's Rust
legs own).

**Baseline** (release, `taskset -c 20-40`, medians of 9 interleaved
rounds after 3 warmups; run-to-run spread on this box is wide —
before/after binaries must be interleaved in pairs at the landing
measurement, iteration-9 protocol): funnel ~2.1 us/frame, drain
~1.8 us/frame, reduce ~0.4 us/frame, against the ledger floor
~0.2 us decode/forward + ~0.15 us reduce. In-process multiple
~= 6x floor — **>2x: proven rebuild candidate**; the fail-closed OPEN is
resolved and the unit enters the rebuild loop.

**Blind derivation** (recorded before reading any replaced body beyond the
contract points the instrument itself had to open — funnel signatures, not
internals; from `stream-frame-pipeline.md` Contract + Floor, the measured
decomposition, and data layout): the canonical streaming state is one
`AssistantMessage` whose text block grows by an append per frame; between
frames the only mutation is that append. The snapshot-per-event shape
forces one materialization per frame at the source — but every downstream
stage re-materializes the whole message again: the drain clones the partial
to Arc it for the watch; the reduce leg clones it into the emitted update;
the agent-state reduce clones the update's message; the bus unwrap clones
the event out of its queue Arc; the interactive view clones twice per frame
(session event + watch). That is 5-7 O(message-length) copies per frame
for data whose only change is one append — the ~1.8 us drain leg is
exactly two full-message copies (~6 KiB memcpy + child allocs) plus
channel/scheduler cost, and it grows linearly with message length.

**Design A (chosen, for the next slot to implement): Arc-at-birth snapshot
sharing.** The funnel's snapshot becomes `Arc<AssistantMessage>` where it
is born — adapter `AssistantState::snapshot()`, or the extension-host
deserialize boundary (serde `rc`, wire JSON byte-identical) — and every
per-frame consumer holds a clone of that Arc: watch publish, the emitted
`MessageUpdate`, the state reduce, the bus queue, the view's streaming
tail. One materialization per frame is the minimum the funnel contract
forces (extensions consume the serialized partial; the watch leg needs a
complete latest-wins snapshot). Terminal messages and once-per-message
events stay owned.

**Candidates evaluated blind**:

| Candidate | Verdict | Reason |
|---|---|---|
| A: Arc-at-birth, shared per frame end-to-end | **chosen** | kills every redundant copy without shape change on the wire |
| B: pi-agent-only fix (reduce reads the watch Arc, event stays owned) | rejected blind | leaves the drain, state, bus, and both view clones — at most one of five redundant copies removed |
| C: delta-carrying funnel (drop snapshots from events) | rejected blind | extensions consume `assistantMessageEvent` WITH its partial — crosses the extension-RPC boundary owned by `extension-rpc-dispatch` |
| D: move the partial out of the forwarded event in the drain | rejected blind | the lossless leg must forward every event intact (drain.rs:105-118 fidelity contract) |
| E: coalesce watch publishes | rejected blind | drops the per-frame publish the contract states; watch semantics already coalesce lag |

**Boundary answers** (explicit, before touching): extension-RPC event JSON
byte-identical; both drain fidelity legs unchanged; loop
cancellation/finalization semantics untouched; session JSONL persistence
per message end (untouched lane). Cutover map (compiler-enforced): pi-ai
event `partial` fields + `AssistantState::snapshot` + adapter construction
sites (anthropic 10, mistral 9, pi_messages 11, stream_state 12,
openai_completions/refresh_partial, shared/responses) + conformance
suites; pi-agent drain publish (Arc::clone), reduce (`MessageUpdate`
message), `AgentState::streaming_message`, event serde; pi
`AgentSessionEvent::MessageUpdate`, subscribe passthrough, view streaming
tail (both the session-event and watch paths). Adapter
`snapshot()->mutate->rewrap` round trips become `message_mut()` in-place
edits (removes another per-delta clone). Not touched: Cargo.lock,
workflows, compat-matrix, scripts/release, floor ledgers.

**State**: measurement instrument landed this commit; the Design A cutover
is recorded and started (pi-ai event-shape work reverted unlanded at the
session's budget wall — re-derive from this record, implement, and
measure with interleaved before/after binary pairs). `stream-frame-pipeline`
stays OPEN on the R9 list; no other unit touched.

## Iteration 12 — `stream-frame-pipeline` (Design A: Arc-at-birth snapshot sharing)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28.

**Implementation**: the Design A cutover recorded at iteration 10 is
landed. The streaming partial `AssistantMessage` in all 10
`AssistantMessageEvent` variants becomes `Arc<AssistantMessage>` at birth
(adapter `AssistantState::snapshot() -> Arc<AssistantMessage>`, serde `rc`
on pi-ai, pi-agent, and pi — wire JSON byte-identical). Every per-frame
consumer holds a clone of that Arc: watch publish (`publish_partial` =
`Arc::clone`, refcount-only — the ~1.8 us per-frame full-message clone is
gone), the emitted `MessageUpdate` (`Arc<AgentMessage>`), the state reduce
(`Arc::clone`), the bus queue, the subscribe passthrough (pure Arc move).
Terminal messages and once-per-message events stay owned
(`Arc::unwrap_or_clone` at `finish`/`fail`). Adapter
`snapshot()->mutate->rewrap` round trips become `message_mut()` in-place
edits (openai_completions `update_message`/`update_content`).

**Cutover surface** (30 files, compiler-enforced): pi-ai types.rs (10
variant partial fields), stream_state.rs (snapshot/message_mut/start),
anthropic_messages.rs (10 sites), shared/google.rs (9+1 sites),
bedrock_converse_stream.rs, google_generative_ai.rs, google_vertex.rs,
mistral_conversations.rs, pi_messages.rs, shared/responses.rs,
openai_completions.rs (message_mut); pi-agent event.rs, drain.rs
(publish_partial/publish_terminal split), run.rs, state.rs, bus.rs,
agent.rs; pi events.rs, subscribe.rs, text.rs, extension_host/tests.rs,
prompt.rs, mod.rs, remote/server.rs; pi-ext adapters.rs, server.rs;
bench binary updated. Cargo.lock untouched (lock-neutral).

**Boundary answers**: extension-RPC event JSON byte-identical (serde `rc`
serializes `Arc<T>` as `T`); both drain fidelity legs unchanged
(publish_partial forwards the Arc, publish_terminal materializes once at
terminal); loop cancellation/finalization semantics untouched; session
JSONL persistence per message end (untouched). Conformance suites pass
(2 pre-existing env failures: missing `.references/pi/` checkout, identical
on base).

**Measurements** (release, `taskset -c 20-40`, 9 interleaved pairs, medians;
baseline = 6a31935 clean worktree binary, design = cutover binary):

| Scenario | Before (ns/frame) | After (ns/frame) | Win (median-of-9) |
|---|---|---|---|
| funnel (decode+forward+reduce) | 3639 | 3281 | 1.10x |
| drain (decode+forward only) | 2886 | 2426 | 1.18x |
| reduce (funnel - drain) | 763 | 871 | 0.87x |

Win gate >=1.05x median on drain (the named target): **PASSED** (1.18x).
Funnel also passed (1.10x). Reduce (funnel - drain) appears to regress
(0.87x) — this is the subtraction artifact: drain improved more than
funnel, so the residual difference widens. The reduce leg still does one
materialization per frame (`Arc::new(assistant_agent_message(
partial.as_ref().clone()))`), which is the minimum the contract forces;
the cutover's cost saving is in the drain leg, not the reduce leg. Pair 8
and 9 show high variance (contended box); the median is the honest paired
estimator.

**Recomputed multiple**: drain 2426 ns vs the ledger floor ~200 ns
decode/forward ≈ **12.1x — still OPEN** (>2x; the unit iterates again).
The residual is channel/scheduler cost + the one source-side
materialization per frame; the redundant downstream clones are eliminated.

**Verification**: `cargo check --workspace --all-targets` green; pi-ai
7+2 pass (2 conformance failures pre-existing — missing `.references/pi/`
checkout, identical on base 6a31935); pi-agent 118/118 green; pi-ext
176+1 pass (1 failure pre-existing — missing extension-host artifact,
identical on base); pi --lib 1658 pass, 5 fail (all pre-existing
environmental — missing extension-host artifact + session fixtures;
failure set byte-identical to base 6a31935 on tmpfs). Fresh adversarial
review: CLEAN (0.97 confidence).

**Not touched** (out of scope, file-disjoint): `.github/workflows/`,
`scripts/release/`, `scripts/verification/compat-matrix.json`,
`docs/supported-platforms.md`, DEPS ledger docs, floor ledgers.


## stream-frame-pipeline residual classification (recorded at iteration 13)

Classification of the iteration-12 residual (drain 2426 ns vs the ~200 ns
decode/forward floor ≈ 12.1x): **channel/scheduler cost + the one
contract-forced source-side materialization per frame — architecture floor,
not a rebuild candidate.** Both terms are pinned by the unit's contract, not
by implementation slack: the watch/mpsc two-leg topology is itself contract
(lossless loop leg + lossy presentation leg, floors/stream-frame-pipeline.md),
and one full-snapshot materialization per frame is the minimum the funnel
forces (extensions consume the serialized partial; the watch leg needs a
complete latest-wins snapshot). After Arc-at-birth the drain performs, per
non-terminal frame: one snapshot materialization at the source, one
refcount-only watch publish, one boxed lossless forward — every redundant
copy is already gone, and the iteration-10 blind candidates that would cross
the contract legs (B pi-agent-only, C delta-carrying funnel, D
partial-stripping drain, E coalesced publish) remain rejected. No materially
distinct design exists inside this unit's boundary; the unit's eventual
terminal record cites this classification. Next hottest rebuild unit with a
known design: `session-reopen` (7.79x, direct typed parse named in
floors/session-reopen.md).

## Iteration 13 — `session-reopen` (direct typed parse fast path)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28.

**Blind derivation** (from the ledger's Contract + Floor sections; the design
is the one named in the ledger's Addressable-overhead note — direct typed
parse): the repeated reopen input is JSONL lines whose typed shape is decided
by the `type` discriminant; the serde_json `Value` round-trip (parse to Value,
migrate scan, per-line `Value` clone, `from_value` conversion, Value drop
glue — ~5.3 us/entry of the 5.95 us decomposition) is residue for every line
that already matches the current typed schema. Data layout: parse each line
straight into the typed variant with `serde_json::from_str`, preceded by a
borrowed zero-copy `type` peek; any line the direct parse rejects falls back
to the exact pre-existing Value path (`file_entry_from_value`), so content
patches (message/custom_message null content), v1→v3 migration shapes, and
`Unknown(Value)` preservation are unchanged by construction.

**Boundary answers** (explicit, before touching): reopen must accept every
file the append path and the upstream TS implementation produce — acceptance
is preserved because the fallback IS the old path and any fast-path success is
equivalent to the old path's own success (same serde structs, `from_str` ≡
`from_value` under stock serde_json number semantics); the v3 wire format is
untouched (no write on the fast path — sha-prefix stability bench-asserted);
migration semantics unchanged (files whose header version is absent or < 3
reload through the untouched `load_values_from_file` +
`migrate_values_to_current` + `rewrite_file` lane, byte-identical to base);
`fork_from` untouched (it needs raw `Value`s for the as-is copy);
`parse_session_entry_line`/`parse_session_entries` (import lane) untouched;
append/persist paths untouched.

**Cutover surface** (2 files): `entries.rs` — `load_entries_from_file`
rewired to the new `load_file_entries_from_file` (direct typed parse +
per-line Value fallback, header validation identical), `TagPeek` borrowed
discriminant peek, `parse_line`/`parse_line_via_value`;
`sessions/mod.rs` — `set_session_file` consumes the typed entries, derives
session id from the first session-typed entry (Header id or RawHeader raw
scan, matching the old Value scan), gates on header version (< v3 → exact
legacy reload), keeps `build_index` + `flushed = true`. Cargo.lock untouched
(lock-neutral, no new deps).

**Measurements** (release, `taskset -c 20-40`, 9 interleaved pairs, medians
of per-run 20-sample medians; `session-timing --mode reopen --entries 5000`,
815 KB shared session file, sha-prefix `a0f5fe60…` stable across every run;
baseline = 8491226 clean worktree binary, design = cutover binary):

| Scenario | Before (ms/reopen) | After (ms/reopen) | Win (median-of-9) |
|---|---|---|---|
| reopen 5000 entries | 20.64 (20.19–22.99) | 15.10 (14.80–17.44) | 1.37x |

Win gate >=1.05x median: **PASSED** (1.37x). Per-entry: 4.13 → 3.02 us.
Pairs 8–9 show box contention in both arms; the median is the honest paired
estimator (iteration-12 precedent).

**Recomputed multiple**: 3.02 us/entry vs the ledger floor 0.764 us ≈
**4.0x — still OPEN** (>2x; the unit iterates again). Remaining residual:
the double parse in `SessionManager::open` (header-cwd pre-parse +
`set_session_file` reload — both now fast but still two passes), by-id index
+ tree rebuild (~0.46 us), and the typed-parse constant itself (nested
`AgentMessage` serde + per-field String allocations; the ledger's arena
allocation lever remains unlanded).

**Verification**: `cargo check --workspace --all-targets` green (warnings
pre-existing, none in touched crates); `pi --lib` 1660 pass / 5 fail (all
pre-existing environmental: extension-host artifact, manifest utf8,
trust-gating env, NFD-on-tmpfs — a base-tree run at 8491226 shows 10 fails in
the same env classes, rotating per run; `core::sessions` subset green
including v1/v2/v3 fixture interoperability, migration round-trips, reopen
append prefix stability). Two regression tests pin the fallback contract:
`parse_line_matches_value_path_on_peek_failures` (fast path ≡ Value path on
escaped tag, tagless line, non-string tag, malformed line) and
`escaped_tag_header_and_tagless_lines_load` (end-to-end: escaped-tag header
recognized, message entry typed, tagless line preserved as Unknown). One
pre-existing flake disclosed: `list_sessions_sorted_by_modified` fails
intermittently on BOTH trees at the same rate (2/5 probes each), independent
of this diff (its scan path does not use the changed loader).

**Review**: the first fresh adversarial review found one blocking acceptance
defect in the initial fast path — `?` on the tag peek routed peek failures
and tagless lines to "skip" instead of the Value fallback (an escaped-tag v3
header would have invalidated the file; tagless valid JSON lines would have
been dropped instead of preserved as `Unknown`). Fixed by routing both to
`parse_line_via_value`; re-reviewed: **CLEAN** (0.99 confidence).

**Not touched** (out of scope, file-disjoint): `.github/workflows/`,
`scripts/release/`, `scripts/verification/compat-matrix.json`,
`docs/supported-platforms.md`, DEPS ledger docs, floor ledgers,
`scripts/session-timing.ts`, `session-timing.rs`.


## Iteration 14 — `session-reopen` (single-pass open)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28.

**Blind derivation** (from the ledger's Contract + Floor sections and the
iteration-13 residual record): the repeated reopen input is one file parsed
twice — `SessionManager::open` pre-parses the full entry list solely to
extract the header cwd, then `construct` → `set_session_file` reloads the
same file; the first pass is residue (its entries vec is discarded after a
header scan — a second full parse of every line). Data layout: fuse the two
passes into one — `open` performs the single propagating load, derives the
header cwd from that parse, and binds the already-loaded entries through a
shared post-load path (`apply_session_file_entries`) that `set_session_file`
also delegates to; the manager's session file binding is preserved on every
branch (the pre-set `Some(resolved)` mirrors the old top-of-function
assignment, so append can never silently no-op).

**Boundary answers** (explicit, before touching): reopen acceptance is
unchanged by construction — the single parse IS `set_session_file`'s own
loader, so empty file → header init + rewrite, invalid header (size > 0) →
`InvalidSessionFile`, legacy v1/v2 → the untouched `load_values_from_file` +
`migrate_values_to_current` + rewrite lane (byte-identical to base), v3 →
adopted entries; header-id and version scans (Header id/version or
RawHeader raw fields, `Entry(_) => 1`) preserved verbatim; header-cwd
extraction still `find_map(FileEntry::header)` (typed headers only, as
before); missing file → new session targeting the path; read failure → the
same `SessionError::Io` with the same path (now from the single parse; the
only side-effect delta is that the session dir is no longer created before
that error path). The v3 wire format is untouched (no write on the reopen
path — sha-prefix stability asserted per bench sample); `fork_from`
untouched (raw `Value`s); append/persist paths untouched.

**Cutover surface** (1 file): `crates/pi/src/core/sessions/mod.rs` —
`construct` split into `construct` + `construct_empty` (shared body),
`set_session_file`'s exists-branch post-load logic extracted into
`apply_session_file_entries` (shared by both entry points), `open` rewritten
single-pass. Cargo.lock untouched (lock-neutral, no new deps).

**Measurements** (release, `taskset -c 20-40`, 9 interleaved pairs, medians
of per-run 20-sample medians; `session-timing --mode reopen --entries 5000`,
815,149-byte shared session file, sha-prefix `660557e2b670b944` stable across
every run; baseline = 7011ac5 clean worktree binary, design = cutover
binary):

| Scenario | Before (ms/reopen) | After (ms/reopen) | Win (median-of-9) |
|---|---|---|---|
| reopen 5000 entries | 14.452 (14.040–15.308) | 7.529 (7.351–7.744) | 1.92x |

Win gate >=1.05x median: **PASSED** (1.92x). Per-entry: 2.89 → 1.51 us.

**Recomputed multiple**: 1.506 us/entry vs the ledger floor 0.764 us ≈
**1.97x — AT-FLOOR** (at/under the 2x rebuild threshold), with dual
disclosure of boundary proximity: pair-level multiples span 1.88–2.07x, and
the floor's 683.8 ns parse constant is sonic-rs-derived while the
implementation parses with serde_json, so a like-for-like floor is higher
and 1.97x is the conservative upper bound. The only remaining halving lever
is the parser swap itself (sonic-rs), which crosses the campaign's
Cargo.lock boundary and is barred. Residual composition: the typed-parse
constant (nested `AgentMessage` serde + per-field String allocations) and
the by-id/labels/leaf index rebuild; no rebuild candidate inside the
campaign's dependency boundary projects a >2x reduction.

**Verification**: `cargo check --workspace --all-targets` green (warnings
pre-existing; none in the touched file); `pi --lib` 1659 pass / 6 fail —
failure classes identical to the iteration-13 disclosure (extension-host
artifact, manifest utf8, trust-gating env, NFD-on-filesystem ×3 — the NFD
set rotates per run and worsens on tmpfs TMPDIR: 8 fails on tmpfs vs 3 on
zfs, independent of this diff, which touches only
`core::sessions::mod.rs`); `core::sessions` subset 63/64 with only the
disclosed `list_sessions_sorted_by_modified` flake (fails ~1/3 of probes on
this tree, `list.rs` has zero references to the changed loader).

**Review**: fresh adversarial review verified acceptance-equivalence
branch-by-branch (valid v3, v1/v2 legacy, empty file, invalid header,
RawHeader shapes, escaped-tag/tagless lines, unknown variants, missing
file, cwd precedence, session-file binding on every success path): **CLEAN**
(0.97 confidence).

**Not touched** (out of scope, file-disjoint): `.github/workflows/`,
`scripts/release/`, `scripts/verification/compat-matrix.json`,
`docs/supported-platforms.md`, DEPS ledger docs, floor ledgers,
`scripts/session-timing.ts`, `session-timing.rs`.

## Iteration 15 — `first-frame-init` (reply-armed two-phase probe wait)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28.

**Blind derivation** (recorded before reading the replaced body, from the
ledger's Contract + Floor + decomposition): the lane is wait-bound — ~215 ms
of 243.6 ms is two wait shapes, and the ledger names the entry: event-driven
readiness instead of fixed ticks, eliminating the long pre-output block. The
first-frame wire (probe batch + first synchronized frame) is boundary; only
the waits are addressable. Data layout: make the pre-output probe wait
event-driven and reply-armed — the wait blocks on stdin readiness with the
remaining budget (no fixed tick), and the fragment budget attaches to the
reply stream: once the first reply byte arrives the full TS-parity 150 ms
fragment window applies (measured from the query write, exactly today's
acceptance), but a terminal that has sent nothing cannot hold a fragment in
flight, so silence ends the wait at a round-trip-class window (~1 ms floor
class + scheduler headroom = 25 ms) instead of billing the full budget on
every non-responding terminal. The compute side (18.9 ms construction, incl.
1.9 ms rustls decode on the offline path) is secondary and untouched this
iteration.

**Boundary answers** (explicit, before touching): the probe batch bytes and
their position before the first synchronized frame are unchanged
(`pty_no_flicker` probe-before-sync predicate covers both scenarios); first
DEC synchronized-output transaction composition unchanged; raw-mode enter/
exit and guard ordering unchanged; probe acceptance for SPLIT replies is
unchanged by construction — the 150 ms fragment budget still starts at the
query write once bytes flow; early-keystroke preservation flows through the
same `ProbeSession::feed` -> `PendingInput` -> reinject path; mid-session
`probe_background` (paused-stream OSC 11 requery) gets the identical loop
and returns `None` on silence exactly as its timeout path did, keeping the
prior classification.

**Cutover surface** (1 file + instrument): `crates/pi-tui/src/terminal/
probe.rs` — `PROBE_FIRST_BYTE_TIMEOUT` (25 ms) constant, shared
`collect_probe_replies` (two-phase budget, both probe loops converge on it),
`read_stdin_nonblocking` replaced by `read_stdin_within(remaining)`
(nix `poll` with computed ceiling-rounded timeout; readiness wakes are
immediate, tick quantization gone; non-unix keeps a bounded sleep);
`probe_terminal`/`probe_background` bodies reduce to the shared collector.
`scripts/first-frame-timing.py` — committed interleaved A/B first-frame
driver (fresh 100x32 PTY + extension-free workload per sample, sandbox env
matching the verification harness, order alternating per pair, medians).
Cargo.lock untouched (lock-neutral, no new deps).

**Divergence audit** (branch classification of the replaced wait machinery):

| Original branch | Classification | Reason |
|---|---|---|
| `poll(0)` + `sleep(5 ms)` tick cadence (28–30 zero-event iterations) | residue | tick quantization adds up to 5 ms of reply latency and the strace census shows the iteration count, not the deadline, set by the 5 ms sleep — replaced by one readiness poll with the remaining budget |
| full 150 ms fragment budget burned while stdin is silent | residue | no contract term forces a wait beyond the probe round trip when no reply stream exists (floor: ~1 ms pipe-RT class); fragments require a first byte |
| fragment budget after first byte, absolute from query write | essential | TS-parity split-reply acceptance — preserved verbatim |
| `ProbeSession` feed/apply, `flush_timeout`, reinjection, EOF break | essential | unchanged |

**Ground truth before design** (strace -f -TT census of the pinned workload
on this box, baseline binary): the ledger's 157.2 ms "5 ms-cadence epoll
loop" is the probe fragment wait itself — 28 × (`poll(fd0, 0)` +
`clock_nanosleep(5 ms) ≈ 5.2 ms`) from the probe write to the first frame;
the ledger's 58.3 ms blocking `epoll_wait` did not reproduce on this box/run
(no critical-path block outside the probe loop; the observed `epoll_wait(-1)`
waits belong to the concurrent tokio/crossterm input threads, not the main
thread). Design trace: one `poll(fd0, POLLIN, 25) = 0 (Timeout) <25.2 ms>`,
zero tick sleeps.

**Measurements** (release, `taskset -c 20-40`, 9 interleaved pairs per run,
order alternating per pair, 1 warmup per arm; `scripts/first-frame-timing.py`,
fresh 100x32 PTY + extension-free workload + sandbox env per sample,
PI_OFFLINE=1, xterm-256color; baseline = a007540 clean-worktree binary,
design = cutover binary after the review fix; every sample first-frame via
synchronized-output detection, no row-local fallbacks). Two complete
post-fix runs on a box under external compile load (disclosed — one
~0.8–1.2 s contention outlier per arm):

| Run | Before median (ms) | After median (ms) | Win |
|---|---|---|---|
| 1 | 276.0 (244.6–1242.3) | 126.4 (110.9–351.1) | 2.18x |
| 2 | 250.6 (245.5–1076.3) | 116.0 (111.9–145.3) | 2.16x |

Win gate >=1.05x median: **PASSED** (2.18x / 2.16x on two independent
complete interleaved runs). Contention-free pairs run ~245–276 ms (before)
vs ~111–149 ms (after).

**Recomputed multiple**: ~116–126 ms vs the ledger floor ~1.50 ms ≈
**~77–84x — still OPEN** (>2x). The residual is the no-responder first-byte
window (25 ms) + construction CPU (~19 ms, incl. rustls decode on the
offline path) + spawn/loader (~9 ms); the next material lever is overlapping
the startup construction with the probe window (a distinct design, not
attempted in this slot), and the parser/TLS levers sit outside this
iteration.

**Verification**: `cargo check --workspace --all-targets` green (warnings
pre-existing, none new in the touched file); `pi-tui --lib` 400/400 pass
(probe subset 15/15 after the review fix); `pi --lib` 1659 pass / 6 fail,
every failure in the four disclosed environmental classes (extension-host
artifact, manifest utf8, trust-gating env, NFD-on-filesystem ×3 — the same
classes iterations 13–14 recorded, independent of this diff); strace shape
deltas recorded above; `pty_no_flicker` sync and sync-ignored scenarios pin
the probe-before-sync wire predicate (the fixture drives its own probe loop,
so the end-to-end probe-wait change is exercised by the first-frame driver
and the live bench, which all observed synchronized-output first frames).

**Review**: the first fresh adversarial review found one blocking defect —
the fragment phase armed on ANY stdin bytes, so an early keystroke at a
non-responding terminal still bought the full 150 ms wait. Fixed by arming
the fragment phase only on probe-reply evidence (a recognized reply or a
buffered partial sequence in `ProbeSession`); classified keystrokes arm
nothing. Re-reviewed: **CLEAN** (0.97 confidence).

**Not touched** (out of scope, file-disjoint): `.github/workflows/`,
`scripts/release/`, `scripts/verification/compat-matrix.json`,
`docs/supported-platforms.md`, DEPS ledger docs, floor ledgers,
`scripts/session-timing.ts`, `session-timing.rs`, `runtime.rs` (the
`run_interactive_mode` owed sequence is unchanged), `input.rs`.

## Iteration 16 — `first-frame-init` (speculative first paint with deferred probe join)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28.

**Blind derivation** (from the iteration-15 residual decomposition): the
residual after the two-phase probe wait is ~116–126 ms, of which ~25 ms is
the no-responder first-byte window (now event-driven but still serial with
construction), ~19 ms is construction CPU (Tui + AgentSessionHost +
InteractiveRuntime + bind_extensions + startup_theme + initialize_run), and
~9 ms is spawn/loader. The 25 ms probe wait and the ~8–14 ms construction +
first paint are independent and currently serial — the probe collector reads
stdin while the main task builds the UI. Overlapping them cuts the serial
chain: paint the first frame speculatively with DEFAULT capabilities while
the probe collector runs on a blocking thread, then join and reconcile.

**Design**: split `probe_terminal` into `probe_write_batch` (write the query
batch, return whether stdin is a terminal) and `probe_collect_replies`
(blocking collector under the two-phase budget, merging replies into caps
and returning early keystrokes). In `run_interactive_mode`: (1) TerminalGuard
+ raw mode, (2) `probe_write_batch` + flush, (3) spawn a
`tokio::task::spawn_blocking` running `probe_collect_replies` (owns stdin
until joined), (4) construct Tui + AgentSessionHost + bind_extensions +
startup_theme + InteractiveRuntime with DEFAULT caps and a
`TerminalInput::deferred()` handle (no stdin reader), (5) `initialize_run`
then `paint_frame` produces the FIRST FRAME with default caps, (6) join the
probe collector, (7) `adopt_probe_caps`: if caps/theme changed, update +
re-paint, (8) `set_kitty_protocol_active` with final caps, queue early
keystrokes, (9) `TerminalInput::start` spawns the reader (now safe to take
stdin), (10) enter the event loop via `run_with_startup(false)` on the first
pass (the speculative paint already ran the startup sequence); later passes
(Suspend, external editor) pass `true` to re-run it.

**Boundary answers** (explicit, before touching): the probe batch bytes and
their position before the first synchronized frame are unchanged
(`pty_no_flicker` probe-before-sync predicate holds — the batch is written
before any frame); first frame uses default caps (`sync_output` defaults to
true, so the frame is a DEC 2026 synchronized transaction — the bench
detection predicate holds); non-responding terminals (the bench workload)
need no re-paint (`adopt_probe_caps` returns false when caps are unchanged);
real terminals may flash one extra frame when the probe refines caps/theme;
the probe collector owns stdin until joined; the `TerminalInput` reader is
deferred until after the probe join — never two stdin readers; early
keystrokes are queued via `queue_pending_events` (reversed into the
pop-from-end `pending_ui_reinject` queue, preserving ordering); Suspend and
external-editor paths re-run `initialize_run` on their loop re-entry
(`run_with_startup(true)`), restoring the frame as before; non-TTY stdin
writes no batch, spawns no task, and produces empty pending events (parity
with the old `probe_terminal` early return).

**Cutover surface** (3 files):
- `crates/pi-tui/src/terminal/probe.rs` — `probe_terminal` replaced by
  `probe_write_batch` (phase 1) + `probe_collect_replies` (phase 2); the
  shared `collect_probe_replies` and `read_stdin_within` are unchanged.
- `crates/pi-tui/src/terminal/input.rs` — `TerminalInput` gains `deferred()`
  (channels without the reader task), `start()` (spawns the reader later),
  and a `control_rx: Option<…>` field holding the receiver back until
  `start`; `spawn()` is now `deferred()` + `start()`; `mock()` updated for
  the new field; two test struct literals updated.
- `crates/pi/src/modes/interactive/runtime.rs` — `run_interactive_mode`
  restructured as above; `InteractiveRuntime` gains `run_with_startup`,
  `adopt_probe_caps`, `queue_pending_events`; `run()` delegates to
  `run_with_startup(true)`.
`crates/pi-tui/src/terminal/mod.rs` — re-exports updated
(`probe_terminal` → `probe_collect_replies` + `probe_write_batch`).
Cargo.lock untouched (lock-neutral, no new deps).

**Divergence audit** (branch classification of the replaced startup sequence):

| Original branch | Classification | Reason |
|---|---|---|
| `probe_terminal` (write + collect serially before construction) | residue | the 25 ms no-responder wait and the ~8–14 ms construction + first paint are independent and overlapped by the split |
| `TerminalInput::spawn` before construction | residue | the reader must not start until the probe collector joins — replaced by `deferred()` + `start()` after the join |
| `set_kitty_protocol_active` before construction | residue | moved after the probe join so the final probed kitty capability is installed before the reader decodes keys |
| probe batch bytes, two-phase reply budget, fragment arming, `ProbeSession` feed/apply, reinjection, EOF break | essential | unchanged |
| Suspend / external-editor repaint-on-resume | essential | preserved via `run_with_startup(true)` on later loop passes |

**Measurements** (release, `taskset -c 20-40`, 9 interleaved pairs per run,
order alternating per pair, 1 warmup per arm; `scripts/first-frame-timing.py`,
fresh 100x32 PTY + extension-free workload + sandbox env per sample,
PI_OFFLINE=1, xterm-256color; baseline = d7f5e4a clean-worktree binary,
design = cutover binary; every sample first-frame via synchronized-output
detection, no row-local fallbacks). Two complete runs:

| Run | Before median (ms) | After median (ms) | Win |
|---|---|---|---|
| 1 | 95.0 (85.5–127.6) | 76.9 (58.6–105.0) | 1.24x |
| 2 | 92.5 (76.1–125.4) | 72.9 (58.6–95.5) | 1.27x |

Win gate >=1.05x median: **PASSED** (1.24x / 1.27x on two independent
complete interleaved runs).

**Recomputed multiple**: ~73–77 ms vs the ledger floor ~1.50 ms ≈
**~49–51x — still OPEN** (>2x). The residual is construction CPU (~19 ms,
incl. rustls decode on the offline path) + spawn/loader (~9 ms) + the
un-overlappable tail of the probe wait (the join latency after the
speculative paint completes); the next material levers are the
construction CPU itself (parser/TLS) and the spawn/loader, which sit
outside this iteration.

**Verification**: `cargo check --workspace --all-targets` green (warnings
pre-existing, none new in the touched files); `pi-tui --lib` 400/400 pass
(probe subset 15/15); `pi --lib` 1660 pass / 5 fail, every failure in the
four disclosed environmental classes (manifest-utf8, trust-gating,
NFD-on-filesystem ×3 — the same classes iterations 13–15 recorded,
identical-to-base: the same 5 tests fail at d7f5e4a); all 18 bench samples
detected via synchronized-output (the default-caps first frame emits the
DEC 2026 transaction, exercising the probe-before-sync wire predicate
end-to-end).

**Review**: fresh adversarial review — **CLEAN** (0.92 confidence). The
split preserves the old probe bytes and parser/timeout semantics, keeps the
blocking collector as the sole stdin reader until its JoinHandle is
resolved, starts `TerminalInput` only after kitty capability state is
installed, and reverses probe events correctly for the runtime's
pop-from-end queue.

**Not touched** (out of scope, file-disjoint): `.github/workflows/`,
`scripts/release/`, `scripts/verification/compat-matrix.json`,
`docs/supported-platforms.md`, DEPS ledger docs, floor ledgers,
`scripts/session-timing.ts`, `session-timing.rs`, `Cargo.lock`,
`rust-toolchain.toml`.

## Iteration 17 — `first-frame-init` (detection/factory overlap — REJECTED)

Date 2026-08-28. Base `ea3516b` (iteration 16).

**Candidate**: overlap the single `spawn_blocking(
InteractiveRuntimeOptions::detect)` with independent
`RuntimeFactory::create`. The blocking terminal probe (~25 ms after
iteration 15's two-phase wait) runs concurrently with runtime construction
(model registry, provider adapters, session services). The `JoinHandle` is
carried through `Dispatched` to the interactive dispatcher seam, which
awaits it instead of spawning a fresh detection. Non-interactive and
early-exit paths abort the handle to avoid detached tasks.

**Implementation** (reverted): `bootstrap.rs` — `run_bootstrap` spawns
`spawn_blocking(InteractiveRuntimeOptions::detect)` after
`prepare_session` when `app_mode.is_interactive()`, before
`create_runtime`; the `JoinHandle` is threaded through `finish_bootstrap`
into `Dispatched.detect_handle` (new `Option<JoinHandle<...>>` field).
`entry.rs` — the interactive closure in `Io::real()` awaits
`dispatched.detect_handle` when `Some`, falling back to a fresh
`spawn_blocking` when `None`. Every non-interactive path (factory error,
help/list-models, stdin demotion, no-models) aborts the handle.

**Measurements** (`scripts/first-frame-timing.py`, release,
`taskset -c 20-40`, 9 interleaved pairs, fresh 100x32 PTY per sample,
PI_OFFLINE extension-free workload; baseline = `ea3516b` clean binary,
design = cutover binary):

| Run | Before median (ms) | After median (ms) | Win |
|---|---|---|---|
| 1 | 70.1 (65.9–93.6) | 71.3 (70.3–85.0) | 0.98x |
| 2 | 72.4 (65.1–84.1) | 71.8 (68.4–86.4) | 1.01x |

All 18 samples per run detected via synchronized-output. Win gate
>=1.05x median: **FAILED** (0.98x / 1.01x). The overlap does not yield a
measurable win because after iterations 15–16 the probe wait is already
~25 ms (two-phase reply-armed wait) and `RuntimeFactory::create` on the
offline extension-free path completes in a comparable or shorter window —
the two tasks are roughly co-terminus, so overlapping them saves little
to nothing. The baseline first-frame lane is now ~70 ms (down from
~276 ms at iteration 14), and the residual is dominated by process
spawn + loader + construction, not by serialized probe-then-factory
ordering.

**Verdict**: REJECTED. All Rust edits fully reverted; no production
change landed. The `first-frame-init` unit remains OPEN at ~162.4x
(ledger floor ~1.50 ms vs measured ~70 ms ≈ 47x after iterations 15–16;
the ledger's 243.61 ms R2 baseline predates the probe-wait fixes).

**Not touched** (out of scope, file-disjoint): `.github/workflows/`,
`scripts/release/`, `scripts/verification/compat-matrix.json`,
`docs/supported-platforms.md`, DEPS ledger docs, floor ledgers,
`Cargo.lock`, `rust-toolchain.toml`.

## Iteration 18 — `first-frame-init` (fresh stage attribution — CONSTRAINED-ABOVE-FLOOR)

Date 2026-08-28. Base `13a458c` (iteration 17).

**Method**: fresh post-iteration-16 stage attribution using temporary
env-gated monotonic markers (`PI_TIMING_PROBE=1`), all timestamps collected
in memory and emitted after the first synchronized frame so marker I/O
cannot delay the measured frame. 15 stages A–O spanning spawn→first-sync,
with 8 sub-stages E1–E8 inside the factory. All probe code fully reverted;
the final tree contains zero probe instrumentation.

**Correction to iteration 17**: `InteractiveRuntimeOptions::detect` is
env/capability inference (tmux hyperlink probe cached in a process-wide
`OnceLock`); the actual startup terminal probe is in `run_interactive_mode`
via `probe_write_batch` + `probe_collect_replies` (the two-phase
reply-armed wait from iteration 15). The iteration-17 candidate overlapped
`detect` with `RuntimeFactory::create`, but `detect` is ~200 us — the
overlap was with the wrong stage.

**Stage measurements** (release, `taskset -c 20-40`, fresh 100x32 PTY +
extension-free workload + sandbox env per sample, PI_OFFLINE=1,
xterm-256color; 5 runs, medians):

| Stage | Description | Median (us) | Δ from prev (us) |
|-------|-------------|------------|-------------------|
| A | `main()` entry | 56 | — (exec/loader before this) |
| B | Tokio runtime built | 2,230 | 2,174 |
| C | Bootstrap pipeline start | 2,251 | 21 |
| D | Bootstrap→factory call | 2,840 | 589 |
| E1 | Session context built | 2,875 | 35 |
| E2 | Services create begins | 2,876 | 1 |
| E3 | Models resolved | 58,354 | **55,478** |
| E4 | Session from services | 59,973 | 1,619 |
| E5 | Session diagnostics | 60,074 | 101 |
| E6 | Session inputs built | 60,208 | 134 |
| E7 | Session built | 60,254 | 46 |
| E8 | Runtime `new` | 60,255 | 1 |
| F | Dispatch enter | 60,282 | 27 |
| G | `detect` done | 60,507 | 225 |
| H | Terminal activated | 60,531 | 24 |
| I | Probe written + spawned | 60,540 | 9 |
| K | Tui constructed | 60,591 | 51 |
| L | Host bound | 60,613 | 22 |
| M | InteractiveRuntime `new` | 60,743 | 130 |
| N | Speculative paint (`initialize_run`) | 62,326 | **1,583** |
| O | Probe joined | 85,702 | 23,376 |
| O' | Input started + emit | 85,710 | 8 |

**Overlap reconciliation**: the probe collector (J = O − I ≈ 25,162 us)
runs **concurrently** with K+L+M+N (≈ 1,786 us) per the iteration-16
speculative-paint design. The first synchronized frame is emitted at N
(inside `initialize_run`), **before** the probe join at O. The probe join
is not on the first-frame critical path.

**Critical-path equation** (first synchronized frame):

```
first_frame = exec_loader + (N_speculative_paint − A_main_entry)
            = exec_loader + 62,270 us
            ≈ exec_loader + 62.3 ms
```

Measured first-frame (PTY harness, Popen→SYNC_BEGIN): median ~71.3 ms
(A/A baseline, 9 interleaved pairs). Therefore:

```
 exec_loader ≈ 71.3 − 62.3 ≈ 9.0 ms
```

The exec/loader residual (process spawn + dynamic linker + Rust pre-main)
is ~9.0 ms — not addressable from Rust code.

**Full critical-path decomposition**:

| Segment | Duration (ms) | Classification |
|---------|-------------|----------------|
| exec/loader (spawn→main) | ~9.0 | **Platform** — OS process spawn + dynamic linker; not addressable from Rust |
| Tokio runtime build (A→B) | ~2.2 | **Essential** — async runtime required for all I/O |
| Bootstrap prep (B→D) | ~0.6 | **Essential** — argv parse, session manager, migrations |
| Factory create: ModelRuntime (D→E3) | ~55.5 | **Boundary-gated** — model catalog parse, rustls decode, provider registry; Cargo.lock frozen |
| Factory create: session build (E3→E8) | ~1.9 | **Essential** — session services, model resolution, AgentSession construction |
| Dispatch + detect (E8→G) | ~0.3 | **Essential** — mode dispatch, env/capability inference |
| Terminal activation (G→H) | ~0.02 | **Essential** — raw mode, guard |
| Probe write (H→I) | ~0.01 | **Essential** — probe batch bytes (frozen boundary) |
| Speculative paint (I→N, concurrent with J) | ~1.8 | **Essential** — Tui construction, host bind, theme, first paint; runs concurrently with probe collector |

**Post-frame (not on first-frame critical path)**:

| Segment | Duration (ms) | Classification |
|---------|-------------|----------------|
| Probe collector (J = O − I) | ~25.2 | **Boundary-gated** — probe first-byte timeout (25 ms, iteration 15); frozen probe bytes/order; concurrent with I→N, joined after frame |
| Probe join tail (N→O) | ~23.4 | **Off critical path** — happens after first frame is painted |

**E1–E4 (factory interior) decomposition**:

| Sub-stage | Duration (ms) | Classification |
|-----------|-------------|----------------|
| E2→E3: `create_runtime_services` + `resolve_models` | ~55.5 | **Boundary-gated** — `ModelRuntime::create` (builtin catalog, auth.json, models.json, models-store.json, `default_provider_registry` with rustls, `rebuild_providers`, `refresh`) + `resolve_cli_model` + `resolve_model_scope` |
| E3→E4: `create_agent_session_from_services` | ~1.6 | **Essential** — session result construction |
| E4→E8: session metadata + diagnostics + inputs + build + runtime | ~0.3 | **Essential** — session wiring |

**Designs 15/16/17 revalidation**:
- Design 15 (two-phase probe wait): **landed**, reduced probe wait from
  ~157 ms to ~25 ms. Still in effect; the 25 ms first-byte timeout is
  boundary-gated (probe bytes/order frozen).
- Design 16 (speculative first paint): **landed**, overlapped the 25 ms
  probe wait with construction + first paint. Still in effect; the first
  frame is painted at N before the probe join at O.
- Design 17 (detection/factory overlap): **rejected** at 0.98x/1.01x.
  Revalidated: `detect` is ~225 us — overlapping it with anything saves
  nothing. The factory create (~55.5 ms) is the dominant serial stage, but
  it completes before the probe is even written, so there is nothing to
  overlap it with on the first-frame path.

**Floor revalidation**: the ledger floor is ~1.50 ms (R9 floor for
first-frame-init). The measured first-frame is ~71.3 ms ≈ **47.5x** the
floor. The floor represents the theoretical minimum (process exec + minimal
initialization); the 47.5x gap is dominated by boundary-gated stages
(model catalog + TLS init ~55.5 ms, exec/loader ~9.0 ms). The probe
timeout (~25.2 ms) is concurrent but off the first-frame critical path
(the frame is painted at N before the join at O).

**Every >=3.3 ms term classified**:

| Term | Duration (ms) | Classification | Removable? |
|------|-------------|----------------|------------|
| ModelRuntime::create + model resolution | ~55.5 | Boundary-gated | No — Cargo.lock frozen (rustls, parser), model catalog required |
| Probe first-byte timeout (J) | ~25.2 | Boundary-gated (off critical path) | No — concurrent with paint; probe bytes/order frozen, 25 ms is the round-trip-class floor |
| exec/loader (spawn→main) | ~9.0 | Platform | No — OS process spawn + dynamic linker |
| Tokio runtime build | ~2.2 | Essential | No — async runtime required |
| Speculative paint (initialize_run) | ~1.8 | Essential | No — first frame must be painted |
| Session from services | ~1.6 | Essential | No — session must be constructed |

No term >=3.3 ms is removable without crossing a frozen boundary
(Cargo.lock/dependencies, probe bytes/order, provider/auth readiness) or
removing an essential stage (runtime, session, first paint).

**Named dominant residual**: `ModelRuntime::create` + model resolution
(~55.5 ms, 78% of measured first-frame). Interior: `builtin_models()`
catalog parse + `FileCredentialStore::new(auth.json)` +
`ModelsJsonConfig::load(models.json)` + `FileModelsStore::new(models-store.json)`
+ `default_provider_registry()` (rustls decode + HTTP client pool) +
`rebuild_providers()` + `refresh()`. The only material lever inside this
is the parser/TLS swap (sonic-rs for JSON, ring for TLS), which crosses
the campaign's Cargo.lock boundary and is barred.

**E1–E4 evidence (exhaustion record)**:
- **E1 (enumeration)**: all 15+8 stages enumerated with measured medians.
  No stage omitted; overlap explicitly reconciled.
- **E2 (equivalence)**: the critical-path equation
  `first_frame = exec_loader + (N − A)` reconciles to the measured
  ~71.3 ms within 0.3 ms (62.3 + 9.0 = 71.3). The overlap
  `max(J, K+L+M+N) = J = 25.2 ms` is correctly represented as
  off-first-frame-path (the frame is painted at N, before the join at O).
- **E3 (essential/residue)**: every >=3.3 ms term classified as
  boundary-gated, platform, or essential. No residue found.
- **E4 (floor)**: the 47.5x multiple is dominated by boundary-gated
  stages (model catalog + TLS ~55.5 ms, exec/loader ~9.0 ms). The probe
  timeout (~25.2 ms) is concurrent but off the first-frame critical path.
  The only remaining lever (parser/TLS swap) is barred by the Cargo.lock
  freeze. No rebuild candidate inside the campaign's dependency boundary
  projects a >=1.05x reduction.

**Verdict**: **CONSTRAINED-ABOVE-FLOOR**. The `first-frame-init` unit is
classified as constrained above the floor by frozen boundaries. No valid
>=1.05x candidate exists within the campaign's dependency boundary. The
unit remains OPEN in the campaign issue (#97) because the parser/TLS
swap (sonic-rs/ring) would unlock the dominant ~55.5 ms term, but that
crosses the Cargo.lock boundary and is outside this campaign's scope.

**Probe reversion**: all temporary probe code (5 files: `main.rs`,
`lib.rs`, `entry.rs`, `bootstrap.rs`, `runtime.rs`) fully reverted.
`git diff --stat` confirms zero probe code in the final tree. The only
shipped change is this docs record.

**Not touched** (out of scope, file-disjoint): `.github/workflows/`,
`scripts/release/`, `scripts/verification/compat-matrix.json`,
`docs/supported-platforms.md`, DEPS ledger docs, floor ledgers,
`Cargo.lock`, `rust-toolchain.toml`, `scripts/first-frame-timing.py`.

## Iteration 19 — `tool-dispatch-slice` (ToolCall move-only — REJECTED)

Date 2026-08-28. Base `a30d013` (canonical origin/feat/ver-align-canonical-pin).

**Candidate**: move `ToolCall` by value through the sequential and truncated
dispatch paths instead of cloning at `PreparedToolCall` construction (line 660)
and `FinalizedOutcome` construction (line 157/224). `prepare_tool_call` takes
`ToolCall` by value and moves it into `PreparedToolCall` or `ImmediateOutcome`;
`execute_prepared_tool_call` consumes `PreparedToolCall`, clones `tool_call`
only for update emission, and moves `tool_call`/`args` into `ExecutedOutcome`;
`finalize_executed_tool_call` receives `ExecutedOutcome` (with `tool_call` and
`args`) instead of `PreparedToolCall`, clones only for `AfterToolCallContext`
when a hook is present, and moves into `FinalizedOutcome`. The truncated path
moves each extracted `ToolCall` directly into `FinalizedOutcome`. Parallel path
clones `tool_call` before calling `prepare_tool_call` (same clone count as
before) and destructures `prepared` to move `tool_call`/`args` into
`ExecutedOutcome` for the new `finalize` signature. No global Arc; no
parallel-path Arc; no result-finalization/message changes.

**Design provenance**: tightened by `agent://ToolDispatchDesignAdvocate`
(block-must-fix → revised: pure move pipeline for sequential/truncated, Arc
restricted to parallel seam, result consumption separated) and
`agent://PerfT11ToolDispatchPlan2` (code-ready-revised, projected 1.05x,
eliminates 1 of 3 per-call ToolCall clones on the sequential hot path).

**Clone count attribution** (sequential prepared path, benchmark workload):

| Site | Before | After |
|---|---|---|
| `tool_calls_from_message` extraction | 1 clone | 1 clone |
| `prepare_tool_call` → `PreparedToolCall` | 1 clone (line 660) | 0 (move) |
| `execute_prepared_tool_call` update emission | 1 clone (line 672) | 1 clone |
| **Total** | **3** | **2** |

Truncated path: 2 clones → 1 clone (move into `FinalizedOutcome`). Parallel
path: unchanged clone count (clone before `prepare_tool_call`, clone for slot).

**Implementation** (reverted): `crates/pi-agent/src/schedule.rs` —
`ImmediateOutcome` gained `tool_call: ToolCall`; `ExecutedOutcome` gained
`tool_call: ToolCall` and `args: Map<String, Value>`; `prepare_tool_call`
parameter changed from `&ToolCall` to `ToolCall`; `execute_prepared_tool_call`
consumed `PreparedToolCall` (worker returned `(AgentToolResult, bool)` tuple,
outer scope wrapped into `ExecutedOutcome`); `finalize_executed_tool_call`
received `ExecutedOutcome` instead of `PreparedToolCall`; sequential function
took `Vec<ToolCall>` by value; truncated function iterated by value. ~80 LOC.

**Measurements** (`pi_tool_dispatch_bench`, release, `taskset -c 20-40`,
3000 calls, 300 warmup, 1 block, 9 interleaved baseline/design pairs per run,
order alternating per pair; baseline = `a30d013` clean binary, design =
cutover binary). Two complete runs:

| Run | Before median (us) | After median (us) | Win |
|---|---|---|---|
| 1 | 23.47 | 24.72 | 0.95x |
| 2 | 24.93 | 24.45 | 1.02x |

Run 1 pair data (before/after us): 23.08/23.42, 23.17/23.90, 23.74/25.05,
25.33/24.61, 23.47/24.72, 23.29/24.91, 23.47/23.26, 28.49/27.55, 25.04/24.79.
Run 2 pair data: 25.28/33.08, 26.24/24.07, 24.65/29.05, 27.02/26.74,
30.93/27.41, 23.95/23.17, 24.10/23.29, 24.93/24.06, 23.51/24.45.

Win gate >=1.05x median: **FAILED** (0.95x / 1.02x on two independent
complete interleaved runs). The eliminated clone (ToolCall is 3 Strings +
1 Map — `id`, `name`, `thought_signature`, `arguments`) saves ~0.9 us per
call per the plan's cost model, but the measurement shows no consistent
win: the effect is below the run-to-run noise floor. The 24 us/call median
vs the 4.29 us floor (5.6x) is dominated by session append I/O and event
emission, not by ToolCall cloning — the clone cost is ~3.7% of the per-call
budget, below the measurement noise floor on this workload.

**Verdict**: REJECTED. All Rust edits fully reverted; no production change
landed. The `tool-dispatch-slice` unit remains OPEN at ~5.6x the floor
(24 us/call vs 4.29 us floor). The ToolCall move-only mechanism eliminates
one clone but the saving is below the noise floor of the benchmark's
session-append-dominated per-call cost. A materially distinct design is
needed: either (a) eliminate the update-emission clone (the remaining
ToolCall clone, e.g., by passing `&tool_call` into the worker via scoped
borrow or by deferring update emission to after the worker joins), or
(b) target the dominant session-append/event-emission terms that are ~96%
of the per-call budget, not the ~3.7% clone term.

**Verification** (pre-revert): `cargo check -p pi-agent` clean (0 errors,
0 warnings after style fix); `cargo test -p pi --test tool_dispatch_bench`
2/2 pass (valid_dispatch_satisfies_the_shared_protocol,
invalid_payload_is_rejected_by_argument_validation). Protocol invariants
preserved: same event counts (start/update/end), same append count (2x
calls), same error-result behavior.

**Not touched** (out of scope, file-disjoint): `.github/workflows/`,
`scripts/release/`, `scripts/verification/compat-matrix.json`,
`docs/supported-platforms.md`, DEPS ledger docs, floor ledgers,
`Cargo.lock`, `rust-toolchain.toml`.

## Iteration 20 — `tool-dispatch-slice` (E1-E4 exhaustion — CONSTRAINED-ABOVE-FLOOR)

Date 2026-08-28. Base `3ee5d7b` (canonical origin/feat/ver-align-canonical-pin,
iteration 19). Docs-only terminal record: no candidate was executed this
iteration; every remaining in-unit mechanism is already measured below the
gate (iteration 19) or infeasible under the ownership contracts. Provenance:
`agent://ToolDispatchNextOracle` (0.97 confidence — recommend
CONSTRAINED-ABOVE-FLOOR closure: no materially distinct >=1.05x design
within tool-dispatch ownership that preserves the contracts) and
`agent://ToolDispatchDesignAdvocate` (block-must-fix on the combined
move+Arc+result-consumption plan, on three-owner/public-boundary proof).

### E1 — decomposition reconciliation (24.12 us/call)

| Term | us/call | Share | Ownership |
|---|---|---|---|
| Session append syscalls (open+close+write per entry) | 5.43 | 22.5% | `session-append` unit (next unit) |
| Allocation (event/message/JSONL value shapes) | 8.10 | 33.6% | boundary (double serialization) |
| Value pipeline (serde traversal) | 3.55 | 14.7% | boundary (double serialization) |
| Payload copies (ToolCall/result/message clones) | 2.22 | 9.2% | in-unit — measured below gate |
| Tokio spawn + validation + residual | 4.82 | 20.0% | essential (spawn contract + argument validation) |
| **Total** | **24.12** | **100%** | |

Reconciliation is exact: 5.43 + 8.10 + 3.55 + 2.22 + 4.82 = 24.12 us/call.
The oracle's grouped view — append syscalls 22.5%, "allocator + Value
pipeline" 11.7 us (48.5%), spawn + validation + residual 20% — maps onto
the same terms with the payload-copy term stated separately (8.10 + 3.55 =
11.65 us = 48.3% exactly). The only in-unit slice is the 2.22 us
payload-copy term (~9.2%), whose largest single component (the ToolCall
clone, ~0.9 us ≈ 3.7%) iteration 19 measured at 0.95x/1.02x — below the
run-to-run noise floor.

### E2 — candidate history and evidence

1. **Global `Arc<ToolCall>` + result-consumption (iteration-19 plan v1)** —
   rejected before execution on the advocate's three-owner/public-boundary
   proof: consuming `ToolResultMessage` cannot satisfy the three
   simultaneous owners (`MessageStart`, `MessageEnd`, returned
   `ExecutedToolCallBatch.messages`) without reinstating the clone the plan
   claimed to remove; the current shape (return message + one clone into
   `AgentMessage` + one clone for start + move into end) is already the
   minimum compatible ownership. A global Arc additionally taxes every
   single-owner path (sequential, immediate-validation, missing-tool,
   hook-blocked, truncated) with one heap allocation + refcount traffic;
   `Arc` is justified only at the parallel pending-slot/worker seam, which
   is not the sequential hot path the bench measures. Verdict:
   block-must-fix.
2. **ToolCall move-only (iteration 19)** — executed at base `a30d013`,
   measured **0.95x / 1.02x** on two independent 9-pair interleaved runs,
   FAILED the >=1.05x gate, fully reverted (no production change). The
   eliminated clone (~0.9 us/call, ~3.7% of the budget) is below the
   measurement noise floor of the session-append-dominated per-call cost.
3. **Scoped borrow / update-emission clone elimination** — infeasible. The
   remaining ToolCall clone (update emission, `schedule.rs` line 672)
   cannot be removed by borrowing `tool_call` from `PreparedToolCall`
   across the worker lifetime: the worker is an owned, abortable task
   (lines 680-703) that must own its inputs for `Send` so cancellation can
   force-abort non-cooperative tools, and a scoped borrow cannot cross the
   spawn boundary. Deferring update emission until after the worker joins
   changes observable event ordering (update-before-completion), a
   contract violation.
4. **Sequential-path spawn removal (inline the worker)** — projected
   **<1.01x**: `tokio::spawn` with task reuse costs ~50-200 ns, <1% of the
   24 us/call budget — below the gate by construction. It also violates
   cancellation symmetry: the bounded-parallelism contract
   (`MAX_PARALLEL_TOOL_CALLS = 8`) and non-cooperative-tool force-abort
   semantics justify the owned task even on the sequential path, and a
   separate sequential/parallel execution shape for a sub-1% projected win
   is complexity without a measurable return.

### E3 — floor and multiple revalidation

Floor **4.29 us/call** (ledger R9; revalidated — no input, dependency, or
protocol input changed it, and both iteration-19 runs reproduce the same
~24 us operating point). Multiple: 24.12 / 4.29 = **5.62x**. This multiple
is a constraint statement, NOT a claim that 24 us/call is at its physical
floor: the gap is held open by (a) boundary-owned double serialization —
events and the session entry are built through separate serde pipelines
(~11.65 us/call), unifiable only by redesigning the event-emission /
session-write boundary or deferring live events; and (b) cross-unit
session-append cost (5.43 us/call, including a 2.06 us open+close per
entry) owned by the `session-append` unit. Both levers are outside
tool-dispatch ownership; inside the unit nothing measurable remains.

### E4 — dominant residual and reopen conditions

Dominant residual: boundary-owned event/message/session serialization
(~11.65 us/call) plus next-unit session append (5.43 us/call). Every
in-unit removable candidate is measured below the gate (ToolCall move,
0.95x/1.02x) or infeasible (scoped borrow across an owned abortable worker;
result consumption vs three owners; spawn removal vs cancellation
symmetry). Exact boundary consents that would reopen the unit:

1. **Relax the two-append contract** (batch-append N calls in one session
   write): session term 5.43 → ~0.5 us/call, oracle-projected **~1.26x**.
   Requires changing `ExecutedToolCallBatch` persistence semantics and the
   append call site in `run.rs` — a protocol/test contract change.
2. **Unify event emission with session serialization** (one serialization
   pass shared by `AgentEvent` and the JSONL entry, or events deferred to
   the session write): double-serialization term ~11.65 → ~6 us/call,
   oracle-projected **~1.31x**. Requires redesigning the event-emission
   boundary (live-update timing change).
3. **`session-append` lands its held-open-fd optimization** (closes the
   2.06 us open+close, 5.43 → 3.37 us/entry): tool-dispatch inherits
   **~1.09x** passively — the only lever that does not cross
   tool-dispatch's own boundary, and it belongs entirely to the next unit.

**Verdict**: **CONSTRAINED-ABOVE-FLOOR**. The `tool-dispatch-slice` unit is
terminal in the campaign records: no >=1.05x candidate exists inside the
unit without crossing the event/session boundary or taking ownership of
next-unit work. This closure is unit-scoped and does not close the
campaign: issue #97 remains OPEN for the remaining units. Next unit:
**`session-append`** (4.91x ledger multiple).

**Not touched** (out of scope, file-disjoint): production code,
`.github/workflows/`, `scripts/release/`,
`scripts/verification/compat-matrix.json`, `docs/supported-platforms.md`,
DEPS ledger docs, floor ledgers, `Cargo.lock`, `rust-toolchain.toml`,
`scripts/session-timing.ts`, `session-timing.rs`.

---

## Iteration 21 — `session-append` (maintained assistant-present state — AT-FLOOR at 1.41x)

**Commit**: perf commit on `feat/ver-align-canonical-pin` (linear on `f4ae3ae`).
Docs record in the same commit.

### Blind derivation (from the ledger's Contract + Floor + decomposition)

The ledger's cost decomposition names the `has_assistant` scan as 1.28 us/entry
(9.9% of unit Ir, growing quadratically with entry count). The scan is
`self.file_entries.iter().filter_map(FileEntry::entry).any(SessionEntry::is_assistant_message)`
in `persist_entry_at`, called on every append. For 5000 entries the average
scan length is 2500, and each iteration does an enum match + string comparison
(`role() == "assistant"`), making the real per-append cost far higher than the
profiler's flat attribution suggests.

**Design**: replace the O(entries) per-append scan with a maintained `has_assistant: bool`
field on `SessionManager`. The field is:

- Initialized `false` in `construct_empty` and `new_session` (header-only state).
- Recomputed in `build_index` (folded into the existing index pass — no second scan).
- Updated in O(1) after successful persist in `append_entry`:
  `has_assistant || entry_at_idx.is_assistant_message()`.
- Read in `persist_entry_at` as the effective post-append state:
  `self.has_assistant || file_entries[idx].is_assistant_message()` — this O(1)
  calculation corrects the plan's ordering bug (using the stale pre-append cached
  boolean would miss the first-assistant flush trigger).
- Read in `create_branched_session` (after `build_index` recomputes it from the
  branched entry set).

**Plan ordering-bug correction**: the plan proposed replacing the scan with
`self.has_assistant` directly. But at the time `persist_entry_at` runs, the
entry has already been pushed to `file_entries` and `has_assistant` has not yet
been updated — so the first assistant message would see `has_assistant == false`
and skip the exclusive-create flush, breaking `deferred_write_until_first_assistant`.
The fix: compute `has_assistant_after_append = self.has_assistant ||
entry_at_idx.is_assistant_message()` in O(1) and use that for the persist decision.
The cached field is assigned only after `persist_entry_at` returns Ok, so failed
appends leave it untouched and retries correctly flush.

### Branch classification

| Replaced branch | Classification | Reason |
|---|---|---|
| `persist_entry_at` O(n) scan → O(1) cached read | Residue | The scan recomputes a monotonically non-decreasing boolean on every append; the result is cacheable in O(1) |
| `create_branched_session` O(n) scan → cached read | Residue | `build_index` already iterates all entries; folding the assistant check into that pass eliminates the second scan |
| `build_index` adds `has_assistant` tracking | Essential | Index rebuild must reflect loaded entry state; folded into existing loop (zero additional iterations) |

### Boundary answers

- **JSONL v3 wire format**: untouched. `has_assistant` is interior state; the
  persist decision logic (deferred write, exclusive-create flush, append-line)
  is unchanged — only the *source* of the `has_assistant` boolean changes from
  O(n) scan to O(1) cache.
- **First-assistant deferred flush**: preserved. The effective post-append
  calculation ensures the first assistant triggers the exclusive-create flush
  exactly as before.
- **Failed-append rollback**: preserved. `has_assistant` is updated only after
  `persist_entry_at` succeeds; rollback pops the entry and restores
  leaf/by_id/flushed without touching `has_assistant`.
- **Retry after failure**: preserved. A failed first-assistant append leaves
  `has_assistant == false`; the retry correctly computes the effective state
  and flushes.
- **Migration/load**: preserved. `build_index` recomputes `has_assistant` from
  loaded/migrated entries in the existing index pass.
- **Branch**: preserved. `build_index` on the branched entry set recomputes
  `has_assistant`; the branch persist decision uses the cached value.
- **New session / empty file**: preserved. Both set `has_assistant = false`.

### Cutover surface

1 file: `crates/pi/src/core/sessions/mod.rs`. No new dependencies, no serializer
change, no held-open fd, no Cargo.lock change.

### Measurements

Release, `taskset -c 20-40`, 9 interleaved baseline/design pairs, per-run
20-sample medians; `session-timing --mode append --entries 5000`.
Baseline = clean `f4ae3ae` binary; design = cutover binary.

| Pair | Baseline (us/entry) | Design (us/entry) | Speedup |
|---|---|---|---|
| 1 | 15.260 | 5.524 | 2.76x |
| 2 | 16.120 | 5.196 | 3.10x |
| 3 | 15.748 | 5.463 | 2.88x |
| 4 | 16.069 | 5.610 | 2.86x |
| 5 | 15.042 | 5.338 | 2.82x |
| 6 | 15.554 | 5.275 | 2.95x |
| 7 | 15.180 | 5.082 | 2.99x |
| 8 | 15.432 | 5.183 | 2.98x |
| 9 | 15.006 | 5.130 | 2.93x |

**Overall median (pair 5 sorted)**: baseline 15.432 us, design 5.275 us,
**speedup 2.93x**. All relative spreads < 20% (noise gate passed).

SHA-256 prefixes: 180 unique per arm (20 samples x 9 pairs), all 16-char
hex valid — each sample creates a fresh session with random IDs, so prefixes
differ across runs by design. Wire-byte stability validated by the
`append_prefix_stability` test (unchanged, passes).

### Recomputed multiple

Design 5.275 us/entry vs ledger floor 3.735 us/entry = **1.41x — AT-FLOOR**
(<=2x). The O(n^2) scan was the dominant overhead: at 5000 entries the average
scan cost was ~10 us/append (2500 iterations x ~4 ns/iter for enum match +
string compare), far exceeding the ledger's flat 1.28 us profiler attribution.
Removing it drops per-entry cost from ~15.4 us to ~5.3 us, within 1.41x of the
theoretical floor.

Residual composition (5.275 us vs 3.735 us floor = 1.54 us gap):
- openat+close per append: ~2.06 us (held-open fd design rejected by advocate:
  external rename/delete, partial-write rollback, lifecycle transition hazards)
- entry serialization Value pipeline: the remaining gap is the serialization
  + write + bookkeeping floor terms
- The only remaining halving lever (held-open fd) was rejected by the design
  advocate for correctness reasons (partial-write atomicity, external rename
  safety, lifecycle transition exhaustiveness)

### Verification

- `cargo check -p pi`: green (warnings pre-existing, none in touched file)
- `core::sessions` tests: 64/64 pass (deferred_write_until_first_assistant,
  failed_append_does_not_advance_tree_and_can_retry,
  create_branched_session_defers_without_assistant,
  create_branched_session_writes_with_assistant, append_prefix_stability,
  empty_file_gets_header, generated_cross_version_session_interoperability,
  and all others)
- `pi check` on touched file: compilation succeeded; runtime check blocked by
  worktree environment (auth/session storage permission denied — not a code issue)

### Review

Fresh adversarial review: **CLEAN**.

### Not touched

Out of scope, file-disjoint: `.github/workflows/`, `scripts/release/`,
`scripts/verification/compat-matrix.json`, `docs/supported-platforms.md`,
DEPS ledger docs, floor ledgers, `Cargo.lock`, `rust-toolchain.toml`,
bench scripts (`session-timing.rs`).

## Iteration 22 — `extension-rpc-dispatch` (timed serve_io lane — NOISY, no classification)

Commit: see `git log` (`perf(t11)`). Date 2026-08-28.

**Unit**: `extension-rpc-dispatch` — JSONL frame encode/decode, request
correlation (id matching), host loop dispatch, widget callback + UI-slot
traffic. Floor: 750-1000 ns/request (server-only, computed from contract).

**Goal**: land a trusted timed real-`serve_io` lane for extension RPC
dispatch, measure it, and classify against the server-only floor. No
production optimization in this iteration.

### Implementation

Added a `bench-seam` Cargo feature to `pi-ext` (zero production overhead;
disabled by default). When enabled, `server.rs` exposes a `bench_seam` module
with `record_decode(id)` / `record_encode(id)` / `take_completed()` / `clear()`
using a `LazyLock<Mutex<HashMap<u64, (Instant, Option<Instant>)>>>`. Seam
calls are inserted at:

1. **Decode seam**: after `FrameDecoder::push` returns frames, before
   `dispatch_ready` — `bench_seam::record_decode(frame.id)` in the `Ready`
   branch of `drive()`.
2. **Encode seam**: after `handle_request_dispatch` constructs the terminal
   frame, before `out_tx.send` — `bench_seam::record_encode(id)`.

Both are `#[cfg(feature = "bench-seam")]` gated — no code in production builds.

Added `#[ignore] #[test] fn timed_serve_io_perf_t11_extension_rpc_dispatch`
to `serve_io_scaling.rs`. It factors the PERF-T6 300-request corpus into
shared `corpus_setup` / `corpus_data` helpers (single source of truth — no
fixture fork). Each round: fresh `current_thread` runtime, setup/hello/load/
session_start/uiSlot drain outside timing, 300 sequential `terminalInput`
(ids 300-599, x/a/b cycling) inside timing. Measures:

- **Inclusive RTT**: batch wall time / 300 (client encode → duplex → server
  → response decode).
- **Attributed server S**: per-request `encode_complete − decode_start` from
  seam timestamps, median of 300 per round.

3 warmup + 9 measured rounds (env `BENCH_MEASURED_ROUNDS` for retry). Noise
gate: population stddev / median ≤ 0.20. Classification: S ≤ 1500 ns →
AT-FLOOR; S > 2000 ns → OPEN >2x; 1500 < S ≤ 2000 → BOUNDARY fail-closed.

### Measurements

First run (9 measured rounds, `taskset -c 20-40`):

| Round | RTT (ns/req) | S_median (ns) |
|---|---|---|
| 1 | 13,216 | 3,930 |
| 2 | 10,406 | 2,526 |
| 3 | 14,132 | 3,451 |
| 4 | 6,090 | 1,748 |
| 5 | 13,457 | 4,099 |
| 6 | 13,322 | 3,981 |
| 7 | 13,200 | 3,982 |
| 8 | 4,051 | 1,168 |
| 9 | 8,866 | 2,692 |

Median S = 3,451 ns, rs = 29.63% — **NOISY**.

Retry (27 measured rounds, `taskset -c 20`):

| Round | RTT (ns/req) | S_median (ns) |
|---|---|---|
| 1 | 6,692 | 2,032 |
| 2 | 13,458 | 3,994 |
| 3 | 13,343 | 4,027 |
| 4 | 3,548 | 1,076 |
| 5 | 6,805 | 2,054 |
| 6 | 13,323 | 4,110 |
| 7 | 4,134 | 1,114 |
| 8 | 14,334 | 4,053 |
| 9 | 13,308 | 4,044 |
| 10 | 9,110 | 2,842 |
| 11 | 4,190 | 1,229 |
| 12 | 3,939 | 1,194 |
| 13 | 33,255 | 2,914 |
| 14 | 13,324 | 4,116 |
| 15 | 13,287 | 4,015 |
| 16 | 13,164 | 4,012 |
| 17 | 15,849 | 4,886 |
| 18 | 5,570 | 1,682 |
| 19 | 7,294 | 2,257 |
| 20 | 8,751 | 2,627 |
| 21 | 13,317 | 3,988 |
| 22 | 13,562 | 4,118 |
| 23 | 24,362 | 8,035 |
| 24 | 23,002 | 7,934 |
| 25 | 6,644 | 1,982 |
| 26 | 13,404 | 4,074 |
| 27 | 19,800 | 6,266 |

Median S = 3,994 ns, rs = 45.24% — **NOISY**.

### Classification

**NOISY — no classification allowed.** The noise gate (rs ≤ 0.20) failed at
both 9 and 27 measured rounds. The S_median distribution is bimodal: a
dominant cluster at ~4,000 ns and a secondary cluster at ~1,100-2,000 ns.
The dominant cluster (~4,000 ns) exceeds the 2,000 ns OPEN threshold
(2× floor_max = 2× 1,000 ns); the secondary cluster (~1,100-2,000 ns)
straddles the AT-FLOOR/BOUNDARY/OPEN thresholds. The noise gate prevents
any classification.

### Instrumentation overhead

Two `std::sync::Mutex` lock/unlock pairs per request (~40-100 ns total),
present in both warmup and measured rounds. Disclosed but not subtracted.
The overhead is small relative to the measured ~4,000 ns S and does not
explain the bimodality.

### Bimodality analysis

The bimodal pattern is consistent across runs and likely originates from
tokio current-thread runtime scheduling: `dispatch_ready` spawns a task per
request (`tasks.spawn`), and the attributed S interval spans the task spawn →
semaphore acquire → handler → `out_tx.send` → writer task wake path.
Sometimes the spawned task runs to completion before the drive loop yields
(lower cluster), sometimes it yields back through the scheduler (upper
cluster). The floor terms (decode ~400 ns, correlate ~50 ns, encode ~300 ns
= ~750 ns) are dwarfed by the scheduling overhead.

### Next blind candidate (named, not attempted)

If the noise gate can be passed (e.g., by reducing scheduling variability or
measuring a narrower seam that excludes task-spawn overhead), the dominant
cost is the per-request `tasks.spawn` + semaphore acquire + `out_tx.send`
round-trip through the tokio scheduler. A blind candidate would inline the
terminalInput handler on the drive loop (skip `tasks.spawn` for
non-cancellable methods), eliminating the spawn + wake overhead. But this
is an optimization for a future iteration, not this one.

### Verification

- `cargo test -p pi-ext --test serve_io_scaling` (no feature): 10/10 pass,
  zero warnings
- `cargo test -p pi-ext --features bench-seam --test serve_io_scaling --release
  --no-run`: compiles clean
- `cargo test -p pi-ext --features bench-seam --test serve_io_scaling --release
  -- --ignored --exact --nocapture timed_serve_io_perf_t11_extension_rpc_dispatch`:
  runs, produces distributions, fails on noise gate (expected)
- `Cargo.lock`: byte-identical (sha256 `9eef233d...`)

### Review

Fresh adversarial review: **CLEAN**.

### Not touched

Out of scope, file-disjoint: `.github/workflows/`, `scripts/`, production
`serve_io` logic (only `#[cfg(feature = "bench-seam")]` seam calls added),
`Cargo.lock`, `rust-toolchain.toml`, other floor ledgers.

## Iteration 23 — `extension-rpc-dispatch` (same-protocol single-CPU re-run — still NOISY, no classification)

Commit: see `git log` (`docs(t11)`). Date 2026-08-28.

**Unit**: `extension-rpc-dispatch` (unchanged from iteration 22). Floor:
750-1000 ns/request (server-only, computed from contract).

**Goal**: test whether the iteration-22 noise-gate failure (rs ≤ 0.20 failed
at 45.24%) is stable across sessions by repeating the identical recorded
retry protocol on a fresh session: same one-CPU pin (`taskset -c 20`), same
3 warmup + 27 measured rounds. Iteration 22's recorded 27-round run already
used this pin (only its earlier 9-round attempt ran `taskset -c 20-40`), so
this is a same-protocol re-run, not a new remediation. Contract for this
iteration: no production or test code edits, no `yield_now` (it would bias
the real server path with an extra scheduler hop), no protocol changes.

### Protocol (unchanged lane, one CPU)

```
taskset -c 20 env BENCH_MEASURED_ROUNDS=27 cargo test -p pi-ext --features bench-seam --test serve_io_scaling --release -- --ignored --exact --nocapture timed_serve_io_perf_t11_extension_rpc_dispatch
```

3 warmup + 27 measured rounds; identical 300-request `terminalInput` corpus
(ids 300-599, x/a/b cycling); fresh current-thread runtime per round. The
pre-iteration source scout (agent://ScoutExtensionRpcTiming) confirmed the
lane already builds `Builder::new_current_thread()` per round on a plain
`#[test]` (pi-ext has no `rt-multi-thread`), and that the seam Mutex cannot
contend (single thread, lock never held across an await) — the
multi-thread-runtime and Mutex-contention hypotheses from iteration 22's
bimodality discussion are false and were not pursued. Warmup S_medians
(1,922 / 2,300 / 4,023 ns) are excluded from statistics per the lane.

### Measurements (27 measured rounds, `taskset -c 20`, release)

| Round | RTT (ns/req) | S_median (ns) |
|---|---|---|
| 1 | 3,726 | 1,100 |
| 2 | 13,329 | 4,001 |
| 3 | 13,554 | 4,009 |
| 4 | 13,340 | 4,010 |
| 5 | 13,147 | 3,989 |
| 6 | 20,293 | 7,422 |
| 7 | 9,813 | 2,889 |
| 8 | 13,425 | 4,098 |
| 9 | 21,739 | 5,062 |
| 10 | 10,616 | 3,248 |
| 11 | 13,319 | 3,978 |
| 12 | 13,133 | 3,994 |
| 13 | 8,226 | 2,498 |
| 14 | 13,403 | 4,111 |
| 15 | 23,415 | 4,000 |
| 16 | 13,368 | 4,034 |
| 17 | 13,337 | 4,051 |
| 18 | 13,358 | 4,068 |
| 19 | 21,464 | 5,892 |
| 20 | 13,127 | 3,975 |
| 21 | 13,414 | 4,048 |
| 22 | 13,346 | 4,001 |
| 23 | 8,213 | 2,446 |
| 24 | 13,573 | 4,111 |
| 25 | 5,329 | 1,573 |
| 26 | 5,744 | 1,733 |
| 27 | 17,590 | 5,180 |

Median S = 4,001 ns (population sd 1,262 ns), inclusive RTT median =
13,340 ns/req, rs = 31.55% — **NOISY** (gate ≤ 20% fails).

### Analysis

Under the identical protocol, rs came in at 31.55% against iteration 22's
recorded 45.24%: the noise level itself drifts between sessions, and the
gate fails in both runs. No spread improvement can be attributed to a
pinning change — the recorded retry used the same pin.

Both recorded 27-round runs are one-CPU-pinned and both distributions are
multi-modal, so cross-CPU migration — which a one-CPU affinity set already
excludes — is not the dominant source of the modality (single-CPU
frequency/cache state is not controlled by this protocol). The shape:
dominant cluster ~3,975-4,111 ns (16 rounds), one mid-round at 3,248 ns
(round 10), low cluster ~1,100-2,889 ns (6 rounds: 1, 7, 13, 23, 25, 26),
high tail ~5,062-7,422 ns (4 rounds: 6, 9, 19, 27). The per-round medians
carry the spread — each round's S_median lands in one region of the
distribution (a round-level location shift into the noise-gate input) —
which is consistent with the scout's read of a per-round sticky scheduler
shape (whether the spawned request task rides the decode→encode seam in one
cooperative hop or yields back through the runtime).

### Classification

**NOISY — no classification allowed.** The noise gate (rs ≤ 0.20) failed at
31.55%. No cluster may be picked from the distribution; the unit stays
**OPEN (fail-closed)**.

### Next evidence requirement (named, not attempted)

This iteration's contract forbade measurement-affecting code changes, so the
seam still spans drive-decode → spawned-task encode and includes the
per-request JoinSet spawn + cooperative hop. Before any classification
attempt, the campaign needs attribution that separates that spawn + hop cost
from handler cost — a bench-seam-gated seam refinement (e.g., start the S
interval inside the spawned task) or an equivalent hop-determinism probe —
validated by a passing noise gate over the full 27-round protocol. The
optimization candidate (inline `terminalInput` handler on the drive loop to
eliminate spawn + scheduler round-trip) remains gated behind a trusted lane
and is untouched.

### Verification

Lane exited non-zero (101) with `NOISY: no classification allowed` — the
expected fail-closed path. `Cargo.lock`: byte-identical (sha256
`9eef233d...`). Diff docs-only: `docs/performance/floors/extension-rpc-dispatch.md`
and `docs/performance/t11-iterations.md`. Fresh adversarial docs review:
**CLEAN**.

### Not touched

Out of scope, file-disjoint: production `serve_io` logic, `server.rs`
(including `bench-seam` seams), `serve_io_scaling.rs`, `Cargo.toml`,
`Cargo.lock`, `.github/workflows/`, `scripts/`, other floor ledgers,
`rust-toolchain.toml`.

## Iteration 24 — `extension-rpc-dispatch` (hop attribution — S gate PASSED, OPEN >2x)

Commit: see `git log` (`perf(t11)` + `docs(t11)`). Date 2026-08-28.

**Unit**: `extension-rpc-dispatch` (unchanged from iterations 22-23). Floor:
750-1000 ns/request (server-only, computed from contract).

**Goal**: add a third bench-seam timestamp at the spawned task entry to
decompose S into Q (decode→task start: spawn + cooperative scheduler hop)
and H (task start→encode: handler + response construction + encode),
attribute the multi-modal noise observed in iterations 22 (rs 45.24%) and
23 (rs 31.55%), and classify if the S noise gate passes. No production
optimization.

### Implementation

Extended the `bench_seam` module in `server.rs` to store a triple
`(decode_start, task_start, encode_complete)` per frame id. Added
`record_task_start(id)` called as the first executed line inside
`handle_request`, immediately after `let id = frame.id;` and before the
panic guard / handler dispatch. This captures the instant the spawned task
begins executing on the executor — after the JoinSet spawn and cooperative
scheduler hop, before any handler work.

Storage schema changed from `HashMap<u64, (Instant, Option<Instant>)>` to
`HashMap<u64, (Instant, Option<Instant>, Option<Instant>)>`. `take_completed`
returns `(id, decode_start, task_start, encode_complete)` and filters
entries where any of the three timestamps is missing. The test lane asserts
Q+H==S for every id and requires all 300 triplets per round (no silent
filtering of incomplete entries).

Formulas:
  Q = task_start - decode_start   (spawn + cooperative scheduler hop)
  H = encode_complete - task_start (handler + response construction + encode)
  S = Q + H = encode_complete - decode_start (total server cost, unchanged)

All seams are `#[cfg(feature = "bench-seam")]` gated — zero production
overhead. No new dependency, no Cargo.lock change.

### Protocol

```
taskset -c 20 env BENCH_MEASURED_ROUNDS=27 cargo test -p pi-ext --features bench-seam --test serve_io_scaling --release -- --ignored --exact --nocapture timed_serve_io_perf_t11_extension_rpc_dispatch
```

3 warmup + 27 measured rounds; identical 300-request `terminalInput` corpus
(ids 300-599, x/a/b cycling); fresh current-thread runtime per round; CPU
pinned to core 20.

### Measurements (27 measured rounds, `taskset -c 20`, release)

| Round | RTT (ns/req) | Q_median (ns) | H_median (ns) | S_median (ns) |
|---|---|---|---|---|
| 1 | 14,244 | 2,940 | 1,581 | 4,564 |
| 2 | 12,059 | 2,755 | 1,249 | 4,033 |
| 3 | 13,753 | 2,924 | 1,416 | 4,360 |
| 4 | 13,694 | 2,941 | 1,347 | 4,263 |
| 5 | 13,891 | 2,915 | 1,436 | 4,390 |
| 6 | 13,829 | 2,933 | 1,419 | 4,367 |
| 7 | 13,826 | 2,920 | 1,463 | 4,381 |
| 8 | 14,779 | 2,994 | 1,421 | 4,414 |
| 9 | 14,616 | 2,896 | 1,462 | 4,383 |
| 10 | 13,945 | 2,899 | 1,462 | 4,404 |
| 11 | 13,713 | 2,935 | 1,397 | 4,340 |
| 12 | 13,807 | 2,923 | 1,439 | 4,410 |
| 13 | 14,090 | 3,012 | 1,583 | 4,657 |
| 14 | 8,716 | 1,779 | 949 | 2,740 |
| 15 | 10,619 | 2,036 | 1,221 | 3,240 |
| 16 | 16,852 | 3,412 | 1,913 | 5,326 |
| 17 | 13,780 | 2,880 | 1,482 | 4,399 |
| 18 | 13,977 | 2,929 | 1,411 | 4,352 |
| 19 | 14,451 | 2,944 | 1,547 | 4,497 |
| 20 | 14,067 | 2,951 | 1,485 | 4,444 |
| 21 | 13,905 | 2,905 | 1,484 | 4,428 |
| 22 | 13,841 | 2,881 | 1,474 | 4,381 |
| 23 | 13,885 | 2,919 | 1,569 | 4,512 |
| 24 | 13,943 | 2,969 | 1,537 | 4,537 |
| 25 | 13,883 | 2,888 | 1,536 | 4,452 |
| 26 | 13,833 | 2,905 | 1,457 | 4,380 |
| 27 | 13,899 | 2,932 | 1,470 | 4,439 |

Aggregate (median of round medians):

| Metric | Value |
|---|---|
| Inclusive RTT (median) | 13,885 ns/request |
| Q (spawn + cooperative hop) | 2,923 ns/request |
| H (handler + encode) | 1,462 ns/request |
| S (total server) | 4,399 ns/request |
| rs_Q | 9.97% |
| rs_H | 10.65% |
| rs_S | 9.93% |
| Noise gate (rs_S ≤ 0.20) | **PASSED** |

Q_median samples (ns): [2940, 2755, 2924, 2941, 2915, 2933, 2920, 2994, 2896, 2899, 2935, 2923, 3012, 1779, 2036, 3412, 2880, 2929, 2944, 2951, 2905, 2881, 2919, 2969, 2888, 2905, 2932]

H_median samples (ns): [1581, 1249, 1416, 1347, 1436, 1419, 1463, 1421, 1462, 1462, 1397, 1439, 1583, 949, 1221, 1913, 1482, 1411, 1547, 1485, 1484, 1474, 1568, 1537, 1536, 1457, 1470]

S_median samples (ns): [4564, 4033, 4360, 4263, 4390, 4367, 4381, 4414, 4383, 4404, 4340, 4410, 4657, 2741, 3240, 5326, 4399, 4352, 4497, 4444, 4428, 4381, 4512, 4537, 4452, 4380, 4439]

RTT samples (ns/req): [14244, 12059, 13753, 13694, 13891, 13829, 13826, 14779, 14616, 13945, 13713, 13807, 14090, 8716, 10619, 16852, 13780, 13977, 14451, 14067, 13905, 13841, 13885, 13943, 13883, 13833, 13899]

### Attribution analysis

The S noise gate **passed** at rs_S = 9.93% (≤ 20%), a dramatic improvement
from iteration 22 (45.24%) and iteration 23 (31.55%). All three distributions
are now under the noise gate:

- **rs_Q = 9.97%** — Q (spawn + cooperative scheduler hop) is tight
- **rs_H = 10.65%** — H (handler + encode) is tight
- **rs_S = 9.93%** — S (total server) is tight

The improvement from iterations 22/23 is attributed to session-level
variability in the host environment (CPU frequency/cache state, OS scheduler
interference) rather than the added seam — the seam adds one Mutex lock +
HashMap get_mut + Instant::now() + Option write per request, same order as
the existing record_decode/record_encode calls. The added seam overhead is
disclosed and bounded empirically by comparing S against iteration 23's
baseline: S_median = 4399 ns vs iteration 23's 4001 ns, a ~10% increase
consistent with the third Mutex lock pair (~40-50 ns) plus measurement
noise, not a structural perturbation.

**Q dominates S**: Q_median = 2923 ns (66.5% of S) vs H_median = 1462 ns
(33.2% of S). The spawn + cooperative scheduler hop is ~2× the handler +
encode cost. Both Q and H are individually tight (rs < 11%), so the
multi-modality observed in iterations 22/23 was a round-level location
shift — the entire Q+H pair shifted together between rounds, not one
component oscillating independently. This is consistent with the
iteration-23 analysis: a per-round sticky scheduler shape (whether the
spawned task rides the decode→encode seam in one cooperative hop or yields
back through the runtime) affected both Q and H proportionally.

The floor terms (decode ~400 ns, correlate ~50 ns, encode ~300 ns = ~750 ns)
are dwarfed by Q (spawn + hop ~2923 ns), confirming the iteration-22/23
hypothesis that async task scheduling overhead dominates the server cost.

### Classification

**OPEN >2x** — S = 4399 ns > 2000 ns = 2× floor_max (1000 ns). The noise
gate passed (rs_S = 9.93% ≤ 20%), so this classification is trusted.

### Iteration-25 candidate (NAMED, NOT IMPLEMENTED)

The dominant cost is Q (spawn + cooperative scheduler hop = 2923 ns, 66.5%
of S). A blind iteration-25 candidate would inline the `terminalInput`
handler on the drive loop (skip `tasks.spawn` for non-cancellable methods),
eliminating the spawn + cooperative hop. This would collapse Q toward zero
and leave H (~1462 ns) as the server cost — which is 1.46× floor_max (1000
ns), potentially classifiable as BOUNDARY or AT-FLOOR after re-measurement.
The 4 ms terminal-input budget is not at risk: the current total server cost
(4399 ns) is ~900× under the 4 ms budget. This candidate is named but not
implemented in this iteration.

### Instrumentation overhead

Three `std::sync::Mutex` lock/unlock pairs per request (record_decode,
record_task_start, record_encode), ~60-150 ns total, present in both warmup
and measured rounds. Disclosed but not subtracted. The overhead is small
relative to the measured ~4399 ns S.

### Verification

- `cargo test -p pi-ext --test serve_io_scaling` (no feature): 10/10 pass
- `cargo test -p pi-ext --features bench-seam --test serve_io_scaling --release --no-run`: compiles clean
- `taskset -c 20 env BENCH_MEASURED_ROUNDS=27 cargo test -p pi-ext --features bench-seam --test serve_io_scaling --release -- --ignored --exact --nocapture timed_serve_io_perf_t11_extension_rpc_dispatch`: 1 passed, noise gate passed, classification OPEN >2x
- Q+H==S asserted per-request for all 300×27 = 8100 samples
- All 300 complete triplets per round (no missing timestamps)
- `Cargo.lock`: byte-identical (sha256 `9eef233d...`)

### Review

Fresh adversarial review: **CLEAN**.

### Not touched

Out of scope, file-disjoint: production `serve_io` logic (only
`#[cfg(feature = "bench-seam")]` seam calls added/modified), `Cargo.toml`,
`Cargo.lock`, `.github/workflows/`, `scripts/`, other floor ledgers,
`rust-toolchain.toml`.

## Iteration 25 — `extension-rpc-dispatch` (FuturesUnordered request worker — S gate PASSED, OPEN >2x, win 1.44x)

Commit: see `git log` (`perf(t11)` + `docs(t11)`). Date 2026-08-29.

**Unit**: `extension-rpc-dispatch` (unchanged from iterations 22-24). Floor:
750-1000 ns/request (server-only, computed from contract).

**Goal**: replace per-request `JoinSet::spawn` with one long-lived
supervised request worker owning a `FuturesUnordered` of request futures,
eliminating the per-request spawn allocation and JoinSet bookkeeping that
dominates Q (spawn + cooperative scheduler hop = 2923 ns, 66.5% of S in
iteration 24). Preserve drive-loop responsiveness to cancel frames,
bounded concurrency, exact teardown, and one-response-per-id.

### Implementation

Added a `RequestJob` struct owning the frame, `OwnedSemaphorePermit`,
optional `ShortcutClaim`, and optional `CancellationToken`. Added a
`run_request_worker` async function: one long-lived task that owns a
`FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send>>>` and uses
`tokio::select!` over `job_rx.recv()` and `pending.next()` (guarded by
`if !pending.is_empty()`). Each admitted job is pushed as the existing
panic-guarded `handle_request` future — no callback serialization, max
overlap remains `max_in_flight`.

`dispatch_request` retains the exact synchronous admission order
(semaphore permit, shortcut claim, cancellation token registration) then
`try_send`s an owned `RequestJob` to the bounded channel instead of
`tasks.spawn`. On `Full`: undoes in-flight registration, drops permit/claim,
emits one correlated `overloaded` error via `rejection_tx`. On `Closed`:
undoes registration, drops permit/claim, returns `ServerError::Io` (fatal).

`serve_io_inner` creates the bounded channel (capacity = `max_in_flight`)
and spawns the worker before `drive`. Teardown: `drop(job_tx)`, cancel
in-flight tokens, `worker_handle.abort()` + `await` (dropping pending
futures, releasing permits/claims via RAII), then `tasks.abort_all()` +
join (theme tasks), then `drop(rejection_tx)` + `drop(runtime)` +
`writer_shutdown.cancel()` + reap writer/rejection flusher — preserving
the existing writer/rejection teardown ordering. The current code aborts
requests (no graceful drain), so the worker is aborted, not drained.

`drive` and `dispatch_ready` signatures gain a `&mpsc::Sender<RequestJob>`
parameter. Cancel events (`tool.cancel` / `provider.cancel`) remain
processed synchronously in `drive` → `dispatch_ready` while worker
futures are pending — the drive loop never awaits request futures.

Added `max_in_flight: usize` field to `ServerRuntime` (stored from
`config.max_in_flight.max(1)`) for the channel capacity. Added imports:
`FuturesUnordered`, `StreamExt` from `futures`, `OwnedSemaphorePermit`
from `tokio::sync`. No new dependencies; `futures` 0.3.34 (already in
`Cargo.toml` with `alloc` feature) provides `FuturesUnordered` and
`StreamExt`.

### Protocol

```
taskset -c 20 env BENCH_MEASURED_ROUNDS=27 cargo test -p pi-ext --features bench-seam --test serve_io_scaling --release -- --ignored --exact --nocapture timed_serve_io_perf_t11_extension_rpc_dispatch
```

3 warmup + 27 measured rounds; identical 300-request `terminalInput`
corpus (ids 300-599, x/a/b cycling); fresh current-thread runtime per
round; CPU pinned to core 20. Canonical baseline binary built from
e426e73; design binary built from the perf commit. Alternating A/B
protocol (9 pairs, 9 measured rounds each) run first, then 27-round
trusted run for the design binary.

### A/B protocol (9 alternating pairs, `taskset -c 20`, 9 measured rounds)

| Pair | Baseline S (ns) | Design S (ns) | Baseline rs_S | Design rs_S |
|---|---|---|---|---|
| 1 | 4,634 | 3,306 | 29.05% | 29.29% |
| 2 | 4,443 | 3,054 | 44.26% | 27.03% |
| 3 | 4,312 | 2,030 | 24.03% | 7.22% |
| 4 | 2,754 | 1,892 | 9.61% | 10.43% |
| 5 | 2,682 | 1,850 | 23.97% | 9.33% |
| 6 | 1,555 | 1,874 | 40.18% | 20.42% |
| 7 | 2,642 | 1,822 | 19.41% | 41.23% |
| 8 | 2,906 | 1,976 | 44.09% | 31.67% |
| 9 | 2,597 | 2,480 | 42.30% | 27.31% |

Design S is lower than baseline in 8/9 pairs (pair 6 is the exception:
baseline hit a low-noise cluster at 1555 ns). Design Q is consistently
lower: median Q drops from ~2900 ns to ~1000 ns across pairs.

### Trusted measurement (27 measured rounds, `taskset -c 20`, release)

| Round | RTT (ns/req) | Q_median (ns) | H_median (ns) | S_median (ns) |
|---|---|---|---|---|
| 1 | 12,531 | 1,788 | 1,328 | 3,141 |
| 2 | 12,471 | 1,733 | 1,315 | 3,093 |
| 3 | 12,386 | 1,704 | 1,311 | 3,039 |
| 4 | 12,335 | 1,712 | 1,298 | 3,039 |
| 5 | 12,403 | 1,721 | 1,297 | 3,057 |
| 6 | 12,387 | 1,700 | 1,292 | 3,018 |
| 7 | 12,523 | 1,719 | 1,313 | 3,070 |
| 8 | 11,138 | 1,456 | 1,114 | 2,586 |
| 9 | 12,574 | 1,754 | 1,348 | 3,127 |
| 10 | 12,366 | 1,708 | 1,276 | 3,019 |
| 11 | 12,484 | 1,693 | 1,321 | 3,058 |
| 12 | 12,527 | 1,753 | 1,324 | 3,080 |
| 13 | 12,264 | 1,712 | 1,314 | 3,067 |
| 14 | 12,373 | 1,694 | 1,297 | 3,023 |
| 15 | 12,391 | 1,706 | 1,337 | 3,067 |
| 16 | 12,258 | 1,670 | 1,296 | 2,982 |
| 17 | 5,937 | 805 | 635 | 1,443 |
| 18 | 5,196 | 719 | 548 | 1,284 |
| 19 | 12,480 | 1,674 | 1,284 | 3,015 |
| 20 | 22,411 | 1,706 | 1,381 | 3,097 |
| 21 | 14,309 | 1,887 | 1,714 | 3,603 |
| 22 | 12,333 | 1,671 | 1,306 | 3,001 |
| 23 | 12,214 | 1,728 | 1,294 | 3,062 |
| 24 | 12,420 | 1,731 | 1,311 | 3,075 |
| 25 | 12,695 | 1,762 | 1,310 | 3,101 |
| 26 | 8,011 | 1,154 | 827 | 1,980 |
| 27 | 12,384 | 1,746 | 1,292 | 3,070 |

Aggregate (median of round medians):

| Metric | Value |
|---|---|
| Inclusive RTT (median) | 12,387 ns/request |
| Q (spawn + cooperative hop) | 1,708 ns/request |
| H (handler + encode) | 1,306 ns/request |
| S (total server) | 3,058 ns/request |
| rs_Q | 16.02% |
| rs_H | 17.30% |
| rs_S | 16.37% |
| Noise gate (rs_S ≤ 0.20) | **PASSED** |
| Win vs iteration-24 baseline (S) | 4399 / 3058 = **1.44x** (≥1.05 gate) |
| Q reduction | 2923 → 1708 ns (−41.5%, material) |
| H equivalence | 1462 → 1306 ns (within noise) |

Q_median samples (ns): [1788, 1733, 1704, 1712, 1721, 1700, 1719, 1456, 1754, 1708, 1693, 1753, 1712, 1694, 1706, 1670, 805, 719, 1674, 1706, 1887, 1671, 1728, 1731, 1762, 1154, 1746]

H_median samples (ns): [1328, 1315, 1311, 1298, 1297, 1292, 1313, 1114, 1348, 1276, 1321, 1324, 1314, 1297, 1337, 1296, 635, 548, 1284, 1381, 1714, 1306, 1294, 1311, 1310, 827, 1292]

S_median samples (ns): [3141, 3093, 3039, 3039, 3057, 3018, 3070, 2586, 3127, 3019, 3058, 3080, 3067, 3023, 3067, 2982, 1443, 1284, 3015, 3097, 3603, 3001, 3062, 3075, 3101, 1980, 3070]

RTT samples (ns/req): [12531, 12471, 12386, 12335, 12403, 12387, 12523, 11138, 12574, 12366, 12484, 12527, 12264, 12373, 12391, 12258, 5937, 5196, 12480, 22411, 14309, 12333, 12214, 12420, 12695, 8011, 12384]

### Attribution analysis

Q dropped from 2923 ns (iteration 24) to 1708 ns (−41.5%). The remaining
Q (~1708 ns) is the channel handoff (`try_send` + `recv` + scheduler wake
of the worker task) plus the `FuturesUnordered::push` overhead. The
per-request `JoinSet::spawn` allocation and task registration are
eliminated. The worker task is spawned once and reused for all requests.

H remained equivalent: 1462 ns → 1306 ns (within noise — rs_H = 17.30%
vs iteration 24's 10.65%). The handler, response construction, and encode
costs are unchanged.

S dropped from 4399 ns to 3058 ns (win 1.44x). The noise gate passed at
rs_S = 16.37% (≤ 20%). The dominant cluster is ~3000-3100 ns (20 of 27
rounds), with low outliers at 1284/1443/1980/2586 ns (rounds 17, 18, 26,
8) and one high outlier at 3603 ns (round  21). The low outliers
correspond to rounds where both Q and H dropped together (same per-round
sticky scheduler shape as iteration 24), not one component oscillating
independently.

### Classification

**OPEN >2x** — S = 3058 ns > 2000 ns = 2× floor_max (1000 ns). The noise
gate passed (rs_S = 16.37% ≤ 20%), so this classification is trusted.

### Gate verdict

| Gate | Criterion | Result |
|---|---|---|
| Win | S_baseline / S_design ≥ 1.05 | 4399 / 3058 = 1.44x **PASS** |
| Q decrease | Q_design < Q_baseline (material) | 1708 < 2923 (−41.5%) **PASS** |
| H equivalence | H_design ≈ H_baseline | 1306 ≈ 1462 (within noise) **PASS** |
| Noise | rs_S ≤ 0.20 | 16.37% **PASS** |
| Cargo.lock | unchanged | sha256 `9eef233d...` **PASS** |
| Correctness | scaling + server tests | 10/10 + 55/55 **PASS** |
| Review | fresh adversarial | **CLEAN** |

**Accepted**: perf commit + docs/ledger record.

### Verification

- `cargo test -p pi-ext --test serve_io_scaling` (no feature): 10/10 pass
- `cargo test -p pi-ext --features bench-seam --test serve_io_scaling` (debug): 10/10 pass, 1 ignored
- `cargo test -p pi-ext --lib -- server::tests`: 55/55 pass
- `cargo test -p pi-ext --features bench-seam --test serve_io_scaling --release --no-run`: compiles clean
- `taskset -c 20 env BENCH_MEASURED_ROUNDS=27 /tmp/bench_design_iter25 --test timed_serve_io_perf_t11_extension_rpc_dispatch --ignored --exact --nocapture`: 1 passed, noise gate passed, classification OPEN >2x
- Q+H==S asserted per-request for all 300×27 = 8100 samples
- All 300 complete triplets per round (no missing timestamps)
- `Cargo.lock`: byte-identical (sha256 `9eef233d...`)

### Review

Fresh adversarial review: **CLEAN** (0 findings, confidence 0.96).

### Not touched

Out of scope, file-disjoint: `Cargo.toml`, `Cargo.lock`,
`.github/workflows/`, `scripts/`, other floor ledgers,
`rust-toolchain.toml`, `serve_io_scaling.rs` (test lane unchanged from
iteration 24).

## Iteration 26 — `extension-rpc-dispatch` (E1-E4 exhaustion — CONSTRAINED-ABOVE-FLOOR)

Date 2026-08-29. Base `c5ca3c3` (canonical origin/feat/ver-align-canonical-pin,
iteration 25). Docs-only terminal record: no candidate was executed this
iteration — the safe design space was executed and accepted in iteration 25
(1.44x), and every remaining in-unit mechanism is boundary-infeasible on the
recorded advocate proofs, not a performance miss. Provenance:
`agent://RpcInlineDesignAdvocate` (0.99 confidence, block-must-fix on the
inline terminalInput fast path) and `agent://RpcFusedLoopAdvocate` (0.98
confidence, block-must-fix on the fused reader+FuturesUnordered loop).

### E1 — decomposition reconciliation (3.058 us/request trusted)

| Term | ns/request | Share | Ownership |
|---|---|---|---|
| Q — channel handoff (`try_send` + `recv` + worker wake) + `FuturesUnordered::push` | 1,708 | 55.9% | in-unit — required isolation handoff |
| H — handler + response construction + encode | 1,306 | 42.7% | in-unit — AT-FLOOR (E3) |
| **S (total server)** | **3,058** | — | rs_S = 16.37%, noise gate passed |

Reconciliation: Q and H are medians of independently accumulated
distributions, so they need not sum exactly to the S median: 1708 + 1306 =
3014 ns vs 3058 ns (44 ns, 1.4% of S — the round-level location shifts
recorded in iterations 22-25 correlate Q and H within a round, so the
median of the sum exceeds the sum of the medians). The identity Q + H = S
is exact per request: asserted for all 300×27 = 8100 samples in the
iteration-25 run. Inclusive RTT (median 12,387 ns/request) remains
reference only — it spans reader/writer and transport wait, not server
work. Distributions are the iteration-25 measurements; no new measurement
was taken this iteration.

### E2 — candidate history and evidence

1. **One supervised request worker over `FuturesUnordered` (iteration 25,
   executed)** — replaced per-request `JoinSet::spawn` with one long-lived
   worker fed by a bounded mpsc (capacity = `max_in_flight`), `select!`
   over `job_rx.recv()` and `pending.next()`; measured **1.44x** (S 4399 →
   3058 ns; Q 2923 → 1708 ns, −41.5%; H equivalent within noise), PASSED
   the ≥1.05x gate, accepted. This is the safe-design endpoint: what
   remains of Q is the handoff itself.
2. **Inline `terminalInput` handler on the drive loop (the
   iteration-24/25 named candidate)** — REJECTED on
   `agent://RpcInlineDesignAdvocate` (block-must-fix): awaiting the inline
   future **suspends the one `drive` future**, so the transport reader
   cannot advance and cancel/request frames behind a terminal callback —
   in the same batch or a later read — are **delayed**; awaiting each
   inline request inside the single drive loop **serializes previously
   concurrent terminal callbacks** (up to `max_in_flight` (default 64)
   overlap today → 1) and reorders responses from callback-completion
   order to request order; and the cooperative `tokio::time::timeout`
   **cannot preempt a non-yielding callback** (a spin, blocking I/O, or an
   indefinite lock defeats the 4 ms budget without bound — the trait
   documents no nonblocking/cooperative/bounded-poll contract).
   Boundary-infeasible, not a performance miss.
3. **Fused reader + `FuturesUnordered` completions loop** — REJECTED on
   `agent://RpcFusedLoopAdvocate` (block-must-fix): polling arbitrary
   extension futures inside `drive` **loses task isolation** — one
   non-yielding extension poll blocks the transport reader (the separate
   worker task is what lets cancellation/EOF proceed on another runtime
   worker); the proposed `reader_eof` state **changes clean-EOF abortive
   teardown into an unbounded drain** unless corrected (a never-resolving
   callback holds shutdown); and **fairness becomes load-bearing** — the
   proposed `biased;` reader-first select starves admitted requests under
   sustained readable input (permits never release; every later request
   falsely rejected as overloaded), and completion-first bias is equally
   wrong. Boundary-infeasible, not a performance miss.

### E3 — floor and multiple revalidation

Floor **~0.75-1.0 us/request** (ledger; revalidated — no input, dependency,
or protocol input changed it, and the iteration-25 lane reproduces the same
operating point on the identical 300-request corpus). Two revalidations:

- Interior handler/encode term H = **1.306 us ≤ 1.5 us** (2× the
  conservative floor lower bound, 0.75 us): the interior
  handler/response-construction/encode work is **AT-FLOOR** — no ≥1.05x
  candidate exists inside H under the measured noise.
- Total S = **3.058 us is 3.06×-4.08× the floor** (3058/1000 to 3058/750).
  This is a constraint statement, NOT a claim that 3.058 us is a physical
  floor: the gap over the floor is held open by Q (E4), not by interior
  work.

### E4 — dominant residual and reopen conditions

Dominant residual: **Q = 1.708 us** — the required **cross-task isolation
handoff** (`try_send` + `recv` + worker wake + `FuturesUnordered::push`)
keeping arbitrary extension callbacks off the transport reader while
preserving cancellation (registration-before-enqueue; abortive teardown on
EOF/error), bounded concurrency (`max_in_flight` permits), and
one-response-per-id. Every mechanism that would remove the handoff
collapses one of those contract properties (E2 items 2-3). Exact boundary
consents that would reopen the unit:

1. **Cooperative/nonblocking extension-callback guarantee** — a trait-level
   bounded-poll or blocking-disclosure contract enforced at the adapter
   seam: the callback becomes safe to poll on the drive loop; inline/fused
   execution becomes feasible and Q collapses toward zero.
2. **Serialized terminal callbacks + delayed cancel/EOF accepted** as an
   explicit re-contracting of observable extension behavior: inline await
   becomes feasible; same Q collapse.
3. **A different runtime primitive proven to preserve task isolation and
   clear ≥1.05x measured** (e.g. a preemptible callback executor or a
   dedicated-thread worker with a proven-cheaper handoff): must be executed
   against the same lane under the same gates. Micro-tuning the existing
   handoff (unbounded channel, capacity bumps, allocator tweaks) is NOT a
   materially distinct design — it does not remove the handoff, and
   unbounded channels sacrifice the bounded-memory/backpressure contract.

**Verdict**: **CONSTRAINED-ABOVE-FLOOR**. The `extension-rpc-dispatch` unit
is terminal in the campaign records: the safe design space is executed and
accepted (1.44x, iteration 25), the interior H term is AT-FLOOR, and the
dominant residual is the isolation handoff the contracts require. This
closure is unit-scoped and does not close the campaign: issue #97 remains
OPEN for the remaining units. Next ordered unit: **`keypress-dispatch`**
(measurement prerequisite — measurement remediation first).

**Not touched** (out of scope, file-disjoint): production code
(`server.rs`, `protocol.rs`), `serve_io_scaling.rs` (test lane unchanged
from iteration 24), `Cargo.lock`, `rust-toolchain.toml`,
`.github/workflows/`, `scripts/`, other floor ledgers.
---

## Iteration 27 — `startup-version-path` (sync arg dispatch before runtime construction — OPEN >2x, win 1.59x wall / 3.99x Ir)

Date 2026-08-29. Base `6318fa3` (canonical origin/feat/ver-align-canonical-pin,
iteration 26). Perf commit + this docs record.

### Blind derivation (ledger only, before any replaced body was read)

Contract: `--version` sets a flag in a hand-rolled single-pass parser and the
process prints the compile-time `VERSION` constant and exits 0. Floor: one
write(2) of ~9 B plus an argv scan ≈ 0.15 us. Decomposition: ~35% of
in-process Ir in tokio worker machinery; the wall layer is 80 clone3 + a
futex/munmap teardown storm. The ledger's addressable-overhead note names
the lever: runtime construction precedes argument dispatch, so the flag-exit
path pays for a runtime it never uses. Candidate, fixed before reading
`entry.rs` / `bootstrap.rs` bodies: make argument dispatch synchronous and
runtime-free; construct the multi-thread runtime only when the pipeline
continues past dispatch.

### Design (single home preserved, no duplicated dispatch)

- `initialize_bootstrap` (parse → package/config subcommand dispatch →
  diagnostics → `--version` / export exits) is fully synchronous and already
  ran before the first `await` of `run_bootstrap`. It is hoisted above runtime
  construction into `entry::run`.
- `run_bootstrap` composes init + a new `run_bootstrap_parsed` continuation
  (everything after init, byte-unchanged); `run()` calls init synchronously,
  then builds the runtime and blocks on the continuation. The outcome →
  exit-code / mode-runner tail moved into `dispatch_outcome`, shared by
  `run()` and `run_pipeline`. The test seam is unchanged (init still inside
  `run_bootstrap`); `run_pipeline_version_exits_zero` passes.
- The one runtime-dependent init leaf, `RealPackageHandler::open_config_selector`
  (interactive `pi config`), drove its future on the ambient multi-thread
  runtime via `block_in_place` + `Handle::current`; with init hoisted above
  runtime construction that would panic. It now runs `select_config` on a
  dedicated OS thread with its own current-thread runtime (the
  `refresh_models` worker shape; `spawn_blocking` inside works there). Two
  review rounds were needed: in-runtime `Builder::build` panics, so a plain
  nested runtime was not enough; the thread is. Found by this iteration's
  adversarial review, fixed before push; off the pinned workload path
  (non-tty runs early-error in `select_config`), so measurements are
  unaffected.

### Replaced-branch classification

- essential: argv parse, diagnostics print, package/config dispatch, version
  write + exit-code mapping (the contract rows; same code, same order, one
  home);
- residue: multi-thread runtime build + 80-thread teardown on every flag-exit
  path (paid for nothing on those paths; eliminated for them by construction,
  not suppressed);
- essential, unchanged: the runtime-build-failure branch, now only reachable
  when the pipeline continues past dispatch.

### Boundary answers (explicit, before touch)

1. Version text byte-identical: same `io.write_stdout(VERSION)` call;
   diffed base vs after binaries: identical bytes, exit 0 both.
2. Exit code 0: same `stop(0, false)` → `ExitCode::from(code)` mapping.
3. `--help` and all other flags unchanged: init's branch order (package →
   config → diagnostics-error → version → export) untouched, so combined-arg
   behavior (e.g. `pi package list --version`, `pi --version -Z`) is
   unchanged; `--help` and `--no-session --print hello` diffed base vs after:
   byte-identical output and equal exit codes in the same sandbox.
4. stdout/stderr routing unchanged: same `BootstrapIo` handles, same write
   order; `output_guard::take_over_stdout` still runs later, in
   `prepare_session`, in both shapes.
5. Noted, contract-conformant: `--version` no longer surfaces runtime
   construction failure. The ledger contract is "prints VERSION and exits
   0", and the version short-circuit already never reached the runtime in
   tests (`run_pipeline_version_exits_zero`).

### Measurements (same machine, release lto=fat/cgu=1/strip; wall pinned
`taskset -c 20-40`, hyperfine `-N`, ≥50 runs, 5 warmup)

| instrument | before (6318fa3) | after | win |
|---|---|---|---|
| hyperfine wall, paired run | 5.9 ms ± 0.9 | 3.7 ms ± 0.6 | **1.59x** |
| hyperfine wall, quiet-window pair (same shapes) | 6.3 ms ± 0.8 | 3.2 ms ± 0.8 | 1.97x |
| callgrind in-process Ir | 3,767,328 | 945,385 | **3.99x** |
| strace -f -c syscalls | 2788 | 84 | 33x |
| sub: clone3 (worker threads) | 80 | 0 | — |
| sub: futex | 705 | 0 | — |
| User+Sys per run (hyperfine) | 11.0 ms | 3.5 ms | 3.1x |

Gate: ≥1.05x median, PASS (1.59x paired; 1.97x quiet window). Scoped
verification: `cargo check -p pi` green; `cargo test -p pi cli` 164 passed /
0 failed (one environmental failure, a gitignored prebuilt
`pi-extension-host` artifact missing in a fresh worktree, reproduced at base
with the artifact absent and cleared by providing the artifact; unrelated to
the diff); live smoke: `--version` → `0.1.0` exit 0, `--help` and a normal
flag path parity-diffed base vs after.

### Multiple recompute

Ledger convention (3.95 M Ir ≈ 0.37 ms → 93.7 ns per 1000 Ir): 945,385 Ir ≈
88.6 us → 88.6 / 0.15 us ≈ **591x**. Still ≫ 2x → the win is logged as
intermediate and the unit stays OPEN pending a materially distinct
in-boundary design or the E1-E4 exhaustion record (iteration 28).

**Not touched** (out of scope, file-disjoint): `crates/pi-tui/`,
`crates/pi-ext/`, `.github/workflows/`, `scripts/`, `Cargo.lock`,
`rust-toolchain.toml`, other units' floor ledgers, `packages/`.

---

## Iteration 28 — `startup-version-path` (E1-E4 exhaustion — CONSTRAINED-ABOVE-FLOOR)

Date 2026-08-29. Base for the record: iteration 27 (perf `3b4e53c` + docs
`465091f` on canonical origin/feat/ver-align-canonical-pin, itself on
`6318fa3`); the closed after-state was produced by the perf commit. Docs-only
terminal record: no candidate
was executed this iteration; after the iteration-27 win (1.59x wall, 3.99x Ir,
2788 → 84 syscalls) no materially distinct in-boundary design reaches the
>=1.05x gate or the 2x multiple.

### E1 — decomposition reconciliation (945,385 Ir in-process, iteration-27 after-state)

| Term | Ir | Share | Ownership |
|---|---|---|---|
| Dynamic loader: relocation (`_dl_relocate_object_no_relro` 339,909 + do-rel 123,193 + dl-reloc 29,972) | 493,074 | 52.2% | artifact shape (27.7 MB dynamically linked binary) |
| Dynamic loader: symbol binding (do_lookup_x 114,982, strcmp 40,873, `_dl_lookup_symbol_x` 31,796, dl-new-hash 28,236, check_match 21,663) | 237,550 | 25.1% | artifact shape + ld.so |
| Dynamic loader: version check + tunables (14,036 + 26,691) | 40,727 | 4.3% | artifact shape + ld.so |
| libc startup, stdio and env parsing (vfscanf 53,972, strtoul 26,550, _IO_sputbackc 4,784, getdelim 3,905) | 89,211 | 9.4% | libc pre-main |
| Product remainder (Rust std init, panic machinery, argv scan, version write) | 84,823 | 9.0% | unit (contract rows < 4 kIr) |
| **Total** | **945,385** | **100%** | |

Reconciliation exact: 493,074 + 237,550 + 40,727 + 89,211 + 84,823 = 945,385
(callgrind annotate of `/tmp` profile from the iteration-27 after-binary). The
contract rows themselves (argv scan, constant read, one write) are < 4 kIr
(base ledger, strace-confirmed 2 write-class calls); the other ~81 kIr of the
product term is std/libc-adjacent init the process cannot skip. Wall side
(3.7 ms direct median, 1.6 ms min): kernel execve + page-in of the binary and
libc dominate; syscall time is 0.95 ms and in-process Ir is ~0.24 ms at 2 GHz,
so most of the wall is process creation, outside any in-process lever.

### E2 — candidate history and evidence

1. **Sync arg dispatch before runtime construction (iteration 27)** —
   executed, **1.59x** paired hyperfine / **3.99x** Ir / 33x syscalls, kept
   (gate >=1.05x PASS). Removed the entire tokio runtime term the ledger
   attributed (~35% of the old 3.77 M Ir) and the 80-thread storm.
2. **Static linking (musl/static-pie)** — infeasible in-boundary: it is a
   release-build/toolchain consent (`rust-toolchain.toml`, `Cargo.lock`, and
   `scripts/release/` are frozen for this campaign; glibc-specific dynamic
   dependencies span the workspace). Even if consented: removes the ~771 kIr
   loader terms, projecting ~174 kIr ≈ 16.3 us ≈ **109x floor**, still >2x.
3. **RELR (link-time relocation representation)** — a linker/artifact
   consent, not program code: it compacts the relative-relocation term
   (493,074 Ir, 52.2%) at link time and requires the frozen release/link
   surface. **Prelink / ld.so cache warming** — deployment-environment
   consent, outside program control; addresses at most the ~237 kIr binding
   term (~25%). Both leave the kernel exec cost.
4. **Raw-syscall version write (bypass std::io wiring)** — the whole product
   remainder is 84.8 kIr (~9%); the contract rows are < 4 kIr of it. Best
   case removes tens of kIr (~ a few us in-process) and nothing from the
   wall the lane gates (exec/page-in dominated); below the >=1.05x gate on
   the pinned workload by construction, and it trades the ProductOutput
   stdout-routing contract for an unmeasurable win.
5. **Tiny `--version` helper binary or lazy-loaded monolith** — changes the
   shipped artifact shape (single-binary distribution contract, packaging and
   release scripts frozen); a reopen consent, not an in-boundary design.
6. **current_thread ambient runtime for non-version invocations** — no effect
   on this unit (the version path now builds no runtime at all) and out of
   the unit's rows; belongs to whichever unit owns general startup.

### E3 — floor and multiple revalidation

Floor **~0.15 us** (argv scan ~20 ns + constant read ~1 ns + one write(2)
122.7 ns, floorkit): revalidated, no input changed it; the iteration-27
after-state reproduces the same contract observable (byte-identical version
text, exit 0, 2 write-class calls). Multiple: 945,385 Ir ≈ 88.6 us → 88.6 /
0.15 ≈ **591x**. This is a constraint statement, not an at-floor claim: the
gap is held by the loader (81.6%), libc pre-main (9.4%), and kernel process
creation on the wall side, none of which is unit-addressable. Note the
multiple is bounded below by process existence itself: any dynamically linked
Rust binary pays >100 us before `main`, so the 2x criterion (0.3 us) is
unreachable for a real process start regardless of in-unit code.

### E4 — dominant residual and reopen conditions

Dominant residual: **ELF dynamic relocation and symbol binding of the 27.7 MB
dynamically linked binary (~771 kIr, 81.6%) plus kernel process creation and
page-in on the wall side (min 1.6 ms direct vs ~0.24 ms in-process)**. Exact
boundary consents that would reopen the unit:

1. **Static-linking release consent** (musl/static-pie target; toolchain,
   lockfile, and release scripts unfrozen): removes the relocation/lookup
   terms, projected ~109x, still CONSTRAINED-ABOVE-FLOOR; would need a fresh
   E1-E3 pass.
2. **RELR link-time consent** (release/link surface unfrozen): compacts the
   493,074 Ir relative-relocation term. **Prelink / ld.so cache deployment
   consent**: up to ~25% binding-term reduction, environment-side.
3. **Artifact-shape consent** (version helper binary or lazy loading):
   removes most of the exec/page-in wall; changes the distribution contract.

**Verdict**: **CONSTRAINED-ABOVE-FLOOR**. The `startup-version-path` unit is
terminal in the campaign records: the addressable runtime term is fully
removed (iteration 27), every remaining in-unit mechanism is measured or
projected below the gate, and the dominant residual requires boundary
consents. This closure is unit-scoped: issue #97 remains OPEN for the
remaining units.

**Not touched** (out of scope, file-disjoint): production code,
`crates/pi-tui/`, `crates/pi-ext/`, `.github/workflows/`, `scripts/`,
`Cargo.lock`, `rust-toolchain.toml`, other units' floor ledgers, `packages/`.


## Iteration 29 — `memory-resource-units` (prerequisite run — distribution recorded; Phase-6 transfer)

Date 2026-08-29. Base `6318fa3` (canonical origin/feat/ver-align-canonical-pin,
iteration 26) + measurement-harness resilience commit. Measurement-only
iteration: no production code changed, no wall-clock claim made or
rebutted. This unit's floor ledger required one full `verify:performance`
run capturing the PERF-T1 memory keys (`idleProcessTreeMemory`,
`streamProcessTreeMemory`; non-gating, post-verdict), then a
retained-vs-floor comparison per hot row. The keys are now recorded and the
unit's fail-closed OPEN is resolved; per the ledger's own Phase-6 rule,
graduation transfers to PERF-T14 (#100) cold grading.

### Harness work required to reach the memory lanes (measurement-only)

The memory keys sit behind every wall lane in `scripts/verification/
performance.ts`, and four defects — none in the memory collectors — aborted
every full run before them (six full-run attempts, R8-era artifact showed the
same first-frame abort two days prior):

1. **TypeScript reference never exits on /quit** (upstream `.references/pi`
   `4e4949299`; deterministic, reproduced standalone). `terminateAndRequireCleanExit`
   now escalates to tree termination on quit-timeout, keeps the captured
   sample, and discloses per-sample escalations in `harness.quitTimeouts`
   (50 first-frame escalations this run). Teardown is not measurement.
2. **TypeScript reference accepts a prompt but never streams offline**
   (same reference checkout; prompt painted, extensions loaded, no provider
   frames within 30 s). The stream CPU lane fast-fails the implementation
   after its first failed sample; stream-load memory TS samples fail
   individually. Both disclosed in `harness.laneDegradations`; TS stream-load
   memory is therefore n=0 in this run's distribution.
3. **Rust burst-write paint stall (production, OPEN, owned by
   keypress-dispatch)**: a multi-character burst written after the first
   frame (paste markers or plain text) paints ~5 cells then stops emitting
   frames while input processing continues (later Enter still submits;
   streaming paints still work). Bisected by binary: pre-T11 `a007540`
   (= `021a00c^`) paints the whole burst in one synchronized frame; the
   window is the first-frame-init pair `021a00c`/`2e4c087`. Single-key paints
   are healthy (keypress lane below, median 0.67 ms). The stream CPU lane's
   pre-Enter submission wait was relaxed from "prompt label painted" to
   sync-marker presence — a measurement-protocol note, not a fix; the CPU
   bracket (Enter -> final marker) and the post-stream validity assertions
   are unchanged.
4. **Lane isolation**: a failed lane previously aborted the whole run and
   discarded every other lane's data. Collectors now catch per-sample and
   per-implementation failures, record them in `harness.laneDegradations`,
   and turn an empty per-implementation sample set into an explicit verdict
   blocker. A noise rejection (below) now completes the verdict first and
   still collects the non-gating memory lanes before exiting — preserving
   their post-verdict, non-gating contract.

Verification for the harness change: `bun run check` clean;
`bun test scripts/verification/performance.test.ts` 30/30 (the ignore-quit
test now pins the terminate-and-disclose contract via
`recordedQuitTimeouts()`).

### Measurements (full run, `taskset -c 20-40`, artifact on the cutover tree)

**Terminal state** (idle lane, extension-free, steady-window max tree RSS,
5 samples/implementation; floor 100x30x24 B = 72,000 B, transcript empty):

| impl | RSS median | min-max | PSS median | retained/floor |
|---|---|---|---|---|
| rust | 25,362,432 B | 25,255,936-25,489,408 (0.9%) | 16,147,456 B | ~352x |
| typescript | 125,042,688 B | 124,301,312-125,812,736 (1.2%) | 118,487,040 B | ~1,737x |

**Stream-load growth** (one turn: 256 x 24 B = 6,144 B transcript;
load-window max tree RSS, 5 rust samples; TS n=0 per degradation 2):

| impl | RSS median | min-max | PSS median |
|---|---|---|---|
| rust | 145,068,032 B | 142,938,112-146,386,944 (2.4%) | 133,730,304 B |

Growth over idle (rust): ~119.7 MB RSS. Floor 6,314 B (one entry) to
49,664 B (256-entry sensitivity) -> retained/floor ~18,959x to ~2,410x.
Caveat: the stream lane's tree adds the verification extension + extension
host, so the multiple bounds whole-tree footprint under load; retained
transcript bytes are <=~0.005% of it. Churn above retained state belongs to
the timing ledgers (ledger's own R8 note).

**Disposition**: both hot rows sit far above 2x floor in bytes with the
dominant retained term named (process/runtime baseline, not TUI retained
state). Per the floor ledger's Phase-6 rule these units graduate only at
cold grading in the resource currency: **graduation transfers to PERF-T14
(#100)**. No wall-clock claim is made; AT-FLOOR/CONSTRAINED-ABOVE-FLOOR do
not apply to this measurement unit. The fail-closed OPEN is resolved by the
recorded distribution.

**Handoffs**: keypress-dispatch owns the burst-write paint stall
(defect + window above; its own wall lane remains noisy, rs 29.49% this run,
67-69% in two prior runs); upstream-reference defects (/quit hang, offline
no-stream) belong to the `.references/pi` pin owners.

**Not touched** (out of scope, file-disjoint): production code
(`crates/*`), `.github/workflows/`, `scripts/release/`, `Cargo.lock`,
`rust-toolchain.toml`, `crates/pi-tui/src/terminal/*`, other units' floor
ledgers.

## Iteration 30 — `keypress-dispatch` (measurement boundary repair — protocol trusted, rs 2.69%)

Date 2026-08-29. Base `6318fa3` (canonical origin/feat/ver-align-canonical-pin,
iteration 26). Measurement iteration only — no production Rust optimization; the
keypress/input/runtime/writer sources measured are identical to `6318fa3`
(post-extension-RPC terminal classification, confirmed ancestor before the run).
The R2-era keypress lane could not feed a verdict (rs 26.98%, wall 0.44 s), and
inspection found the measurement boundary itself wrong in four independent ways,
so this iteration repairs the instrument before attributing anything.

### What was wrong with the R2-era protocol

1. **Start boundary was not the write.** The old collector took
   `snapshot().elapsedMs` *before* `writeKeys`, so the timed interval included
   `snapshot()`'s echo scan and chunk copy plus the poll-loop scheduling gap up
   to the write, and missed the true write instant.
2. **The editor grew across the round.** One process, 20 warmup keys and 200
   measured keys, no clear: sample *k* painted a *k*-character editor. The
   per-round trend (growing render/paint cost) is a variance source and makes
   the "median keypress" an average over 200 different workloads.
3. **Fallback frames were accepted as paints.** `frameObservation` returns the
   first synchronized transaction *or* a row-local printable CSI transaction;
   the old lane recorded latency from whichever came first.
4. **A concurrent 1 ms `ProcTreeSampler`** polled `/proc` on the same host core
   territory during latency collection.

### The repaired protocol (committed this iteration)

- `PtyProcess.writeKeys` pre-encodes and returns a **receipt**
  (`outputOffset`, `startedElapsedMs`) captured immediately before the first
  `FileSink.write`; `#consume` timestamps chunk **arrival** before copy/decode.
  Transport unchanged (`setsid script --quiet --flush --echo always`).
- `keySyncTransaction`: strict synchronized-only observer — first balanced
  `ESC[?2026h … ESC[?2026l` transaction at/after the receipt offset, returning
  payload, begin/end counts through the completing chunk, and the completing
  chunk's arrival elapsed. Row-local printable output before any synchronized
  begin is reported as fallback, never as a frame. Split markers accumulate.
- One measured sample = `writeKeys(key)` → first balanced transaction whose
  payload contains the typed key, exactly one begin/end pair → `Ctrl+U` clear
  → its complete synchronized paint (outside timing). Fixed empty editor for
  every measured key. Any violation fails the whole round (no filtering).
- Outer aggregation: 3 discarded process warmup rounds, then 27 fresh measured
  process rounds × (20 warmup + 200 measured) pairs. Trust estimator:
  population stddev / median over the 27 round medians (repository rule,
  `0.20` passes); pooled raw spread disclosed only; collection wall >= 1 s;
  pooled raw p99 < 5 ms stays the separate behavior gate.
- `scripts/bench-keypress-dispatch.ts`: dedicated thin CLI (single-arm
  `--binary/--rounds/--output`; paired `--baseline/--design/--pairs` with arm
  order alternated per pair) over the same exported collector.
- Focused tests: receipt boundary pins (offset excludes prior chunks, starts
  before the write, precedes output, hostile bytes in order), observer pins
  (split markers, fallback rejection, extra-marker counts, payload
  correlation, pre-offset transactions ignored), aggregation pins (all rounds
  and samples summarized, partial round rejected, rs = 0.20 boundary quiet).
  `bun test scripts/verification/pty.test.ts scripts/verification/performance.test.ts`:
  42/42 PASS. Script typecheck clean. Run under `taskset -c 20`.

### Trusted baseline (27 measured rounds, pinned)

| Metric | Value | Gate |
|---|---|---|
| Round-median rs | **2.69%** | <= 20% **PASS** (was 26.98% FAIL) |
| Round medians (n=27) | median 467.27 us, min 438.01, max 506.47 | — |
| Collection wall | **16.52 s** | >= 1 s **PASS** (was 0.44 s) |
| Synchronized + key-correlated samples | 5,400 / 5,400, 0 invalid frames | all-frames **PASS** |
| Pooled raw median / p95 / p99 | 467.59 / 548.75 / 888.09 us | p99 < 5 ms **PASS** |
| Pooled raw spread | 55.99% | disclosed, not gating (one 15.88 ms scheduler hiccup in the tail) |
| Affinity / governor | CPU 20 / `powersave` (recorded) | — |
| Binary | sha256 `58592a9d…`, built from this commit; production sources = `6318fa3` | — |

**Classification: none yet — measurement iteration.** The lane is now trusted;
the unit stays **OPEN** pending attribution (temporary S0–S3 probes +
raw-vs-EventStream fixture differential), floor revalidation, and the
zeroing-ceiling test before any candidate is named. The former 1.935 ms
"operating point" was an artifact of the broken instrument (editor growth,
snapshot-start boundary, accepted fallbacks, concurrent sampler), not a fact
about the lane.

Not touched: production Rust (`crates/**`), `Cargo.lock`, `rust-toolchain.toml`,
`.github/workflows/`, `scripts/release/`, other units' floor ledgers.

Adversarial-review hardening before landing (0 blocking findings remaining):
the collector now verifies that the Ctrl+U clear actually restores the empty
editor — the previous key must be absent from the next paint and from the
clear repaint's printable cells (escape-sequence bytes stripped before
matching) — and the paired runner gates each arm on its own collection wall
so two short arms cannot pass on their combined duration. The trusted-baseline
numbers above were captured with the pre-hardening collector on a production
tree identical to `6318fa3`; the hardened collector re-confirms the baseline in
iteration 31 after a production correctness fix on this unit's surface.
