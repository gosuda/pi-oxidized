# Floor ledger: startup fast path (arg parse, version lookup, output write)

Owning R2 hot rows (lane 1): *CLI argument parsing*, *Version lookup*, *One output
write + clean exit*. State: **OPEN**, ~2480x on in-process CPU (de-minimis absolute).

## Contract (from call sites, tests, signatures — never internals)

- Argument parsing is a hand-rolled single-pass parser (crates/pi/src/cli/args.rs:225-279 (Args struct at :148)); `--version` sets `flags.version` (args.rs:278).
- `initialize_bootstrap` prints `VERSION` and exits 0 (crates/pi/src/cli/bootstrap.rs:484-487); `VERSION = env!(CARGO_PKG_VERSION)` (config.rs:23) — a compile-time constant, so *version lookup* is a constant read.
- The lane harness spawns one fresh PTY per sample and measures spawn-to-exit wall (performance.ts runVersionSample :1283-1317); the exit-0 + version-text observable is the contract (D3).

Boundary classification: the version string and the flag surface are **boundary**
(CLI contract; help snapshots pin it). The parser internals are **interior**.
Unresolved channels: none.

## Floor (computed)

```
scan ~2 argv tokens            ~20 ns
version constant read           ~1 ns
one write(2) of ~9 B + exit    122.7 ns   (floorkit write syscall)
                              ---------
floor                         ~0.15 us
```

## Measured cost

Trusted lane baseline (R2): 40.07 ms cold / 40.93 ms warm (PTY-spawned). Fresh direct
hyperfine (no PTY): 15.1 ms mean, User 29.6 / Sys 29.0 ms (multithreaded). Callgrind
in-process CPU (syscalls excluded): **3.95 M Ir ~= 0.37 ms**. Syscall census: **2814
syscalls — 80 clone3 (one thread per core), 735 futex, 188 munmap, 427
rt_sigprocmask**; the minimal write+exit binary executes ~65 syscalls and 1.9 ms
direct-exec wall.

**Multiple (in-process CPU vs floor) = ~372 us / 0.15 us ~= 2480x => OPEN** (absolute
cost small; ranked 8/11 by time share).

## Cost decomposition

| Category | Cost | Method |
|---|---|---|
| ELF dynamic relocation + symbol lookup (loader) | ~54 us | profiler attribution (callgrind: _dl_relocate/do-rel/dl-lookup = 14.4% of 3.95 M Ir) |
| tokio multi-thread runtime construction + teardown (worker machinery across 80 threads) | ~132 us | profiler attribution (~35% of Ir in tokio worker::run frames) |
| allocator init + early malloc/free | ~26 us | profiler attribution (6.9% of Ir) |
| libc/pre-main/init + misc remainder | ~160 us | subtraction (closes the in-process sum) |
| the contract hot rows themselves (argv scan + version + write) | < 4 us | subtraction + strace (exactly 5 write-class calls observed) |
| wall layer: 2814 syscalls incl. 80 thread spawns + futex/munmap teardown storm; PTY spawn overhead 24.97 ms (40.07 lane minus 15.1 direct) | ~14.7 ms direct-side wall | instrumented counters (strace census) + subtraction |

## Addressable-overhead notes for Phase 5

The fast-exit path pays for a runtime it never uses: deferring runtime construction
past argument dispatch (or lazily on first async need) removes the thread-spawn/futex
storm and most of the 15 ms direct wall. The hot rows themselves are effectively at
floor. Boundary: CLI observable (version text, exit 0) unchanged.

## Measured cost — iteration 27 (PERF-T11 #97, 2026-08-29)

Sync arg dispatch before runtime construction (perf commit on `6318fa3`,
iteration 27): argument dispatch (parse, package/config subcommands,
diagnostics, `--version`) now runs before any tokio machinery exists; the
multi-thread runtime is constructed only when the pipeline continues past
dispatch. Pinned instruments, same machine, release lto=fat: hyperfine wall
(paired run, `taskset -c 20-40`, `-N`, ≥50 runs) 5.9 ms ± 0.9 → 3.7 ms ± 0.6
(1.59x; quiet window 6.3 → 3.2 ms, 1.97x); callgrind in-process Ir
3,767,328 → 945,385 (3.99x); strace census 2788 → 84 syscalls, clone3 80 → 0,
futex 705 → 0. Contract observable verified byte-identical (`--version` →
`0.1.0`, exit 0; `--help` and a normal flag path parity-diffed base vs after).

Multiple recompute (93.7 ns per 1000 Ir): 945,385 Ir ≈ 88.6 us → ≈ 591x the
0.15 us floor. **OPEN** (intermediate win logged; >2x).

Remaining decomposition (callgrind attribution, 945,385 Ir): dynamic loader
(relocation, symbol lookup, version check, tunables) ≈ 751 kIr ≈ 79%; libc
startup, stdio and env parsing ≈ 89 kIr ≈ 9%; product remainder (static
init, arg scan, version write) ≈ 106 kIr ≈ 11%.
