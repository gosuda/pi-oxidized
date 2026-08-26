/**
 * XC-8 deadline, cancellation, error-isolation, and stale-guard witnesses.
 *
 * Static witnesses that verify the TypeScript reference code implements the
 * deadlines, cancellation routing, error isolation, and stale-command-context
 * guard from docs/extension-compatibility-contract.md section 10. Each witness
 * targets one mutation (M15–M18): if the referenced guard or logic is removed
 * from the reference source, the witness reports a violation.
 *
 * Mutations:
 *  M15 — terminal-input 4 ms deadline disabled → scaling tests fail
 *  M16 — hook throw crashes host instead of emitting extensionError
 *  M17 — stale replacement token accepted (markStale dropped)
 *  M18 — generation-token check dropped from cancel routing
 *
 * Plus the navigateTree summarize:true exemption witness (section 10 contract
 * exemption table row).
 */

import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");

export interface XcDeadlineInputs {
	/** Contents of `packages/extension-host/src/host.ts`. */
	hostSource: string;
	/** Contents of `packages/extension-host/src/lean-runner.ts`. */
	leanSource: string;
	/** Contents of `packages/extension-host/tests/scaling.test.ts`. */
	scalingTestSource: string;
	/** Contents of `packages/extension-host/tests/acceptance.test.ts`. */
	acceptanceTestSource: string;
	/** Contents of `packages/extension-host/tests/host.test.ts`. */
	hostTestSource: string;
	/** Contents of `packages/extension-host/tests/lean.test.ts`. */
	leanTestSource: string;
}

export function loadXcDeadlineInputs(root: string): XcDeadlineInputs {
	const base = join(root, "packages/extension-host");
	return {
		hostSource: readFileSync(join(base, "src/host.ts"), "utf8"),
		leanSource: readFileSync(join(base, "src/lean-runner.ts"), "utf8"),
		scalingTestSource: readFileSync(join(base, "tests/scaling.test.ts"), "utf8"),
		acceptanceTestSource: readFileSync(join(base, "tests/acceptance.test.ts"), "utf8"),
		hostTestSource: readFileSync(join(base, "tests/host.test.ts"), "utf8"),
		leanTestSource: readFileSync(join(base, "tests/lean.test.ts"), "utf8"),
	};
}

// ============================================================================
// 30 s hook deadline constant
// ============================================================================

/**
 * The mutable hook deadline must be pinned at 30 s (30_000 ms).
 *
 * `witness: host.ts::EXTENSION_HOOK_TIMEOUT_MS` (line 108).
 */
export function verifyHookDeadlineConstant(hostSource: string): string[] {
	const violations: string[] = [];

	const match = hostSource.match(/EXTENSION_HOOK_TIMEOUT_MS\s*=\s*([\d_]+)/);
	if (!match) {
		violations.push(
			"EXTENSION_HOOK_TIMEOUT_MS constant is missing from host.ts — " +
				"the 30 s mutable hook deadline is not pinned",
		);
	} else if (Number.parseInt(match[1].replace(/_/g, ""), 10) !== 30_000) {
		violations.push(
			`EXTENSION_HOOK_TIMEOUT_MS is ${match[1]}, expected 30000 — ` +
				"the hook deadline value drifted from 30 s",
		);
	}

	return violations;
}

// ============================================================================
// Input queue capacity-64
// ============================================================================

/**
 * The terminal-input sequential actor queue must be bounded at 64 entries.
 *
 * `witness: host.ts::EXTENSION_INPUT_QUEUE_CAPACITY` (line 118).
 */
export function verifyInputQueueCapacity(hostSource: string): string[] {
	const violations: string[] = [];

	const match = hostSource.match(/EXTENSION_INPUT_QUEUE_CAPACITY\s*=\s*([\d_]+)/);
	if (!match) {
		violations.push(
			"EXTENSION_INPUT_QUEUE_CAPACITY constant is missing from host.ts — " +
				"the capacity-64 terminal-input queue bound is not pinned",
		);
	} else if (Number.parseInt(match[1].replace(/_/g, ""), 10) !== 64) {
		violations.push(
			`EXTENSION_INPUT_QUEUE_CAPACITY is ${match[1]}, expected 64 — ` +
				"the queue capacity drifted from 64",
		);
	}

	return violations;
}

// ============================================================================
// M15: terminal-input 4 ms deadline
// ============================================================================

