import { afterAll, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
	AUTHORITATIVE_RPC_TYPES_PATH,
	assertFullCoverage,
	buildScenario,
	canonicalStringify,
	createScenarioStepBuilder,
	deriveRpcCommandTypes,
	driveBinary,
	executeScenarioStep,
	MAX_STDOUT_BUFFER_CHARS,
	normalizeTranscript,
	scenarioCommandTypes,
	type ScenarioState,
	TranscriptWaiter,
} from "./rpc-parity.ts";

// Any rejection that escapes the driver's pump/exit observation would have
// crashed the old implementation; record and assert none occurred.
const unhandledRejections: unknown[] = [];
function recordUnhandledRejection(reason: unknown): void {
	unhandledRejections.push(reason);
}
process.on("unhandledRejection", recordUnhandledRejection);

const tempRoots: string[] = [];

afterAll(() => {
	process.off("unhandledRejection", recordUnhandledRejection);
	for (const root of tempRoots) rmSync(root, { recursive: true, force: true });
	expect(unhandledRejections).toEqual([]);
});

test("derives the authoritative RPC command set from the source-pinned union", () => {
	const derived = deriveRpcCommandTypes(readFileSync(AUTHORITATIVE_RPC_TYPES_PATH, "utf8"));
	expect(derived.length).toBe(31);
	expect(derived).toContain("prompt");
	expect(derived).toContain("get_commands");
	expect(derived).toContain("switch_session");
	expect(new Set(derived).size).toBe(derived.length);
});

test("derivation is scoped to the RpcCommand union, not the whole file", () => {
	const source = [
		'export type RpcCommand =\n\t| { id?: string; type: "alpha" }\n\t| { id?: string; type: "beta"; level: ThinkingLevel };\n',
		'export type Other = { type: "gamma" };\n',
	].join("\n");
	expect(deriveRpcCommandTypes(source)).toEqual(["alpha", "beta"]);
});

test("scenario covers every authoritative command exactly", () => {
	const derived = deriveRpcCommandTypes(readFileSync(AUTHORITATIVE_RPC_TYPES_PATH, "utf8"));
	const scenario = buildScenario();
	expect(() => assertFullCoverage(scenarioCommandTypes(scenario), derived)).not.toThrow();
	// Correlation ids must be unique so responses match one step each.
	const names = scenario.map((step) => step.name);
	expect(new Set(names).size).toBe(names.length);
});

test("scenario inspects replacement sessions before switching away", () => {
	const commandTypes = buildScenario().map((step) => step.commandType);
	const clone = commandTypes.indexOf("clone");
	expect(commandTypes.slice(clone, clone + 5)).toEqual([
		"clone",
		"get_state",
		"new_session",
		"get_state",
		"switch_session",
	]);
});

test("a newly added authoritative command cannot silently escape", () => {
	const derived = deriveRpcCommandTypes(readFileSync(AUTHORITATIVE_RPC_TYPES_PATH, "utf8"));
	const scenario = scenarioCommandTypes(buildScenario());
	expect(() => assertFullCoverage(scenario, [...derived, "future_command"])).toThrow(/future_command/);
});

test("a scenario command missing from the authoritative set fails coverage", () => {
	const derived = deriveRpcCommandTypes(readFileSync(AUTHORITATIVE_RPC_TYPES_PATH, "utf8"));
	const scenario = new Set(scenarioCommandTypes(buildScenario()));
	scenario.add("not_a_real_command");
	expect(() => assertFullCoverage(scenario, derived)).toThrow(/not_a_real_command/);
});

