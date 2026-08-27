# Floor ledger: terminal diff/encode/write (paint)

Owning R2 hot rows: *Terminal diff/encode/write (paint)* (lanes 3, 4), *Terminal
diff/encode* + *NullTerminal write* (lane 7), *First terminal paint (synchronized
output)* (lane 2). State: **OPEN**, ~7.5x paint-only floor (~41x on the recompose+paint block).

## Contract (from call sites, tests, signatures — never internals)

- `Tui::commit` -> `commit_frame` (crates/pi-tui/src/terminal/writer.rs:283-343): per committed frame, the caller is owed a correct cell diff encoded as terminal bytes with cursor positioning.
- Synchronized-output framing: every emitted frame is wrapped `ESC[?2026h ... ESC[?2026l` (backend.rs:275-284); sync-begin/end balance is test-pinned (crates/pi-tui/tests/pty_no_flicker.rs:236-290); probe-before-sync pinned at pty_no_flicker.rs:54-66.
- `stage3_write` performs `write_all` + `flush` per frame (writer.rs:522 via write_stage3_frame :550-568) — one complete write transaction per frame is the observable contract the harness detects (performance.ts frameObservation 1010-1044 keys on the SYNC_END-terminated chunk).
- Consumers: interactive repaint (runtime.rs:4548), stream partial paints (coalescer <=16 ms, runtime.rs:104 BACKGROUND_COALESCE_WINDOW, armed :2193), keypress immediate repaints (runtime.rs:2148, :2174).

Boundary classification: the escape-byte stream and the DEC 2026 framing are
**boundary** (terminal wire format; external terminals consume them). The diff/encode
machinery is **interior**. Unresolved channels: named — external terminal emulators
consume the wire; byte-level stability is enforced by the transcript fixtures rather
than by enumerating emulators.

## Floor (computed)

Per painted frame the contract forces: encode the changed-cell bytes (~39 B measured in
the editor churn scenario) + the ~20 B sync wrapper, and one `write(2)` (+flush) of the
frame transaction.

```
encode ~39 B changed-cell payload      ~40 ns   (format-class build, floorkit 42.8 ns for 170 B)
sync wrapper bytes                     ~10 ns
one write(2) to the tty (pipe proxy)  598 ns   (floorkit pipe write 170 B, reader draining)
                                     ---------
floor                                ~0.64 us/painted frame
```

The full-buffer diff scan is not floor work: the wire owes only changed cells, and
damage information is available to the producer of the mutation (see
render-churn-recomposition.md).

## Measured cost

Stream workload (lane 3, trusted R2 baseline 1.133 ms CPU/frame process-tree): paint
share derived by subtraction — lane-7 editor frame (recompose+diff+encode) is 212 us;
the stream coalescer paints at most one frame per 16 ms window over a 512 ms turn
(<=32 paints per 256 provider frames, `paintedSynchronizedFrames` recorded by the
artifact), so amortized paint+recompose ~= 212 us x 32 / 256 = 26.5 us/frame, of which
the paint (diff+encode+write) share per lane-7 callgrind is ~18% of the churn frame.

Amortized paint-only ~= 0.18 x 26.5 us + per-frame write share ~= 4.8-5.5 us/frame:

**Multiple (paint-only, amortized) = 4.8 / 0.64 ~= 7.5x => OPEN.**

The amortized recompose+paint block it sits in is 26.5 / 0.64 ~= 41x its write floor
(the block's own dominant term is recomposition, owned and decomposed in
render-churn-recomposition.md).

## Cost decomposition (per painted frame, from the lane-7 callgrind shares)

| Category | Cost (per painted frame) | Method |
|---|---|---|
| ratatui BufferDiff + Cell::eq full-buffer scan (3000 cells) | 29.5 us | profiler attribution (lane-7 callgrind, 13.9%) |
| encode + set_symbol + ANSI assembly | 13.8 us | profiler attribution (3.9% + 2.6%) |
| write(2) + flush of the frame transaction | ~0.7 us | floorkit pipe-write constant + syscall census |
| recomposition feeding the paint (separate ledger) | (booked in render-churn-recomposition.md) | — |

## Addressable-overhead notes for Phase 5

Damage-scoped diff (skip the 3000-cell equality scan), direct region encoding. The
write syscall itself is at floor. Boundary: byte-identical wire output including sync
balance (pty_no_flicker pins).
