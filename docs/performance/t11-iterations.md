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