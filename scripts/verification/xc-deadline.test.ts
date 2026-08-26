import { describe, expect, test } from "bun:test";

import {
	loadXcDeadlineInputs,
	REPO_ROOT,
	runXcDeadlineWitnesses,
	verifyCancelRouting,
	verifyErrorIsolation,
	verifyHookDeadlineConstant,
	verifyInputQueueCapacity,
	verifyNavigateTreeSummarizeExemption,
	verifyStaleReplacementTokenGuard,
	verifyTerminalInputDeadline,
} from "./xc-deadline.ts";

const INPUTS = loadXcDeadlineInputs(REPO_ROOT);

describe("XC-8 deadline, cancellation, error-isolation, stale-guard witnesses", () => {
	test("real repository passes every XC-8 witness", () => {
		expect(runXcDeadlineWitnesses(INPUTS)).toEqual([]);
	});

	// --- 30 s hook deadline constant ---

	test("EXTENSION_HOOK_TIMEOUT_MS is pinned at 30000", () => {
		expect(verifyHookDeadlineConstant(INPUTS.hostSource)).toEqual([]);
	});

	test("M15-adjacent: changing EXTENSION_HOOK_TIMEOUT_MS fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/EXTENSION_HOOK_TIMEOUT_MS\s*=\s*30_000/,
			"EXTENSION_HOOK_TIMEOUT_MS = 60_000",
		);
		expect(verifyHookDeadlineConstant(mutated)).not.toEqual([]);
	});

	// --- Input queue capacity-64 ---

	test("EXTENSION_INPUT_QUEUE_CAPACITY is pinned at 64", () => {
		expect(verifyInputQueueCapacity(INPUTS.hostSource)).toEqual([]);
	});

	test("changing EXTENSION_INPUT_QUEUE_CAPACITY fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/EXTENSION_INPUT_QUEUE_CAPACITY\s*=\s*64/,
			"EXTENSION_INPUT_QUEUE_CAPACITY = 128",
		);
		expect(verifyInputQueueCapacity(mutated)).not.toEqual([]);
	});

	// --- M15: terminal-input 4 ms deadline ---

	test("M15: terminal-input 4 ms deadline is present", () => {
		expect(verifyTerminalInputDeadline(INPUTS.hostSource, INPUTS.scalingTestSource)).toEqual([]);
	});

	test("M15 mutation: removing EXTENSION_INPUT_TIMEOUT_MS fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/export const EXTENSION_INPUT_TIMEOUT_MS = 4;/,
			"/* removed */",
		);
		expect(verifyTerminalInputDeadline(mutated, INPUTS.scalingTestSource)).not.toEqual([]);
	});

	test("M15 mutation: changing the timeout value fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/EXTENSION_INPUT_TIMEOUT_MS\s*=\s*4/,
			"EXTENSION_INPUT_TIMEOUT_MS = 100",
		);
		expect(verifyTerminalInputDeadline(mutated, INPUTS.scalingTestSource)).not.toEqual([]);
	});

	test("M15 mutation: removing the timeout race fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/setTimeout\s*\(\s*\(\)\s*=>\s*resolve\s*\(\s*\{\s*kind:\s*"timeout"\s*\}\s*\)\s*,\s*EXTENSION_INPUT_TIMEOUT_MS\s*\)/,
			"/* timeout race removed */",
		);
		expect(verifyTerminalInputDeadline(mutated, INPUTS.scalingTestSource)).not.toEqual([]);
	});

	test("M15 mutation: removing the scaling test deadline assertion fails the witness", () => {
		const mutated = INPUTS.scalingTestSource.replace(
			/toBeGreaterThanOrEqual\s*\(\s*EXTENSION_INPUT_TIMEOUT_MS/,
			"/* deadline assertion removed */",
		);
		expect(verifyTerminalInputDeadline(INPUTS.hostSource, mutated)).not.toEqual([]);
	});

	test("M15 mutation: removing the disable-on-timeout path fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/outcome\.kind\s*===\s*"timeout"\s*\|\|\s*outcome\.kind\s*===\s*"error"/,
			"/* disable path removed */ false",
		);
		expect(verifyTerminalInputDeadline(mutated, INPUTS.scalingTestSource)).not.toEqual([]);
	});

	// --- M16: error isolation ---

	test("M16: error isolation is present in both host and lean-runner", () => {
		expect(
			verifyErrorIsolation(INPUTS.hostSource, INPUTS.leanSource, INPUTS.acceptanceTestSource),
		).toEqual([]);
	});

	test("M16 mutation: removing host.ts handleLifecycleHook catch fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/catch\s*\(\s*err\s*\)\s*\{\s*\n\s*await this\.client\.respondError\s*\(\s*id,\s*eventType as Method,\s*\{[^}]*extension_error/,
			"/* catch removed — rethrow */ throw err;",
		);
		expect(
			verifyErrorIsolation(mutated, INPUTS.leanSource, INPUTS.acceptanceTestSource),
		).not.toEqual([]);
	});

	test("M16 mutation: removing lean-runner runHooks per-handler catch fails the witness", () => {
		const mutated = INPUTS.leanSource.replace(
			/catch\s*\(\s*err\s*\)\s*\{\s*\n\s*this\.emitExtensionError\s*\(\s*extensionPath,\s*eventType/,
			"/* catch removed — rethrow */ throw err;",
		);
		expect(
			verifyErrorIsolation(INPUTS.hostSource, mutated, INPUTS.acceptanceTestSource),
		).not.toEqual([]);
	});

	test("M16 mutation: removing lean-runner handleLifecycleHook catch fails the witness", () => {
		// Target the handleLifecycleHook catch specifically: it's the one
		// where catch(err) is followed by respondError(id, eventType...extension_error.
		const mutated = INPUTS.leanSource.replace(
			/catch\s*\(\s*err\s*\)\s*\{\s*\n\s*await this\.client\.respondError\s*\(\s*id,\s*eventType as Method,\s*\{\s*\n\s*code:\s*"extension_error"/,
			"/* catch removed — rethrow */ throw err;",
		);
		expect(
			verifyErrorIsolation(INPUTS.hostSource, mutated, INPUTS.acceptanceTestSource),
		).not.toEqual([]);
	});

	test("M16 mutation: removing crash isolation test suite fails the witness", () => {
		const mutated = INPUTS.acceptanceTestSource.replace(
			/describe\s*\(\s*["']acceptance:\s*crash isolation["']/,
			'describe("acceptance: removed crash isolation"',
		);
		expect(
			verifyErrorIsolation(INPUTS.hostSource, INPUTS.leanSource, mutated),
		).not.toEqual([]);
	});

	// --- M17: stale replacement token guard ---

	test("M17: stale replacement token guard is present", () => {
		expect(verifyStaleReplacementTokenGuard(INPUTS.hostSource, INPUTS.hostTestSource)).toEqual([]);
	});

	test("M17 mutation: removing markStale call fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/markStale\s*\?\.\s*\(\s*\)/,
			"/* markStale removed */",
		);
		expect(verifyStaleReplacementTokenGuard(mutated, INPUTS.hostTestSource)).not.toEqual([]);
	});

	test("M17 mutation: removing the stale guard in createCommandContext fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/if\s*\(\s*stale\s*\|\|\s*this\.runner\s*!==\s*runner\s*\)/,
			"/* guard removed */ if (false)",
		);
		expect(verifyStaleReplacementTokenGuard(mutated, INPUTS.hostTestSource)).not.toEqual([]);
	});

	test("M17 mutation: removing STALE_COMMAND_CONTEXT_MESSAGE fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/STALE_COMMAND_CONTEXT_MESSAGE\s*=/,
			"/* removed */ REMOVED_MESSAGE =",
		);
		expect(verifyStaleReplacementTokenGuard(mutated, INPUTS.hostTestSource)).not.toEqual([]);
	});

	test("M17 mutation: removing the staleness test suite fails the witness", () => {
		const mutated = INPUTS.hostTestSource.replace(
			/describe\s*\(\s*["']host:\s*per-command replacement staleness["']/,
			'describe("host: removed staleness"',
		);
		expect(verifyStaleReplacementTokenGuard(INPUTS.hostSource, mutated)).not.toEqual([]);
	});

	test("M17 mutation: moving markStale after token extraction fails the witness", () => {
		// Move markStale?.() to after the token extraction line, so the
		// ordering check fires.
		const src = INPUTS.hostSource;
		const markStaleLine = src.match(/^[ \t]*markStale\?\.\(\);$/m);
		if (!markStaleLine) throw new Error("could not find markStale line");
		const tokenLine = src.match(/^[ \t]*const token = payload\["replacementToken"\];$/m);
		if (!tokenLine) throw new Error("could not find token extraction line");
		// Remove markStale from its original position and insert it after the token line.
		const withoutMarkStale = src.replace(/^[ \t]*markStale\?\.\(\);\n/m, "");
		const mutated = withoutMarkStale.replace(
			/([ \t]*const token = payload\["replacementToken"\];\n)/,
			"$1\t\tmarkStale?.();\n",
		);
		expect(verifyStaleReplacementTokenGuard(mutated, INPUTS.hostTestSource)).not.toEqual([]);
	});

	// --- M18: cancel routing ---

	test("M18: cancel routing is present in both host and lean-runner", () => {
		expect(verifyCancelRouting(INPUTS.hostSource, INPUTS.leanSource, INPUTS.leanTestSource)).toEqual([]);
	});

	test("M18 mutation: dropping requestId extraction in host.ts fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/typeof\s+payload\s*\[\s*["']id["']\s*\]\s*===\s*["']number["']\s*\?\s*payload\s*\[\s*["']id["']\s*\]\s*:\s*undefined/,
			"/* requestId dropped */ undefined",
		);
		expect(verifyCancelRouting(mutated, INPUTS.leanSource, INPUTS.leanTestSource)).not.toEqual([]);
	});

	test("M18 mutation: dropping requestId extraction in lean-runner fails the witness", () => {
		const mutated = INPUTS.leanSource.replace(
			/typeof\s+payload\s*\[\s*["']id["']\s*\]\s*===\s*["']number["']\s*\?\s*payload\s*\[\s*["']id["']\s*\]\s*:\s*undefined/,
			"/* requestId dropped */ undefined",
		);
		expect(verifyCancelRouting(INPUTS.hostSource, mutated, INPUTS.leanTestSource)).not.toEqual([]);
	});

	test("M18 mutation: removing the undefined-requestId guard in host.ts fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/if\s*\(\s*requestId\s*===\s*undefined\s*\)\s*return\s*;/,
			"/* guard removed */",
		);
		expect(verifyCancelRouting(mutated, INPUTS.leanSource, INPUTS.leanTestSource)).not.toEqual([]);
	});

	test("M18 mutation: replacing keyed abort with abort-all in host.ts fails the witness", () => {
		const mutated = INPUTS.hostSource
			.replace(
				/this\.inFlightTools\.get\s*\(\s*requestId\s*\)\s*\?\.\s*abort\s*\(\s*\)/,
				"this.inFlightTools.forEach((c) => c.abort()) /* abort-all */",
			)
			.replace(
				/this\.inFlightProviders\.get\s*\(\s*requestId\s*\)\s*\?\.\s*abort\s*\(\s*\)/,
				"this.inFlightProviders.forEach((c) => c.abort()) /* abort-all */",
			);
		expect(verifyCancelRouting(mutated, INPUTS.leanSource, INPUTS.leanTestSource)).not.toEqual([]);
	});

	test("M18 mutation: removing the lean test cancel assertion fails the witness", () => {
		const mutated = INPUTS.leanTestSource.replace(
			/err\s*\[\s*["']code["']\s*\]\s*\)\s*\.toBe\s*\(\s*["']cancelled["']/g,
			"/* assertion removed */",
		);
		expect(verifyCancelRouting(INPUTS.hostSource, INPUTS.leanSource, mutated)).not.toEqual([]);
	});

	// --- navigateTree summarize:true exemption ---

	test("navigateTree summarize:true exemption is witnessed with intent", () => {
		expect(verifyNavigateTreeSummarizeExemption(INPUTS.hostSource)).toEqual([]);
	});

	test("navigateTree exemption mutation: removing the conditional timeout fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/summarize\s*\?\s*\{\s*\}\s*:\s*\{\s*timeoutMs:\s*EXTENSION_HOOK_TIMEOUT_MS\s*\}/,
			"{ timeoutMs: EXTENSION_HOOK_TIMEOUT_MS } /* exemption removed */",
		);
		expect(verifyNavigateTreeSummarizeExemption(mutated)).not.toEqual([]);
	});

	test("navigateTree exemption mutation: removing the intent comment fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/Summarized navigation delegates to a provider-backed branch/,
			"/* intent comment removed */",
		);
		expect(verifyNavigateTreeSummarizeExemption(mutated)).not.toEqual([]);
	});
});
