# Gate-sweep lessons (earned closing G21-G31, G4, G8, G30 at ba07a04)

Each rule below cost a real defect or a near-miss. Instances named so the
next sweep can check itself.

1. Never conclude from a truncated pipeline. `grep ... | head -8` hid every
   production `unbounded_channel` behind test-harness hits and produced a
   false "bounded-only" claim (G25). Census first without a head limit,
   classify every hit, then write the invariant to match the inventory —
   not the reverse.

2. Triage every advisory against the current tree before acting. Four of
   this round's advisories described pre-fix states (builtin `--check`
   missing, cargo two-filter error, worktree-confounded reference test).
   Re-running the cited command takes seconds; debating the text takes
   longer and risks fixing what is already fixed.

3. Temporal brackets beat relocated experiments for path-coupled tests.
   A worktree run cannot reproduce reference-coupled results because the
   reference resolves by path. Commit timestamps plus the empty
   `git log b90ab44..HEAD -- <classifier paths>` proved the E-vs-S flip
   window contained no classifier change — decisive without moving trees.

4. `git status` clean proves nothing about ignored files. The E2 drift
   lived in a gitignored build output plus install state. Instruments for
   ignored state are mtime and digests, not the index. Conversely, never
   mutate authority state to satisfy a fixture; scope the check to
   tracked files or define a re-capture protocol.

5. One durable number gets one base, one command, one regex. Two pub-item
   deltas (+27/+25 on different bases) sat in the transcript waiting to
   become stale evidence. The ledger now cites exactly the track range
   `a019604..13b9ac6` with the grep invocation inline.

6. A completed checkbox is a claim; re-verify at HEAD before trusting it.
   G4, G8, and G30 were all marked done against states the tree had left
   behind. The reopen→prove→close loop (G4: 5/5 battery green) is cheaper
   than defending a stale mark.

7. Split data convergence from mechanism even when the proof couples
   them. `e76827f` bundled check-modes with the catalog regen their
   fresh-proof required. Data-first stacking (`de2f47b` then `ba07a04`)
   keeps every intermediate commit green and tells the story honestly.

8. Scout-map, owner-verify. Delegated maps are fast and wrong at the
   margins (G24's phantom gaps, G21's miscounted census). Use scouts for
   the census, then personally re-check each load-bearing site before
   closing the gate.