test("normalization scrubs only generated ids, timestamps, and temp paths", () => {
	const record = {
		type: "response",
		id: "c01-get_state",
		data: {
			sessionId: "0198e3a0-1111-7000-8000-abcdef012345",
			sessionFile: "/run/root/sessions/2026-07-26T14-05-07-968Z_0198e3a0-1111-7000-8000-abcdef012345.jsonl",
			entryId: "c4bbb1af",
			parentId: "c4bbb1af",
			leafId: "d18f1557",
			timestamp: 1785074709332,
			durationMs: 42,
			model: { id: "model", provider: "verification" },
			text: "verification-chunk-0001",
			createdAt: "2026-07-26T14:05:09.326Z",
		},
	};
	const [normalized] = normalizeTranscript([record], {
		volatileRoots: ["/run/root"],
		repoRoot: "/repo",
	}) as [Record<string, unknown>];
	const data = normalized.data as Record<string, unknown>;
	// Generated ids map in sorted-key order (deterministic regardless of
	// insertion order) and stay referentially consistent.
	expect(data.entryId).toBe("<id-1>");
	expect(data.parentId).toBe("<id-1>");
	expect(data.leafId).toBe("<id-2>");
	expect(data.sessionId).toBe("<uuid-3>");
	expect(data.sessionFile).toBe("<tmp>/sessions/<ts>_<uuid-3>.jsonl");
	// Timestamps and elapsed-time values are zeroed/blanked.
	expect(data.timestamp).toBe(0);
	expect(data.durationMs).toBe(0);
	expect(data.createdAt).toBe("<ts>");
	// Non-generated values stay literal so real divergences cannot hide.
	expect(normalized.id).toBe("c01-get_state");
	expect((data.model as Record<string, unknown>).id).toBe("model");
	expect(data.text).toBe("verification-chunk-0001");
});

test("normalization preserves every streaming update and its payload", () => {
	const records: Parameters<typeof normalizeTranscript>[0] = [
		{ type: "message_start", message: { text: "" } },
		{ type: "message_update", message: { text: "a" } },
		{ type: "message_update", message: { text: "ab" } },
		{ type: "tool_execution_update", partialResult: { content: [] } },
		{ type: "tool_execution_update", partialResult: { content: [{ text: "bash\n" }], details: {} } },
		{ type: "agent_settled" },
	];

	expect(
		normalizeTranscript(records, { volatileRoots: [], repoRoot: "/repo" }),
	).toEqual([...records]);
});

test("canonicalStringify ignores object key order but not values", () => {
	expect(canonicalStringify({ b: 1, a: [{ d: 2, c: 3 }] })).toBe(canonicalStringify({ a: [{ c: 3, d: 2 }], b: 1 }));
	expect(canonicalStringify({ a: 1 })).not.toBe(canonicalStringify({ a: 2 }));
});

test("normalization orders ignorable/unicode keys by code unit, not locale", () => {
	// U+00E9 (precomposed é) and e + U+0301 (decomposed) are distinct code-unit
	// sequences but collate equal in many locales. A localeCompare-based sort
	// would preserve insertion order for these distinct keys and make generated-id
	// placeholder allocation depend on field order.
	const k1 = "e" + String.fromCodePoint(0x0301);
	const k2 = String.fromCharCode(0x00e9);
	const uuid1 = "0198e3a0-1111-7000-8000-abcdef012345";
	const uuid2 = "0198e3a0-2222-7000-8000-abcdef012345";
	const ctx = { volatileRoots: [], repoRoot: "/repo" };
	const a = { type: "response", data: { [k1]: uuid1, [k2]: uuid2 } };
	const b = { type: "response", data: { [k2]: uuid2, [k1]: uuid1 } };
	const [normA] = normalizeTranscript([a], ctx) as [Record<string, unknown>];
	const [normB] = normalizeTranscript([b], ctx) as [Record<string, unknown>];
	expect(canonicalStringify(normA as never)).toBe(canonicalStringify(normB as never));
	const dataA = normA.data as Record<string, unknown>;
	const dataB = normB.data as Record<string, unknown>;
	expect(dataA[k1]).toBe(dataB[k1]);
	expect(dataA[k2]).toBe(dataB[k2]);
	expect(dataA[k1]).toBe("<uuid-1>");
	expect(dataA[k2]).toBe("<uuid-2>");
});

