# Session format

Ported from `.references/pi/packages/coding-agent/docs/session-format.md` at
pin `8fa7eebd`. Claims below are bound to the evidence manifest
[evidence/session-format.json](evidence/session-format.json); anything not
yet provable in the Rust port is listed under "Pending port surface" instead
of being described as working.

This page describes only what the session-interop verification run proved
about the JSONL format: the Rust session layer processed nine fixtures across
the v1, v2, and v3 formats and wrote 27 session files (three per fixture:
a migrated `continued.jsonl` plus two forks), and the source-pinned TypeScript
`SessionManager` reopened every produced file. Record shapes below are quoted
from the fixture inputs and the Rust-produced outputs of that run.

## Version lineage

A session file is JSONL: the first line is a header with a `version` field,
and every following line is one entry. Three versions exist in the fixture
corpus:

- Version 1 is the legacy linear format: the header carries no `version`
  field and entries have no `id`/`parentId`.
- Version 2 adds the `version` field and per-entry `id`/`parentId`, enabling
  branched trees.
- Version 3 is the current format written by this port.

The v1 fixture header and first entry (no `version`, no entry ids):

```text
{"type":"session","id":"00000000-0000-7000-8000-000000000001","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/tmp/pi-fixtures/cwd"}
{"type":"message","timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"user","content":"V1 U1.","timestamp":1735689601000}}
{"type":"message","timestamp":"2025-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"V1 A1."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp":1735689602000}}
{"type":"message","timestamp":"2025-01-01T00:00:03.000Z","message":{"role":"user","content":"V1 U2.","timestamp":1735689603000}}
{"type":"message","timestamp":"2025-01-01T00:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"V1 A2."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp":1735689604000}}
{"type":"message","timestamp":"2025-01-01T00:00:05.000Z","message":{"role":"user","content":"V1 U3 kept.","timestamp":1735689605000}}
{"type":"message","timestamp":"2025-01-01T00:00:06.000Z","message":{"role":"assistant","content":[{"type":"text","text":"V1 A3 kept."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp":1735689606000}}
<!-- doc-c:fence=session-format.01 source=.agent-tasks/pi-rust-rewrite/fixtures/sessions/v1/linear-with-compaction.jsonl -->
```

The v2 fixture header declares `version` 2, and its entries chain through
`id`/`parentId`; a branch entry points back to an ancestor rather than the
immediately preceding entry:

```text
{"type":"session","version":2,"id":"00000000-0000-7000-8000-000000000002","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/tmp/pi-fixtures/cwd"}
{"type":"message","id":"e0000001","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"user","content":"V2 U1 main.","timestamp":1735689601000}}
<!-- doc-c:fence=session-format.02 source=.agent-tasks/pi-rust-rewrite/fixtures/sessions/v2/branched.jsonl -->
```

```text
{"type":"session","version":2,"id":"00000000-0000-7000-8000-000000000002","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/tmp/pi-fixtures/cwd"}
{"type":"message","id":"e0000001","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"user","content":"V2 U1 main.","timestamp":1735689601000}}
{"type":"message","id":"e0000002","parentId":"e0000001","timestamp":"2025-01-01T00:00:02.000Z","message":{"role":"hookMessage","customType":"onboarding-hook","content":"Welcome hint from hook.","display":true,"timestamp":1735689602000}}
{"type":"message","id":"e0000003","parentId":"e0000002","timestamp":"2025-01-01T00:00:03.000Z","message":{"role":"assistant","content":[{"type":"text","text":"V2 A1 main."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp":1735689603000}}
{"type":"message","id":"e0000004","parentId":"e0000003","timestamp":"2025-01-01T00:00:04.000Z","message":{"role":"user","content":"V2 U2 main.","timestamp":1735689604000}}
{"type":"message","id":"e0000005","parentId":"e0000004","timestamp":"2025-01-01T00:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"V2 A2 main."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp":1735689605000}}
{"type":"message","id":"e0000006","parentId":"e0000003","timestamp":"2025-01-01T00:00:06.000Z","message":{"role":"user","content":"V2 U3 branch.","timestamp":1735689606000}}
{"type":"message","id":"e0000007","parentId":"e0000006","timestamp":"2025-01-01T00:00:07.000Z","message":{"role":"assistant","content":[{"type":"text","text":"V2 A3 branch."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp":1735689607000}}
<!-- doc-c:fence=session-format.03 source=.agent-tasks/pi-rust-rewrite/fixtures/sessions/v2/branched.jsonl -->
```

