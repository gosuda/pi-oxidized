# Execution map

The canonical stable-ID DAG registry for the port program (MAP-1, issue #134). One row per ticket. The live GitHub issue tree rooted at #12 is authoritative. `scripts/verification/fixtures/execution-map-ticket-records.json` is the tracked, commit-pinned v2 structural ticket-record witness published from that authority for `gosuda/pi-oxidized`; it is an offline witness, not a live API view. This document is the derived published view and is never hand-edited. `bun run verify:map-ledger` validates the fixture's source hash and provenance metadata, re-derives the 151-row registry from it, checks every row's Issue, Title, and blocked_by field against the records exactly, and validates the mapped structural hash below.

- Snapshot structural sha256: `4f6997271223f1f3bf477e6562fac138b49acd17e72d5f384635a068122d2426`
- Witness source hash: `sha256:217db5892d7eeb0f171e7bde1bf85c2e2008eac6b8b2688a217d5630b7cff743` — the publisher's canonical SHA-256 (UTF-8 JSON, sorted keys, compact separators) over all 159 structural ticket records; mutable issue status is intentionally absent from the witness so issue closure never perturbs structural provenance.
- Row count: 151 — 137 sibling graduate tickets (the legacy 109 plus the architecture siblings including the ARC-CLOSE closer), 6 map tickets MAP-1 through MAP-6, the 7 prerequisite external nodes, and MAP-ROOT for canonical issue #12.
- Published `blocked_by` cells never contain synthetic root edges: for a published row, `A blocked_by B` means prerequisite `B` feeds dependent `A`. During verification only, the checker adds `MAP-ROOT -> F` for each fixture-derived canonical zero-blocker frontier row `F`; every registry row must be reachable from MAP-ROOT alone and must reach the terminal closure node MAP-6.
- REL-DOCS is registered exactly once and dominates documentation closure: REL-CLOSE and DOC-F are each blocked by it, and no REL-* node reaches DOC-F except through the REL-DOCS/REL-CLOSE gate.
- The prerequisite externals are the six named by MAP-1 (EXT-14, EXT-21, EXT-23, EXT-24, EXT-25, EXT-26) plus EXT-15, which XC-1 and externals EXT-24/EXT-25 cite.
- The architecture track (issues #153-#180) is registered by the same authority; its closer ARC-CLOSE is a required predecessor of the final cross-plan gate MAP-5 alongside the seven settled track closers.
- Modality vocabulary is pinned to the settled kinds of docs/PARITY_LEDGER.md (`task`, `prototype`, `research`, `grilling`, `external`); PAR-track rows exactly match the ledger's graduated parity-ticket DAG, all four graduation modalities stay populated, external rows are `external`, and MAP-ROOT is `task`. Modalities classify each ticket's graduation shape: `research` investigates and decides, `grilling` audits adversarially, `prototype` proves a harness or measurement, `task` executes.

## Registry

| Stable ID | Modality | Issue | Title | blocked_by |
| --- | --- | --- | --- | --- |
| ARC-CLOSE | task | #180 | [ARC-CLOSE] Close the structural refactor track on final-tree proof | ARC-T21 |
| ARC-D1 | grilling | #155 | [ARC-D1] Preserve load-bearing native architecture | — |
| ARC-D2 | grilling | #156 | [ARC-D2] Define extension witness and delivery guarantees | — |
| ARC-D3 | grilling | #157 | [ARC-D3] Define the three-layer extension-host test model | — |
| ARC-R1 | grilling | #153 | [ARC-R1] Define repository-wide structural refactor completion contract | — |
| ARC-R2 | research | #154 | [ARC-R2] Audit deep-module architecture against pi2 | — |
| ARC-R3 | research | #173 | [ARC-R3] Prove the public pi-tui overlay stack has no live consumers | ARC-T2 |
| ARC-T1 | task | #158 | [ARC-T1] Consolidate same-run compaction as the refactor checkpoint | ARC-D1 |
| ARC-T10 | task | #167 | [ARC-T10] Classify extension event delivery and make dialogs lossless | ARC-T2, ARC-D2 |
| ARC-T11 | task | #168 | [ARC-T11] Complete the cross-language extension witness | ARC-T10 |
| ARC-T12 | task | #169 | [ARC-T12] Make ExtensionUiEvent the only mode-layer currency | ARC-T10 |
| ARC-T13 | task | #170 | [ARC-T13] Hide session-control wire types behind the extension seam | ARC-T12 |
| ARC-T14 | task | #171 | [ARC-T14] Rebuild ExtensionRuntimeSet behind its existing interface | ARC-T11, ARC-T13 |
| ARC-T15 | task | #172 | [ARC-T15] Replace mutable process fixtures with three-layer proof | ARC-T2, ARC-D3 |
| ARC-T16 | task | #174 | [ARC-T16] Give terminal input one ownership handoff | ARC-T2 |
| ARC-T17 | task | #175 | [ARC-T17] Resolve pi-tui overlay and focus ownership | ARC-R3 |
| ARC-T18 | task | #176 | [ARC-T18] Remove shallow pi-tui residue and bench leakage | ARC-T16, ARC-T17 |
| ARC-T19 | task | #177 | [ARC-T19] Move generated fixtures to their owner and add check modes | ARC-T2 |
| ARC-T2 | task | #159 | [ARC-T2] Register the structural refactor in the canonical map ledger | ARC-R1, ARC-T1, ARC-R2 |
| ARC-T20 | task | #178 | [ARC-T20] Give release protocol and archive integrity one proof owner | ARC-T11, ARC-T19 |
| ARC-T21 | task | #179 | [ARC-T21] Integrate and audit the structural refactor | ARC-T3, ARC-T5, ARC-T6, ARC-T9, ARC-T11, ARC-T14, ARC-T15, ARC-T16, ARC-T17, ARC-T18, ARC-T19, ARC-T20 |
| ARC-T3 | task | #160 | [ARC-T3] Pin agent transcript and persistence invariants | ARC-T2 |
| ARC-T4 | task | #161 | [ARC-T4] Consolidate product policy ownership | ARC-T2 |
| ARC-T5 | task | #162 | [ARC-T5] Give each session build one SettingsManager owner | ARC-T4 |
| ARC-T6 | task | #163 | [ARC-T6] Give direct and bridge reload one session choreography | ARC-T4 |
| ARC-T7 | task | #164 | [ARC-T7] Consolidate builtin provider authentication metadata | ARC-T2 |
| ARC-T8 | task | #165 | [ARC-T8] Give provider stream options one typed vocabulary | ARC-T7 |
| ARC-T9 | task | #166 | [ARC-T9] Keep ProviderRegistry as pure dispatch | ARC-T8 |
| DEPS-B1 | task | #121 | [DEPS-B1] Bin P: patch sweep (9 Rust crates + npm ignore) in one commit | DEPS-R1 |
| DEPS-B2 | task | #123 | [DEPS-B2] Bin M: minor sweep with per-member primary changelogs and behavioral smoke | DEPS-B1 |
| DEPS-D1 | task | #130 | [DEPS-D1] Closing audit and generated-doc handoff to DOC-F | DEPS-T1 |
| DEPS-G1 | grilling | #127 | [DEPS-G1] Future major: ratatui + crossterm coupled unit (three-crate backend pairing) | PAR-CLOSE, EXT-23 |
| DEPS-R1 | research | #117 | [DEPS-R1] Re-ground dependency bins against live registries before any upgrade executes | EXT-23, PAR-CLOSE, EXT-26 |
| DEPS-R2 | task | #128 | [DEPS-R2] Stand up the out-of-band CVE/yanked-version remediation runbook with the shipped-exposure predicate checker | EXT-23 |
| DEPS-R3 | research | #126 | [DEPS-R3] Watch: retire syntect-transitive deny.toml ignores on the qualifying syntect release | — |
| DEPS-T1 | task | #124 | [DEPS-T1] Toolchain: Rust 1.97.1 → re-grounded stable (≥1.98.0) as its own atomic unit | DEPS-X1, DEPS-X2, DEPS-X3 |
| DEPS-X1 | task | #120 | [DEPS-X1] Major: base64 0.22.1 → 0.23.x with Engine-API migration evidence | DEPS-B2 |
| DEPS-X2 | task | #122 | [DEPS-X2] Major: serde-saphyr 0.0.29 → 1.1.x with frontmatter fixture parity | DEPS-B2 |
| DEPS-X3 | task | #125 | [DEPS-X3] Major: typescript 5.9.3 → 7.0.x across all three manifests | DEPS-B2 |
| DOC-A | task | #129 | [DOC-A] Doc-evidence ledger and closed-class sync-boundary checker | EXT-24, VER-ALIGN |
| DOC-B | task | #139 | [DOC-B] Pinned-version and compatibility-matrix generation | DOC-A, EXT-14, EXT-21 |
| DOC-C | task | #137 | [DOC-C] User-doc corpus port with behavior-transcript evidence | DOC-A, DOC-B, DOC-G2, PAR-CLOSE, XC-CLOSE, TUI-CLOSE, EXT-25 |
| DOC-D | task | #133 | [DOC-D] Fenced-snippet compile harness, minimal contract fixtures, and crate READMEs | DOC-C, PAR-CLOSE |
| DOC-E | task | #136 | [DOC-E] CHANGELOG discipline, release instructions, generated-artifact guards, seven-row platform matrix (consume-only) | DOC-B, DOC-C, REL-CLOSE |
| DOC-F | task | #138 | [DOC-F] Publication verification, root README, final generated-docs rerun, all-scope audit (consume-only) | PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, REL-DOCS, DEPS-D1, DOC-D, DOC-E |
| DOC-G1 | prototype | #132 | [DOC-G1] Fenced-snippet extraction and compile harness prototype | DOC-A |
| DOC-G2 | grilling | #135 | [DOC-G2] Adversarial review of the doc-evidence program before DOC-C/D/E fan out | DOC-A, DOC-B, DOC-G1 |
| EXT-14 | external | #14 | Ground all pinned versions from release channels | — |
| EXT-15 | external | #15 | Inventory upstream runtime package capabilities | — |
| EXT-21 | external | #21 | Audit current dependency freshness and licenses | EXT-14 |
| EXT-23 | external | #23 | Define dependency upgrade policy | EXT-14, EXT-21 |
| EXT-24 | external | #24 | Map public documentation and release surfaces | EXT-15 |
| EXT-25 | external | #25 | Audit canonical terminal interaction flows | EXT-15 |
| EXT-26 | external | #26 | Define supported release platforms | EXT-14 |
| MAP-1 | task | #134 | [MAP-1] Build the canonical stable-ID DAG registry consuming the published ticket records | — |
| MAP-2 | grilling | #142 | [MAP-2] Ratify the fifteen arbitration rulings (telemetry fixed at five sites; cfg(unix) and REL-DOCS folded in) and insert binding edges | MAP-1 |
| MAP-3 | task | #141 | [MAP-3] Stand up the Decisions-so-far index, map update rules, and fog-graduation lint | MAP-1 |
| MAP-4 | task | #140 | [MAP-4] Encode the six integration sequencing gates as registry predicates | MAP-2, MAP-3 |
| MAP-5 | task | #144 | [MAP-5] Execute the final nine-witness cross-plan gate ledger against the final tree (single global closing gate) | MAP-4, PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, DEPS-D1, DOC-F, REL-R2, DEPS-R3, DEPS-R2, DEPS-G1, DOC-D, DOC-E, ARC-CLOSE |
| MAP-6 | task | #143 | [MAP-6] Close issue #12 on a green graph and green ledger | MAP-5 |
| PAR-CLI | task | #51 | [PAR-CLI] Add pi-ai crate-local OAuth CLI binary ([[bin]]) | PAR-CLI-PROTO |
| PAR-CLI-PROTO | prototype | #48 | [PAR-CLI-PROTO] Hermetic pi-ai CLI parity harness prototype | PAR-LEDGER |
| PAR-CLIENT | task | #33 | [PAR-CLIENT] pi::remote ByteTransport with portable neutral surface + #[cfg(unix)] unix adapter + client + five-class errors (R3) | PAR-CODEC |
| PAR-CLOSE | task | #39 | [PAR-CLOSE] Close the parity ledger and ratify the boundary (finalize issue #20) | PAR-FOLD, PAR-CLIENT, PAR-SERVER, PAR-COMPAT-AUDIT, PAR-COMPAT-DISPO, PAR-PTY-GRILL, XC-2 |
| PAR-CODEC | task | #31 | [PAR-CODEC] Port pi::remote codec, framing, and schemas — codec-layer errors only (R1–R2) | PAR-WIRE |
| PAR-COMPAT-AUDIT | grilling | #59 | [PAR-COMPAT-AUDIT] Source/corpus ./compat compatibility audit (parity blocker, A8) | PAR-LEDGER |
| PAR-COMPAT-DISPO | task | #45 | [PAR-COMPAT-DISPO] Execute compat disposition and delete the dead config_value wrapper | PAR-COMPAT-AUDIT |
| PAR-FOLD | task | #34 | [PAR-FOLD] Duplicate-fold and callsite-migration sweep | PAR-TEL, PAR-CLI, PAR-COMPAT-DISPO |
| PAR-LEDGER | task | #29 | [PAR-LEDGER] Build parity ledger and boundary witness suite | — |
| PAR-MATH | task | #37 | [PAR-MATH] Implement pi-tui math rendering per settled strategy (T4) | PAR-MATH-RESEARCH |
| PAR-MATH-RESEARCH | research | #36 | [PAR-MATH-RESEARCH] Research KaTeX-class math port strategy for pi-tui | PAR-LEDGER |
| PAR-PTY-GRILL | grilling | #46 | [PAR-PTY-GRILL] Grill runtime-dependent UI claims with host-tier PTY evidence | PAR-LEDGER, PAR-MATH |
| PAR-SERVER | task | #32 | [PAR-SERVER] pi::remote multi-session server (portable) with #[cfg(unix)] unix listener preset (R4) | PAR-CLIENT |
| PAR-TEL | task | #71 | [PAR-TEL] Implement pi-agent::telemetry with single injected no-fail context (all FIVE literals migrated) | PAR-LEDGER |
| PAR-WIRE | research | #30 | [PAR-WIRE] Decide remote-session wire format — upstream CBOR vs landed JSONL | PAR-LEDGER |
| PERF-CLOSE | task | #105 | [PERF-CLOSE] Assemble final performance acceptance evidence for issue #22 | PERF-G12, PERF-G13, PERF-G15, PERF-G16 |
| PERF-G10 | grilling | #98 | [PERF-G10] Audit floor ledgers and cost decompositions | PERF-R9 |
| PERF-G12 | grilling | #96 | [PERF-G12] Audit divergence classifications and boundary answers of rebuilt targets | PERF-T11 |
| PERF-G13 | grilling | #92 | [PERF-G13] Audit iteration, rollback, and exhaustion discipline on the campaign | PERF-T11 |
| PERF-G15 | grilling | #101 | [PERF-G15] Audit cold-path verdicts for complexity-bought microseconds | PERF-T14 |
| PERF-G16 | grilling | #99 | [PERF-G16] Audit the final performance claims against the non-claim registry and claim vocabulary | PERF-T11, PERF-T14 |
| PERF-R18 | research | #103 | [PERF-R18] Pre-flight — confirm plan prerequisites not already landed upstream (external node: issue #12 map, status open) | — |
| PERF-R2 | research | #90 | [PERF-R2] Rank the repo-wide performance workload surface with trusted baselines | PERF-T1 |
| PERF-R8 | research | #95 | [PERF-R8] Measure paired baselines on the newly symmetric lanes | PERF-T3, PERF-T4, PERF-T5, PERF-T6, PERF-T7 |
| PERF-R9 | research | #94 | [PERF-R9] Write per-hot-unit floor ledgers with cost decompositions and blind-derivation contracts | PERF-R2, PERF-R8 |
| PERF-T1 | task | #85 | [PERF-T1] Instrument the noise gate and process memory in the verification runners | PERF-R18 |
| PERF-T11 | task | #97 | [PERF-T11] Execute the iterative hot-unit rebuild campaign, one atomic commit per iteration | PERF-R9, PERF-G10 |
| PERF-T14 | task | #100 | [PERF-T14] Grade every cold unit fixed, at-floor, or left | PERF-T11 |
| PERF-T3 | task | #89 | [PERF-T3] Build the Rust render-churn benchmark matching the upstream TUI churn workload | PERF-T1 |
| PERF-T4 | task | #86 | [PERF-T4] Add isolated session append and reopen timing lanes | PERF-T1 |
| PERF-T5 | task | #93 | [PERF-T5] Add a dispatch-only tool benchmark with no-op deterministic tools on both implementations | PERF-T1 |
| PERF-T6 | task | #88 | [PERF-T6] Extension-host scaling lane on production serve_io with a deterministic NativeExtension adapter | PERF-T1 |
| PERF-T7 | task | #91 | [PERF-T7] Define symmetric install-footprint accounting | PERF-T1 |
| REL-CLOSE | grilling | #119 | [REL-CLOSE] Grill the seven-platform release proof (release closer) | REL-T4, REL-T5, REL-T6, REL-T7, REL-T8, REL-DOCS, REL-T9, REL-R2 |
| REL-DOCS | task | #111 | [REL-DOCS] Stage documentation into the seven release archives | REL-T4, REL-T6 |
| REL-R1 | prototype | #102 | [REL-R1] Bake off the musl toolchain and userland provisioning | — |
| REL-R2 | research | #118 | [REL-R2] Research the signed and notarized macOS release channel | — |
| REL-R3 | prototype | #115 | [REL-R3] Prototype the Windows ConPTY interaction witness | — |
| REL-T1 | task | #106 | [REL-T1] Extend the release target model to seven triples | VER-ALIGN |
| REL-T2 | task | #104 | [REL-T2] Pin Bun 1.3.14 musl fallback-runtime assets | REL-T1 |
| REL-T3 | task | #112 | [REL-T3] Provision musl toolchains and userland in CI recipes | REL-R1, REL-T2 |
| REL-T4 | task | #108 | [REL-T4] Wire seven legs, native runners, musl gates, and the five Tier N witnesses into release-verification.yml | REL-T3, TUI-CLOSE |
| REL-T5 | task | #113 | [REL-T5] Land compat-matrix 0.2.0 with seven release target rows and tier-accurate terminology | REL-T4 |
| REL-T6 | task | #110 | [REL-T6] Offline pre-cache for the Bun fallback runtime | REL-T2 |
| REL-T7 | task | #114 | [REL-T7] Wire the Windows ConPTY witness or escalate the topology reopen | REL-R3, REL-T4 |
| REL-T8 | task | #109 | [REL-T8] Attest build provenance for the seven archives | REL-T4, REL-DOCS |
| REL-T9 | task | #116 | [REL-T9] Write the supported-platforms document with dated pins, verbatim tier language, and limitations | REL-T5, REL-T6, REL-T7, REL-T8, REL-DOCS |
| TUI-CLOSE | task | #82 | [TUI-CLOSE] Close terminal polish track with consolidated evidence | TUI-G1, TUI-G2, TUI-G3, TUI-G4, TUI-G5, TUI-G6, TUI-G7, TUI-G8, TUI-R1, TUI-R2, TUI-V1, TUI-V2, TUI-V3, TUI-V4, TUI-V5, TUI-V6, TUI-T6, TUI-T8, TUI-T4, TUI-T7, TUI-P3, TUI-T3, TUI-T10, TUI-T11, TUI-T9, TUI-P4 |
| TUI-G1 | grilling | #49 | [TUI-G1] Reduced-motion and spinner opt-out policy (Routed decision: settings category) | EXT-25 |
| TUI-G2 | grilling | #53 | [TUI-G2] Hardware cursor as first-class setting (Routed decision: settings + focus categories) | EXT-25, TUI-G1 |
| TUI-G3 | grilling | #40 | [TUI-G3] Rail-only doctrine, dead-token disposition, and editor-state consumption (Routed decision: public tokens + state ownership) | EXT-25 |
| TUI-G4 | grilling | #35 | [TUI-G4] Alt-screen / scroll-view scope classification | EXT-25, TUI-G3 |
| TUI-G5 | grilling | #50 | [TUI-G5] Copy policy ledger — capitalization, terminology, error taxonomy, empty states | EXT-25 |
| TUI-G6 | grilling | #63 | [TUI-G6] Color doctrine — capability-driven depth, hyperlinks, extension guardrails | EXT-25 |
| TUI-G7 | grilling | #61 | [TUI-G7] Confirm dialog default-selection and Esc semantics (Routed decision: focus/navigation + dispatch) | EXT-25, TUI-G5 |
| TUI-G8 | grilling | #56 | [TUI-G8] Narrow-width viewport floor policy (Routed decision: viewport policy) | EXT-25, TUI-P1 |
| TUI-P1 | prototype | #67 | [TUI-P1] Portable PTY harness with deterministic schema v1, driver interface, and width ladder | EXT-25 |
| TUI-P2 | prototype | #58 | [TUI-P2] Deterministic contrast measurement prototype | TUI-P1 |
| TUI-P3 | prototype | #70 | [TUI-P3] Extension-UI gauntlet fixture | TUI-P1 |
| TUI-P4 | prototype | #84 | [TUI-P4] Static-frame spinner prototype | TUI-G1 |
| TUI-R1 | research | #66 | [TUI-R1] Provenance audit of non-default built-in palettes | — |
| TUI-R2 | research | #62 | [TUI-R2] Terminal width-table divergence survey | — |
| TUI-T1 | task | #80 | [TUI-T1] Derive all rendered key hints from the keybinding registry (Classifier: PASS — presentation truthfulness) | EXT-25, TUI-G5 |
| TUI-T10 | task | #75 | [TUI-T10] Onboarding and first-run copy (Classifier: PASS — copy) | TUI-G5 |
| TUI-T11 | task | #78 | [TUI-T11] Reduced-motion implementation — EXECUTED ONLY IN ITS OWN DECISION TRACK (Classifier: ROUTE — settings/persistence) | TUI-G1, TUI-P4 |
| TUI-T2 | task | #74 | [TUI-T2] Capability-driven color depth (Classifier: PASS — existing-token selection at existing render sites) | TUI-G6 |
| TUI-T3 | task | #73 | [TUI-T3] Hyperlink capability surfaces (Classifier: PASS — presentation at existing surfaces) | TUI-G6 |
| TUI-T4 | task | #65 | [TUI-T4] Editor border thinking/bash colors — EXECUTED ONLY IN ITS OWN DECISION TRACK (Classifier: ROUTE — state ownership) | TUI-G3 |
| TUI-T5 | task | #68 | [TUI-T5] Error-copy remediation pack (Classifier: PASS — copy) | TUI-G5 |
| TUI-T6 | task | #57 | [TUI-T6] Consequence-repeating confirm LABELS and selector empty aisles (Classifier: PASS — copy only; selection semantics routed to TUI-G7) | TUI-G5 |
| TUI-T7 | task | #69 | [TUI-T7] Dead-token and dead-code cleanup — EXECUTED ONLY IN ITS OWN DECISION TRACK (Classifier: ROUTE — public tokens) | TUI-G3 |
| TUI-T8 | task | #64 | [TUI-T8] Truncation-honesty glyph unification (Classifier: PASS — presentation) | EXT-25, TUI-G5 |
| TUI-T9 | task | #83 | [TUI-T9] Narrow-width floor implementation — EXECUTED ONLY IN ITS OWN DECISION TRACK (Classifier: ROUTE — viewport policy) | TUI-G8 |
| TUI-V1 | task | #76 | [TUI-V1] Two-tier state-matrix conformance (Classifier: PASS — measurement) | TUI-P1, EXT-26, TUI-T1, TUI-T5 |
| TUI-V2 | task | #77 | [TUI-V2] Keyboard and focus gauntlet within current dispatch semantics (Classifier: PASS — measurement; semantics changes routed to TUI-G7) | TUI-T1, TUI-G2 |
| TUI-V3 | task | #81 | [TUI-V3] Unicode and width gauntlet on real terminals | TUI-P1, TUI-R2 |
| TUI-V4 | task | #87 | [TUI-V4] Resize storm, settle, and progressive-disclosure integrity (measurement of current viewport semantics) | TUI-P1, TUI-G8 |
| TUI-V5 | task | #79 | [TUI-V5] Theme and contrast matrix with numeric oracle | TUI-P2, TUI-T2 |
| TUI-V6 | task | #72 | [TUI-V6] Accessibility evidence: automated invariants on all Tier N rows + named manual screen-reader sign-off | TUI-P1, TUI-G1, TUI-G2 |
| VER-ALIGN | task | #145 | [VER-ALIGN] Align workflow reference pin to canonical baseline | — |
| XC-1 | task | #52 | [XC-1] Author TypeScript extension compatibility contract document | EXT-15 |
| XC-2 | task | #41 | [XC-2] Close frames.jsonl witness gaps and own the single witness-manifest lockstep proof | XC-1 |
| XC-3 | research | #54 | [XC-3] Produce the A8 extension-import legacy-surface audit record for the parity A8 adjudication | XC-1 |
| XC-4 | task | #42 | [XC-4] Pin the three-mode handshake asymmetry matrix with mutation witnesses | XC-1 |
| XC-5 | research | #38 | [XC-5] Reconcile pi-ext Registry conflict semantics against the registration conflict matrix | XC-1 |
| XC-6 | task | #55 | [XC-6] Build the hook-dispatch semantics lattice for all 33 lifecycle discriminants | XC-1 |
| XC-7 | grilling | #43 | [XC-7] Prove the Rust sanitization trust boundary on every inbound uiSlot path | XC-1 |
| XC-8 | task | #44 | [XC-8] Pin deadline, cancellation, error-isolation, and stale-guard witnesses | XC-1 |
| XC-9 | task | #47 | [XC-9] Pin discovery, packaging, and source-pinned host-resolution witnesses | XC-1 |
| XC-CLOSE | task | #60 | [XC-CLOSE] Close issue #19 with contract, witness, mutation-proof, and A8-delivery evidence after parity ratification | XC-2, XC-3, XC-4, XC-5, XC-6, XC-7, XC-8, XC-9, PAR-CLOSE |
| MAP-ROOT | task | #12 | Port program root; anchors the zero-prerequisite frontier | — |

## Pinned telemetry migration surface

Exactly six AgentLoopConfig struct-literal sites — the shared arbitration oracle, imported from `PINNED_AGENT_LOOP_CONFIG_SITES` in scripts/verification/parity.ts:

- crates/pi-agent/src/agent.rs:62-88
- crates/pi-agent/src/config.rs:360-389
- crates/pi-agent/src/run.rs:835-861
- crates/pi-agent/src/schedule.rs:902-928
- crates/pi/src/core/agent_session/mod.rs:463-489
- crates/pi-agent/src/bin/pi_agent_stream_frame_bench.rs:267-294