test("normalization maps generated ids identically for reordered nested objects", () => {
	// Two semantically equal objects whose fields arrive in different
	// insertion order must allocate the same generated-id placeholders,
	// including inside nested objects. Before the sorted-key traversal
	// fix, the leafId/entryId/sessionId fields were visited in insertion
	// order, so the <id-N>/<uuid-N> tokens diverged and canonicalStringify
	// flagged a false mismatch.
	const orderA = {
		type: "response",
		data: {
			leafId: "d18f1557",
			entryId: "c4bbb1af",
			parentId: "c4bbb1af",
			sessionId: "0198e3a0-1111-7000-8000-abcdef012345",
			nested: { zeta: "a1b2c3d4", entryId: "e5f67890", alpha: "literal-stays" },
		},
	};
	const orderB = {
		type: "response",
		data: {
			sessionId: "0198e3a0-1111-7000-8000-abcdef012345",
			parentId: "c4bbb1af",
			entryId: "c4bbb1af",
			leafId: "d18f1557",
			nested: { alpha: "literal-stays", entryId: "e5f67890", zeta: "a1b2c3d4" },
		},
	};
	const ctx = { volatileRoots: [], repoRoot: "/repo" };
	const [a] = normalizeTranscript([orderA], ctx) as [Record<string, unknown>];
	const [b] = normalizeTranscript([orderB], ctx) as [Record<string, unknown>];
	// Both orderings must canonicalize to the same string — i.e. the
	// generated-id placeholders were allocated in the same deterministic
	// (sorted-key) sequence, at every nesting depth.
	expect(canonicalStringify(a as never)).toBe(canonicalStringify(b as never));
	const data = a.data as Record<string, unknown>;
	// Spot-check the placeholder assignment is sorted-key driven:
	// entryId/leafId/parentId sort before sessionId at the top level, and
	// the nested object's entryId (an ID_KEY) is visited after the top
	// level — so it gets <id-3>, not a number that depends on field order.
	expect(data.entryId).toBe("<id-1>");
	expect(data.parentId).toBe("<id-1>");
	expect(data.leafId).toBe("<id-2>");
	expect(data.sessionId).toBe("<uuid-4>");
	const nested = data.nested as Record<string, unknown>;
	expect(nested.entryId).toBe("<id-3>");
	// Non-id-keyed fields stay literal regardless of order.
	expect(nested.alpha).toBe("literal-stays");
	expect(nested.zeta).toBe("a1b2c3d4");
});

test("normalization keeps cross-references consistent across reordered records", () => {
	// A later record references a generated id produced by an earlier one.
	// Both transcripts carry the same ids but in different field order; the
	// cross-reference must still resolve to the same placeholder in both.
	const baseRecords = (
		firstFieldOrder: "entry" | "leaf",
	): Parameters<typeof normalizeTranscript>[0] => [
		{
			type: "response",
			id: "c01-fetch",
			data: firstFieldOrder === "entry"
				? { entryId: "c4bbb1af", leafId: "d18f1557" }
				: { leafId: "d18f1557", entryId: "c4bbb1af" },
		},
		{
			type: "response",
			id: "c02-related",
			data: { targetId: "c4bbb1af", fromId: "d18f1557" },
		},
	];
	const ctx = { volatileRoots: [], repoRoot: "/repo" };
	const a = normalizeTranscript(baseRecords("entry"), ctx);
	const b = normalizeTranscript(baseRecords("leaf"), ctx);
	expect(canonicalStringify(a[0] as never)).toBe(canonicalStringify(b[0] as never));
	expect(canonicalStringify(a[1] as never)).toBe(canonicalStringify(b[1] as never));
	// The cross-reference in the second record must match the first
	// record's allocation: entryId "c4bbb1af" -> <id-1>, leafId "d18f1557"
	// -> <id-2>, regardless of which field was listed first.
	const first = (a[0] as Record<string, unknown>).data as Record<string, unknown>;
	const second = (a[1] as Record<string, unknown>).data as Record<string, unknown>;
	expect(first.entryId).toBe("<id-1>");
	expect(first.leafId).toBe("<id-2>");
	expect(second.targetId).toBe("<id-1>");
	expect(second.fromId).toBe("<id-2>");
	// The reordered transcript must agree on the cross-reference too.
	const secondB = (b[1] as Record<string, unknown>).data as Record<string, unknown>;
	expect(secondB.targetId).toBe("<id-1>");
	expect(secondB.fromId).toBe("<id-2>");
});

// ============================================================================
// Step builder (N12/N13)
// ============================================================================