## Migration on reopen

When this port reopens an older-format file it writes v3: the header gains
`"version":3`, every entry gains `id` and `parentId`, and the historical
prefix is preserved rather than regenerated: original timestamps, message
payloads, and existing v2 entry ids survive. The v1 fixture above was
rewritten by the Rust session layer as:

```text
{"type":"session","version":3,"id":"<uuid>","timestamp":"<ts>","cwd":"/tmp/pi-fixtures/cwd"}
{"type":"message","id": "<id>","parentId":null,"timestamp":"<ts>","message":{"role":"user","content":"V1 U1.","timestamp": <ms>}}
{"type":"message","id": "<id>","parentId": "<id>","timestamp":"<ts>","message":{"role":"assistant","content":[{"type":"text","text":"V1 A1."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp": <ms>}}
{"type":"message","id": "<id>","parentId": "<id>","timestamp":"<ts>","message":{"role":"user","content":"V1 U2.","timestamp": <ms>}}
{"type":"message","id": "<id>","parentId": "<id>","timestamp":"<ts>","message":{"role":"assistant","content":[{"type":"text","text":"V1 A2."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp": <ms>}}
{"type":"message","id": "<id>","parentId": "<id>","timestamp":"<ts>","message":{"role":"user","content":"V1 U3 kept.","timestamp": <ms>}}
{"type":"message","id": "<id>","parentId": "<id>","timestamp":"<ts>","message":{"role":"assistant","content":[{"type":"text","text":"V1 A3 kept."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp": <ms>}}
<!-- doc-c:fence=session-format.04 source=target/verification/docs-topics/harness/session-interop-v1-continued.jsonl -->
```

The interop verifier enforces the same contract on the TypeScript side: an
already-v3 file must survive reopen byte-identical, and a pre-v3 file must
come back with a v3 header. The run passed for all 27 produced files:

```ts
#!/usr/bin/env bun
/**
 * Offline cross-version session interoperability verifier.
 *
 * Generated source-pinned fixtures are opened and evolved by Rust's
 * SessionManager, then every resulting JSONL is reopened by the pinned
 * TypeScript SessionManager without rewriting its historical prefix.
 */

import { readdir, readFile, rm } from "node:fs/promises";
import { join, relative, resolve } from "node:path";

const REPO_ROOT = resolve(import.meta.dirname, "../..");
const FIXTURES = join(
	REPO_ROOT,
	".agent-tasks/pi-rust-rewrite/fixtures/sessions",
);
const OUTPUT = join(REPO_ROOT, "target/verification/session-interop");
const REFERENCE_ROOT = join(REPO_ROOT, ".references/pi");
const REFERENCE_UUID = join(REFERENCE_ROOT, "packages/agent/src/harness/session/uuid.ts");
const REFERENCE_AI_TYPES = join(REFERENCE_ROOT, "packages/ai/src/types.ts");

interface SessionManagerLike {
	getHeader(): Record<string, unknown> | null;
	getEntries(): readonly Record<string, unknown>[];
	getTree(): readonly unknown[];
	buildSessionContext(): { messages: readonly unknown[] };
	getLeafId(): string | null;
<!-- doc-c:fence=session-format.05 source=scripts/verification/session-interop.ts -->
```

## Record shapes

The v3 header is metadata only (no `id`/`parentId`); it carries the session
id, creation timestamp, working directory, and, for sessions produced by a
fork, a `parentSession` pointer to the source file. The first two lines of
the v3 basic fixture:

```text
{"type":"session","version":3,"id":"00000000-0000-7000-8000-000000000003","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/tmp/pi-fixtures/cwd"}
{"type":"message","id":"e0000001","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"user","content":"What is 2+2?","timestamp":1735689601000}}
{"type":"message","id":"e0000002","parentId":"e0000001","timestamp":"2025-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"2+2 = 4."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp":1735689602000}}
{"type":"message","id":"e0000003","parentId":"e0000002","timestamp":"2025-01-01T00:00:03.000Z","message":{"role":"user","content":"And 3+3?","timestamp":1735689603000}}
{"type":"message","id":"e0000004","parentId":"e0000003","timestamp":"2025-01-01T00:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"3+3 = 6."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp":1735689604000}}
<!-- doc-c:fence=session-format.06 source=.agent-tasks/pi-rust-rewrite/fixtures/sessions/v3/basic.jsonl -->
```

A fork's header points at the session it was copied from, as in this
Rust-produced fork of the basic session:

```text
{"type":"session","version":3,"id":"<uuid>","timestamp":"<ts>","cwd":"/target/verification/session-interop/v3/basic","parentSession":"/target/verification/session-interop/v3/basic/continued.jsonl"}
{"id": "<id>","message":{"content":"What is 2+2?","role":"user","timestamp": <ms>},"parentId":null,"timestamp":"<ts>","type":"message"}
{"id": "<id>","message":{"api":"anthropic-messages","content":[{"text":"2+2 = 4.","type":"text"}],"model":"claude-sonnet-4-5","provider":"anthropic","role":"assistant","stopReason":"stop","timestamp": <ms>,"usage":{"cacheRead":0,"cacheWrite":0,"input":10,"output":5}},"parentId": "<id>","timestamp":"<ts>","type":"message"}
{"id": "<id>","message":{"content":"And 3+3?","role":"user","timestamp": <ms>},"parentId": "<id>","timestamp":"<ts>","type":"message"}
{"id": "<id>","message":{"api":"anthropic-messages","content":[{"text":"3+3 = 6.","type":"text"}],"model":"claude-sonnet-4-5","provider":"anthropic","role":"assistant","stopReason":"stop","timestamp": <ms>,"usage":{"cacheRead":0,"cacheWrite":0,"input":10,"output":5}},"parentId": "<id>","timestamp":"<ts>","type":"message"}
{"id": "<id>","message":{"api":"anthropic-messages","content":[{"text":"Rust interop continuation","type":"text"}],"model":"claude-test","provider":"anthropic","role":"assistant","stopReason":"stop","timestamp":42,"usage":{"cacheRead":0,"cacheWrite":0,"cost":{"cacheRead":0.0,"cacheWrite":0.0,"input":0.0,"output":0.0,"total":0.0},"input":1,"output":1,"totalTokens":2}},"parentId": "<id>","timestamp":"<ts>","type":"message"}
<!-- doc-c:fence=session-format.07 source=target/verification/docs-topics/harness/session-interop-fork.jsonl -->
```

Message entries wrap a message object with `role` and `content`; assistant
messages additionally carry provider, model, usage, and stop-reason fields.
When the Rust session layer appends to a reopened file, the new entry follows
the same shape and chains onto the previous leaf:

```text
{"type":"session","version":3,"id":"<uuid>","timestamp":"<ts>","cwd":"/tmp/pi-fixtures/cwd"}
{"type":"message","id": "<id>","parentId":null,"timestamp":"<ts>","message":{"role":"user","content":"What is 2+2?","timestamp": <ms>}}
{"type":"message","id": "<id>","parentId": "<id>","timestamp":"<ts>","message":{"role":"assistant","content":[{"type":"text","text":"2+2 = 4."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp": <ms>}}
{"type":"message","id": "<id>","parentId": "<id>","timestamp":"<ts>","message":{"role":"user","content":"And 3+3?","timestamp": <ms>}}
{"type":"message","id": "<id>","parentId": "<id>","timestamp":"<ts>","message":{"role":"assistant","content":[{"type":"text","text":"3+3 = 6."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp": <ms>}}
<!-- doc-c:fence=session-format.08 source=target/verification/docs-topics/harness/session-interop-v3-continued.jsonl -->
```

A compaction entry records a summary of earlier entries, the id of the first
entry kept after the cut, and the token count before compaction:

