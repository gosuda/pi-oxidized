# REL-R3 — Windows ConPTY interaction witness (prototype record + verdict)

Stable ID `REL-R3`, issue metaphorics/pi-oxidized#115. This document records
the throwaway ConPTY witness harness and the portable-pty / ConPTY behavior
study behind its assertion design, and delivers the go/no-go for wiring the
windows-x64 Tier N release row (REL-T7, issue #114).

Authored 2026-08-27. **Evidence status: source-derived.** No windows-latest
runner was available to this prototype; nothing below claims an executed
windows-latest transcript. Native execution of the harness is explicitly
deferred to REL-T7, which wires the CI leg or escalates. Every behavior claim
cites its primary source; the two executed claims (Linux compile checks and
unit tests of the harness itself) are marked *executed*.

---

## 1. Deliverable map

| Piece | Path | Status |
| --- | --- | --- |
| Throwaway harness (recipe + source) | `prototype/rel-r3-conpty/` | `cargo check` (host) and `cargo check --target x86_64-pc-windows-msvc` clean; 2 unit tests pass *(executed, Linux)* |
| Windows run recipe | §2 below + `prototype/rel-r3-conpty/src/main.rs` header | pending windows-latest (REL-T7) |
| ConPTY behavior study | §3 | source-derived, citations inline |
| Assertion map (hard vs advisory) | §4 | design contract for REL-T7 |
| Go/no-go verdict | §5 | provisional GO, conditions in §5.2 |
| No-go escalation path | §5.3 | quoted verbatim from #114 |

The harness crate is standalone on purpose: its `Cargo.toml` carries an empty
`[workspace]` table, so it is outside the repository workspace
(`cargo metadata --no-deps` at the root lists zero `rel-r3` packages), touches
no `crates/**`, `scripts/**`, or workflow files, and is not wired into CI.
This commit surface is docs + throwaway prototype only.

## 2. Windows run recipe (for REL-T7 wiring)

```text
1. Build + unpack the x86_64-pc-windows-msvc release archive (the same
   unpack+digest discipline REL-R1 §3 applies to its smoke commands).
2. cargo run --release --manifest-path prototype/rel-r3-conpty/Cargo.toml -- \
     --pi <unpacked-dir>/pi.exe --out rel-r3-evidence \
     [--expect-ready <substr>]        # pins the archive's ready marker
3. Require process exit code 0.
4. Upload the --out directory:
     rel-r3-transcript.jsonl   one JSON object per line:
                               {"seq","t_ms","kind","fields":{...}}
     rel-r3-raw-output.bin     full raw master-side ConPTY byte stream
```

Exit codes: `0` every hard assertion passed; `1` hard-assertion failures
(named in the `verdict` event); `2` non-Windows host (stub); `3` harness or
PTY error (recorded as a `fatal` event). The `--expect-ready` marker is where
REL-T7 pins the archive's TUI-ready string (the host-hello observable, §4.4);
the harness records the decoded boot frame regardless.

## 3. ConPTY behavior study (portable-pty 0.9.0 + primary sources)

`portable-pty` is pinned `=0.9.0` in `crates/pi-tui/Cargo.toml`; the study
below reads the exact vendored source at
`~/.cargo/registry/src/index.crates.io-*/portable-pty-0.9.0/src/win/{conpty,mod,psuedocon,procthreadattr}.rs`
(the code that compiles into the release binary) against the ConPTY host
sources in `microsoft/terminal@main` and Microsoft Learn.

### 3.1 Architecture: two pipes, a signal pipe, and a conhost sibling

`ConPtySystem::openpty` creates two anonymous pipes (input, output) and calls
`CreatePseudoConsole(size, in.read, out.write, flags)` (vendored
`win/conpty.rs:13-43`). On the OS side, `CreatePseudoConsole` additionally
creates a ConDrv server handle, a `\Reference` client handle, and a signal
pipe, then **spawns `conhost.exe` as a child of the calling process** with a
command line of the shape `"conhost.exe" --headless [--inheritcursor]
--width <cols> --height <rows> --signal 0x.. --server 0x..`
(`microsoft/terminal` `src/winconpty/winconpty.cpp`, `_CreatePseudoConsole`;
header `src/winconpty/winconpty.h`, `PseudoConsole` struct comments).

The client (pi.exe) is spawned separately by `slave.spawn_command` via
`CreateProcessW` with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` (0x00020016)
(vendored `win/procthreadattr.rs:8,47-65`; `win/psuedocon.rs:113-167`), so the
process tree during the witness is:

```text
harness (rel-r3-conpty-witness.exe)
├── conhost.exe --headless ...        # ConPTY host; sibling, NOT under pi.exe
└── pi.exe                            # attached via the \Reference handle
    └── (any pi.exe children, e.g. the extension-host sidecar)
```

Consequences for teardown (§3.8): `taskkill /PID <pi.exe> /T /F` kills the
pi.exe subtree but **not** conhost, which is reaped by `ClosePseudoConsole`
(vendored `win/conpty.rs` `Drop for PsuedoCon` → `win/psuedocon.rs:73-76`)
once the reference count drops; the header comments describe the exact
lifetime rule ("As long as hPtyReference exists it'll keep the server handle
alive and thus keep conhost alive. Closing this handle will make conhost exit
as soon as all currently connected clients have disconnected").

### 3.2 The flags portable-pty passes, and what modern conhost does with them

portable-pty 0.9.0 always passes
`PSUEDOCONSOLE_INHERIT_CURSOR | PSEUDOCONSOLE_RESIZE_QUIRK | PSEUDOCONSOLE_WIN32_INPUT_MODE`
= `0x7` (vendored `win/psuedocon.rs:87-89`); `PSEUDOCONSOLE_PASSTHROUGH_MODE`
(`0x8`) is declared but never used (`#[allow(dead_code)]`, same file).

Current `microsoft/terminal@main` `winconpty.cpp` consumes only
`PSEUDOCONSOLE_INHERIT_CURSOR` (`0x1` → `--inheritcursor`),
`PSEUDOCONSOLE_AMBIGUOUS_IS_WIDE` (`0x20`) and the
`PSEUDOCONSOLE_GLYPH_WIDTH__MASK` (`0x18`) bits. The `0x2`/`0x4` bits
portable-pty sets are **inert on modern conhost**:

- `0x4` (`WIN32_INPUT_MODE`) was added in microsoft/terminal commit
  `f32761849f` (2020-06-08, PR #6309) and later removed with the behavior
  made always-on (the OSS header at `main` no longer defines it; the
  In-process ConPTY spec `doc/specs/#13000` documents the architecture that
  replaced it).
- `0x2` (`RESIZE_QUIRK`) is a preview-era flag; the spec's goals list
  "Remove `--resizeQuirk`" alongside "Remove VtEngine", executed by the
  Windows 11 24H2 alignment (commits `450eec48de` "A minor ConPTY
  refactoring: Goodbye VtEngine Edition", `7fd9c5c789` "Align the OSS ConPTY
  API with Windows 11 24H2", both 2024-08).

Microsoft Learn documents only flags `0` and `0x1` for `CreatePseudoConsole`
("Windows 10 October 2018 Update (version 1809)" minimum). Net effect on a
windows-2025 / windows-latest image: **the effective portable-pty ConPTY
configuration is "standard pseudoconsole + inherit-cursor"; the other two set
bits are ignored.** This is a determinism-relevant fact: the same harness
bytes behave differently across Windows builds solely because of conhost
version, not harness input.

### 3.3 Spawn determinism hazards

1. **INHERIT_CURSOR blocks input until answered.** The ConPTY source comment
  above `ConptyCreatePseudoConsole` in `winconpty.cpp` states that with
  `PSEUDOCONSOLE_INHERIT_CURSOR`, "The created conpty will immediately emit
  a 'Device Status Request' VT sequence to hOutput, that should be replied
  to on hInput in the format `\x1b[<r>;<c>R` … **if a caller does not reply
  to this message, the conpty will not process any input until it does**."
  MS Learn's `CreatePseudoConsole` remarks give the closely related warning:
  "Failure to do so may cause the calling application to hang while making
  another request of the pseudoconsole system." portable-pty sets this flag
  unconditionally, so a harness that writes scripted input before replying
  has its input silently held. Mitigation (already in the testkit and
  mirrored by the harness): write the `ConhostVtDec2026Fallback` probe reply
  immediately after spawn — its trailing `\x1b[1;1R` answers the DSR — and
  additionally echo a standalone `\x1b[1;1R` if `\x1b[6n` is observed in the
  first batch (`crates/pi-tui/src/testkit/profile.rs:76-85`;
  `prototype/rel-r3-conpty/src/witness.rs` phase 1).
2. **Sideloaded conpty.dll preference.** portable-pty prefers a
   `conpty.dll`/`openconsole.exe` pair deployed *next to the harness binary*
   over the OS kernel32 functions (vendored `win/psuedocon.rs:52-58`). The
   release archive contains no such files, but the harness must run with a
   deploy directory that does not accidentally contain one, or the ConPTY
   host version changes under the witness.
3. **Zero-size rejection.** `CreatePseudoConsole` returns `E_INVALIDARG` when
   either dimension is 0 (`winconpty.cpp` `_CreatePseudoConsole`), and
   portable-pty converts `size.cols as i16` — so sizes above 32767 wrap.
   The witness's fixed 120x30 is far inside the valid range.

### 3.4 Sizing and resize semantics

Initial size becomes conhost's `--width 120 --height 30`; attached console
API clients observe it through normal console dimensions (Learn,
`ResizePseudoConsole`: "This ensures that attached CUI applications using the
Console Functions … will have the correct dimensions returned in their
calls"). `MasterPty::resize` maps to `ResizePseudoConsole`
(vendored `win/conpty.rs:53-88`), which posts `PTY_SIGNAL_RESIZE_WINDOW (8)`
on the conhost signal pipe (`winconpty.h` signal constants). The child
receives the dimension change as a console buffer-size event (the Windows
equivalent of SIGWINCH), and conhost's renderer re-emits invalidated screen
regions on the output pipe.

**The master-side resize stream is renderer-derived.** In the pre-24H2
VtEngine architecture conhost owned a screen buffer and re-serialized dirty
rectangles; a resize historically invalidated the whole screen, producing
full-repaint bytes (including clear-screen-equivalent output) that the
*child never wrote*. The 24H2-era rework (commits above; spec #13000) moved
to direct VT translation during Console API calls, changing the derivation
again — "VT input from the shell or other clients will be given 1:1 to the
hosting terminal" is a spec *goal* statement about input, not a promise that
output round-trips byte-identically. Therefore:

- Byte-equality assertions on the raw master stream are **not portable
  across runner-image rotations**; the witness asserts on the *decoded
  frame* (avt) instead, exactly like the shipped testkit
  (`snapshot_from_raw`, `crates/pi-tui/src/testkit/session.rs:518-546`).
- A "no destructive clears" assertion is only sound where every byte is
  child-attributable: the harness records a post-boot clear-count baseline
  (`clears_boot_baseline`) and hard-asserts that clears in the pre-resize
  stream do not exceed that baseline — a delta, not absolute absence, since
  conhost may translate pi's `\x1b[?1049h` alt-buffer entry as a clear during
  boot. Clear counts in each resize window are recorded as advisory (§4).

### 3.5 Alt-buffer switching

DECSET 1049 (alternate screen buffer) is
documented console-supported output (Learn, "Console Virtual Terminal
Sequences" → Alternate Screen Buffer table). pi's terminal guard uses it
(TUI-G4 alt-screen scope). The witness records `\x1b[?1049h` / `\x1b[?1049l`
counts as advisory observations — whether the wrapper bytes survive conhost
translation to the master stream is conhost-build dependent, so the render
contract is asserted on the decoded active-buffer frame, not on the presence
of the mode-set bytes. One decode trap found *executed* while building the
harness: avt 0.18's `Vt::text()` returns the **main** buffer; after 1049 the
UI lives in the alt buffer and must be read via `Vt::lines()` (the testkit's
active-buffer-first, main-buffer-fallback order). The harness mirrors that
order and carries a regression unit test for it.

### 3.6 Synchronized output (DEC 2026)

Conhost's VT support includes BSU/ESU (`ESC[?2026 h/l`) — declared in
`microsoft/terminal` `src/terminal/adapter/DispatchTypes.hpp:547`
(`SO_SynchronizedOutput = DECPrivateMode(2026)`) and handled in
`adaptDispatch.cpp` — but in translation mode conhost is the *consumer* of
the child's stream and the master sees conhost's own re-emission, not the
child's wrapper bytes. (The Microsoft Learn "Console Virtual Terminal
Sequences" page does not list DEC-2026; the source of truth is the
`microsoft/terminal` source and MicrosoftDocs/Console-Docs, §6.) The
repo already pins the deterministic equivalent: capability profile
`ConhostVtDec2026Fallback` denies synchronized-output support in the probe
reply, and `expects_synchronized_output()` is false for it
(`crates/pi-tui/src/testkit/profile.rs:76-91`). Under that profile pi never
emits 2026 wrappers, so the row's synchronized-output assertion becomes:

1. hard: no unbalanced 2026 frames in the master stream (trivially, zero
   markers — the harness records the counts), and
2. hard: the fallback path was actually selected (probe reply bytes written
   at spawn are recorded in the transcript).

A byte-level "child emitted BSU…ESU" assertion under ConPTY is **not
available**; asserting it would require passthrough mode (`0x8`), which
portable-pty 0.9.0 does not request. This is a designed substitution, not a
weakened row: the profile exists in the shipped testkit precisely for
conhost.

### 3.7 Event delivery (input direction)

The input pipe carries UTF-8 text with embedded VT sequences (Learn,
`CreatePseudoConsole` remarks: "On the input stream, plain text represents
standard keyboard keys … complicated operations are represented by encoding
control keys and mouse movements as virtual terminal sequences"). conhost
parses that stream into Win32 `INPUT_RECORD`s for the attached client;
pi.exe reads them through the console API via crossterm. Scripted typing
therefore exercises the same key-event path a real user does. The witness
sends `hello conpty witness` (echo assertion) then `\r` (Enter delivery,
recorded without a response assertion, since submitting text to a real agent
session is out of witness scope).

### 3.8 Teardown: taskkill tree + ClosePseudoConsole reap

portable-pty's `WinChild::kill` is `TerminateProcess(handle, 1)` on the
**direct child only** — no job object, no tree walk (vendored `win/mod.rs:41-57`).
Its published 0.9.0 form, recorded verbatim:

```rust
let res = unsafe { TerminateProcess(proc.as_raw_handle() as _, 1) };
let err = IoError::last_os_error();
if res != 0 { Err(err) } else { Ok(()) }
```

(the success/failure arms are inverted relative to the Win32 contract, but
`ChildKiller::kill` discards the result with `.ok()` and returns `Ok(())`,
so the observable contract is "TerminateProcess was attempted on the direct
child; failure is never reported"). Therefore **the release row's teardown
must not rely on portable-pty kill semantics for the tree**; the witness
uses exactly what REL-T7 names:

- `taskkill /PID <pid> /T /F` — Learn, taskkill: `/t` "Ends the specified
  process and any child processes started by it", `/f` force. This covers
  pi.exe **and the extension-host sidecar it spawned**, which is the
  actual archive process tree (the tree snapshot is recorded beforehand via
  `Get-CimInstance Win32_Process -Filter 'ParentProcessId=<pid>'`).
- PID comes from `child.process_id()` (`GetProcessId`, vendored
  `win/mod.rs:109-116`).
- conhost (a sibling of pi.exe, §3.1) is reaped by dropping writer + master
  → `ClosePseudoConsole`; the witness hard-asserts reader EOF within 10s as
  proof, and `child.wait()` records the exit code after the kill
  (`WaitForSingleObject` + `GetExitCodeProcess`, vendored `win/mod.rs:92-107`).
- Residual hazard noted for REL-T7: `GetExitCodeProcess` cannot distinguish
  a genuine exit code 259 (`STILL_ACTIVE`) from a live process
  (`win/mod.rs:26-39`); irrelevant post-taskkill but worth remembering if a
  future row ever polls `try_wait` instead of waiting on the handle.

### 3.9 What is already executed on windows runners today

The ConPTY *driver machinery* is not novel: the Tier N transcript job already
runs `cargo test -p pi --test tui_transcripts` on `windows-2025`
(`PI_TUI_TIER_ROW=tier-n/windows-x64@windows-2025` in
`.github/workflows/release-verification.yml`), and on Windows that suite
selects `ConPtyDriver` (`crates/pi/tests/tui_transcripts.rs:493-495`) — the
same `portable-pty` 0.9.0 `ConPtySystem` spawn/settle/snapshot path the
witness reuses, against the in-tree fixture. REL-R3's open surface is the
*unpacked-archive pi.exe* end to end: real product boot under ConPTY, real
host-sidecar process tree, and the taskkill/ClosePseudoConsole teardown
contract. That is what the harness covers and what the windows-latest run
must prove.

## 4. Harness assertion map (contract for REL-T7)

| Row assertion (from #114) | Harness phase | Class | Assertion |
| --- | --- | --- | --- |
| `pi.exe --version` | 0 | **hard** | exit 0, non-empty stdout, against the unpacked archive |
| deterministic spawn, 120x30 | 1 | **hard** | openpty at 120x30 succeeds; probe reply (incl. DSR answer) written before scripted input; PID recorded |
| host hello handshake | 1–2 | **hard via marker** | decoded boot frame non-empty; `--expect-ready` substring pins the archive's ready marker (handshake observable). Raw JSONL hello is spoken to the *sidecar*, not the PTY (REL-R1 §3 `helloRequestLine`/`isHelloAck`); binding the marker string is REL-T7's wiring step |
| scripted input echo | 2 | **hard** | decoded active-buffer frame contains the typed line after settle |
| render | 2–3 | **hard** | avt-decoded frame assertions at each geometry (boot non-empty; content present) |
| synchronized output | 1 | **hard (fallback form)** | DEC2026-fallback probe reply written; zero unbalanced 2026 markers in stream (§3.6) |
| no-clear-equivalent | 2–3 | **hard pre-resize (delta) / advisory in resize windows** | pre-resize `ESC[2J`/`ESC[3J` count ≤ post-boot baseline; counts recorded per resize window (§3.4) |
| resize semantics | 3 | **hard** | 100x28 → 132x40 → 120x30; decoded content preserved at each step |
| console-mode cleanup | 4 | **advisory** | `\x1b[?1049l` count + trailing stream recorded (conhost-derived; not asserted, §3.5) |
| taskkill-tree teardown | 4 | **hard** | `taskkill /PID <pid> /T /F` exit 0; PID absent from `tasklist`; `child.wait()` returns; reader EOF within 10s of master drop (conhost reaped) |

Transcript events: `environment, version_probe, spawn, output, observation,
frame, input, no_clear_scope, resize, tree_snapshot, taskkill, child_exit,
tasklist_check, conhost_reap, verdict` (+ `fatal`). The `verdict` event
carries `pass`, `hard_failures`, the advisory counts, and
`deferred_to: "REL-T7 (issue #114) windows-latest execution"`.

## 5. Verdict

### 5.1 Verdict: **GO (provisional, source-derived)**

No primary-source blocker exists for any of the five questioned capabilities
— deterministic spawn (with the DSR-reply condition), 120x30 sizing, scripted
input echo, render/synchronized-output/no-clear assertion capture (with the
assertion forms of §4), and `taskkill /pid /t /f` process-tree teardown (with
the conhost-reap condition). The repo's shipped testkit already pins the
conhost-specific pieces (`ConhostVtDec2026Fallback` profile, alt-buffer-aware
snapshots) that make the assertions deterministic, and the windows-2025
transcript leg already exercises the same portable-pty ConPTY driver against
the fixture. The word *provisional* is load-bearing: this GO is derived from
the exact vendored portable-pty 0.9.0 source, `microsoft/terminal@main`, and
Microsoft Learn — it is not an executed windows-latest transcript. REL-T7
converts it to a final GO by running the harness on windows-latest and
requiring exit 0 with the artifacts of §2.

### 5.2 Conditions attached to the GO (all implementable in REL-T7 wiring)

1. Probe/DSR reply bytes are written **before** any scripted input (else
   ConPTY input processing stalls, §3.3).
2. Render/no-clear/echo assertions operate on the avt-decoded active-buffer
   frame, never on raw master-stream byte equality (§3.4, §3.5); no-clear is
   a pre-resize delta against the post-boot baseline (not absolute absence).
3. The synchronized-output row assertion uses the DEC2026-fallback form
   (§3.6); a raw BSU/ESU passthrough assertion is out of scope for
   portable-pty 0.9.0.
4. Teardown is `taskkill /PID <pid> /T /F` (tree, force) **plus** master
   drop with EOF assert (conhost reap, §3.8); portable-pty kill is never the
   row's teardown primitive.
5. The harness deploy directory must not contain a sideloaded
   `conpty.dll`/`openconsole.exe` (§3.3.2), and the row pins its runner
   image so conhost-derivation changes are image changes, not silent ones.

### 5.3 No-go escalation path (verbatim, from #114)

If the windows-latest execution falsifies any condition above, REL-T7 must
take the no-go path: *"record the prototype findings verbatim and raise the
topology reopen as a blocking objection rather than weakening the row in
place"*, and *"the topology reopen is recorded as a blocking release objection with
REL-R3's verbatim findings; no four-row Tier N topology ships."* A no-go is
never silently converted to a four-row release.

## 6. Primary sources

- Vendored `portable-pty 0.9.0` source (the compiled artifact):
  `src/win/conpty.rs`, `src/win/mod.rs`, `src/win/psuedocon.rs` (Drop 73-76, flag call 87-89, `spawn_command` 113-167, sideload preference 52-58),
  `src/win/procthreadattr.rs`, `src/cmdbuilder.rs` —
  `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/portable-pty-0.9.0/`.
- `microsoft/terminal` (read 2026-08-27, `main`): `src/winconpty/winconpty.h`
  (PseudoConsole struct + lifetime comments, signal constants, flag
  definitions), `src/winconpty/winconpty.cpp` (`_CreatePseudoConsole`,
  conhost command line, consumed flags, INHERIT_CURSOR DSR comment),
  `doc/specs/#13000 - In-process ConPTY.md`; commits `f32761849f` (#6309),
  `450eec48de` (#17510), `7fd9c5c789` (#17704).
- Microsoft Learn: CreatePseudoConsole, ResizePseudoConsole, Console Virtual
  Terminal Sequences (Alternate Screen Buffer), taskkill.
- MicrosoftDocs/Console-Docs `main` `docs/console-virtual-terminal-sequences.md`
  (Synchronized Output section, BSU/ESU).
- `microsoft/terminal` `src/terminal/adapter/{DispatchTypes.hpp:547,
  adaptDispatch.cpp}` (DEC-2026 Synchronized Output handling).
- Repository: `crates/pi-tui/src/testkit/{conpty,profile,session}.rs`,
  `crates/pi/tests/tui_transcripts.rs`,
  `.github/workflows/release-verification.yml`,
  `docs/REL-R1-musl-toolchain-bakeoff.md` (host hello contract),
  `docs/REL-R2-macos-signing.md` (verdict-record convention).
