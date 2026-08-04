import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import {
	AUTHORITATIVE_RPC_TYPES_PATH,
	assertFullCoverage,
	buildScenario,
	buildScenarioStep,
	canonicalStringify,
	deriveRpcCommandTypes,
	drainStream,
	normalizeTranscript,
	PumpErrorHolder,
	recordPumpFailure,
	scenarioCommandTypes,
	TranscriptWaiter,
} from "./rpc-parity.ts";

test("derives the authoritative RPC command set from the source-pinned union", () => {
	const derived = deriveRpcCommandTypes(readFileSync(AUTHORITATIVE_RPC_TYPES_PATH, "utf8"));
	expect(derived.length).toBe(32);
	expect(derived).toContain("prompt");
	expect(derived).toContain("get_commands");
	expect(derived).toContain("get_available_thinking_levels");
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

test("settle and harvest options compose on the same scenario step without loss", () => {
	// The step helper must spread both settle and harvest onto the SAME object,
	// not use if/return which silently drops one option. Build a step that asks
	// for both and require the returned object to carry both — separate
	// .some() checks across different steps cannot catch the dropped option.
	const step = buildScenarioStep("get_state", {}, { settle: true, harvest: () => {} }, 99);
	expect(step.settle).toBe(true);
	expect(typeof step.harvest).toBe("function");
});

test("manual RPC correlation ids are ordered from c23 onward with no collisions", () => {
	const scenario = buildScenario();
	const names = scenario.map((s) => s.name);
	expect(new Set(names).size).toBe(names.length);
	// The last step()-generated step is c22-follow_up; manual steps start at c23.
	expect(names).toContain("c22-follow_up");
	expect(names).not.toContain("c22-get_state-harvest");
	expect(names[scenario.length - 1]).toBe("c40-get_state-final");
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

test("streaming delta runs collapse; boundary events keep full payloads", () => {
	const update = (text: string) => ({ type: "message_update", message: { text } });
	const normalized = normalizeTranscript(
		[
			{ type: "message_start", message: { text: "" } },
			update("a"),
			update("ab"),
			update("abc"),
			{ type: "message_end", message: { text: "abc" } },
			update("x"),
			{ type: "tool_execution_update", partial: "1" },
			{ type: "tool_execution_update", partial: "12" },
			{ type: "agent_settled" },
		],
		{ volatileRoots: [], repoRoot: "/repo" },
	);
	expect(normalized).toEqual([
		{ type: "message_start", message: { text: "" } },
		{ type: "message_update", collapsed: true },
		{ type: "message_end", message: { text: "abc" } },
		{ type: "message_update", collapsed: true },
		{ type: "tool_execution_update", collapsed: true },
		{ type: "agent_settled" },
	]);
});

test("canonicalStringify ignores object key order but not values", () => {
	expect(canonicalStringify({ b: 1, a: [{ d: 2, c: 3 }] })).toBe(canonicalStringify({ a: [{ c: 3, d: 2 }], b: 1 }));
	expect(canonicalStringify({ a: 1 })).not.toBe(canonicalStringify({ a: 2 }));
});

test("a stderr stream failure is observed and cannot remain unhandled", async () => {
	const transcript = new TranscriptWaiter();
	const holder: PumpErrorHolder = { pumpError: null };
	const failure = new Error("stderr stream exploded");

	async function* explodingStream(): AsyncIterable<Uint8Array> {
		throw failure;
	}

	// A waiter is pending; the unguarded pump would have left it hanging forever.
	const pending = transcript.waitFor((record) => record.type === "response", 0, 60_000, "probe");
	// abort() rejects this waiter synchronously through the pump; observe the
	// rejection eagerly so it is never momentarily unhandled while we await.
	pending.catch(() => {});

	const stderrPump = drainStream(
		explodingStream(),
		() => {},
		(error) => recordPumpFailure(error, holder, transcript),
	);

	// The pump promise resolves on failure instead of becoming an unhandled rejection.
	await expect(stderrPump).resolves.toBeUndefined();
	// The real Error object is recorded in shared state, not stringified away.
	expect(holder.pumpError).toBe(failure);
	// The pending waiter was woken through the existing abort mechanism.
	await expect(pending).rejects.toBe(failure);
});

test("drainStream drains chunks normally and forwards a mid-stream failure without rejecting", async () => {
	const failure = new Error("stderr stream exploded");
	async function* partialStream(): AsyncIterable<Uint8Array> {
		yield new Uint8Array([0x68, 0x69]); // "hi"
		throw failure;
	}

	const chunks: Uint8Array[] = [];
	const failures: unknown[] = [];
	const pump = drainStream(partialStream(), (chunk) => chunks.push(chunk), (error) => failures.push(error));

	await expect(pump).resolves.toBeUndefined();
	expect(chunks.map((chunk) => new TextDecoder().decode(chunk))).toEqual(["hi"]);
	expect(failures).toEqual([failure]);
	expect(drainStream(null, () => {}, () => {})).resolves.toBeUndefined();
});

test("recordPumpFailure preserves the original diagnostic and aborts waiters with it", async () => {
	const transcript = new TranscriptWaiter();
	const holder: PumpErrorHolder = { pumpError: null };
	const original = new Error("stderr pump died");

	const pending = transcript.waitFor((record) => record.type === "response", 0, 60_000, "probe");
	pending.catch(() => {});

	// A real Error is preserved by reference, not reduced to a string message.
	recordPumpFailure(original, holder, transcript);
	expect(holder.pumpError).toBe(original);
	await expect(pending).rejects.toBe(original);

	// A non-Error is normalized to a real Error carrying the original text.
	const holder2: PumpErrorHolder = { pumpError: null };
	recordPumpFailure("raw stderr fault", holder2, { abort: () => {} });
	expect(holder2.pumpError).toBeInstanceOf(Error);
	expect((holder2.pumpError as Error).message).toBe("raw stderr fault");

	// The first failure owns the shared slot, so the original diagnostic survives a later one.
	const shared: PumpErrorHolder = { pumpError: null };
	const noop = { abort: () => {} };
	recordPumpFailure(new Error("first"), shared, noop);
	recordPumpFailure(new Error("second"), shared, noop);
	expect(shared.pumpError?.message).toBe("first");
});
