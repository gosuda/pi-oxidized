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
	// Generated ids map in first-seen order and stay referentially consistent.
	expect(data.sessionId).toBe("<uuid-1>");
	expect(data.sessionFile).toBe("<tmp>/sessions/<ts>_<uuid-1>.jsonl");
	expect(data.entryId).toBe("<id-2>");
	expect(data.parentId).toBe("<id-2>");
	expect(data.leafId).toBe("<id-3>");
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
