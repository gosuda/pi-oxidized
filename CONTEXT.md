# Repository context

## Execution-map publication

An **execution-map generation** is one immutable Markdown file. It contains the rendered execution map and one strictly delimited canonical JSON witness. The file is the complete publication unit.

The **current-generation pointer** is the only canonical selection point. Every reader resolves it once and reads the named generation. The old standalone map and witness paths are not authorities and have no fallback role.

Publication guarantees atomic visibility across ordinary file-system errors and process termination. It does not claim power-loss durability or atomic reads during an active Git checkout. Git remains the recovery mechanism for interrupted durable storage.

The SHA-256 of the complete Markdown bytes is the generation ID. Publication retains every generation. Concurrent publishers can install independent content-addressed files; the last completed pointer swap selects the current generation.

The pointer and generations are co-located under `scripts/verification/fixtures/execution-map/`. The pointer is a small Markdown file with one generated link. Tools parse its exact generated form; humans follow the same link.

The pointer bytes are exactly `[Current execution map](generations/<64 lowercase hexadecimal characters>.md)\n`. A generation ends with `## Canonical witness`, one `json` fence, and the closing fence plus newline at end of file.

One pure registry derivation owns row selection and prerequisite closure. The publisher renders its result. The verifier checks the selected bundle, exact row fields, fixed pins, and graph invariants without reimplementing derivation policy.

Publication has one internal file-operations seam. The production adapter uses the runtime filesystem. A deterministic fault-injection adapter drives the same state machine in tests. The interface contains only operations required to install a generation and swap the pointer.

Publication registers each staging path before writing it. Ordinary failures remove their own staging files when cleanup succeeds. A cleanup failure is reported with the primary failure. Process termination can leave files in the reserved ignored staging namespace; these files are unreachable from the strict digest pointer and are never treated as generations.

Generation installation uses an atomic no-replace hard link from complete staged bytes. An existing generation or a hard-link error is accepted only when the target's exact bytes match its digest. A pointer-swap error is reconciled by reading the pointer: the publication committed if the pointer names the requested generation.

The staging and destination paths stay on one filesystem. If that filesystem rejects hard links or atomic rename replacement, publication fails without changing the pointer.

The alignment classifier treats every content-addressed generation as a structural historical witness only after its full hash and bundle grammar pass. Retired identity is allowed only inside the canonical-witness JSON fence; the rendered map remains subject to normal scanning.

`scripts/verification/map.ts` owns pointer and bundle parsing plus registry verification. `scripts/verification/publish-map.ts` owns generation installation and pointer selection. `scripts/verification/publish-map.test.ts` is the executable failure matrix.
