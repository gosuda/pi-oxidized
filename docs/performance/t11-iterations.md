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