test("step builder renumbers downstream ids when a step is inserted", () => {
	const original = createScenarioStepBuilder();
	expect([original("get_state", {}), original("compact", {})].map((scenarioStep) => scenarioStep.name)).toEqual([
		"c01-get_state",
		"c02-compact",
	]);
	const inserted = createScenarioStepBuilder();
	expect(
		[inserted("get_state", {}), inserted("abort", {}), inserted("compact", {})].map(
			(scenarioStep) => scenarioStep.name,
		),
	).toEqual(["c01-get_state", "c02-abort", "c03-compact"]);
});

test("every scenario id is builder-generated, monotonic c01…c41", () => {
	const scenario = buildScenario();
	expect(scenario.length).toBe(41);
	const state: ScenarioState = {
		workDir: "/tmp/rpc-parity-ids",
		sessionFile: "/tmp/rpc-parity-ids/session.jsonl",
		firstEntryId: "entry-1",
		forkEntryId: "fork-1",
	};
	scenario.forEach((scenarioStep, index) => {
		expect(scenarioStep.name.startsWith(`c${String(index + 1).padStart(2, "0")}-`)).toBe(true);
		expect(scenarioStep.build(state).id).toBe(scenarioStep.name);
	});
	const probe = scenario.find((scenarioStep) => scenarioStep.commandType === undefined);
	expect(probe?.name).toBe("c40-unknown-probe");
	expect(probe?.build(state).type).toBe("rpc_parity_probe");
});

test("a step with both settle and harvest retains and executes both", async () => {
	const builder = createScenarioStepBuilder();
	const scenarioStep = builder(
		"get_state",
		{},
		{
			settle: true,
			harvest: (response, state) => {
				const data = response.data as { sessionFile?: string } | undefined;
				state.sessionFile = data?.sessionFile ?? "harvest-failed";
			},
		},
	);
	expect(scenarioStep.settle).toBe(true);
	expect(typeof scenarioStep.harvest).toBe("function");

	const transcript = new TranscriptWaiter();
	const state: ScenarioState = { workDir: "/tmp/rpc-parity-step" };
	const sent: unknown[] = [];
	let completed = false;
	const running = executeScenarioStep(scenarioStep, state, transcript, (command) => {
		sent.push(command);
	}, "fake").then(() => {
		completed = true;
	});

	transcript.push({ type: "response", id: "c01-get_state", success: true, data: { sessionFile: "/tmp/session.jsonl" } });
	// Deterministic microtask drain (no wall-clock wait): the step has no
	// timers to fire, so if the settle gate were missing it would complete
	// within these hops.
	for (let hop = 0; hop < 16; hop++) await Promise.resolve();
	// The response alone must not complete the step: settle still gates it,
	// and harvest only runs once the step is fully observed.
	expect(completed).toBe(false);
	expect(state.sessionFile).toBeUndefined();

	transcript.push({ type: "agent_settled" });
	await running;
	expect(completed).toBe(true);
	expect(state.sessionFile).toBe("/tmp/session.jsonl");
	expect(sent).toEqual([{ id: "c01-get_state", type: "get_state" }]);
});

// ============================================================================
// TranscriptWaiter failure channel (N02/N03)
// ============================================================================

test("TranscriptWaiter rejects pending and late waiters with the stored first failure", async () => {
	const transcript = new TranscriptWaiter();
	const pending = transcript.waitFor((record) => record.type === "response", 0, 60_000, "response");
	const first = new Error("pump failed first");
	transcript.fail(first);
	transcript.fail(new Error("pump failed second"));
	await expect(pending).rejects.toBe(first);
	expect(transcript.failure).toBe(first);
	// Late waiters reject immediately with the same stored error, even when
	// a matching record arrived after the failure.
	transcript.push({ type: "response", id: "c01-late" });
	await expect(transcript.waitFor((record) => record.type === "response", 0, 60_000, "late")).rejects.toBe(first);
});

test("TranscriptWaiter timeout removes its waiter; a later failure stays clean", async () => {
	const transcript = new TranscriptWaiter();
	await expect(transcript.waitFor(() => true, 0, 1, "never-arrives")).rejects.toThrow(
		"timed out after 1ms waiting for never-arrives",
	);
	// The timed-out waiter is gone: failing now must not double-settle it.
	const late = new Error("failure after timeout");
	transcript.fail(late);
	await expect(transcript.waitFor(() => true, 0, 60_000, "post-failure")).rejects.toBe(late);
});