```text
{"type":"session","version":3,"id":"<uuid>","timestamp":"<ts>","cwd":"/tmp/pi-fixtures/cwd"}
{"type":"message","id": "<id>","parentId":null,"timestamp":"<ts>","message":{"role":"user","content":"V1 U1.","timestamp": <ms>}}
{"type":"message","id": "<id>","parentId": "<id>","timestamp":"<ts>","message":{"role":"assistant","content":[{"type":"text","text":"V1 A1."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp": <ms>}}
{"type":"message","id": "<id>","parentId": "<id>","timestamp":"<ts>","message":{"role":"user","content":"V1 U2.","timestamp": <ms>}}
{"type":"message","id": "<id>","parentId": "<id>","timestamp":"<ts>","message":{"role":"assistant","content":[{"type":"text","text":"V1 A2."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp": <ms>}}
{"type":"message","id": "<id>","parentId": "<id>","timestamp":"<ts>","message":{"role":"user","content":"V1 U3 kept.","timestamp": <ms>}}
<!-- doc-c:fence=session-format.09 source=target/verification/docs-topics/harness/session-interop-v1-continued.jsonl -->
```

The fixture corpus also exercises entries this port preserves but does not
produce itself: a session display-name entry, a branch summary, and an opaque
entry type unknown to both implementations:

```text
{"type":"session","version":3,"id":"00000000-0000-7000-8000-000000000004","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/tmp/pi-fixtures/cwd"}
{"type":"message","id":"e0000001","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"user","content":"Start of main path.","timestamp":1735689601000}}
<!-- doc-c:fence=session-format.10 source=.agent-tasks/pi-rust-rewrite/fixtures/sessions/v3/branched-labels.jsonl -->
```

```text
{"type":"session","version":3,"id":"00000000-0000-7000-8000-000000000009","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/tmp/pi-fixtures/cwd","parentSession":"/tmp/pi-fixtures/source-session.jsonl"}
{"type":"message","id":"e0000001","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"user","content":"Start of main path.","timestamp":1735689601000}}
<!-- doc-c:fence=session-format.11 source=.agent-tasks/pi-rust-rewrite/fixtures/sessions/v3/branched-session.jsonl -->
```

```text
{"type":"session","version":3,"id":"00000000-0000-7000-8000-000000000006","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/tmp/pi-fixtures/cwd"}
{"type":"message","id":"e0000001","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"user","content":"Before unknown entry.","timestamp":1735689601000}}
{"type":"message","id":"e0000002","parentId":"e0000001","timestamp":"2025-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Reply before unknown."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp":1735689602000}}
{"type":"future_thing","id":"e0000003","parentId":"e0000002","timestamp":"2025-01-01T00:00:03.000Z","payload":{"kind":"quantum-annotations","nested":{"values":[1,2,3],"flag":true}},"enabled":true}
{"type":"message","id":"e0000004","parentId":"e0000003","timestamp":"2025-01-01T00:00:04.000Z","message":{"role":"user","content":"After unknown entry.","timestamp":1735689604000}}
{"type":"message","id":"e0000005","parentId":"e0000004","timestamp":"2025-01-01T00:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Reply after unknown."}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp":1735689605000}}
<!-- doc-c:fence=session-format.12 source=.agent-tasks/pi-rust-rewrite/fixtures/sessions/v3/unknown-entries.jsonl -->
```

The verifier asserts the TypeScript reopen retained the opaque
`future_thing` entry in its parsed entry list, not just in the on-disk bytes
(see the fence above from `scripts/verification/session-interop.ts`), and the
run passed, so forward-compatible unknown entry types survive a
Rust-write/TypeScript-reopen round trip.

For the CLI surface that creates and reopens these files, see
[sessions.md](sessions.md); for compaction behavior, see
[compaction.md](compaction.md).

## Pending port surface

- full entry-type and `AgentMessage` type reference for extension and SDK
  authors, including content-block and stop-reason unions — PAR-CLOSE
- `buildContextEntries()`/`buildSessionContext()` context-building semantics
  and the `SessionManager` API surface beyond open/append — PAR-CLOSE
- newer compaction entries embedding retained post-compaction context
  (`retainedTail`) alongside `firstKeptEntryId` — PAR-CLOSE
- interactive deletion from the session picker and `trash` CLI integration —
  unported-feature
- extension entry renderers (`registerEntryRenderer`) for custom entries —
  unported-feature
