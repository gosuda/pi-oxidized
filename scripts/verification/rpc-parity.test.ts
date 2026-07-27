import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import {
	AUTHORITATIVE_RPC_TYPES_PATH,
	assertFullCoverage,
	buildScenario,
	canonicalStringify,
	deriveRpcCommandTypes,
	normalizeTranscript,
	scenarioCommandTypes,
} from "./rpc-parity.ts";

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