test("TranscriptWaiter resolves from the requested index and via the fast path", async () => {
	const transcript = new TranscriptWaiter();
	transcript.push({ type: "response", id: "old" });
	const next = transcript.waitFor((record) => record.type === "response", transcript.records.length, 60_000, "next");
	transcript.push({ type: "note" });
	transcript.push({ type: "response", id: "new" });
	await expect(next).resolves.toBe(2);
	await expect(transcript.waitFor((record) => record.id === "old", 0, 60_000, "old")).resolves.toBe(0);
});

// ============================================================================
// Drive lifecycle against misbehaving children (N02/N03)
// ============================================================================

// The fixture scripts below run inside the SPAWNED child, not in this test
// process. Their `Bun.sleep(60_000)` never elapses: it pins the child alive
// so the observed failure is deterministically the pump's (not a racing
// child exit); the driver's failure path SIGKILLs the child immediately.

function tempRunRoot(): string {
	const dir = mkdtempSync(join(tmpdir(), "rpc-parity-fake-"));
	tempRoots.push(dir);
	return dir;
}

async function expectDriveFailure(argv: readonly string[], pattern: RegExp): Promise<{ message: string; runRoot: string }> {
	const runRoot = tempRunRoot();
	const startedAt = Date.now();
	let caught: unknown;
	try {
		await driveBinary("fake", argv, runRoot);
	} catch (error) {
		caught = error;
	}
	const elapsedMs = Date.now() - startedAt;
	expect(caught).toBeInstanceOf(Error);
	const message = (caught as Error).message;
	expect(message).toMatch(pattern);
	// Prompt failure: nowhere near the 120s step deadline.
	expect(elapsedMs).toBeLessThan(20_000);
	return { message, runRoot };
}

test("early child exit fails the pending waiter promptly with evidence", async () => {
	const { message, runRoot } = await expectDriveFailure(
		[process.execPath, "-e", "process.exit(0);"],
		/exited with code 0 before the scenario completed/,
	);
	const partialPath = join(runRoot, "partial-transcript.jsonl");
	expect(message).toContain(`fake partial transcript: ${partialPath} (0 records)`);
	expect(message).toContain("fake stderr tail:");
	expect(existsSync(partialPath)).toBe(true);
}, 30_000);

test("malformed stdout JSON fails the active waiter and keeps decoded evidence", async () => {
	const script = [
		'console.log(JSON.stringify({ type: "response", id: "c01-get_state", success: true, data: {} }));',
		'console.log("rpc-parity-not-json");',
		"await Bun.sleep(60_000);",
	].join(" ");
	const { message, runRoot } = await expectDriveFailure(
		[process.execPath, "-e", script],
		/non-JSON stdout line: "rpc-parity-not-json"/,
	);
	expect(message).toContain("(1 records)");
	expect(readFileSync(join(runRoot, "partial-transcript.jsonl"), "utf8")).toContain("c01-get_state");
}, 30_000);

test("a non-object stdout JSON value fails the waiter", async () => {
	await expectDriveFailure(
		[process.execPath, "-e", 'console.log("42"); await Bun.sleep(60_000);'],
		/stdout line is not a JSON object/,
	);
}, 30_000);

test("an unterminated stdout line beyond 64 KiB fails promptly", async () => {
	const overflowLength = MAX_STDOUT_BUFFER_CHARS + 4_096;
	const script = `require("node:fs").writeSync(1, "a".repeat(${overflowLength})); await Bun.sleep(60_000);`;
	await expectDriveFailure([process.execPath, "-e", script], /stdout unterminated line exceeded 65536 characters/);
}, 30_000);

test("a truncated stdout tail at EOF fails instead of waiting out the deadline", async () => {
	// `exec 1>&- 2>&-` closes the real pipe write ends while sh stays alive,
	// so the pump observes EOF (not a racing child exit) with a non-empty
	// unterminated tail, and the `sleep` grandchild inherits no pipe that
	// could delay pump settlement. bun cannot stage this: its runtime holds
	// a dup of fd 1, so closeSync(1) leaves the pipe open until death.
	const script = "printf '{\"truncated\":'; exec 1>&- 2>&-; sleep 30";
	await expectDriveFailure(["sh", "-c", script], /stdout ended with a truncated line/);
}, 30_000);