/**
 * The terminal-input actor must enforce a 4 ms per-handler deadline via a
 * Promise.race timeout. If `EXTENSION_INPUT_TIMEOUT_MS` is removed or the
 * timeout race in `invokeTerminalHandler` is dropped, slow handlers never time
 * out and the scaling p99/deadline assertions become vacuous.
 *
 * `witness: host.ts::EXTENSION_INPUT_TIMEOUT_MS` (line 116),
 * `host.ts::invokeTerminalHandler` timeout race (lines 1445-1450),
 * `scaling.test.ts` deadline assertion (line 539).
 */
export function verifyTerminalInputDeadline(
	hostSource: string,
	scalingTestSource: string,
): string[] {
	const violations: string[] = [];

	// EXTENSION_INPUT_TIMEOUT_MS must be defined and set to 4.
	const timeoutConstMatch = hostSource.match(
		/EXTENSION_INPUT_TIMEOUT_MS\s*=\s*([\d_]+)/,
	);
	if (!timeoutConstMatch) {
		violations.push(
			"EXTENSION_INPUT_TIMEOUT_MS constant is missing from host.ts — " +
				"the terminal-input 4 ms deadline is not pinned",
		);
	} else if (Number.parseInt(timeoutConstMatch[1].replace(/_/g, ""), 10) !== 4) {
		violations.push(
			`EXTENSION_INPUT_TIMEOUT_MS is ${timeoutConstMatch[1]}, expected 4 — ` +
				"the terminal-input deadline value drifted",
		);
	}

	// The timeout race in invokeTerminalHandler must use setTimeout with the
	// constant.  If this is removed, a slow handler blocks forever.
	const timeoutRacePattern =
		/setTimeout\s*\(\s*\(\)\s*=>\s*resolve\s*\(\s*\{\s*kind:\s*"timeout"\s*\}\s*\)\s*,\s*EXTENSION_INPUT_TIMEOUT_MS\s*\)/;
	if (!timeoutRacePattern.test(hostSource)) {
		violations.push(
			"invokeTerminalHandler timeout race is missing — " +
				"the Promise.race against EXTENSION_INPUT_TIMEOUT_MS must fire " +
				"a { kind: 'timeout' } resolver via setTimeout",
		);
	}

	// The scaling test must assert the elapsed time against the deadline.
	const deadlineAssertPattern =
		/toBeGreaterThanOrEqual\s*\(\s*EXTENSION_INPUT_TIMEOUT_MS/;
	if (!deadlineAssertPattern.test(scalingTestSource)) {
		violations.push(
			"scaling.test.ts deadline assertion is missing — " +
				"the slow-handler test must assert firstElapsed >= EXTENSION_INPUT_TIMEOUT_MS",
		);
	}

	// The disabled-on-timeout path must exist.
	const disablePattern = /outcome\.kind\s*===\s*"timeout"\s*\|\|\s*outcome\.kind\s*===\s*"error"/;
	if (!disablePattern.test(hostSource)) {
		violations.push(
			"runTerminalInputHandlers timeout/error disable path is missing — " +
				"a timed-out or throwing handler must be disabled and the " +
				"original key passed through (fail open)",
		);
	}

	return violations;
}

// ============================================================================
// M16: error isolation — hook throw emits extensionError, does not crash host
// ============================================================================

/**
 * A lifecycle hook that throws must be isolated: the host emits an
 * `extensionError` notification and the correlated request returns a
 * non-retryable `extension_error` response. The host process must NOT crash.
 *
 * In host.ts, `handleLifecycleHook` wraps the entire dispatch in a try/catch
 * that calls `respondError` with `extension_error`. In lean-runner.ts,
 * `runHooks` wraps each individual handler in a try/catch that calls
 * `emitExtensionError`, and `handleLifecycleHook` wraps the dispatch in a
 * try/catch that calls `respondError`.
 *
 * `witness: host.ts::handleLifecycleHook` catch (lines 1052-1058),
 * `lean-runner.ts::runHooks` per-handler catch (lines 1593-1599),
 * `lean-runner.ts::handleLifecycleHook` catch (lines 1815-1821),
 * `acceptance.test.ts` crash isolation suite (lines 305-504).
 */
export function verifyErrorIsolation(
	hostSource: string,
	leanSource: string,
	acceptanceTestSource: string,
): string[] {
	const violations: string[] = [];

	// host.ts handleLifecycleHook must have a catch that responds with
	// extension_error (not a rethrow).
	const hostCatchPattern =
		/catch\s*\(\s*err\s*\)\s*\{[^}]*respondError\s*\(\s*id\s*,\s*eventType[^)]*extension_error/;
	if (!hostCatchPattern.test(hostSource)) {
		violations.push(
			"host.ts handleLifecycleHook catch is missing or does not call " +
				"respondError with extension_error — a hook throw would crash " +
				"the host instead of being isolated",
		);
	}

	// lean-runner runHooks must have a per-handler try/catch that calls
	// emitExtensionError (not a rethrow).
	const runHooksCatchPattern =
		/catch\s*\(\s*err\s*\)\s*\{[^}]*emitExtensionError\s*\(\s*extensionPath\s*,\s*eventType/;
	if (!runHooksCatchPattern.test(leanSource)) {
		violations.push(
			"lean-runner.ts runHooks per-handler catch is missing or does not " +
				"call emitExtensionError — a single handler throw would abort " +
				"all remaining handlers instead of being isolated",
		);
	}

	// lean-runner handleLifecycleHook must have a catch that responds with
	// extension_error.
	const leanCatchPattern =
		/catch\s*\(\s*err\s*\)\s*\{[^}]*respondError\s*\(\s*id\s*,\s*eventType[^)]*extension_error/;
	if (!leanCatchPattern.test(leanSource)) {
		violations.push(
			"lean-runner.ts handleLifecycleHook catch is missing or does not " +
				"call respondError with extension_error — a hook throw would " +
				"crash the lean runner instead of being isolated",
		);
	}

	// The acceptance test must contain the crash isolation suite.
	if (!/describe\s*\(\s*["']acceptance:\s*crash isolation["']/.test(acceptanceTestSource)) {
		violations.push(
			"acceptance.test.ts crash isolation suite is missing — " +
				"the 'acceptance: crash isolation' describe block must exist",
		);
	}

	// The acceptance test must assert extensionError with retryable=false.
	if (!/payload\s*\[\s*["']retryable["']\s*\]\s*\)\s*\.toBe\s*\(\s*false/.test(acceptanceTestSource)) {
		violations.push(
			"acceptance.test.ts crash isolation assertion is missing — " +
				"the test must assert retryable === false on the extensionError payload",
		);
	}

	return violations;
}

// ============================================================================
// M17: stale replacement token guard
// ============================================================================

/**
 * After a session replacement (newSession/fork/switchSession/reload), the old
 * command context must be stale. `captureReplacementToken` calls `markStale?.()`
 * before returning the token, and `createCommandContext` has a `guard` that
 * throws `STALE_COMMAND_CONTEXT_MESSAGE` when the stale flag is set.
 *
 * If `markStale?.()` is removed from `captureReplacementToken`, the stale flag
 * never sets and the old context silently accepts the stale replacement token.
 *
 * `witness: host.ts::captureReplacementToken` markStale call (line 2817),
 * `host.ts::createCommandContext` guard (lines 2968-2971),
 * `host.test.ts` per-command replacement staleness suite (lines 1277-1374).
 */
export function verifyStaleReplacementTokenGuard(
	hostSource: string,
	hostTestSource: string,
): string[] {
	const violations: string[] = [];

	// captureReplacementToken must call markStale?.() before the token-shaped
	// early return.
	const markStalePattern = /markStale\s*\?\.\s*\(\s*\)/;
	if (!markStalePattern.test(hostSource)) {
		violations.push(
			"captureReplacementToken markStale call is missing — " +
				"markStale?.() must fire before any token-shaped early return " +
				"so that createCommandContext guards reject a stale context",
		);
	}

	// The markStale call must come BEFORE the token extraction (not after).
	// We check that markStale appears before `payload["replacementToken"]`
	// in the captureReplacementToken function body.
	const captureFnMatch = hostSource.match(
		/captureReplacementToken\s*\([^)]*\)\s*:[^{]*\{([\s\S]*?)\n\t\t\};/,
	);
	if (captureFnMatch) {
		const body = captureFnMatch[1];
		const markStaleIdx = body.search(/markStale\s*\?\.\s*\(\s*\)/);
		const tokenIdx = body.search(/payload\s*\[\s*["']replacementToken["']\s*\]/);
		if (markStaleIdx !== -1 && tokenIdx !== -1 && markStaleIdx > tokenIdx) {
			violations.push(
				"captureReplacementToken markStale call appears AFTER token " +
					"extraction — staleness must be marked before any token-shaped " +
					"early return so the guard fires even when the token is invalid",
			);
		}
	}

	// createCommandContext must have a guard that checks the stale flag.
	const guardPattern = /if\s*\(\s*stale\s*\|\|\s*this\.runner\s*!==\s*runner\s*\)/;
	if (!guardPattern.test(hostSource)) {
		violations.push(
			"createCommandContext stale guard is missing — " +
				"the guard must check `stale || this.runner !== runner` and " +
				"throw STALE_COMMAND_CONTEXT_MESSAGE",
		);
	}

	// STALE_COMMAND_CONTEXT_MESSAGE must be defined.
	if (!/STALE_COMMAND_CONTEXT_MESSAGE\s*=/.test(hostSource)) {
		violations.push(
			"STALE_COMMAND_CONTEXT_MESSAGE constant is missing from host.ts — " +
				"the stale context marker message must be defined and thrown " +
				"verbatim on misuse",
		);
	}

	// The host test must contain the per-command replacement staleness suite.
	if (!/describe\s*\(\s*["']host:\s*per-command replacement staleness["']/.test(hostTestSource)) {
		violations.push(
			"host.test.ts per-command replacement staleness suite is missing — " +
				"the 'host: per-command replacement staleness' describe block " +
				"must exist and assert the stale context message",
		);
	}

	// The host test must assert the stale context message verbatim.
	if (!/staleContextMessage/.test(hostTestSource)) {
		violations.push(
			"host.test.ts stale context message assertion is missing — " +
				"the test must define staleContextMessage and assert it against " +
				"the error payload",
		);
	}

	return violations;
}

// ============================================================================
// M18: cancel routing — request-id keyed abort
// ============================================================================

/**
 * `tool.cancel` and `provider.cancel` events must route the abort to the
 * specific in-flight controller keyed by request id. The handler must extract
 * `requestId` from the payload, guard against `undefined`, and call
 * `.abort()` only on the matching `inFlightTools`/`inFlightProviders` entry.
 *
 * If the request-id extraction is dropped (aborting all controllers or
 * aborting without a key), an unrelated cancel event would abort a different
 * in-flight request — a cancel race.
 *
 * `witness: host.ts::handleControlEvent` (lines 2191-2198),
 * `lean-runner.ts::handleControlEvent` (lines 1916-1924),
 * `lean.test.ts` tool.cancel tests (lines 924-1044).
 */
export function verifyCancelRouting(
	hostSource: string,
	leanSource: string,
	leanTestSource: string,
): string[] {
	const violations: string[] = [];

	// Both host.ts and lean-runner.ts must have the cancel method guard.
	for (const [label, source] of [["host.ts", hostSource], ["lean-runner.ts", leanSource]] as const) {
		// Must check for tool.cancel and provider.cancel method names.
		const methodGuardPattern =
			/frame\.method\s*!==\s*["']tool\.cancel["']\s*&&\s*frame\.method\s*!==\s*["']provider\.cancel["']/;
		if (!methodGuardPattern.test(source)) {
			violations.push(
				`${label} handleControlEvent cancel method guard is missing — ` +
					"must check `frame.method !== 'tool.cancel' && frame.method !== 'provider.cancel'",
			);
		}

		// Must extract requestId from payload["id"] as a number.
		const requestIdPattern =
			/typeof\s+payload\s*\[\s*["']id["']\s*\]\s*===\s*["']number["']/;
		if (!requestIdPattern.test(source)) {
			violations.push(
				`${label} handleControlEvent requestId extraction is missing — ` +
					"must extract `typeof payload['id'] === 'number'` and guard " +
					"against undefined before aborting",
			);
		}

		// Must guard requestId === undefined with early return.
		const undefinedGuardPattern = /if\s*\(\s*requestId\s*===\s*undefined\s*\)\s*return/;
		if (!undefinedGuardPattern.test(source)) {
			violations.push(
				`${label} handleControlEvent undefined-requestId guard is missing — ` +
					"must return early when requestId is undefined to prevent " +
					"aborting the wrong controller (cancel race)",
			);
		}

		// Must abort the specific controller by requestId, not all controllers.
		const keyedAbortPattern = /inFlightTools\.get\s*\(\s*requestId\s*\)\s*\?\.\s*abort\s*\(\s*\)/;
		if (!keyedAbortPattern.test(source)) {
			violations.push(
				`${label} handleControlEvent keyed abort is missing — ` +
					"must call `inFlightTools.get(requestId)?.abort()` to abort " +
					"only the matching controller, not all in-flight requests",
			);
		}

		// Must also abort inFlightProviders by requestId.
		const providerAbortPattern = /inFlightProviders\.get\s*\(\s*requestId\s*\)\s*\?\.\s*abort\s*\(\s*\)/;
		if (!providerAbortPattern.test(source)) {
			violations.push(
				`${label} handleControlEvent provider keyed abort is missing — ` +
					"must call `inFlightProviders.get(requestId)?.abort()` to abort " +
					"only the matching provider controller",
			);
		}
	}

	// The lean test must contain the tool.cancel test.
	if (!/tool\.execute honors tool\.cancel with a cancelled error frame/.test(leanTestSource)) {
		violations.push(
			"lean.test.ts tool.cancel test is missing — " +
				"the 'tool.execute honors tool.cancel with a cancelled error frame' " +
				"test must exist to witness the cancel routing",
		);
	}

	// The lean test must assert the cancelled code.
	if (!/err\s*\[\s*["']code["']\s*\]\s*\)\s*\.toBe\s*\(\s*["']cancelled["']/.test(leanTestSource)) {
		violations.push(
			"lean.test.ts cancelled-code assertion is missing — " +
				"the cancel test must assert err['code'] === 'cancelled'",
		);
	}

	return violations;
}

// ============================================================================
// navigateTree summarize:true exemption
// ============================================================================

/**
 * `navigateTree` with `summarize: true` deliberately omits the 30 s hook
 * deadline because a provider-backed branch summary can legitimately exceed
 * it. Only non-summarizing navigation stays under the generic timeout.
 *
 * The exemption is witnessed by the conditional timeout options:
 * `summarize ? {} : { timeoutMs: EXTENSION_HOOK_TIMEOUT_MS }`.
 *
 * `witness: host.ts::navigateTree` (lines 2927-2930).
 */
export function verifyNavigateTreeSummarizeExemption(
	hostSource: string,
): string[] {
	const violations: string[] = [];

	// The navigateTree request must conditionally apply the timeout based on
	// the summarize flag.
	const conditionalTimeoutPattern =
		/summarize\s*\?\s*\{\s*\}\s*:\s*\{\s*timeoutMs:\s*EXTENSION_HOOK_TIMEOUT_MS\s*\}/;
	if (!conditionalTimeoutPattern.test(hostSource)) {
		violations.push(
			"navigateTree summarize exemption is missing — " +
				"the request must use `summarize ? {} : { timeoutMs: EXTENSION_HOOK_TIMEOUT_MS }` " +
				"so summarized navigation is exempt from the 30 s hook deadline " +
				"while non-summarizing navigation stays bounded",
		);
	}

	// The summarize flag must be extracted from options.
	const summarizeFlagPattern = /options\s*\?\.\s*summarize\s*===\s*true/;
	if (!summarizeFlagPattern.test(hostSource)) {
		violations.push(
			"navigateTree summarize flag extraction is missing — " +
				"must read `options?.summarize === true` to determine whether " +
				"the 30 s deadline exemption applies",
		);
	}

	// The intent comment must exist explaining why the exemption is deliberate.
	// The comment spans multiple lines, so match the first line.
	const intentCommentPattern =
		/Summarized navigation delegates to a provider-backed branch/;
	if (!intentCommentPattern.test(hostSource)) {
		violations.push(
			"navigateTree summarize exemption intent comment is missing — " +
				"the code must document that the exemption is deliberate: " +
				"a provider-backed branch summary can legitimately exceed the " +
				"30 s hook deadline",
		);
	}

	return violations;
}

// ============================================================================
// Orchestration
// ============================================================================

/** Run every XC-8 witness; empty means green. */
export function runXcDeadlineWitnesses(inputs: XcDeadlineInputs): string[] {
	return [
		...verifyHookDeadlineConstant(inputs.hostSource),
		...verifyInputQueueCapacity(inputs.hostSource),
		...verifyTerminalInputDeadline(inputs.hostSource, inputs.scalingTestSource),
		...verifyErrorIsolation(inputs.hostSource, inputs.leanSource, inputs.acceptanceTestSource),
		...verifyStaleReplacementTokenGuard(inputs.hostSource, inputs.hostTestSource),
		...verifyCancelRouting(inputs.hostSource, inputs.leanSource, inputs.leanTestSource),
		...verifyNavigateTreeSummarizeExemption(inputs.hostSource),
	];
}

if (import.meta.main) {
	const inputs = loadXcDeadlineInputs(REPO_ROOT);
	const violations = runXcDeadlineWitnesses(inputs);
	if (violations.length > 0) {
		console.error("XC-8 deadline/cancellation/isolation/stale-guard witness violations:");
		for (const v of violations) {
			console.error(`  - ${v}`);
		}
		process.exit(1);
	}
	console.error("XC-8 deadline/cancellation/isolation/stale-guard witnesses: all green");
}
